/*
 * Unit tests for setup over USB (dh_setup, dh_usb, dh_usb_line): every rule a bundle must keep,
 * every refusal in its order, failing writes, the reply lines against protocol/v1/examples/usb,
 * and the setup's stored form. The store is a fake that keeps what a real one would keep in
 * flash, and can be told to fail.
 */
#include <inttypes.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "dh_setup.h"
#include "dh_usb.h"
#include "dh_usb_line.h"

static int checks, failures;

#define CHECK(c) \
    do { \
        checks++; \
        if (!(c)) { \
            failures++; \
            fprintf(stderr, "%s:%d: CHECK(%s) failed\n", __FILE__, __LINE__, #c); \
        } \
    } while (0)

#define LEN(a) (sizeof(a) / sizeof((a)[0]))

/* A failure found inside a loop, reported once with its own words. */
#define FAIL(what) \
    do { \
        checks++; \
        failures++; \
        fprintf(stderr, "%s:%d: %s\n", __FILE__, __LINE__, what); \
    } while (0)

/* Three public keys from protocol/v1/firmware's vectors (maker, recovery, stranger), and one that
 * has the right form but isn't a point on the curve. */
#define MAKER \
    "04795c666d0cdda58bfb27c60b936bba8ad0c8801218dcb6d5f50a73c00a9904229fa366102a70677d0ca541" \
    "0d6300b8273e102bba8920cef822483339734b7eb3"
#define RECOVERY \
    "049c4b8faad133bdb521d9e5f419343b0f5579d64ae71a88227b19d0b07b59c7d047f5f695a9671d01043223" \
    "ac9cf38246e8d54f3e47cc9832b68efba12d918f30"
#define STRANGER \
    "04d60f588ff97c238828d8b662100faf6cdc3a873a57458203b81eb0ccc498d8518590614975395bf016774b" \
    "5570df2ad3f95b46e6107ae5ab5d9319e40711849c"
#define NOT_A_POINT \
    "04000000000000000000000000000000000000000000000000000000000000000000000000000000000000" \
    "00000000000000000000000000000000000000000000"

static void unhex(const char *hex, uint8_t *out, size_t n)
{
    for (size_t i = 0; i < n; i++) {
        unsigned v = 0;
        if (sscanf(hex + 2 * i, "%2x", &v) != 1) {
            fprintf(stderr, "bad hex in a test\n");
            exit(1);
        }
        out[i] = (uint8_t)v;
    }
}

static dh_fw_key key_of(const char *hex)
{
    dh_fw_key k;
    unhex(hex, k.bytes, DH_FW_KEY_LEN);
    return k;
}

/* ---- The fake store ---- */

typedef struct {
    /* "Flash". */
    bool has_setup;
    size_t setup_len;
    uint8_t setup[DH_SETUP_BLOB_MAX];
    uint8_t keys[DH_FW_KEYS_BLOB_LEN];
    uint8_t record[DH_FW_RECORD_BLOB_LEN];
    /* Test switches and counters. */
    bool unreadable;
    bool restart_needed;
    bool fail_keys, fail_record, fail_setup, fail_erase;
    unsigned key_writes, record_writes, setup_writes, erases;
} fake_store;

static dh_held fake_held(void *ctx)
{
    fake_store *f = ctx;
    if (f->unreadable) {
        return DH_HELD_UNKNOWN;
    }
    if (f->has_setup) {
        return DH_HELD_SETUP;
    }
    return f->restart_needed ? DH_HELD_RESTART_NEEDED : DH_HELD_NOTHING;
}

static bool fake_save_keys(void *ctx, const dh_fw_keys *keys)
{
    fake_store *f = ctx;
    f->key_writes++;
    if (f->fail_keys) {
        return false;
    }
    dh_fw_keys_encode(keys, f->keys);
    return true;
}

static bool fake_save_record(void *ctx, const dh_fw_record *record)
{
    fake_store *f = ctx;
    f->record_writes++;
    if (f->fail_record) {
        return false;
    }
    dh_fw_record_encode(record, f->record);
    return true;
}

static bool fake_save_setup(void *ctx, const dh_setup *setup)
{
    fake_store *f = ctx;
    f->setup_writes++;
    if (f->fail_setup) {
        return false;
    }
    f->setup_len = dh_setup_encode(setup, f->setup);
    f->has_setup = f->setup_len > 0;
    return f->has_setup;
}

static bool fake_erase_setup(void *ctx)
{
    fake_store *f = ctx;
    f->erases++;
    if (f->fail_erase) {
        return false;
    }
    f->has_setup = false;
    f->setup_len = 0;
    memset(f->setup, 0, sizeof f->setup);
    return true;
}

/* A display: the fake flash, what is in memory, and the handler's view of both. */
typedef struct {
    fake_store flash;
    dh_setup_store store;
    dh_fw_keys keys;
    dh_fw_record record;
    dh_setup work;
    dh_usb_display usb;
    char reply[DH_USB_REPLY_MAX];
} display;

/* A display fresh from a first install: no setup, no keys, a full budget. */
static display *new_display(void)
{
    static display d;
    memset(&d, 0, sizeof d);
    dh_fw_keys_init(&d.keys);
    dh_fw_record_init(&d.record);
    dh_fw_keys_encode(&d.keys, d.flash.keys);
    dh_fw_record_encode(&d.record, d.flash.record);
    d.store = (dh_setup_store){&d.flash,         fake_held,       fake_save_keys,
                               fake_save_record, fake_save_setup, fake_erase_setup};
    d.usb = (dh_usb_display){"display-hub-waveshare-amoled-1.8", "0.1.0",
                             "waveshare-amoled-1.8-v2", 6291456, &d.store, &d.keys, &d.record,
                             &d.work, false};
    return &d;
}

/* Gives the display keys and a floor, in memory and in flash. */
static void give_keys(display *d, const char *a, const char *b, uint32_t floor)
{
    dh_fw_keys_init(&d->keys);
    d->keys.keys[d->keys.key_count++] = key_of(a);
    if (b != NULL) {
        d->keys.keys[d->keys.key_count++] = key_of(b);
    }
    d->keys.security_floor = floor;
    dh_fw_keys_encode(&d->keys, d->flash.keys);
}

static void spend_budget(display *d, uint8_t left)
{
    d->record.attempts_left = left;
    d->record.credit_hours = left < DH_FW_FULL_BUDGET ? 2 : 0;
    dh_fw_record_encode(&d->record, d->flash.record);
}

/* Whether what is in memory is exactly what is in flash. */
static bool memory_matches_flash(const display *d)
{
    uint8_t keys[DH_FW_KEYS_BLOB_LEN], record[DH_FW_RECORD_BLOB_LEN];
    dh_fw_keys_encode(&d->keys, keys);
    dh_fw_record_encode(&d->record, record);
    return memcmp(keys, d->flash.keys, sizeof keys) == 0 &&
           memcmp(record, d->flash.record, sizeof record) == 0;
}

static dh_usb_answer send(display *d, const char *line)
{
    return dh_usb_handle_line(&d->usb, (const uint8_t *)line, strlen(line), d->reply,
                              sizeof d->reply);
}

static bool refused(display *d, const char *line, dh_usb_error error)
{
    dh_usb_answer a = send(d, line);
    char start[96];
    snprintf(start, sizeof start, "{\"type\":\"error\",\"code\":\"%s\",\"message\":\"",
             dh_usb_error_name(error));
    return a.outcome == DH_USB_REFUSED && a.error == error && a.why != NULL &&
           strlen(a.why) <= 200 && a.reply_len == strlen(d->reply) &&
           strncmp(d->reply, start, strlen(start)) == 0 && d->reply[a.reply_len - 1] == '\n';
}

static bool ignored(display *d, const char *line)
{
    dh_usb_answer a = send(d, line);
    return a.outcome == DH_USB_IGNORED && a.reply_len == 0 && d->reply[0] == '\0';
}

static bool provisioned(display *d, const char *line)
{
    dh_usb_answer a = send(d, line);
    return a.outcome == DH_USB_PROVISIONED &&
           strcmp(d->reply, "{\"type\":\"provisioned\"}\n") == 0 &&
           a.reply_len == strlen(d->reply) && d->flash.has_setup;
}

/* ---- Bundles ---- */

/* Each part is JSON text, so a test can put anything in its place. NULL leaves a member out
 * where a bundle may. */
typedef struct {
    const char *format, *version, *hosts, *port, *server_name, *anchors, *secret, *expires_at,
        *wifi, *firmware;
} parts;

#define PEM "-----BEGIN CERTIFICATE-----\\nMIIBkzCCATig\\n-----END CERTIFICATE-----\\n"
#define SPKI "9a54396d429aa58fc9ac78413a9e84a0b7a45f125256f91697c977c2acbfde88"
#define ANCHOR "{\"certificate_pem\":\"" PEM "\",\"spki_sha256\":\"" SPKI "\"}"

static parts base(void)
{
    parts p = {"\"display-hub.provisioning\"",
               "1",
               "[\"raspberrypi.local\",\"192.168.1.20\"]",
               "8443",
               "\"hub.display-hub.internal\"",
               "[" ANCHOR "]",
               "\"q3H7cX0f2pS9L1mB8vN4wA\"",
               "\"2026-11-10T20:10:00Z\"",
               "{\"ssid\":\"Example network\",\"password\":\"example-passphrase\"}",
               NULL};
    return p;
}

/* The bundle alone, compactly and in the reference's member order: when nothing in it needs an
 * escape written differently, its length is the length the size rule counts. */
static size_t bundle_text(const parts *p, char *out, size_t cap)
{
    int n = snprintf(out, cap,
                     "{\"format\":%s,\"version\":%s,\"hub\":{\"hosts\":%s,\"port\":%s,"
                     "\"server_name\":%s,\"trust_anchors\":%s},\"enrolment\":{\"secret\":%s,"
                     "\"expires_at\":%s},\"wifi\":%s%s%s}",
                     p->format, p->version, p->hosts, p->port, p->server_name, p->anchors,
                     p->secret, p->expires_at, p->wifi, p->firmware ? ",\"firmware\":" : "",
                     p->firmware ? p->firmware : "");
    if (n < 0 || (size_t)n >= cap) {
        fprintf(stderr, "a test bundle is too long for its buffer\n");
        exit(1);
    }
    return (size_t)n;
}

static char line_buf[DH_MAX_USB_LINE_BYTES + 64];

static const char *line_of(const parts *p)
{
    static char bundle[DH_MAX_USB_LINE_BYTES];
    bundle_text(p, bundle, sizeof bundle);
    snprintf(line_buf, sizeof line_buf, "{\"type\":\"provision\",\"bundle\":%s}", bundle);
    return line_buf;
}

/* The base line with the first `find` swapped for `with`. */
static const char *swapped(const char *find, const char *with)
{
    static char out[DH_MAX_USB_LINE_BYTES];
    parts p = base();
    const char *line = line_of(&p);
    const char *at = strstr(line, find);
    if (at == NULL) {
        fprintf(stderr, "a test looked for text the base bundle doesn't have: %s\n", find);
        exit(1);
    }
    snprintf(out, sizeof out, "%.*s%s%s", (int)(at - line), line, with, at + strlen(find));
    return out;
}

/* n copies of c as a JSON string, with a prefix. */
static const char *long_string(const char *prefix, char c, size_t n)
{
    static char out[4][DH_MAX_USB_LINE_BYTES];
    static unsigned turn;
    char *s = out[turn++ % 4];
    size_t at = (size_t)snprintf(s, DH_MAX_USB_LINE_BYTES, "\"%s", prefix);
    if (at + n + 2 > DH_MAX_USB_LINE_BYTES) {
        fprintf(stderr, "a test string is too long\n");
        exit(1);
    }
    memset(s + at, c, n);
    s[at + n] = '"';
    s[at + n + 1] = '\0';
    return s;
}

/* ---- The tests ---- */

typedef enum { ACCEPT, INVALID, IGNORE } verdict;

static void expect_at(int at, const parts *p, verdict want, const char *problem)
{
    display *d = new_display();
    const char *line = line_of(p);
    dh_usb_answer a = send(d, line);
    bool ok;
    switch (want) {
    case ACCEPT:
        ok = a.outcome == DH_USB_PROVISIONED && d->flash.has_setup;
        break;
    case INVALID:
        ok = a.outcome == DH_USB_REFUSED && a.error == DH_USB_ERROR_INVALID_BUNDLE &&
             a.why != NULL && (problem == NULL || strstr(a.why, problem) != NULL) &&
             !d->flash.has_setup && d->flash.setup_writes == 0 && d->flash.key_writes == 0;
        break;
    default:
        ok = a.outcome == DH_USB_IGNORED && a.reply_len == 0 && !d->flash.has_setup;
        break;
    }
    checks++;
    if (!ok) {
        failures++;
        fprintf(stderr, "%s:%d: a bundle wasn't %s (outcome %d, error %d, why %s)\n", __FILE__,
                at, want == ACCEPT ? "accepted" : want == INVALID ? "invalid" : "ignored",
                (int)a.outcome, (int)a.error, a.why ? a.why : "-");
    }
    /* Whatever happened, the scratch holds a setup only after one was stored. */
    if (want != ACCEPT) {
        static const dh_setup zero;
        CHECK(memcmp(&d->work, &zero, sizeof zero) == 0);
    }
}

#define EXPECT(p, want, problem) expect_at(__LINE__, p, want, problem)

static void test_every_rule(void)
{
    parts p = base();
    EXPECT(&p, ACCEPT, NULL);

    /* format and version */
    p = base(), p.format = "\"display-hub.other\"";
    EXPECT(&p, INVALID, "not a display-hub.provisioning version 1 bundle");
    p = base(), p.version = "2";
    EXPECT(&p, INVALID, "not a display-hub.provisioning version 1 bundle");
    p = base(), p.version = "0";
    EXPECT(&p, INVALID, "version 1");
    p = base(), p.version = "4294967295";
    EXPECT(&p, INVALID, "version 1");
    /* What the reference can't read into its types is ignored, not refused. */
    p = base(), p.version = "4294967296";
    EXPECT(&p, IGNORE, NULL);
    p = base(), p.version = "1.0";
    EXPECT(&p, IGNORE, NULL);
    p = base(), p.version = "-1";
    EXPECT(&p, IGNORE, NULL);
    p = base(), p.version = "\"1\"";
    EXPECT(&p, IGNORE, NULL);
    p = base(), p.format = "1";
    EXPECT(&p, IGNORE, NULL);

    /* hosts */
    p = base(), p.hosts = "[]";
    EXPECT(&p, INVALID, "1-4 hub hosts");
    p = base(), p.hosts = "[\"a\",\"b\",\"c\",\"d\"]";
    EXPECT(&p, ACCEPT, NULL);
    p = base(), p.hosts = "[\"a\",\"b\",\"c\",\"d\",\"e\"]";
    EXPECT(&p, INVALID, "1-4 hub hosts");
    p = base(), p.hosts = "[\"\"]";
    EXPECT(&p, INVALID, "plain host names");
    static char hosts[600];
    snprintf(hosts, sizeof hosts, "[%s]", long_string("", 'h', 253));
    p = base(), p.hosts = hosts;
    EXPECT(&p, ACCEPT, NULL);
    snprintf(hosts, sizeof hosts, "[%s]", long_string("", 'h', 254));
    p = base(), p.hosts = hosts;
    EXPECT(&p, INVALID, "plain host names");
    static const char *const bad_hosts[] = {
        "[\"a b\"]",      "[\"a\\tb\"]",     "[\"a\\nb\"]",     "[\"a\\u0000b\"]",
        "[\"a\\u001fb\"]", "[\"a\\u007fb\"]", "[\"a\\u0085b\"]", "[\"a\\u009fb\"]",
        "[\"a\\u00a0b\"]", "[\"a\\u2003b\"]", "[\"a\\u3000b\"]", "[\"ok\",\" \"]",
    };
    for (size_t i = 0; i < LEN(bad_hosts); i++) {
        p = base(), p.hosts = bad_hosts[i];
        EXPECT(&p, INVALID, "plain host names");
    }
    /* Not white space or a control character, so allowed, as in the reference. */
    p = base(), p.hosts = "[\"h\\u00e9b.local\",\"[fe80::1]\",\"a\\u200bb\"]";
    EXPECT(&p, ACCEPT, NULL);
    p = base(), p.hosts = "[1]";
    EXPECT(&p, IGNORE, NULL);
    p = base(), p.hosts = "\"raspberrypi.local\"";
    EXPECT(&p, IGNORE, NULL);

    /* port */
    p = base(), p.port = "0";
    EXPECT(&p, INVALID, "hub port must not be 0");
    p = base(), p.port = "1";
    EXPECT(&p, ACCEPT, NULL);
    p = base(), p.port = "65535";
    EXPECT(&p, ACCEPT, NULL);
    p = base(), p.port = "65536";
    EXPECT(&p, IGNORE, NULL);
    p = base(), p.port = "8443.0";
    EXPECT(&p, IGNORE, NULL);
    p = base(), p.port = "\"8443\"";
    EXPECT(&p, IGNORE, NULL);

    /* server_name */
    p = base(), p.server_name = "\"hub.display-hub.example\"";
    EXPECT(&p, INVALID, "server_name must be hub.display-hub.internal");
    p = base(), p.server_name = "\"HUB.display-hub.internal\"";
    EXPECT(&p, INVALID, "server_name");
    p = base(), p.server_name = "\"\"";
    EXPECT(&p, INVALID, "server_name");

    /* trust anchors */
    p = base(), p.anchors = "[]";
    EXPECT(&p, INVALID, "1-2 trust anchors");
    p = base(), p.anchors = "[" ANCHOR "," ANCHOR "]";
    EXPECT(&p, ACCEPT, NULL);
    p = base(), p.anchors = "[" ANCHOR "," ANCHOR "," ANCHOR "]";
    EXPECT(&p, INVALID, "1-2 trust anchors");
    p = base(), p.anchors = "[{\"certificate_pem\":\"MIIB\",\"spki_sha256\":\"" SPKI "\"}]";
    EXPECT(&p, INVALID, "PEM certificates");
    p = base(),
    p.anchors = "[{\"certificate_pem\":\" -----BEGIN CERTIFICATE-----\",\"spki_sha256\":\"" SPKI
                "\"}]";
    EXPECT(&p, INVALID, "PEM certificates");
    p = base(),
    p.anchors = "[{\"certificate_pem\":\"-----BEGIN CERTIFICATE-----\",\"spki_sha256\":\"" SPKI
                "\"}]";
    EXPECT(&p, ACCEPT, NULL);
    static char anchors[2 * DH_SETUP_MAX_PEM_BYTES + 600];
    snprintf(anchors, sizeof anchors, "[{\"certificate_pem\":%s,\"spki_sha256\":\"" SPKI "\"}]",
             long_string("-----BEGIN CERTIFICATE-----", 'A', DH_SETUP_MAX_PEM_BYTES - 27));
    p = base(), p.anchors = anchors;
    EXPECT(&p, ACCEPT, NULL);
    snprintf(anchors, sizeof anchors, "[{\"certificate_pem\":%s,\"spki_sha256\":\"" SPKI "\"}]",
             long_string("-----BEGIN CERTIFICATE-----", 'A', DH_SETUP_MAX_PEM_BYTES - 26));
    p = base(), p.anchors = anchors;
    EXPECT(&p, INVALID, "PEM certificates");
    /* The second anchor is checked as well as the first. */
    p = base(),
    p.anchors = "[" ANCHOR ",{\"certificate_pem\":\"nope\",\"spki_sha256\":\"" SPKI "\"}]";
    EXPECT(&p, INVALID, "PEM certificates");
    static const char *const bad_spki[] = {
        "",
        "9a54396d429aa58fc9ac78413a9e84a0b7a45f125256f91697c977c2acbfde8",
        "9a54396d429aa58fc9ac78413a9e84a0b7a45f125256f91697c977c2acbfde888",
        "9A54396d429aa58fc9ac78413a9e84a0b7a45f125256f91697c977c2acbfde88",
        "9g54396d429aa58fc9ac78413a9e84a0b7a45f125256f91697c977c2acbfde88",
    };
    for (size_t i = 0; i < LEN(bad_spki); i++) {
        snprintf(anchors, sizeof anchors,
                 "[{\"certificate_pem\":\"" PEM "\",\"spki_sha256\":\"%s\"}]", bad_spki[i]);
        p = base(), p.anchors = anchors;
        EXPECT(&p, INVALID, "spki_sha256 must be 64 lower-case hex digits");
    }
    p = base(), p.anchors = "[{\"certificate_pem\":\"" PEM "\"}]";
    EXPECT(&p, IGNORE, NULL);
    p = base(), p.anchors = "[{\"spki_sha256\":\"" SPKI "\"}]";
    EXPECT(&p, IGNORE, NULL);
    p = base(), p.anchors = "[\"" PEM "\"]";
    EXPECT(&p, IGNORE, NULL);
    p = base(), p.anchors = "{}";
    EXPECT(&p, IGNORE, NULL);

    /* the enrolment secret and its date */
    p = base(), p.secret = "\"\"";
    EXPECT(&p, INVALID, "enrolment secret");
    p = base(), p.secret = "\"x\"";
    EXPECT(&p, ACCEPT, NULL);
    p = base(), p.secret = long_string("", 's', 64);
    EXPECT(&p, ACCEPT, NULL);
    p = base(), p.secret = long_string("", 's', 65);
    EXPECT(&p, INVALID, "enrolment secret");
    p = base(), p.secret = "7";
    EXPECT(&p, IGNORE, NULL);
    static const char *const bad_dates[] = {
        "\"2026-11-10T20:10:00.5Z\"", "\"2026-11-10T20:10:00+00:00\"", "\"2026-11-10 20:10:00Z\"",
        "\"2026-11-10T20:10:00z\"",   "\"2026-13-10T20:10:00Z\"",      "\"2026-02-30T20:10:00Z\"",
        "\"\"",                       "20261110",                      "null",
    };
    for (size_t i = 0; i < LEN(bad_dates); i++) {
        p = base(), p.expires_at = bad_dates[i];
        EXPECT(&p, IGNORE, NULL);
    }
    /* A date in the past is still a date: whether the secret has run out is the hub's to say. */
    p = base(), p.expires_at = "\"1999-12-31T23:59:59Z\"";
    EXPECT(&p, ACCEPT, NULL);

    /* Wi-Fi */
    p = base(), p.wifi = "null";
    EXPECT(&p, ACCEPT, NULL);
    p = base(), p.wifi = "{\"ssid\":\"\",\"password\":\"example-passphrase\"}";
    EXPECT(&p, INVALID, "Wi-Fi network name must be 1-32 bytes");
    static char wifi[400];
    snprintf(wifi, sizeof wifi, "{\"ssid\":%s,\"password\":\"\"}", long_string("", 'n', 32));
    p = base(), p.wifi = wifi;
    EXPECT(&p, ACCEPT, NULL);
    snprintf(wifi, sizeof wifi, "{\"ssid\":%s,\"password\":\"\"}", long_string("", 'n', 33));
    p = base(), p.wifi = wifi;
    EXPECT(&p, INVALID, "Wi-Fi network name");
    /* Bytes, not characters: eleven three-byte characters are 33 bytes. */
    p = base(),
    p.wifi = "{\"ssid\":\"\\u20ac\\u20ac\\u20ac\\u20ac\\u20ac\\u20ac\\u20ac\\u20ac\\u20ac\\u20ac"
             "\\u20ac\",\"password\":\"\"}";
    EXPECT(&p, INVALID, "Wi-Fi network name");
    static const struct {
        size_t len;
        verdict want;
    } passwords[] = {{0, ACCEPT},  {1, INVALID}, {7, INVALID},
                     {8, ACCEPT},  {63, ACCEPT}, {64, INVALID}};
    for (size_t i = 0; i < LEN(passwords); i++) {
        snprintf(wifi, sizeof wifi, "{\"ssid\":\"n\",\"password\":%s}",
                 long_string("", 'p', passwords[i].len));
        p = base(), p.wifi = wifi;
        EXPECT(&p, passwords[i].want, passwords[i].want == INVALID ? "Wi-Fi password" : NULL);
    }
    p = base(), p.wifi = "{\"ssid\":\"n\"}";
    EXPECT(&p, IGNORE, NULL);
    p = base(), p.wifi = "{\"password\":\"\"}";
    EXPECT(&p, IGNORE, NULL);
    p = base(), p.wifi = "{\"ssid\":1,\"password\":\"\"}";
    EXPECT(&p, IGNORE, NULL);
    p = base(), p.wifi = "false";
    EXPECT(&p, IGNORE, NULL);
    p = base(), p.wifi = "[]";
    EXPECT(&p, IGNORE, NULL);

    /* the firmware member's own rules */
    p = base(), p.firmware = "{\"trusted_keys\":[\"" MAKER "\"],\"security_counter\":0}";
    EXPECT(&p, ACCEPT, NULL);
    p = base(),
    p.firmware = "{\"trusted_keys\":[\"" MAKER "\",\"" RECOVERY "\",\"" STRANGER
                 "\"],\"security_counter\":4294967295}";
    EXPECT(&p, ACCEPT, NULL);
    p = base(), p.firmware = "null";
    EXPECT(&p, ACCEPT, NULL);
    p = base(), p.firmware = "{\"trusted_keys\":[],\"security_counter\":0}";
    EXPECT(&p, INVALID, "firmware.trusted_keys");
    p = base(),
    p.firmware = "{\"trusted_keys\":[\"" MAKER "\",\"" RECOVERY "\",\"" STRANGER "\",\"" NOT_A_POINT
                 "\"],\"security_counter\":0}";
    EXPECT(&p, INVALID, "firmware.trusted_keys");
    p = base(),
    p.firmware = "{\"trusted_keys\":[\"" MAKER "\",\"" MAKER "\"],\"security_counter\":0}";
    EXPECT(&p, INVALID, "firmware.trusted_keys");
    p = base(), p.firmware = "{\"trusted_keys\":[\"04\"],\"security_counter\":0}";
    EXPECT(&p, INVALID, "firmware.trusted_keys");
    static char fw[600];
    snprintf(fw, sizeof fw, "{\"trusted_keys\":[\"05%s\"],\"security_counter\":0}", &MAKER[2]);
    p = base(), p.firmware = fw;
    EXPECT(&p, INVALID, "firmware.trusted_keys");
    snprintf(fw, sizeof fw, "{\"trusted_keys\":[\"04A%s\"],\"security_counter\":0}", &MAKER[3]);
    p = base(), p.firmware = fw;
    EXPECT(&p, INVALID, "firmware.trusted_keys");
    p = base(), p.firmware = "{\"trusted_keys\":[\"" MAKER "\"],\"security_counter\":4294967296}";
    EXPECT(&p, IGNORE, NULL);
    p = base(), p.firmware = "{\"trusted_keys\":[\"" MAKER "\"]}";
    EXPECT(&p, IGNORE, NULL);
    p = base(), p.firmware = "{\"security_counter\":0}";
    EXPECT(&p, IGNORE, NULL);
    p = base(), p.firmware = "{\"trusted_keys\":[7],\"security_counter\":0}";
    EXPECT(&p, IGNORE, NULL);
    p = base(), p.firmware = "[]";
    EXPECT(&p, IGNORE, NULL);

    /* the first broken rule is the one named */
    p = base(), p.version = "2", p.hosts = "[]", p.port = "0";
    EXPECT(&p, INVALID, "version 1");
    p = base(), p.hosts = "[]", p.port = "0";
    EXPECT(&p, INVALID, "hub hosts");
    p = base(), p.port = "0", p.server_name = "\"x\"";
    EXPECT(&p, INVALID, "port");
}

static void test_missing_repeated_and_unknown_members(void)
{
    display *d;
    static const char *const needed[] = {
        "\"format\":\"display-hub.provisioning\",",
        "\"version\":1,",
        "\"hosts\":[\"raspberrypi.local\",\"192.168.1.20\"],",
        "\"port\":8443,",
        "\"server_name\":\"hub.display-hub.internal\",",
        "\"secret\":\"q3H7cX0f2pS9L1mB8vN4wA\",",
    };
    for (size_t i = 0; i < LEN(needed); i++) {
        d = new_display();
        CHECK(ignored(d, swapped(needed[i], "")));
        /* A member twice is unreadable too, as serde says "duplicate field". */
        char twice[200];
        snprintf(twice, sizeof twice, "%s%s", needed[i], needed[i]);
        d = new_display();
        CHECK(ignored(d, swapped(needed[i], twice)));
    }
    d = new_display();
    CHECK(ignored(d, swapped(",\"trust_anchors\":[" ANCHOR "]", "")));
    d = new_display();
    CHECK(ignored(d, swapped(",\"expires_at\":\"2026-11-10T20:10:00Z\"", "")));
    d = new_display();
    CHECK(ignored(d, swapped("\"type\":\"provision\",", "")));
    d = new_display();
    CHECK(ignored(d, swapped("\"type\":\"provision\",",
                             "\"type\":\"provision\",\"type\":\"provision\",")));
    d = new_display();
    CHECK(ignored(d, swapped("\"type\":\"provision\"", "\"type\":\"provisions\"")));
    d = new_display();
    CHECK(ignored(d, swapped("\"bundle\":", "\"bundles\":")));
    d = new_display();
    CHECK(ignored(d, "{\"type\":\"provision\",\"bundle\":null}"));
    d = new_display();
    CHECK(ignored(d, "{\"type\":\"provision\",\"bundle\":[]}"));
    d = new_display();
    CHECK(ignored(d, "{\"type\":\"provision\"}"));

    /* wifi may be left out altogether, as it may be null: the reference reads both as none. */
    d = new_display();
    CHECK(provisioned(
        d, swapped(",\"wifi\":{\"ssid\":\"Example network\",\"password\":\"example-passphrase\"}",
                   "")));
    CHECK(!d->work.has_wifi);
    /* Twice is still unreadable. */
    d = new_display();
    CHECK(ignored(d, swapped(",\"wifi\":", ",\"wifi\":null,\"wifi\":")));

    /* Unknown members are ignored wherever they are, repeated or not. */
    d = new_display();
    CHECK(provisioned(
        d, swapped("\"format\":", "\"extra\":{\"a\":[1,2,{}]},\"extra\":1,\"format\":")));
    d = new_display();
    CHECK(provisioned(d, swapped("\"hosts\":", "\"note\":\"x\",\"hosts\":")));
    d = new_display();
    CHECK(provisioned(d, swapped("\"ssid\":", "\"hidden\":true,\"ssid\":")));
    d = new_display();
    CHECK(provisioned(d, swapped("\"type\":\"provision\",", "\"type\":\"provision\",\"id\":7,")));
    /* Members in another order, and white space between them. */
    d = new_display();
    CHECK(provisioned(
        d, swapped("\"port\":8443,\"server_name\":\"hub.display-hub.internal\",",
                   "\"server_name\" : \"hub.display-hub.internal\" ,\t\"port\" : 8443 ,")));
    /* Escapes are decoded before anything is compared. */
    d = new_display();
    CHECK(provisioned(d, swapped("\"format\":\"display-hub.provisioning\"",
                                 "\"f\\u006frmat\":\"display-hub\\u002eprovisioning\"")));
}

static void test_what_is_stored(void)
{
    display *d = new_display();
    parts p = base();
    p.anchors = "[" ANCHOR "," ANCHOR "]";
    p.firmware = "{\"trusted_keys\":[\"" MAKER "\",\"" RECOVERY "\"],\"security_counter\":3}";
    dh_usb_answer a = send(d, line_of(&p));
    CHECK(a.outcome == DH_USB_PROVISIONED && a.keys_taken);

    /* What the scratch holds is the bundle. */
    const dh_setup *s = &d->work;
    CHECK(s->host_count == 2 && strcmp(s->hosts[0].text, "raspberrypi.local") == 0 &&
          s->hosts[0].len == 17 && strcmp(s->hosts[1].text, "192.168.1.20") == 0);
    CHECK(s->port == 8443 && s->anchor_count == 2);
    CHECK(strcmp(s->anchors[1].pem,
                 "-----BEGIN CERTIFICATE-----\nMIIBkzCCATig\n-----END CERTIFICATE-----\n") == 0 &&
          s->anchors[1].pem_len == strlen(s->anchors[1].pem));
    CHECK(s->anchors[0].spki_sha256[0] == 0x9a && s->anchors[0].spki_sha256[31] == 0x88);
    CHECK(s->secret_len == 22 && strcmp(s->secret, "q3H7cX0f2pS9L1mB8vN4wA") == 0);
    CHECK(strcmp(s->expires_at, "2026-11-10T20:10:00Z") == 0);
    CHECK(s->has_wifi && strcmp(s->ssid, "Example network") == 0 && s->ssid_len == 15 &&
          strcmp(s->password, "example-passphrase") == 0 && s->password_len == 18);

    /* And what is in flash reads back as the same thing. */
    static dh_setup back;
    CHECK(dh_setup_decode(&back, d->flash.setup, d->flash.setup_len));
    CHECK(memcmp(&back, s, sizeof back) == 0);
    dh_fw_keys keys;
    CHECK(dh_fw_keys_decode(&keys, d->flash.keys, sizeof d->flash.keys));
    dh_fw_key maker = key_of(MAKER), recovery = key_of(RECOVERY);
    CHECK(keys.key_count == 2 && keys.security_floor == 3 &&
          memcmp(keys.keys[0].bytes, maker.bytes, DH_FW_KEY_LEN) == 0 &&
          memcmp(keys.keys[1].bytes, recovery.bytes, DH_FW_KEY_LEN) == 0);
    CHECK(memory_matches_flash(d));
    /* One write each for the keys and the setup; none for a budget that was full already. */
    CHECK(d->flash.key_writes == 1 && d->flash.record_writes == 0 && d->flash.setup_writes == 1);

    /* A string may hold a NUL of its own: the lengths say how long it really is. */
    d = new_display();
    p = base();
    p.secret = "\"a\\u0000b\"";
    p.wifi = "{\"ssid\":\"n\\u0000m\",\"password\":\"12345\\u0000678\"}";
    CHECK(provisioned(d, line_of(&p)));
    CHECK(d->work.secret_len == 3 && memcmp(d->work.secret, "a\0b", 3) == 0);
    CHECK(d->work.ssid_len == 3 && d->work.password_len == 9);
    CHECK(dh_setup_decode(&back, d->flash.setup, d->flash.setup_len) &&
          memcmp(&back, &d->work, sizeof back) == 0);
}

static void test_the_size_limit(void)
{
    /* Two anchors padded until the bundle, written compactly, is exactly at the limit. Nothing in
     * it needs an escape, so its own length is what the reference would count. */
    static char bundle[DH_MAX_USB_LINE_BYTES], anchors[4 * DH_SETUP_MAX_PEM_BYTES + 1024];
    parts p = base();
    p.anchors = "[{\"certificate_pem\":\"-----BEGIN CERTIFICATE-----\",\"spki_sha256\":\"" SPKI
                "\"},{\"certificate_pem\":\"-----BEGIN CERTIFICATE-----\",\"spki_sha256\":\"" SPKI
                "\"}]";
    p.firmware = "{\"trusted_keys\":[\"" MAKER "\",\"" RECOVERY "\"],\"security_counter\":12}";
    size_t plain = bundle_text(&p, bundle, sizeof bundle);
    CHECK(plain < DH_MAX_BUNDLE_BYTES);
    for (int over = 1; over >= 0; over--) {
        /* A real bundle can't reach the limit with plain text alone, so the padding is line
         * feeds, which are two bytes each on the wire ("\n") as serde_json writes them too, and
         * one plain letter where an odd byte is needed. */
        size_t pad = DH_MAX_BUNDLE_BYTES - plain + (size_t)over;
        size_t first = pad / 2 < 4000 ? pad / 2 : 4000;
        size_t rest = pad - 2 * first;
        static char one[2 * DH_SETUP_MAX_PEM_BYTES + 64], two[2 * DH_SETUP_MAX_PEM_BYTES + 64];
        size_t at = (size_t)snprintf(one, sizeof one, "\"-----BEGIN CERTIFICATE-----");
        for (size_t i = 0; i < first; i++) {
            at += (size_t)snprintf(one + at, sizeof one - at, "\\n");
        }
        snprintf(one + at, sizeof one - at, "\"");
        at = (size_t)snprintf(two, sizeof two, "\"-----BEGIN CERTIFICATE-----%s",
                              rest % 2 ? "A" : "");
        CHECK(rest / 2 <= 4000);
        for (size_t i = 0; i < rest / 2; i++) {
            at += (size_t)snprintf(two + at, sizeof two - at, "\\n");
        }
        snprintf(two + at, sizeof two - at, "\"");
        snprintf(anchors, sizeof anchors,
                 "[{\"certificate_pem\":%s,\"spki_sha256\":\"" SPKI
                 "\"},{\"certificate_pem\":%s,\"spki_sha256\":\"" SPKI "\"}]",
                 one, two);
        p.anchors = anchors;
        CHECK(bundle_text(&p, bundle, sizeof bundle) == DH_MAX_BUNDLE_BYTES + (size_t)over);
        EXPECT(&p, over ? INVALID : ACCEPT, over ? "bundle larger than 12288 bytes" : NULL);
    }

    /* The count is of the bundle written out again, not of the line: the same bundle at the
     * limit, sent with white space and with a character written as a six-byte escape, is longer
     * on the wire and still accepted; and unknown members don't count. (The loop above ended
     * with the bundle at the limit.) */
    display *d = new_display();
    static char line[DH_MAX_USB_LINE_BYTES];
    snprintf(line, sizeof line,
             "{\"type\":\"provision\",\"bundle\": {\"padding\":\"%0500d\",\"f\\u006frmat\" :  %s",
             0, bundle + strlen("{\"format\":"));
    strcat(line, "}");
    CHECK(strlen(line) > DH_MAX_BUNDLE_BYTES + 500);
    CHECK(provisioned(d, line));

    /* A line of 16 KiB or more never reaches the handler: the line reader drops it whole, and
     * the next line is read as usual. */
    static dh_usb_line_reader reader;
    static uint8_t big[DH_MAX_USB_LINE_BYTES + 100];
    dh_usb_line_init(&reader);
    memset(big, 'x', sizeof big);
    big[0] = '{';
    size_t used = 0, len = 0;
    const uint8_t *got = NULL;
    CHECK(dh_usb_line_feed(&reader, big, sizeof big, &used, &got, &len) == DH_LINE_OVERLONG);
    CHECK(used == DH_MAX_USB_LINE_BYTES);
    CHECK(dh_usb_line_feed(&reader, big + used, sizeof big - used, &used, &got, &len) ==
          DH_LINE_NONE);
    static const char next[] = "tail\n{\"type\":\"hello\",\"protocol_version\":1}\n";
    CHECK(dh_usb_line_feed(&reader, (const uint8_t *)next, sizeof next - 1, &used, &got, &len) ==
          DH_LINE_JSON);
    d = new_display();
    CHECK(dh_usb_handle_line(&d->usb, got, len, d->reply, sizeof d->reply).outcome ==
          DH_USB_HELLO);
    /* One byte short of the limit is still a line. */
    dh_usb_line_init(&reader);
    big[DH_MAX_USB_LINE_BYTES - 1] = '\n';
    CHECK(dh_usb_line_feed(&reader, big, DH_MAX_USB_LINE_BYTES, &used, &got, &len) ==
          DH_LINE_JSON);
    CHECK(len == DH_MAX_USB_LINE_BYTES - 1);
    d = new_display();
    CHECK(dh_usb_handle_line(&d->usb, got, len, d->reply, sizeof d->reply).outcome ==
          DH_USB_IGNORED);
}

static void test_the_refusal_order(void)
{
    parts good = base();
    parts invalid = base();
    invalid.port = "0";
    parts not_a_point = base();
    not_a_point.firmware = "{\"trusted_keys\":[\"" NOT_A_POINT "\"],\"security_counter\":0}";
    parts invalid_and_not_a_point = not_a_point;
    invalid_and_not_a_point.port = "0";
    parts strangers = base();
    strangers.firmware = "{\"trusted_keys\":[\"" STRANGER "\"],\"security_counter\":9}";
    parts stranger_and_not_a_point = base();
    stranger_and_not_a_point.firmware =
        "{\"trusted_keys\":[\"" STRANGER "\",\"" NOT_A_POINT "\"],\"security_counter\":9}";
    parts same = base();
    same.firmware = "{\"trusted_keys\":[\"" RECOVERY "\",\"" MAKER "\"],\"security_counter\":0}";
    parts fewer = base();
    fewer.firmware = "{\"trusted_keys\":[\"" MAKER "\"],\"security_counter\":3}";
    parts more = base();
    more.firmware = "{\"trusted_keys\":[\"" MAKER "\",\"" RECOVERY "\",\"" STRANGER
                    "\"],\"security_counter\":3}";

    /* 1. Already set up comes before everything, and changes nothing. */
    display *d = new_display();
    give_keys(d, MAKER, RECOVERY, 3);
    spend_budget(d, 1);
    CHECK(provisioned(d, line_of(&good)));
    CHECK(d->record.attempts_left == DH_FW_FULL_BUDGET && d->record.credit_hours == 0);
    spend_budget(d, 2);
    fake_store before = d->flash;
    const parts *const all[] = {&good, &invalid, &not_a_point, &strangers, &same, &fewer};
    for (size_t i = 0; i < LEN(all); i++) {
        CHECK(refused(d, line_of(all[i]), DH_USB_ERROR_ALREADY_PROVISIONED));
        CHECK(strcmp(d->reply, "{\"type\":\"error\",\"code\":\"already_provisioned\",\"message\":"
                               "\"Already set up; factory-reset this display first.\"}\n") == 0);
    }
    CHECK(memcmp(&before, &d->flash, sizeof before) == 0 && memory_matches_flash(d));
    CHECK(d->record.attempts_left == 2);
    /* A line that can't be read still gets no answer. */
    CHECK(ignored(d, swapped("\"port\":8443,", "")));

    /* 2. A broken rule comes before the keys are looked at. */
    d = new_display();
    give_keys(d, MAKER, RECOVERY, 3);
    CHECK(refused(d, line_of(&invalid_and_not_a_point), DH_USB_ERROR_INVALID_BUNDLE));
    CHECK(strstr(d->reply, "hub port must not be 0") != NULL);
    invalid.firmware = strangers.firmware;
    CHECK(refused(d, line_of(&invalid), DH_USB_ERROR_INVALID_BUNDLE));

    /* 3. A key that isn't a point is invalid_bundle, before the key sets are compared: with keys
     * of its own, and with none. */
    CHECK(refused(d, line_of(&not_a_point), DH_USB_ERROR_INVALID_BUNDLE));
    CHECK(strstr(d->reply, "P-256") != NULL);
    CHECK(refused(d, line_of(&stranger_and_not_a_point), DH_USB_ERROR_INVALID_BUNDLE));
    d = new_display();
    CHECK(refused(d, line_of(&not_a_point), DH_USB_ERROR_INVALID_BUNDLE));
    CHECK(refused(d, line_of(&stranger_and_not_a_point), DH_USB_ERROR_INVALID_BUNDLE));
    CHECK(d->keys.key_count == 0 && d->flash.key_writes == 0 && !d->flash.has_setup);

    /* 4. A different set is firmware_keys_locked: other keys, fewer and more. */
    d = new_display();
    give_keys(d, MAKER, RECOVERY, 3);
    spend_budget(d, 0);
    before = d->flash;
    const parts *const locked[] = {&strangers, &fewer, &more};
    for (size_t i = 0; i < LEN(locked); i++) {
        CHECK(refused(d, line_of(locked[i]), DH_USB_ERROR_FIRMWARE_KEYS_LOCKED));
        CHECK(strcmp(d->reply,
                     "{\"type\":\"error\",\"code\":\"firmware_keys_locked\",\"message\":\"This "
                     "display trusts other firmware keys. Use Install firmware to change "
                     "them.\"}\n") == 0);
    }
    CHECK(memcmp(&before, &d->flash, sizeof before) == 0 && memory_matches_flash(d));

    /* 5. The same set in another order is accepted and changes no keys, nor the floor, whatever
     * counter it names; the budget is refilled. */
    dh_usb_answer a = send(d, line_of(&same));
    CHECK(a.outcome == DH_USB_PROVISIONED && !a.keys_taken);
    CHECK(d->flash.key_writes == 0 && d->flash.record_writes == 1 && d->flash.setup_writes == 1);
    CHECK(d->keys.security_floor == 3 && d->keys.key_count == 2);
    CHECK(d->record.attempts_left == DH_FW_FULL_BUDGET && memory_matches_flash(d));

    /* No firmware member: accepted by a display with keys, and by one without, which then still
     * has none. */
    d = new_display();
    give_keys(d, MAKER, NULL, 7);
    CHECK(provisioned(d, line_of(&good)));
    CHECK(d->keys.key_count == 1 && d->keys.security_floor == 7 && d->flash.key_writes == 0);
    d = new_display();
    CHECK(provisioned(d, line_of(&good)));
    CHECK(d->keys.key_count == 0 && d->flash.key_writes == 0 && d->flash.record_writes == 0);

    /* A display with no keys takes the bundle's, and its counter as the floor. */
    d = new_display();
    a = send(d, line_of(&strangers));
    CHECK(a.outcome == DH_USB_PROVISIONED && a.keys_taken);
    CHECK(d->keys.key_count == 1 && d->keys.security_floor == 9 && memory_matches_flash(d));
}

static void test_storage(void)
{
    parts p = base();
    p.firmware = "{\"trusted_keys\":[\"" MAKER "\"],\"security_counter\":2}";
    const char *line = line_of(&p);
    static const dh_setup zero;

    /* Each write failing in turn. The setup is written last, so a failure never leaves one. */
    for (int which = 0; which < 3; which++) {
        display *d = new_display();
        spend_budget(d, 1);
        d->flash.fail_keys = which == 0;
        d->flash.fail_record = which == 1;
        d->flash.fail_setup = which == 2;
        CHECK(refused(d, line, DH_USB_ERROR_STORAGE_FAILED));
        CHECK(!d->flash.has_setup && fake_held(&d->flash) == DH_HELD_NOTHING);
        CHECK(memcmp(&d->work, &zero, sizeof zero) == 0);
        /* Memory holds exactly what reached flash, whichever write it was. */
        CHECK(memory_matches_flash(d));
        CHECK(d->keys.key_count == (which == 0 ? 0 : 1));
        CHECK(d->record.attempts_left == (which <= 1 ? 1 : DH_FW_FULL_BUDGET));
        CHECK(d->flash.key_writes == 1 && d->flash.record_writes == (which >= 1 ? 1u : 0u) &&
              d->flash.setup_writes == (which == 2 ? 1u : 0u));
        /* `hello` still says it isn't set up, and the same bundle works once storage does. */
        CHECK(send(d, "{\"type\":\"hello\",\"protocol_version\":1}").outcome == DH_USB_HELLO);
        CHECK(strstr(d->reply, "\"provisioned\":false") != NULL);
        d->flash.fail_keys = d->flash.fail_record = d->flash.fail_setup = false;
        CHECK(provisioned(d, line));
        CHECK(d->keys.key_count == 1 && d->keys.security_floor == 2 &&
              d->record.attempts_left == DH_FW_FULL_BUDGET && memory_matches_flash(d));
    }

    /* A store that can't say what it holds: never guess. */
    display *d = new_display();
    d->flash.unreadable = true;
    CHECK(refused(d, line, DH_USB_ERROR_STORAGE_FAILED));
    CHECK(d->flash.key_writes == 0 && d->flash.setup_writes == 0);
    CHECK(send(d, "{\"type\":\"hello\",\"protocol_version\":1}").outcome == DH_USB_HELLO);
    CHECK(strstr(d->reply, "\"provisioned\":false") != NULL);

    /* A write failed earlier in this run, so what is stored isn't known until a restart: every
     * bundle is refused before anything is written, with the protocol's only code for it and
     * words of its own that say what to do. */
    d = new_display();
    give_keys(d, MAKER, NULL, 2);
    d->flash.restart_needed = true;
    fake_store untouched = d->flash;
    CHECK(refused(d, line, DH_USB_ERROR_STORAGE_FAILED));
    CHECK(strstr(d->reply, "until it restarts") != NULL && strstr(d->reply, "try again") != NULL);
    CHECK(strstr(d->reply, "isn't working") == NULL);
    CHECK(memcmp(&untouched, &d->flash, sizeof untouched) == 0);
    CHECK(memcmp(&d->work, &zero, sizeof zero) == 0);
    /* A broken bundle gets the same answer: nothing about it is looked at. */
    CHECK(refused(d, "{\"type\":\"provision\",\"bundle\":{\"format\":\"x\",\"version\":1,"
                     "\"hub\":{\"hosts\":[],\"port\":0,\"server_name\":\"x\","
                     "\"trust_anchors\":[]},\"enrolment\":{\"secret\":\"\","
                     "\"expires_at\":\"2026-01-01T00:00:00Z\"},\"wifi\":null}}",
                  DH_USB_ERROR_STORAGE_FAILED) ||
          ignored(d, "{}"));
    CHECK(memcmp(&untouched, &d->flash, sizeof untouched) == 0);
    /* `hello` says it isn't set up. The caller also holds back the firmware member then, since
     * the keys and the budget in memory may not be what is stored. */
    CHECK(send(d, "{\"type\":\"hello\",\"protocol_version\":1}").outcome == DH_USB_HELLO);
    CHECK(strstr(d->reply, "\"provisioned\":false") != NULL);
    CHECK(strstr(d->reply, "\"firmware\":{") != NULL);
    d->usb.withhold_firmware = true;
    CHECK(send(d, "{\"type\":\"hello\",\"protocol_version\":1}").outcome == DH_USB_HELLO);
    CHECK(strstr(d->reply, "\"provisioned\":false") != NULL);
    CHECK(strstr(d->reply, "firmware\"") == NULL && strstr(d->reply, "key_ids") == NULL);
    /* Holding the member back changes nothing else: a display that is set up still says so,
     * and still answers a bundle with "already set up". */
    d = new_display();
    CHECK(provisioned(d, line));
    d->flash.restart_needed = true;
    d->usb.withhold_firmware = true;
    CHECK(send(d, "{\"type\":\"hello\",\"protocol_version\":1}").outcome == DH_USB_HELLO);
    CHECK(strstr(d->reply, "\"provisioned\":true") != NULL);
    CHECK(strstr(d->reply, "firmware\"") == NULL);
    CHECK(refused(d, line, DH_USB_ERROR_ALREADY_PROVISIONED));
    /* And without it, a display that takes bundles as usual still takes one. */
    d = new_display();
    d->usb.withhold_firmware = true;
    CHECK(provisioned(d, line));

    /* No storage at all (the storage key couldn't be used): `hello` says not set up and reports
     * no firmware member, and every bundle is refused, a broken one included. */
    d = new_display();
    d->usb.store = NULL;
    d->usb.keys = NULL;
    d->usb.record = NULL;
    CHECK(refused(d, line, DH_USB_ERROR_STORAGE_FAILED));
    p.port = "0";
    CHECK(refused(d, line_of(&p), DH_USB_ERROR_STORAGE_FAILED));
    CHECK(send(d, "{\"type\":\"hello\",\"protocol_version\":1}").outcome == DH_USB_HELLO);
    CHECK(strcmp(d->reply,
                 "{\"type\":\"hello\",\"device_type\":\"display-hub-waveshare-amoled-1.8\","
                 "\"firmware_version\":\"0.1.0\",\"hardware_revision\":\"waveshare-amoled-1.8-v2\","
                 "\"protocol_version\":1,\"provisioned\":false}\n") == 0);
    CHECK(!dh_usb_factory_reset(&d->usb));
    CHECK(d->flash.key_writes + d->flash.record_writes + d->flash.setup_writes + d->flash.erases ==
          0);
}

static void test_factory_reset(void)
{
    parts first = base();
    first.firmware = "{\"trusted_keys\":[\"" MAKER "\",\"" RECOVERY "\"],\"security_counter\":3}";
    parts other = base();
    other.firmware = "{\"trusted_keys\":[\"" STRANGER "\"],\"security_counter\":0}";
    parts none = base();

    display *d = new_display();
    CHECK(provisioned(d, line_of(&first)));
    spend_budget(d, 2);
    uint8_t keys[DH_FW_KEYS_BLOB_LEN], record[DH_FW_RECORD_BLOB_LEN];
    memcpy(keys, d->flash.keys, sizeof keys);
    memcpy(record, d->flash.record, sizeof record);

    /* A reset that fails leaves the display set up. */
    d->flash.fail_erase = true;
    CHECK(!dh_usb_factory_reset(&d->usb));
    CHECK(refused(d, line_of(&first), DH_USB_ERROR_ALREADY_PROVISIONED));
    d->flash.fail_erase = false;

    /* The reset erases the setup and nothing else: keys, floor and record are as they were. */
    CHECK(dh_usb_factory_reset(&d->usb));
    CHECK(!d->flash.has_setup);
    CHECK(memcmp(keys, d->flash.keys, sizeof keys) == 0 &&
          memcmp(record, d->flash.record, sizeof record) == 0 && memory_matches_flash(d));
    CHECK(send(d, "{\"type\":\"hello\",\"protocol_version\":1}").outcome == DH_USB_HELLO);
    CHECK(strstr(d->reply, "\"provisioned\":false") != NULL &&
          strstr(d->reply, "\"security_floor\":3") != NULL &&
          strstr(d->reply, "\"attempts_left\":2") != NULL);

    /* Afterwards: other keys are locked out, and the same keys, or none, set it up again. */
    CHECK(refused(d, line_of(&other), DH_USB_ERROR_FIRMWARE_KEYS_LOCKED));
    CHECK(provisioned(d, line_of(&first)));
    CHECK(d->record.attempts_left == DH_FW_FULL_BUDGET);
    CHECK(dh_usb_factory_reset(&d->usb));
    CHECK(provisioned(d, line_of(&none)));
    CHECK(d->keys.key_count == 2 && d->keys.security_floor == 3);
    /* Resetting a display that isn't set up is harmless. */
    CHECK(dh_usb_factory_reset(&d->usb) && dh_usb_factory_reset(&d->usb));
}

/* ---- hello, and the examples ---- */

static char *read_example(const char *name)
{
    static char text[8192];
    char path[1024];
    snprintf(path, sizeof path, "%s/examples/usb/%s", DH_PROTOCOL_DIR, name);
    FILE *f = fopen(path, "rb");
    if (f == NULL) {
        fprintf(stderr, "can't open %s\n", path);
        exit(1);
    }
    size_t n = fread(text, 1, sizeof text - 1, f);
    fclose(f);
    while (n > 0 && (text[n - 1] == '\n' || text[n - 1] == '\r' || text[n - 1] == ' ')) {
        n--;
    }
    text[n] = '\0';
    return text;
}

/* Whether the reply is the example, byte for byte, with a newline after it. */
static bool reply_is_example(const display *d, const char *name)
{
    const char *want = read_example(name);
    size_t n = strlen(want);
    return strlen(d->reply) == n + 1 && memcmp(d->reply, want, n) == 0 && d->reply[n] == '\n';
}

static void test_the_examples(void)
{
    /* The manager's two lines, as the examples have them, through the line reader. */
    display *d = new_display();
    static dh_usb_line_reader reader;
    static char stream[8192];
    dh_usb_line_init(&reader);
    snprintf(stream, sizeof stream, "%s\r\n", read_example("manager-hello.json"));
    size_t used = 0, len = 0;
    const uint8_t *line = NULL;
    CHECK(dh_usb_line_feed(&reader, (const uint8_t *)stream, strlen(stream), &used, &line, &len) ==
          DH_LINE_JSON);
    dh_usb_answer a = dh_usb_handle_line(&d->usb, line, len, d->reply, sizeof d->reply);
    CHECK(a.outcome == DH_USB_HELLO && a.reply_len == strlen(d->reply));
    CHECK(strstr(d->reply, "\"provisioned\":false,\"firmware\":{\"key_ids\":[],"
                           "\"security_floor\":0,\"slot_bytes\":6291456,\"attempts_left\":5}}\n") !=
          NULL);

    snprintf(stream, sizeof stream, "%s\n", read_example("manager-provision.json"));
    CHECK(dh_usb_line_feed(&reader, (const uint8_t *)stream, strlen(stream), &used, &line, &len) ==
          DH_LINE_JSON);
    a = dh_usb_handle_line(&d->usb, line, len, d->reply, sizeof d->reply);
    CHECK(a.outcome == DH_USB_PROVISIONED);
    CHECK(reply_is_example(d, "display-provisioned.json"));
    CHECK(d->work.host_count == 2 && d->work.port == 8443 && d->work.has_wifi &&
          d->work.anchors[0].pem_len > 500);
    /* Wiping the reader leaves nothing of the line behind. */
    dh_usb_line_wipe(&reader);
    bool clean = true;
    for (size_t i = 0; i < sizeof reader.buf; i++) {
        clean = clean && reader.buf[i] == 0;
    }
    CHECK(clean);

    /* Sent again: the example's refusal, byte for byte. */
    CHECK(refused(d, stream, DH_USB_ERROR_ALREADY_PROVISIONED));
    stream[strlen(stream) - 1] = '\0';
    CHECK(refused(d, stream, DH_USB_ERROR_ALREADY_PROVISIONED));
    CHECK(reply_is_example(d, "display-error.json"));
    CHECK(send(d, "{\"type\":\"hello\",\"protocol_version\":1}").outcome == DH_USB_HELLO);
    CHECK(strstr(d->reply, "\"provisioned\":true,") != NULL);

    /* The keys-locked example. */
    d = new_display();
    give_keys(d, MAKER, NULL, 0);
    parts p = base();
    p.firmware = "{\"trusted_keys\":[\"" STRANGER "\"],\"security_counter\":0}";
    CHECK(refused(d, line_of(&p), DH_USB_ERROR_FIRMWARE_KEYS_LOCKED));
    CHECK(reply_is_example(d, "display-error-keys-locked.json"));

    /* The display's own hello examples, with the examples' device and no storage or keys... */
    d = new_display();
    d->usb.device_type = "llm-usage-display";
    d->usb.firmware_version = "1.0.0";
    d->usb.store = NULL;
    CHECK(send(d, read_example("manager-hello.json")).outcome == DH_USB_HELLO);
    CHECK(reply_is_example(d, "display-hello.json"));

    /* ...and with a firmware member: the same shape as the example, with this display's keys.
     * The ids are the SHA-256 of each key's 65 bytes. */
    d = new_display();
    give_keys(d, MAKER, RECOVERY, 3);
    d->usb.device_type = "llm-usage-display";
    d->usb.firmware_version = "1.1.0";
    d->usb.slot_bytes = 3145728;
    CHECK(send(d, read_example("manager-hello.json")).outcome == DH_USB_HELLO);
    char ids[2][DH_PACKAGE_ID_HEX_LEN + 1], want[DH_USB_REPLY_MAX];
    CHECK(dh_fw_key_id_hex(&d->keys.keys[0], ids[0]) && dh_fw_key_id_hex(&d->keys.keys[1], ids[1]));
    snprintf(want, sizeof want,
             "{\"type\":\"hello\",\"device_type\":\"llm-usage-display\",\"firmware_version\":"
             "\"1.1.0\",\"hardware_revision\":\"waveshare-amoled-1.8-v2\",\"protocol_version\":1,"
             "\"provisioned\":false,\"firmware\":{\"key_ids\":[\"%s\",\"%s\"],\"security_floor\":3,"
             "\"slot_bytes\":3145728,\"attempts_left\":5}}\n",
             ids[0], ids[1]);
    CHECK(strcmp(d->reply, want) == 0);
    /* The example is that line with its own two ids. */
    const char *example = read_example("display-hello-with-firmware.json");
    const char *ours = strstr(d->reply, "\"key_ids\":[\"");
    const char *theirs = strstr(example, "\"key_ids\":[\"");
    CHECK(ours != NULL && theirs != NULL);
    if (ours != NULL && theirs != NULL) {
        CHECK(ours - d->reply == theirs - example &&
              strncmp(d->reply, example, (size_t)(ours - d->reply)) == 0);
        CHECK(strncmp(ours + 12 + 64 + 3 + 64, theirs + 12 + 64 + 3 + 64,
                      strlen(theirs + 12 + 64 + 3 + 64)) == 0);
    }
    /* A real id: the maker key's, worked out separately. */
    uint8_t digest[DH_SHA256_LEN];
    dh_fw_key maker = key_of(MAKER);
    CHECK(dh_sha256_of(maker.bytes, DH_FW_KEY_LEN, digest));
    char hex[3];
    snprintf(hex, sizeof hex, "%02x", digest[0]);
    CHECK(strncmp(ids[0], hex, 2) == 0 && strlen(ids[0]) == 64);
}

static void test_hello_and_other_lines(void)
{
    display *d = new_display();
    /* Any protocol_version that fits 32 bits is answered, as the reference does; the app decides
     * what to make of the display's own. */
    static const char *const answered[] = {
        "{\"type\":\"hello\",\"protocol_version\":1}",
        "{\"type\":\"hello\",\"protocol_version\":2}",
        "{\"type\":\"hello\",\"protocol_version\":0}",
        "{\"type\":\"hello\",\"protocol_version\":4294967295}",
        "{\"protocol_version\":1,\"type\":\"hello\"}",
        "{ \"type\" : \"hello\" , \"protocol_version\" : 1 , \"extra\" : [ ] }  ",
        "{\"type\":\"hel\\u006co\",\"protocol_version\":1}",
    };
    for (size_t i = 0; i < LEN(answered); i++) {
        dh_usb_answer a = send(d, answered[i]);
        CHECK(a.outcome == DH_USB_HELLO && a.reply_len > 0 && d->reply[a.reply_len - 1] == '\n');
        CHECK(strstr(d->reply, "\"protocol_version\":1,") != NULL);
        /* One line, and plain ASCII. */
        CHECK(strchr(d->reply, '\n') == d->reply + a.reply_len - 1);
    }
    static const char *const unanswered[] = {
        "",
        "hello",
        "I (312) boot: ESP-IDF v5.5.6",
        "{",
        "{}",
        "[]",
        "{\"type\":\"hello\"}",
        "{\"type\":\"hello\",\"protocol_version\":\"1\"}",
        "{\"type\":\"hello\",\"protocol_version\":1.0}",
        "{\"type\":\"hello\",\"protocol_version\":-1}",
        "{\"type\":\"hello\",\"protocol_version\":4294967296}",
        "{\"type\":\"hello\",\"protocol_version\":1,\"protocol_version\":1}",
        "{\"type\":\"hello\",\"type\":\"hello\",\"protocol_version\":1}",
        "{\"type\":\"Hello\",\"protocol_version\":1}",
        "{\"type\":\"provisioned\"}",
        "{\"type\":\"error\",\"code\":\"invalid_bundle\",\"message\":\"x\"}",
        "{\"type\":7,\"protocol_version\":1}",
        "{\"type\":\"hello\",\"protocol_version\":1} trailing",
        "{\"type\":\"hello\",\"protocol_version\":1,}",
        "{\"type\":\"hello\",\"protocol_version\":1,\"x\":\"\xff\"}",
        " {\"type\":\"hello\",\"protocol_version\":1}",
    };
    for (size_t i = 0; i < LEN(unanswered); i++) {
        CHECK(ignored(d, unanswered[i]));
    }
    /* A reply buffer that is too small gets nothing, not half a line. */
    char small[64];
    dh_usb_answer a = dh_usb_handle_line(&d->usb, (const uint8_t *)answered[0],
                                         strlen(answered[0]), small, sizeof small);
    CHECK(a.outcome == DH_USB_IGNORED && a.reply_len == 0 && small[0] == '\0');
    CHECK(dh_usb_hello(&d->usb, small, sizeof small) == 0 && small[0] == '\0');

    /* Text that needs escaping is escaped, so the reply stays one line of JSON. */
    d->usb.hardware_revision = "odd \"name\"\\\n\x01";
    CHECK(send(d, answered[0]).outcome == DH_USB_HELLO);
    CHECK(strstr(d->reply,
                 "\"hardware_revision\":\"odd \\\"name\\\"\\\\\\u000a\\u0001\",") != NULL);
    dh_json_doc doc;
    dh_json_value root;
    CHECK(dh_json_parse(&doc, (const uint8_t *)d->reply, strlen(d->reply), NULL, &root) ==
          DH_JSON_OK);
}

/* A whole session as the port delivers it: boot logs, a hello cut into pieces, a bundle, and a
 * second bundle, fed a few bytes at a time. */
static void test_a_session_in_pieces(void)
{
    static char stream[2 * DH_MAX_USB_LINE_BYTES];
    parts p = base();
    p.firmware = "{\"trusted_keys\":[\"" MAKER "\"],\"security_counter\":1}";
    const char *bundle = line_of(&p);
    snprintf(stream, sizeof stream,
             "ESP-ROM:esp32s3-20210327\r\nI (312) boot: {not json\r\n"
             "{\"type\":\"hello\",\"protocol_version\":1}\n\n  {\"type\":\"hello\"}\n"
             "{\"type\":\"hello\",\"protocol_version\":1}\r\n%s\n%s\r\n"
             "{\"type\":\"hello\",\"protocol_version\":1}\n",
             bundle, bundle);
    static const size_t sizes[] = {1, 2, 7, 64, 1000, sizeof stream};
    for (size_t s = 0; s < LEN(sizes); s++) {
        display *d = new_display();
        static dh_usb_line_reader reader;
        dh_usb_line_init(&reader);
        char said[8][24];
        size_t replies = 0;
        size_t total = strlen(stream);
        for (size_t at = 0; at < total;) {
            size_t n = total - at < sizes[s] ? total - at : sizes[s];
            const uint8_t *piece = (const uint8_t *)stream + at;
            size_t left = n;
            while (left > 0) {
                size_t used = 0, len = 0;
                const uint8_t *line = NULL;
                dh_line_event e = dh_usb_line_feed(&reader, piece, left, &used, &line, &len);
                piece += used;
                left -= used;
                if (e == DH_LINE_JSON) {
                    dh_usb_answer a =
                        dh_usb_handle_line(&d->usb, line, len, d->reply, sizeof d->reply);
                    dh_usb_line_wipe(&reader);
                    if (a.reply_len > 0 && replies < LEN(said)) {
                        snprintf(said[replies++], sizeof said[0], "%.23s", d->reply + 9);
                    }
                }
            }
            at += n;
        }
        CHECK(replies == 5);
        if (replies == 5) {
            CHECK(strncmp(said[0], "hello\"", 6) == 0 && strncmp(said[1], "hello\"", 6) == 0);
            CHECK(strncmp(said[2], "provisioned\"", 12) == 0);
            CHECK(strncmp(said[3], "error\",\"code\":\"already", 22) == 0);
            CHECK(strncmp(said[4], "hello\"", 6) == 0);
        }
        CHECK(d->flash.setup_writes == 1 && d->keys.key_count == 1);
    }
}

/* ---- The stored form ---- */

static void test_the_stored_form(void)
{
    static dh_setup s, back;
    static uint8_t blob[DH_SETUP_BLOB_MAX + 8], other[DH_SETUP_BLOB_MAX];

    /* The largest setup there can be fills the blob exactly. */
    memset(&s, 0, sizeof s);
    s.host_count = DH_SETUP_MAX_HOSTS;
    for (unsigned i = 0; i < DH_SETUP_MAX_HOSTS; i++) {
        memset(s.hosts[i].text, 'a' + (int)i, DH_SETUP_MAX_HOST_BYTES);
        s.hosts[i].len = DH_SETUP_MAX_HOST_BYTES;
    }
    s.port = 65535;
    s.anchor_count = DH_SETUP_MAX_ANCHORS;
    for (unsigned i = 0; i < DH_SETUP_MAX_ANCHORS; i++) {
        memset(s.anchors[i].pem, 'P', DH_SETUP_MAX_PEM_BYTES);
        memcpy(s.anchors[i].pem, "-----BEGIN CERTIFICATE-----", 27);
        s.anchors[i].pem_len = DH_SETUP_MAX_PEM_BYTES;
        memset(s.anchors[i].spki_sha256, 0xA0 + (int)i, DH_SHA256_LEN);
    }
    memset(s.secret, 's', DH_SETUP_MAX_SECRET_BYTES);
    s.secret_len = DH_SETUP_MAX_SECRET_BYTES;
    memcpy(s.expires_at, "9999-12-31T23:59:59Z", 20);
    s.has_wifi = true;
    memset(s.ssid, 'n', DH_SETUP_MAX_SSID_BYTES);
    s.ssid_len = DH_SETUP_MAX_SSID_BYTES;
    memset(s.password, 'p', DH_SETUP_MAX_PASSWORD_BYTES);
    s.password_len = DH_SETUP_MAX_PASSWORD_BYTES;
    CHECK(dh_setup_valid(&s));
    size_t n = dh_setup_encode(&s, blob);
    CHECK(n == DH_SETUP_BLOB_MAX && blob[0] == 1);
    CHECK(dh_setup_decode(&back, blob, n) && memcmp(&back, &s, sizeof s) == 0);
    CHECK(!dh_setup_decode(&back, blob, n + 1));

    /* The smallest: one host, one anchor, no Wi-Fi, and the secret cleared after enrolment. */
    memset(&s, 0, sizeof s);
    s.host_count = 1;
    s.hosts[0].len = 1;
    s.hosts[0].text[0] = 'h';
    s.port = 1;
    s.anchor_count = 1;
    memcpy(s.anchors[0].pem, "-----BEGIN CERTIFICATE-----", 27);
    s.anchors[0].pem_len = 27;
    memcpy(s.expires_at, "0000-01-01T00:00:00Z", 20);
    n = dh_setup_encode(&s, blob);
    CHECK(n == 1 + 1 + 2 + 2 + 1 + 2 + 27 + 32 + 1 + 20 + 1);
    CHECK(dh_setup_decode(&back, blob, n) && memcmp(&back, &s, sizeof s) == 0);

    /* A usual one, from a bundle. */
    display *d = new_display();
    parts p = base();
    CHECK(provisioned(d, line_of(&p)));
    s = d->work;
    n = dh_setup_encode(&s, blob);
    CHECK(n > 0 && n == d->flash.setup_len && memcmp(blob, d->flash.setup, n) == 0);

    /* Every shorter blob is refused, and so is every longer one, and *out is wiped. */
    static const dh_setup zero;
    for (size_t cut = 0; cut < n; cut++) {
        memset(&back, 0x55, sizeof back);
        bool ok = dh_setup_decode(&back, blob, cut);
        if (ok || memcmp(&back, &zero, sizeof zero) != 0) {
            FAIL("a cut blob decoded, or left something behind");
            break;
        }
    }
    blob[n] = 0;
    CHECK(!dh_setup_decode(&back, blob, n + 1));
    CHECK(!dh_setup_decode(&back, NULL, n) && !dh_setup_decode(&back, blob, 0));

    /* Any one byte changed: either it is refused, or it decodes to a setup that keeps every rule
     * and encodes back to the same bytes. Never anything in between. */
    unsigned accepted = 0;
    for (size_t at = 0; at < n; at++) {
        for (unsigned bit = 0; bit < 8; bit++) {
            blob[at] ^= (uint8_t)(1u << bit);
            if (dh_setup_decode(&back, blob, n)) {
                accepted++;
                if (!dh_setup_valid(&back) || dh_setup_encode(&back, other) != n ||
                    memcmp(other, blob, n) != 0) {
                    FAIL("a changed blob decoded to something that doesn't encode back");
                }
            } else if (memcmp(&back, &zero, sizeof zero) != 0) {
                FAIL("a refused blob left something behind");
            }
            blob[at] ^= (uint8_t)(1u << bit);
        }
    }
    CHECK(accepted > 0);
    blob[0] = 2; /* another format */
    CHECK(!dh_setup_decode(&back, blob, n));
    blob[0] = 1;

    /* A setup that breaks a rule is never written. */
    static dh_setup bad;
    bad = s, bad.host_count = 0;
    CHECK(dh_setup_encode(&bad, other) == 0);
    bad = s, bad.host_count = 5;
    CHECK(dh_setup_encode(&bad, other) == 0);
    bad = s, bad.hosts[0].text[1] = ' ';
    CHECK(dh_setup_encode(&bad, other) == 0);
    bad = s, bad.hosts[1].len = 0;
    CHECK(dh_setup_encode(&bad, other) == 0);
    bad = s, bad.port = 0;
    CHECK(dh_setup_encode(&bad, other) == 0);
    bad = s, bad.anchor_count = 3;
    CHECK(dh_setup_encode(&bad, other) == 0);
    bad = s, bad.anchors[0].pem[0] = '+';
    CHECK(dh_setup_encode(&bad, other) == 0);
    bad = s, bad.anchors[0].pem_len = DH_SETUP_MAX_PEM_BYTES + 1;
    CHECK(dh_setup_encode(&bad, other) == 0);
    bad = s, bad.secret_len = DH_SETUP_MAX_SECRET_BYTES + 1;
    CHECK(dh_setup_encode(&bad, other) == 0);
    bad = s, bad.expires_at[4] = '/';
    CHECK(dh_setup_encode(&bad, other) == 0);
    bad = s, bad.ssid_len = 0;
    CHECK(dh_setup_encode(&bad, other) == 0);
    bad = s, bad.ssid_len = DH_SETUP_MAX_SSID_BYTES + 1;
    CHECK(dh_setup_encode(&bad, other) == 0);
    bad = s, bad.password_len = 7;
    CHECK(dh_setup_encode(&bad, other) == 0);
    bad = s, bad.password_len = DH_SETUP_MAX_PASSWORD_BYTES + 1;
    CHECK(dh_setup_encode(&bad, other) == 0);
    bad = s, bad.has_wifi = false;
    CHECK(dh_setup_encode(&bad, other) == 0);
    bad = s, bad.password[0] = (char)0xff;
    CHECK(dh_setup_encode(&bad, other) == 0);

    dh_setup_wipe(&s);
    CHECK(memcmp(&s, &zero, sizeof zero) == 0);
}

int main(void)
{
    test_every_rule();
    test_missing_repeated_and_unknown_members();
    test_what_is_stored();
    test_the_size_limit();
    test_the_refusal_order();
    test_storage();
    test_factory_reset();
    test_the_examples();
    test_hello_and_other_lines();
    test_a_session_in_pieces();
    test_the_stored_form();
    printf("usb tests: %d checks, %d failures\n", checks, failures);
    return failures == 0 ? 0 : 1;
}
