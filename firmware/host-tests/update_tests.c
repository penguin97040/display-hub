/*
 * Unit tests for the signed-update core (dh_crypto, dh_update): the edges the firmware vectors
 * in protocol/v1/firmware don't reach. Every refusal reason, the manifest's rules one at a time,
 * pieces of every awkward size, the budget's boundaries, the stored forms, and a power cut at
 * each save point.
 *
 * Packages are signed here with throwaway keys made when the tests start. Signing is the only
 * thing that uses Mbed TLS's PSA calls; the core itself never does.
 */
#include <inttypes.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "dh_update.h"
#include "psa/crypto.h"

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

/* ---- Keys and packages ---- */

typedef struct {
    psa_key_id_t id;
    dh_fw_key public;
} signer;

static signer maker, recovery, fresh, stranger;

static void make_signer(signer *s)
{
    psa_key_attributes_t a = psa_key_attributes_init();
    psa_set_key_type(&a, PSA_KEY_TYPE_ECC_KEY_PAIR(PSA_ECC_FAMILY_SECP_R1));
    psa_set_key_bits(&a, 256);
    psa_set_key_usage_flags(&a, PSA_KEY_USAGE_SIGN_HASH);
    psa_set_key_algorithm(&a, PSA_ALG_ECDSA(PSA_ALG_SHA_256));
    size_t n = 0;
    if (psa_crypto_init() != PSA_SUCCESS || psa_generate_key(&a, &s->id) != PSA_SUCCESS ||
        psa_export_public_key(s->id, s->public.bytes, DH_FW_KEY_LEN, &n) != PSA_SUCCESS ||
        n != DH_FW_KEY_LEN) {
        fprintf(stderr, "can't make a test key\n");
        exit(1);
    }
}

static void to_hex(const uint8_t *in, size_t n, char *out)
{
    static const char DIGITS[] = "0123456789abcdef";
    for (size_t i = 0; i < n; i++) {
        out[2 * i] = DIGITS[in[i] >> 4];
        out[2 * i + 1] = DIGITS[in[i] & 15];
    }
    out[2 * n] = '\0';
}

static const char *key_hex(const dh_fw_key *k, char out[131])
{
    to_hex(k->bytes, DH_FW_KEY_LEN, out);
    return out;
}

/* Signs the SHA-256 of the fixed text (if with_context), header bytes 0 to 72 and the manifest. */
static void sign_header(const signer *s, uint8_t *header, const uint8_t *manifest, size_t len,
                        bool with_context)
{
    static const uint8_t context[24] = "display-hub.firmware.v1";
    uint8_t digest[DH_SHA256_LEN];
    dh_sha256 h;
    dh_sha256_begin(&h);
    if (with_context) {
        dh_sha256_update(&h, context, sizeof context);
    }
    dh_sha256_update(&h, header, DH_FW_SIGNED_HEADER_LEN);
    dh_sha256_update(&h, manifest, len);
    size_t n = 0;
    if (!dh_sha256_finish(&h, digest) ||
        psa_sign_hash(s->id, PSA_ALG_ECDSA(PSA_ALG_SHA_256), digest, sizeof digest,
                      header + DH_FW_SIGNED_HEADER_LEN, DH_FW_SIGNATURE_LEN, &n) != PSA_SUCCESS ||
        n != DH_FW_SIGNATURE_LEN) {
        fprintf(stderr, "can't sign\n");
        exit(1);
    }
}

/* The update form of a package: header, manifest, image. Returns its length. */
static size_t build(uint8_t *out, const signer *s, const char *manifest, size_t manifest_len,
                    const uint8_t *image, size_t image_len)
{
    memset(out, 0, DH_FW_HEADER_LEN);
    memcpy(out, "DHFW", 4);
    out[4] = 1;
    out[5] = 1;
    out[6] = (uint8_t)(manifest_len & 0xff);
    out[7] = (uint8_t)(manifest_len >> 8);
    memcpy(out + 8, s->public.bytes, DH_FW_KEY_LEN);
    memcpy(out + DH_FW_HEADER_LEN, manifest, manifest_len);
    sign_header(s, out, (const uint8_t *)manifest, manifest_len, true);
    memcpy(out + DH_FW_HEADER_LEN + manifest_len, image, image_len);
    return DH_FW_HEADER_LEN + manifest_len + image_len;
}

static const char DEVICE_TYPE[] = "llm-usage-display";
static const char HARDWARE[] = "test-board-v1";

/* A manifest for image, with the usual fields and `extra` (more members, each starting with a
 * comma) at the end. */
static size_t manifest_for(char *out, size_t cap, const uint8_t *image, size_t image_len,
                           const char *version, uint32_t counter, const char *extra)
{
    uint8_t digest[DH_SHA256_LEN];
    char hex[65];
    if (!dh_sha256_of(image, image_len, digest)) {
        exit(1);
    }
    to_hex(digest, sizeof digest, hex);
    int n = snprintf(out, cap,
                     "{\"device_type\":\"%s\",\"hardware_revisions\":[\"other-board\",\"%s\"],"
                     "\"firmware_version\":\"%s\",\"security_counter\":%" PRIu32
                     ",\"image_size\":%zu,\"image_sha256\":\"%s\"%s}",
                     DEVICE_TYPE, HARDWARE, version, counter, image_len, hex, extra);
    if (n < 0 || (size_t)n >= cap) {
        fprintf(stderr, "a test manifest doesn't fit\n");
        exit(1);
    }
    return (size_t)n;
}

static dh_fw_keys keys_of(const signer *a, const signer *b, uint32_t floor)
{
    dh_fw_keys k;
    dh_fw_keys_init(&k);
    if (a) {
        k.keys[k.key_count++] = a->public;
    }
    if (b) {
        k.keys[k.key_count++] = b->public;
    }
    k.security_floor = floor;
    return k;
}

static const dh_fw_device DEVICE = {DEVICE_TYPE, HARDWARE, 1u << 20};

/* ---- Running a download ---- */

typedef struct {
    dh_fw_reason reason; /* NONE: it finished */
    bool opened;
    uint64_t read;
    size_t written;
    dh_fw_manifest manifest;
} outcome;

static dh_fw_download dl;
static uint8_t spare[9u << 20];

/* Feeds body to a download in pieces of `piece` bytes (each in a buffer of exactly its size, so
 * the sanitisers catch a read past it), and gives up when it runs out. */
static outcome download(const dh_fw_keys *keys, const dh_fw_device *device, const uint8_t *body,
                        size_t body_len, uint64_t offer_size, bool has_length,
                        uint64_t content_length, size_t piece)
{
    outcome o;
    memset(&o, 0, sizeof o);
    o.reason = dh_fw_download_begin(&dl, keys, device, offer_size, has_length, content_length);
    if (o.reason != DH_FW_REASON_NONE) {
        CHECK(dh_fw_download_push(&dl, body, body_len).step == DH_FW_STEP_ENDED);
        CHECK(dh_fw_download_bytes_read(&dl) == 0);
        return o;
    }
    size_t at = 0;
    bool over = false, finished = false;
    while (!over) {
        if (at == body_len) {
            o.reason = dh_fw_download_fail(&dl, DH_FW_REASON_DOWNLOAD);
            break;
        }
        size_t n = body_len - at < piece ? body_len - at : piece;
        uint8_t *exact = malloc(n);
        if (!exact) {
            exit(1);
        }
        memcpy(exact, body + at, n);
        size_t used = 0;
        while (used < n && !over) {
            dh_fw_push p = dh_fw_download_push(&dl, exact + used, n - used);
            CHECK(p.used <= n - used);
            CHECK(p.step != DH_FW_STEP_MORE || p.used == n - used);
            CHECK(p.write_len == 0 || (o.opened && p.step != DH_FW_STEP_ENDED &&
                                       p.step != DH_FW_STEP_OPEN_SLOT));
            CHECK(p.write_len == 0 ||
                  (p.write >= exact + used && p.write + p.write_len <= exact + used + p.used));
            if (p.write_len && o.written + p.write_len <= sizeof spare) {
                memcpy(spare + o.written, p.write, p.write_len);
                o.written += p.write_len;
            }
            used += p.used;
            if (p.step == DH_FW_STEP_OPEN_SLOT) {
                CHECK(!o.opened);
                o.opened = true;
                o.manifest = *dh_fw_download_manifest(&dl);
            } else if (p.step == DH_FW_STEP_FINISH) {
                finished = over = true;
            } else if (p.step == DH_FW_STEP_ENDED) {
                o.reason = p.reason;
                over = true;
            }
        }
        free(exact);
        at += used;
    }
    o.read = dh_fw_download_bytes_read(&dl);
    CHECK(o.read <= content_length);
    CHECK(o.read == at);
    CHECK(o.opened == dh_fw_download_slot_open(&dl));
    CHECK((dh_fw_download_manifest(&dl) != NULL) == o.opened);
    CHECK(finished == (o.reason == DH_FW_REASON_NONE));
    /* Once over, it stays over and takes nothing more. */
    dh_fw_push again = dh_fw_download_push(&dl, body, body_len);
    CHECK(again.used == 0 && again.write_len == 0);
    CHECK(again.step == (finished ? DH_FW_STEP_FINISH : DH_FW_STEP_ENDED));
    CHECK(finished || again.reason == o.reason);
    return o;
}

static const size_t PIECES[] = {1, 2, 3, 5, 31, 97, 136, 137, 138, 139, 1021, 4096, 65537};

/* The same body at every piece size must end the same way, having read the same amount. */
static outcome download_all(const dh_fw_keys *keys, const dh_fw_device *device,
                            const uint8_t *body, size_t body_len, uint64_t content_length)
{
    outcome first = download(keys, device, body, body_len, content_length, true, content_length,
                             body_len ? body_len : 1);
    for (size_t i = 0; i < LEN(PIECES); i++) {
        outcome o = download(keys, device, body, body_len, content_length, true, content_length,
                             PIECES[i]);
        CHECK(o.reason == first.reason && o.read == first.read && o.opened == first.opened);
        /* When the hash doesn't match, the last piece isn't handed over to be written, so how
         * much was written depends on where that piece began. */
        CHECK(o.written == first.written || o.reason == DH_FW_REASON_IMAGE_HASH);
    }
    return first;
}

static uint8_t image[70000];
static uint8_t package[80000];
static char manifest[DH_FW_MAX_MANIFEST_LEN + 64];

static void fill_image(void)
{
    uint32_t x = 12345;
    for (size_t i = 0; i < sizeof image; i++) {
        x = x * 1664525u + 1013904223u;
        image[i] = (uint8_t)(x >> 24);
    }
}

/* ---- Cryptography ---- */

static void test_crypto(void)
{
    static const uint8_t abc_digest[32] = {0xba, 0x78, 0x16, 0xbf, 0x8f, 0x01, 0xcf, 0xea,
                                           0x41, 0x41, 0x40, 0xde, 0x5d, 0xae, 0x22, 0x23,
                                           0xb0, 0x03, 0x61, 0xa3, 0x96, 0x17, 0x7a, 0x9c,
                                           0xb4, 0x10, 0xff, 0x61, 0xf2, 0x00, 0x15, 0xad};
    static const uint8_t empty_digest[32] = {0xe3, 0xb0, 0xc4, 0x42, 0x98, 0xfc, 0x1c, 0x14,
                                             0x9a, 0xfb, 0xf4, 0xc8, 0x99, 0x6f, 0xb9, 0x24,
                                             0x27, 0xae, 0x41, 0xe4, 0x64, 0x9b, 0x93, 0x4c,
                                             0xa4, 0x95, 0x99, 0x1b, 0x78, 0x52, 0xb8, 0x55};
    uint8_t d[32], whole[32];
    CHECK(dh_sha256_of((const uint8_t *)"abc", 3, d) && memcmp(d, abc_digest, 32) == 0);
    CHECK(dh_sha256_of(NULL, 0, d) && memcmp(d, empty_digest, 32) == 0);
    /* A byte at a time, and in odd pieces, gives the same digest as all at once. */
    CHECK(dh_sha256_of(image, 5000, whole));
    size_t steps[] = {1, 3, 63, 64, 65, 127, 1000};
    for (size_t s = 0; s < LEN(steps); s++) {
        dh_sha256 h;
        dh_sha256_begin(&h);
        for (size_t at = 0; at < 5000; at += steps[s]) {
            size_t n = 5000 - at < steps[s] ? 5000 - at : steps[s];
            dh_sha256_update(&h, image + at, n);
        }
        CHECK(dh_sha256_finish(&h, d) && memcmp(d, whole, 32) == 0);
        /* Finished: another finish has nothing to give. */
        CHECK(!dh_sha256_finish(&h, d));
    }
    dh_sha256 h;
    memset(&h, 0, sizeof h);
    dh_sha256_abandon(&h);
    dh_sha256_begin(&h);
    dh_sha256_update(&h, image, 10);
    dh_sha256_abandon(&h);
    dh_sha256_update(&h, image, 10);
    CHECK(!dh_sha256_finish(&h, d));
    static const uint8_t zeros[32];
    CHECK(memcmp(d, zeros, 32) == 0);

    /* Public keys: as crates/firmware-package/src/key.rs tests them. */
    CHECK(dh_p256_key_valid(maker.public.bytes));
    dh_fw_key k = maker.public;
    k.bytes[64] ^= 1;
    CHECK(!dh_p256_key_valid(k.bytes)); /* off the curve */
    k = maker.public;
    k.bytes[0] = 0x05;
    CHECK(!dh_p256_key_valid(k.bytes));
    k.bytes[0] = 0x02; /* a compressed point's prefix on 65 bytes */
    CHECK(!dh_p256_key_valid(k.bytes));
    k.bytes[0] = 0x00;
    CHECK(!dh_p256_key_valid(k.bytes));
    memset(k.bytes, 0, sizeof k.bytes);
    CHECK(!dh_p256_key_valid(k.bytes));
    k.bytes[0] = 0x04;
    CHECK(!dh_p256_key_valid(k.bytes)); /* (0, 0) */
    memset(k.bytes, 0xff, sizeof k.bytes);
    k.bytes[0] = 0x04;
    CHECK(!dh_p256_key_valid(k.bytes)); /* coordinates above the field prime */
    /* The vectors' own "not a point": (1, 1). */
    memset(k.bytes, 0, sizeof k.bytes);
    k.bytes[0] = 0x04;
    k.bytes[32] = 1;
    k.bytes[64] = 1;
    CHECK(!dh_p256_key_valid(k.bytes));

    /* Signatures. */
    uint8_t header[DH_FW_HEADER_LEN] = "DHFW";
    memcpy(header + 8, maker.public.bytes, DH_FW_KEY_LEN);
    const uint8_t *m = (const uint8_t *)"{}";
    sign_header(&maker, header, m, 2, true);
    CHECK(dh_fw_signature_valid(&maker.public, header, m, 2));
    CHECK(!dh_fw_signature_valid(&recovery.public, header, m, 2));
    CHECK(!dh_fw_signature_valid(&maker.public, header, (const uint8_t *)"{ }", 3));
    CHECK(!dh_fw_signature_valid(&maker.public, header, m, 1));
    for (size_t bit = 0; bit < DH_FW_SIGNATURE_LEN * 8; bit += 37) {
        uint8_t flipped[DH_FW_HEADER_LEN];
        memcpy(flipped, header, sizeof flipped);
        flipped[DH_FW_SIGNED_HEADER_LEN + bit / 8] ^= (uint8_t)(1u << (bit % 8));
        CHECK(!dh_fw_signature_valid(&maker.public, flipped, m, 2));
    }
    /* Any signed header byte changed. */
    for (size_t i = 0; i < DH_FW_SIGNED_HEADER_LEN; i += 9) {
        uint8_t changed[DH_FW_HEADER_LEN];
        memcpy(changed, header, sizeof changed);
        changed[i] ^= 0x80;
        CHECK(!dh_fw_signature_valid(&maker.public, changed, m, 2));
    }
    /* r or s of zero, and of the group order or more. */
    uint8_t bad[DH_FW_HEADER_LEN];
    memcpy(bad, header, sizeof bad);
    memset(bad + DH_FW_SIGNED_HEADER_LEN, 0, DH_FW_SIGNATURE_LEN);
    CHECK(!dh_fw_signature_valid(&maker.public, bad, m, 2));
    memcpy(bad, header, sizeof bad);
    memset(bad + DH_FW_SIGNED_HEADER_LEN, 0, 32);
    CHECK(!dh_fw_signature_valid(&maker.public, bad, m, 2));
    memcpy(bad, header, sizeof bad);
    memset(bad + DH_FW_SIGNED_HEADER_LEN + 32, 0, 32);
    CHECK(!dh_fw_signature_valid(&maker.public, bad, m, 2));
    memcpy(bad, header, sizeof bad);
    memset(bad + DH_FW_SIGNED_HEADER_LEN, 0xff, 32);
    CHECK(!dh_fw_signature_valid(&maker.public, bad, m, 2));
    memcpy(bad, header, sizeof bad);
    memset(bad + DH_FW_SIGNED_HEADER_LEN + 32, 0xff, 32);
    CHECK(!dh_fw_signature_valid(&maker.public, bad, m, 2));
    /* Signed without the fixed text in front. */
    sign_header(&maker, header, m, 2, false);
    CHECK(!dh_fw_signature_valid(&maker.public, header, m, 2));
    /* A key that isn't a point verifies nothing. */
    dh_fw_key not_a_point = maker.public;
    not_a_point.bytes[40] ^= 4;
    CHECK(!dh_fw_signature_valid(&not_a_point, header, m, 2));

    /* A key id is the SHA-256 of the 65 bytes, in lower-case hex. */
    char id[65], want[65];
    CHECK(dh_sha256_of(maker.public.bytes, DH_FW_KEY_LEN, d));
    to_hex(d, 32, want);
    CHECK(dh_fw_key_id_hex(&maker.public, id) && strcmp(id, want) == 0 && strlen(id) == 64);
}

/* ---- Names ---- */

static void test_names(void)
{
    static const char *reasons[] = {NULL,           "no_trusted_keys",   "too_large",
                                    "bad_format",   "untrusted_key",     "bad_signature",
                                    "wrong_device_type", "wrong_hardware", "downgrade",
                                    "download",     "image_hash",        "flash"};
    for (int r = 0; r <= DH_FW_REASON_FLASH; r++) {
        const char *name = dh_fw_reason_name((dh_fw_reason)r);
        CHECK(r == 0 ? name == NULL : name != NULL && strcmp(name, reasons[r]) == 0);
        dh_fw_report_state st = dh_fw_reason_state((dh_fw_reason)r);
        CHECK(st == (r == 0                          ? DH_FW_REPORT_NONE
                     : r <= DH_FW_REASON_DOWNGRADE   ? DH_FW_REPORT_REFUSED
                                                     : DH_FW_REPORT_FAILED));
    }
    CHECK(dh_fw_reason_name((dh_fw_reason)99) == NULL);
    CHECK(dh_fw_reason_state((dh_fw_reason)99) == DH_FW_REPORT_NONE);
    static const char *states[] = {NULL, "downloading", "installed", "refused", "failed",
                                   "rolled_back"};
    for (int s = 0; s <= DH_FW_REPORT_ROLLED_BACK; s++) {
        const char *name = dh_fw_report_state_name((dh_fw_report_state)s);
        CHECK(s == 0 ? name == NULL : name != NULL && strcmp(name, states[s]) == 0);
    }
    CHECK(dh_fw_report_state_name((dh_fw_report_state)99) == NULL);
}

/* ---- Versions and the header ---- */

static void test_versions(void)
{
    static const char *good[] = {"1.2.0",          "0.0.0",     "1.3.0-beta.1", "10.20.30-rc-2",
                                 "4294967295.0.0", "1.0.0-A.b", "0.0.0-simulator",
                                 "1.0.0-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"};
    for (size_t i = 0; i < LEN(good); i++) {
        CHECK(dh_fw_version_valid(good[i], strlen(good[i])));
    }
    CHECK(strlen(good[7]) == DH_FW_MAX_VERSION_BYTES);
    static const char *bad[] = {"",       "1",           "1.2",           "1.2.3.4",
                                "v1.2.0", "01.2.0",      "1.02.0",        "1.2.x",
                                "1.2.0-", "1.2.0-beta_1", "1.2.0+build",  "1..0",
                                "4294967296.0.0",        "1.2.0-b\xc3\xaata", " 1.2.0",
                                "1.2.0 ", "1.2.00",      ".1.2",          "1.2.",
                                "1.2.-a", "-1.2.3",      "1.2.3-a b",
                                "99999999999999999999999999999999.0.0",
                                "1.0.0-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"};
    for (size_t i = 0; i < LEN(bad); i++) {
        CHECK(!dh_fw_version_valid(bad[i], strlen(bad[i])));
    }
    /* A NUL inside isn't the end. */
    CHECK(!dh_fw_version_valid("1.2.3\0", 6));
    CHECK(!dh_fw_version_valid("1.2.3-a\0b", 9));
}

static void test_header(void)
{
    uint8_t h[DH_FW_HEADER_LEN];
    uint16_t len = 0;
    memset(h, 0, sizeof h);
    memcpy(h, "DHFW", 4);
    h[4] = 1;
    h[5] = 1;
    h[6] = 2;
    CHECK(dh_fw_header_check(h, &len) && len == 2);
    h[6] = 0x00;
    h[7] = 0x10;
    CHECK(dh_fw_header_check(h, &len) && len == 4096);
    h[6] = 0x34;
    h[7] = 0x02; /* little-endian */
    CHECK(dh_fw_header_check(h, &len) && len == 0x0234);
    static const struct {
        uint8_t lo, hi;
    } lengths[] = {{0, 0}, {1, 0}, {1, 0x10}, {0xff, 0xff}, {0, 0x20}};
    for (size_t i = 0; i < LEN(lengths); i++) {
        h[6] = lengths[i].lo;
        h[7] = lengths[i].hi;
        CHECK(!dh_fw_header_check(h, &len));
    }
    h[6] = 2;
    h[7] = 0;
    for (size_t i = 0; i < 6; i++) {
        uint8_t was = h[i];
        h[i] ^= 0x20; /* each magic byte, the format version and the algorithm */
        CHECK(!dh_fw_header_check(h, &len));
        h[i] = was;
    }
    h[4] = 0;
    CHECK(!dh_fw_header_check(h, &len));
    h[4] = 2;
    CHECK(!dh_fw_header_check(h, &len));
    h[4] = 1;
    h[5] = 0;
    CHECK(!dh_fw_header_check(h, &len));
    h[5] = 2;
    CHECK(!dh_fw_header_check(h, &len));
}

/* ---- The manifest ---- */

static dh_text_scratch scratch;
static dh_fw_manifest parsed;

static bool parses(const char *json)
{
    return dh_fw_manifest_parse((const uint8_t *)json, strlen(json), &parsed, &scratch);
}

#define SHA "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
#define BASE_HEAD "{\"device_type\":\"d\",\"hardware_revisions\":[\"h\"],\"firmware_version\":\"1.0.0\""
#define BASE BASE_HEAD ",\"security_counter\":0,\"image_size\":1,\"image_sha256\":\"" SHA "\""

static void test_manifest(void)
{
    char hex[4][131];
    key_hex(&maker.public, hex[0]);
    key_hex(&recovery.public, hex[1]);
    key_hex(&fresh.public, hex[2]);
    key_hex(&stranger.public, hex[3]);
    static char text[8192];

    CHECK(parses(BASE "}"));
    CHECK(strcmp(parsed.device_type, "d") == 0 && parsed.hardware_revision_count == 1 &&
          strcmp(parsed.hardware_revisions[0], "h") == 0 &&
          strcmp(parsed.firmware_version, "1.0.0") == 0 && parsed.security_counter == 0 &&
          parsed.image_size == 1 && parsed.image_sha256[0] == 0x01 &&
          parsed.image_sha256[31] == 0xef && !parsed.has_key_set && parsed.key_count == 0 &&
          !parsed.has_summary);
    CHECK(parses(" \n" BASE "}\r\n\t "));
    /* Members in any order, with ones a display doesn't know. */
    CHECK(parses("{\"zzz\":[1,{\"a\":null}],\"image_sha256\":\"" SHA "\",\"image_size\":8388608,"
                 "\"security_counter\":4294967295,\"firmware_version\":\"2.0.0-rc.1\","
                 "\"hardware_revisions\":[\"a\",\"b\"],\"device_type\":\"x\",\"new\":true}"));
    CHECK(parsed.image_size == 8388608 && parsed.security_counter == 4294967295u &&
          parsed.hardware_revision_count == 2);

    /* Not one JSON object. */
    static const char *not_objects[] = {"",   " ",  "[]", "1",  "null",   "\"x\"", "{",
                                        BASE, BASE "}}", BASE "} {}", BASE "},", "\xef\xbb\xbf" BASE "}",
                                        BASE ",}", "{" BASE "}}"};
    for (size_t i = 0; i < LEN(not_objects); i++) {
        CHECK(!parses(not_objects[i]));
    }
    CHECK(!dh_fw_manifest_parse((const uint8_t *)BASE "}\0", sizeof BASE "}", &parsed, &scratch));
    /* Bytes that aren't UTF-8, in a member a display ignores. */
    CHECK(!parses(BASE ",\"x\":\"\xff\"}"));
    CHECK(!parses(BASE ",\"x\":\"\xc0\xaf\"}"));
    CHECK(!parses(BASE ",\"x\":\"\\ud800\"}"));

    /* Each required member missing, null, or of the wrong type. */
    static const char *required[] = {"device_type",      "hardware_revisions", "firmware_version",
                                     "security_counter", "image_size",         "image_sha256"};
    static const char *values[] = {"\"d\"", "[\"h\"]", "\"1.0.0\"", "0", "1", "\"" SHA "\""};
    static const char *wrong[] = {"null", "true", "0", "\"0\"", "[]", "{}", "[0]", "[\"h\",1]"};
    for (size_t skip = 0; skip < LEN(required); skip++) {
        for (size_t w = 0; w <= LEN(wrong); w++) {
            size_t n = 0;
            text[n++] = '{';
            bool first = true;
            for (size_t i = 0; i < LEN(required); i++) {
                const char *value = values[i];
                if (i == skip) {
                    if (w == LEN(wrong)) {
                        continue; /* missing */
                    }
                    value = wrong[w];
                    /* Some of these are the right type for some members. */
                    bool is_number = strcmp(value, "0") == 0;
                    bool is_string = strcmp(value, "\"0\"") == 0;
                    if ((is_number && i == 3) || (is_string && i == 0)) {
                        continue;
                    }
                }
                n += (size_t)snprintf(text + n, sizeof text - n, "%s\"%s\":%s", first ? "" : ",",
                                      required[i], value);
                first = false;
            }
            snprintf(text + n, sizeof text - n, "}");
            CHECK(!parses(text));
        }
    }

    /* Repeated member names, at any level, known or not, however they are written. */
    CHECK(!parses(BASE ",\"device_type\":\"d\"}"));
    CHECK(!parses(BASE ",\"x\":1,\"x\":1}"));
    CHECK(!parses(BASE ",\"x\":{\"a\":1,\"a\":2}}"));
    CHECK(!parses(BASE ",\"usb_install\":{\"parts\":[{\"name\":\"a\",\"name\":\"b\"}]}}"));
    CHECK(!parses(BASE ",\"x\":1,\"\\u0078\":2}"));
    CHECK(!parses(BASE ",\"image_si\\u007ae\":2}"));
    /* Four levels at most; the manifest object is the first. */
    CHECK(parses(BASE ",\"x\":{\"b\":[{\"c\":1}]}}"));
    CHECK(!parses(BASE ",\"x\":{\"b\":[{\"c\":[]}]}}"));
    CHECK(!parses(BASE ",\"x\":[[[[]]]]}"));
    CHECK(parses(BASE ",\"x\":[[[]]]}"));

    /* Numbers are whole, without sign, fraction or exponent, anywhere, and fit in 64 bits. */
    static const char *numbers[] = {"-1", "-0", "1.0", "1.5", "1e2", "1E2", "0.0",
                                    "18446744073709551616", "99999999999999999999",
                                    "184467440737095516150", "01", "+1"};
    for (size_t i = 0; i < LEN(numbers); i++) {
        snprintf(text, sizeof text, BASE ",\"x\":%s}", numbers[i]);
        CHECK(!parses(text));
        snprintf(text, sizeof text, BASE ",\"x\":[{\"n\":[%s]}]}", numbers[i]);
        CHECK(!parses(text));
        snprintf(text, sizeof text, BASE_HEAD ",\"security_counter\":0,\"image_size\":%s,"
                                              "\"image_sha256\":\"" SHA "\"}", numbers[i]);
        CHECK(!parses(text));
    }
    CHECK(parses(BASE ",\"x\":18446744073709551615}"));
    CHECK(parses(BASE ",\"x\":[0,18446744073709551615,9999999999999999999]}"));
    /* Digits inside strings aren't numbers, whatever the escapes around them. */
    CHECK(parses(BASE ",\"x\":\"99999999999999999999999\"}"));
    CHECK(parses(BASE ",\"x\":\"\\\"99999999999999999999999\"}"));
    CHECK(parses(BASE ",\"x\":\"\\\\\",\"y\":1}"));
    CHECK(!parses(BASE ",\"x\":\"\\\\\",\"y\":99999999999999999999999}"));
    CHECK(parses(BASE ",\"99999999999999999999999\":1}"));

    /* Each field's range. */
    static const struct {
        const char *counter, *size;
        bool ok;
    } ranges[] = {{"0", "1", true},          {"4294967295", "8388608", true},
                  {"4294967296", "1", false}, {"0", "0", false},
                  {"0", "8388609", false},    {"0", "4294967297", false},
                  {"18446744073709551615", "1", false}};
    for (size_t i = 0; i < LEN(ranges); i++) {
        snprintf(text, sizeof text, BASE_HEAD ",\"security_counter\":%s,\"image_size\":%s,"
                                              "\"image_sha256\":\"" SHA "\"}",
                 ranges[i].counter, ranges[i].size);
        CHECK(parses(text) == ranges[i].ok);
    }
    static const char *hashes[] = {"", "0123", SHA "0", SHA "00",
                                   "0123456789ABCDEF0123456789abcdef0123456789abcdef0123456789abcdef",
                                   "g123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                                   "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcde",
                                   "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcd\\u0000"};
    for (size_t i = 0; i < LEN(hashes); i++) {
        snprintf(text, sizeof text, BASE_HEAD ",\"security_counter\":0,\"image_size\":1,"
                                              "\"image_sha256\":\"%s\"}", hashes[i]);
        CHECK(!parses(text));
    }
    /* An escape that spells a hex digit is still that digit. */
    CHECK(parses(BASE_HEAD ",\"security_counter\":0,\"image_size\":1,\"image_sha256\":"
                           "\"\\u0030123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\"}"));

    /* device_type and hardware revisions follow the text rules, with at most 48 characters. */
    static const struct {
        const char *text;
        bool ok;
    } texts[] = {{"llm-usage-display", true},
                 {"T\xc4\x93n\xc4\x81 koe", true},
                 {"123456789012345678901234567890123456789012345678", true},
                 {"1234567890123456789012345678901234567890123456789", false},
                 /* 48 characters of four bytes each: 192 bytes. */
                 {"\xf0\x9f\x98\x80\xf0\x9f\x98\x80\xf0\x9f\x98\x80\xf0\x9f\x98\x80\xf0\x9f\x98\x80\xf0\x9f\x98\x80"
                  "\xf0\x9f\x98\x80\xf0\x9f\x98\x80\xf0\x9f\x98\x80\xf0\x9f\x98\x80\xf0\x9f\x98\x80\xf0\x9f\x98\x80"
                  "\xf0\x9f\x98\x80\xf0\x9f\x98\x80\xf0\x9f\x98\x80\xf0\x9f\x98\x80\xf0\x9f\x98\x80\xf0\x9f\x98\x80"
                  "\xf0\x9f\x98\x80\xf0\x9f\x98\x80\xf0\x9f\x98\x80\xf0\x9f\x98\x80\xf0\x9f\x98\x80\xf0\x9f\x98\x80"
                  "\xf0\x9f\x98\x80\xf0\x9f\x98\x80\xf0\x9f\x98\x80\xf0\x9f\x98\x80\xf0\x9f\x98\x80\xf0\x9f\x98\x80"
                  "\xf0\x9f\x98\x80\xf0\x9f\x98\x80\xf0\x9f\x98\x80\xf0\x9f\x98\x80\xf0\x9f\x98\x80\xf0\x9f\x98\x80"
                  "\xf0\x9f\x98\x80\xf0\x9f\x98\x80\xf0\x9f\x98\x80\xf0\x9f\x98\x80\xf0\x9f\x98\x80\xf0\x9f\x98\x80"
                  "\xf0\x9f\x98\x80\xf0\x9f\x98\x80\xf0\x9f\x98\x80\xf0\x9f\x98\x80\xf0\x9f\x98\x80\xf0\x9f\x98\x80",
                  true},
                 {"", false},
                 {" padded", false},
                 {"padded ", false},
                 {"a\\u0007b", false},
                 {"a\\u0000b", false},
                 {"a\xe2\x80\xae" "b", false}, /* a right-to-left override */
                 {"e\xcc\x81", false},         /* e and a combining acute: not NFC */
                 {"\xc3\xa9", true}};
    for (size_t i = 0; i < LEN(texts); i++) {
        snprintf(text, sizeof text,
                 "{\"device_type\":\"%s\",\"hardware_revisions\":[\"h\"],\"firmware_version\":"
                 "\"1.0.0\",\"security_counter\":0,\"image_size\":1,\"image_sha256\":\"" SHA "\"}",
                 texts[i].text);
        CHECK(parses(text) == texts[i].ok);
        snprintf(text, sizeof text,
                 "{\"device_type\":\"d\",\"hardware_revisions\":[\"h\",\"%s\"],\"firmware_version\":"
                 "\"1.0.0\",\"security_counter\":0,\"image_size\":1,\"image_sha256\":\"" SHA "\"}",
                 texts[i].text);
        CHECK(parses(text) == texts[i].ok);
    }
    /* 1 to 8 hardware revisions, no repeats. */
    static const struct {
        const char *list;
        bool ok;
    } lists[] = {{"[]", false},
                 {"[\"a\"]", true},
                 {"[\"a\",\"b\",\"c\",\"d\",\"e\",\"f\",\"g\",\"h\"]", true},
                 {"[\"a\",\"b\",\"c\",\"d\",\"e\",\"f\",\"g\",\"h\",\"i\"]", false},
                 {"[\"a\",\"b\",\"a\"]", false},
                 {"[\"a\",\"\\u0061\"]", false},
                 {"[\"a\",\"A\"]", true},
                 {"[\"a\",null]", false},
                 {"[[\"a\"]]", false},
                 {"\"a\"", false},
                 {"{\"a\":\"a\"}", false}};
    for (size_t i = 0; i < LEN(lists); i++) {
        snprintf(text, sizeof text,
                 "{\"device_type\":\"d\",\"hardware_revisions\":%s,\"firmware_version\":\"1.0.0\","
                 "\"security_counter\":0,\"image_size\":1,\"image_sha256\":\"" SHA "\"}",
                 lists[i].list);
        CHECK(parses(text) == lists[i].ok);
    }
    /* The version's rules reach the manifest. */
    CHECK(!parses("{\"device_type\":\"d\",\"hardware_revisions\":[\"h\"],\"firmware_version\":"
                  "\"1.0\",\"security_counter\":0,\"image_size\":1,\"image_sha256\":\"" SHA "\"}"));
    CHECK(!parses("{\"device_type\":\"d\",\"hardware_revisions\":[\"h\"],\"firmware_version\":"
                  "\"1.0.0\\u0000\",\"security_counter\":0,\"image_size\":1,\"image_sha256\":\"" SHA "\"}"));

    /* summary: optional, text of at most 280 characters, and never null. */
    CHECK(parses(BASE ",\"summary\":\"Fixes the clock.\"}") && parsed.has_summary &&
          strcmp(parsed.summary, "Fixes the clock.") == 0);
    CHECK(!parses(BASE ",\"summary\":null}"));
    CHECK(!parses(BASE ",\"summary\":1}"));
    CHECK(!parses(BASE ",\"summary\":\"\"}"));
    CHECK(!parses(BASE ",\"summary\":\"line\\nbreak\"}"));
    char long_text[1200];
    memset(long_text, 'a', 281);
    long_text[280] = '\0';
    snprintf(text, sizeof text, BASE ",\"summary\":\"%s\"}", long_text);
    CHECK(parses(text) && strlen(parsed.summary) == 280);
    long_text[280] = 'a';
    long_text[281] = '\0';
    snprintf(text, sizeof text, BASE ",\"summary\":\"%s\"}", long_text);
    CHECK(!parses(text));
    /* 280 characters of three bytes each fit too. */
    for (size_t i = 0; i < 280; i++) {
        memcpy(long_text + 3 * i, "\xe2\x82\xac", 3);
    }
    long_text[840] = '\0';
    snprintf(text, sizeof text, BASE ",\"summary\":\"%s\"}", long_text);
    CHECK(parses(text) && strlen(parsed.summary) == 840);

    /* key_set: 1 to 3 keys, each a valid point, no repeats, never null. */
    snprintf(text, sizeof text, BASE ",\"key_set\":[\"%s\"]}", hex[0]);
    CHECK(parses(text) && parsed.has_key_set && parsed.key_count == 1 &&
          memcmp(parsed.key_set[0].bytes, maker.public.bytes, DH_FW_KEY_LEN) == 0);
    snprintf(text, sizeof text, BASE ",\"key_set\":[\"%s\",\"%s\",\"%s\"]}", hex[0], hex[1], hex[2]);
    CHECK(parses(text) && parsed.key_count == 3 &&
          memcmp(parsed.key_set[2].bytes, fresh.public.bytes, DH_FW_KEY_LEN) == 0);
    snprintf(text, sizeof text, BASE ",\"key_set\":[\"%s\",\"%s\",\"%s\",\"%s\"]}", hex[0], hex[1],
             hex[2], hex[3]);
    CHECK(!parses(text));
    snprintf(text, sizeof text, BASE ",\"key_set\":[\"%s\",\"%s\"]}", hex[0], hex[0]);
    CHECK(!parses(text));
    CHECK(!parses(BASE ",\"key_set\":[]}"));
    CHECK(!parses(BASE ",\"key_set\":null}"));
    CHECK(!parses(BASE ",\"key_set\":\"x\"}"));
    CHECK(!parses(BASE ",\"key_set\":[null]}"));
    snprintf(text, sizeof text, BASE ",\"key_set\":\"%s\"}", hex[0]);
    CHECK(!parses(text));
    char altered[131];
    memcpy(altered, hex[0], sizeof altered);
    altered[129] = altered[129] == '0' ? '1' : '0'; /* off the curve */
    snprintf(text, sizeof text, BASE ",\"key_set\":[\"%s\"]}", altered);
    CHECK(!parses(text));
    memcpy(altered, hex[0], sizeof altered);
    for (size_t i = 0; i < 130; i++) {
        if (altered[i] >= 'a' && altered[i] <= 'f') {
            altered[i] = (char)(altered[i] - 'a' + 'A');
            break;
        }
    }
    snprintf(text, sizeof text, BASE ",\"key_set\":[\"%s\"]}", altered);
    CHECK(!parses(text));
    memcpy(altered, hex[0], sizeof altered);
    altered[128] = '\0'; /* a byte short */
    snprintf(text, sizeof text, BASE ",\"key_set\":[\"%s\"]}", altered);
    CHECK(!parses(text));
    snprintf(text, sizeof text, BASE ",\"key_set\":[\"%s00\"]}", hex[0]);
    CHECK(!parses(text));
    memcpy(altered, hex[0], sizeof altered);
    altered[1] = '2'; /* 02: a compressed point's prefix */
    snprintf(text, sizeof text, BASE ",\"key_set\":[\"%s\"]}", altered);
    CHECK(!parses(text));

    /* usb_install is the app's: a display reads none of it, whatever it holds. */
    static const char *usb[] = {"null", "1", "\"x\"", "[]", "{}", "{\"chip\":7,\"parts\":[{}]}"};
    for (size_t i = 0; i < LEN(usb); i++) {
        snprintf(text, sizeof text, BASE ",\"usb_install\":%s}", usb[i]);
        CHECK(parses(text));
    }

    /* 4096 bytes at most. */
    size_t n = (size_t)snprintf(text, sizeof text, BASE ",\"pad\":\"");
    memset(text + n, 'p', sizeof text - n);
    memcpy(text + 4094, "\"}", 3);
    CHECK(strlen(text) == 4096 && parses(text));
    memcpy(text + 4095, "\"}", 3);
    CHECK(strlen(text) == 4097 && !parses(text));
}

/* ---- Step 7 ---- */

static dh_fw_manifest plain_manifest(uint32_t counter, uint32_t image_size)
{
    dh_fw_manifest m;
    memset(&m, 0, sizeof m);
    strcpy(m.device_type, DEVICE_TYPE);
    m.hardware_revision_count = 2;
    strcpy(m.hardware_revisions[0], "other-board");
    strcpy(m.hardware_revisions[1], HARDWARE);
    strcpy(m.firmware_version, "1.0.0");
    m.security_counter = counter;
    m.image_size = image_size;
    return m;
}

static void test_manifest_check(void)
{
    dh_fw_keys keys = keys_of(&maker, &recovery, 3);
    uint64_t length = 137 + 100 + 1000;
    dh_fw_manifest m = plain_manifest(3, 1000);
    CHECK(dh_fw_manifest_check(&m, &keys, &DEVICE, 100, length) == DH_FW_REASON_NONE);

    /* device_type: byte for byte. */
    static const char *types[] = {"llm-usage-displaz", "LLM-usage-display", "llm-usage-displa",
                                  "llm-usage-display ", ""};
    for (size_t i = 0; i < LEN(types); i++) {
        m = plain_manifest(3, 1000);
        strcpy(m.device_type, types[i]);
        CHECK(dh_fw_manifest_check(&m, &keys, &DEVICE, 100, length) ==
              DH_FW_REASON_WRONG_DEVICE_TYPE);
    }
    /* hardware_revisions: listed exactly, never as a prefix or a longer string. */
    static const char *boards[] = {"test-board-v", "test-board-v10", "Test-board-v1", ""};
    for (size_t i = 0; i < LEN(boards); i++) {
        m = plain_manifest(3, 1000);
        strcpy(m.hardware_revisions[1], boards[i]);
        CHECK(dh_fw_manifest_check(&m, &keys, &DEVICE, 100, length) ==
              DH_FW_REASON_WRONG_HARDWARE);
    }
    m = plain_manifest(3, 1000);
    m.hardware_revision_count = 1; /* only "other-board" */
    CHECK(dh_fw_manifest_check(&m, &keys, &DEVICE, 100, length) == DH_FW_REASON_WRONG_HARDWARE);
    /* A display that doesn't know what it is matches nothing. */
    dh_fw_device unknown = {NULL, NULL, 1u << 20};
    m = plain_manifest(3, 1000);
    CHECK(dh_fw_manifest_check(&m, &keys, &unknown, 100, length) ==
          DH_FW_REASON_WRONG_DEVICE_TYPE);
    unknown.device_type = DEVICE_TYPE;
    CHECK(dh_fw_manifest_check(&m, &keys, &unknown, 100, length) == DH_FW_REASON_WRONG_HARDWARE);

    /* The security counter against the floor. */
    static const struct {
        uint32_t floor, counter;
        bool ok;
    } floors[] = {{3, 2, false},          {3, 3, true},  {3, 4, true}, {0, 0, true},
                  {4294967295u, 4294967294u, false}, {4294967295u, 4294967295u, true},
                  {1, 0, false}};
    for (size_t i = 0; i < LEN(floors); i++) {
        keys = keys_of(&maker, &recovery, floors[i].floor);
        m = plain_manifest(floors[i].counter, 1000);
        CHECK(dh_fw_manifest_check(&m, &keys, &DEVICE, 100, length) ==
              (floors[i].ok ? DH_FW_REASON_NONE : DH_FW_REASON_DOWNGRADE));
    }

    /* A key_set that leaves out a trusted key needs a counter above the floor
     * (drops_key_unraised in crates/firmware-package/src/display.rs has the same table). */
    const signer *all[] = {&maker, &recovery, &fresh};
    static const struct {
        int set[3]; /* indexes into all, -1 to end */
        uint32_t counter;
        bool ok;
    } sets[] = {{{0, 1, -1}, 3, true},  {{1, 0, 2}, 3, true},   {{0, -1, -1}, 3, false},
                {{0, 2, -1}, 3, false}, {{0, -1, -1}, 4, true}, {{2, -1, -1}, 4, true},
                {{2, -1, -1}, 3, false}, {{1, -1, -1}, 4294967295u, true}};
    keys = keys_of(&maker, &recovery, 3);
    for (size_t i = 0; i < LEN(sets); i++) {
        m = plain_manifest(sets[i].counter, 1000);
        m.has_key_set = true;
        for (size_t k = 0; k < 3 && sets[i].set[k] >= 0; k++) {
            m.key_set[m.key_count++] = all[sets[i].set[k]]->public;
        }
        CHECK(dh_fw_manifest_check(&m, &keys, &DEVICE, 100, length) ==
              (sets[i].ok ? DH_FW_REASON_NONE : DH_FW_REASON_DOWNGRADE));
    }
    /* A display that trusts nothing drops nothing. */
    dh_fw_keys none = keys_of(NULL, NULL, 0);
    m = plain_manifest(0, 1000);
    m.has_key_set = true;
    m.key_set[m.key_count++] = maker.public;
    CHECK(dh_fw_manifest_check(&m, &none, &DEVICE, 100, length) == DH_FW_REASON_NONE);

    /* The image against the spare partition, then the lengths against the Content-Length. */
    dh_fw_device small = {DEVICE_TYPE, HARDWARE, 1000};
    m = plain_manifest(3, 1000);
    CHECK(dh_fw_manifest_check(&m, &keys, &small, 100, length) == DH_FW_REASON_NONE);
    m = plain_manifest(3, 1001);
    CHECK(dh_fw_manifest_check(&m, &keys, &small, 100, length + 1) == DH_FW_REASON_TOO_LARGE);
    m = plain_manifest(3, 1000);
    CHECK(dh_fw_manifest_check(&m, &keys, &DEVICE, 100, length + 1) == DH_FW_REASON_BAD_FORMAT);
    CHECK(dh_fw_manifest_check(&m, &keys, &DEVICE, 100, length - 1) == DH_FW_REASON_BAD_FORMAT);
    CHECK(dh_fw_manifest_check(&m, &keys, &DEVICE, 101, length) == DH_FW_REASON_BAD_FORMAT);

    /* The order, when several things are wrong at once. */
    keys = keys_of(&maker, &recovery, 9);
    m = plain_manifest(3, 2000);
    strcpy(m.device_type, "x");
    m.hardware_revision_count = 1;
    CHECK(dh_fw_manifest_check(&m, &keys, &small, 100, 5) == DH_FW_REASON_WRONG_DEVICE_TYPE);
    strcpy(m.device_type, DEVICE_TYPE);
    CHECK(dh_fw_manifest_check(&m, &keys, &small, 100, 5) == DH_FW_REASON_WRONG_HARDWARE);
    m.hardware_revision_count = 2;
    CHECK(dh_fw_manifest_check(&m, &keys, &small, 100, 5) == DH_FW_REASON_DOWNGRADE);
    m.security_counter = 9;
    CHECK(dh_fw_manifest_check(&m, &keys, &small, 100, 5) == DH_FW_REASON_TOO_LARGE);
    m.image_size = 1000;
    CHECK(dh_fw_manifest_check(&m, &keys, &small, 100, 5) == DH_FW_REASON_BAD_FORMAT);
}

/* ---- Downloads ---- */

static void test_download(void)
{
    dh_fw_keys keys = keys_of(&maker, &recovery, 3);
    const size_t image_len = 5000;
    size_t mlen = manifest_for(manifest, sizeof manifest, image, image_len, "1.2.0", 4, "");
    size_t len = build(package, &maker, manifest, mlen, image, image_len);
    size_t image_at = DH_FW_HEADER_LEN + mlen;

    /* A good package, at every piece size: everything is read, and exactly the image written. */
    outcome o = download_all(&keys, &DEVICE, package, len, len);
    CHECK(o.reason == DH_FW_REASON_NONE && o.opened && o.read == len && o.written == image_len);
    CHECK(memcmp(spare, image, image_len) == 0);
    CHECK(strcmp(o.manifest.firmware_version, "1.2.0") == 0 && o.manifest.security_counter == 4 &&
          o.manifest.image_size == image_len);
    /* Signed by the second key. */
    len = build(package, &recovery, manifest, mlen, image, image_len);
    o = download_all(&keys, &DEVICE, package, len, len);
    CHECK(o.reason == DH_FW_REASON_NONE && o.written == image_len);

    /* Before any of the body: steps 1 and 2. */
    dh_fw_keys none = keys_of(NULL, NULL, 0);
    o = download(&none, &DEVICE, package, len, len, true, len, 97);
    CHECK(o.reason == DH_FW_REASON_NO_TRUSTED_KEYS && o.read == 0 && !o.opened);
    dh_fw_keys broken = keys;
    broken.key_count = 4; /* not something the stored form can hold: read as no keys */
    o = download(&broken, &DEVICE, package, len, len, true, len, 97);
    CHECK(o.reason == DH_FW_REASON_NO_TRUSTED_KEYS);
    CHECK(dh_fw_offer_check(&broken, &DEVICE, 100) == DH_FW_REASON_NO_TRUSTED_KEYS);
    o = download(&keys, &DEVICE, package, len, len, false, 0, 97);
    CHECK(o.reason == DH_FW_REASON_TOO_LARGE && o.read == 0);
    o = download(&keys, &DEVICE, package, len, len, false, len, 97);
    CHECK(o.reason == DH_FW_REASON_TOO_LARGE && o.read == 0);
    o = download(&keys, &DEVICE, package, len, len, true, len + 1, 97);
    CHECK(o.reason == DH_FW_REASON_TOO_LARGE && o.read == 0);
    o = download(&keys, &DEVICE, package, len, len + 1, true, len, 97);
    CHECK(o.reason == DH_FW_REASON_TOO_LARGE && o.read == 0);
    /* The limit is a header, the largest manifest and the spare partition. */
    uint64_t limit = 137u + 4096u + DEVICE.slot_bytes;
    CHECK(dh_fw_offer_check(&keys, &DEVICE, limit) == DH_FW_REASON_NONE);
    CHECK(dh_fw_offer_check(&keys, &DEVICE, limit + 1) == DH_FW_REASON_TOO_LARGE);
    CHECK(dh_fw_offer_check(&keys, &DEVICE, UINT64_MAX) == DH_FW_REASON_TOO_LARGE);
    CHECK(dh_fw_offer_check(&keys, &DEVICE, 0) == DH_FW_REASON_NONE);
    dh_fw_device huge = {DEVICE_TYPE, HARDWARE, UINT32_MAX};
    CHECK(dh_fw_offer_check(&keys, &huge, 137u + 4096u + (uint64_t)UINT32_MAX) ==
          DH_FW_REASON_NONE);
    CHECK(dh_fw_offer_check(&keys, &huge, 138u + 4096u + (uint64_t)UINT32_MAX) ==
          DH_FW_REASON_TOO_LARGE);
    o = download(&keys, &DEVICE, package, len, limit + 1, true, limit + 1, 97);
    CHECK(o.reason == DH_FW_REASON_TOO_LARGE && o.read == 0);

    /* Step 3: the header, refused once its 137 bytes are in and no later. */
    len = build(package, &maker, manifest, mlen, image, image_len);
    static const struct {
        size_t at;
        uint8_t value;
    } headers[] = {{0, 'd'}, {3, 'X'}, {4, 0}, {4, 2}, {5, 0}, {5, 2}, {7, 0x11}, {7, 0xff}};
    for (size_t i = 0; i < LEN(headers); i++) {
        uint8_t was = package[headers[i].at];
        package[headers[i].at] = headers[i].value;
        o = download_all(&keys, &DEVICE, package, len, len);
        CHECK(o.reason == DH_FW_REASON_BAD_FORMAT && o.read == 137 && !o.opened);
        package[headers[i].at] = was;
    }
    uint8_t lo = package[6], hi = package[7];
    for (unsigned bad_len = 0; bad_len < 2; bad_len++) {
        package[6] = (uint8_t)bad_len;
        package[7] = 0;
        o = download_all(&keys, &DEVICE, package, len, len);
        CHECK(o.reason == DH_FW_REASON_BAD_FORMAT && o.read == 137);
    }
    package[6] = 0x01;
    package[7] = 0x10; /* 4097 */
    o = download_all(&keys, &DEVICE, package, len, len);
    CHECK(o.reason == DH_FW_REASON_BAD_FORMAT && o.read == 137);
    package[6] = lo;
    package[7] = hi;
    /* A bad header from a stranger is still a bad header: the format comes first. */
    len = build(package, &stranger, manifest, mlen, image, image_len);
    package[4] = 2;
    o = download_all(&keys, &DEVICE, package, len, len);
    CHECK(o.reason == DH_FW_REASON_BAD_FORMAT && o.read == 137);

    /* Step 4: a key the display doesn't hold, however good the signature. */
    len = build(package, &stranger, manifest, mlen, image, image_len);
    o = download_all(&keys, &DEVICE, package, len, len);
    CHECK(o.reason == DH_FW_REASON_UNTRUSTED_KEY && o.read == 137 && !o.opened);
    /* One byte of a trusted key changed: all 65 bytes count. */
    len = build(package, &maker, manifest, mlen, image, image_len);
    package[8 + 64] ^= 1;
    o = download_all(&keys, &DEVICE, package, len, len);
    CHECK(o.reason == DH_FW_REASON_UNTRUSTED_KEY && o.read == 137);
    /* A display that once trusted the signer and no longer does. */
    dh_fw_keys only_recovery = keys_of(&recovery, NULL, 3);
    len = build(package, &maker, manifest, mlen, image, image_len);
    o = download_all(&only_recovery, &DEVICE, package, len, len);
    CHECK(o.reason == DH_FW_REASON_UNTRUSTED_KEY);
    /* The third of three keys. */
    dh_fw_keys three = keys_of(&recovery, &fresh, 3);
    three.keys[three.key_count++] = maker.public;
    o = download_all(&three, &DEVICE, package, len, len);
    CHECK(o.reason == DH_FW_REASON_NONE);

    /* A manifest that can't fit in the Content-Length is never read. */
    len = build(package, &maker, manifest, mlen, image, image_len);
    o = download_all(&keys, &DEVICE, package, len, image_at);
    CHECK(o.reason == DH_FW_REASON_BAD_FORMAT && o.read == 137);
    o = download_all(&keys, &DEVICE, package, len, 138);
    CHECK(o.reason == DH_FW_REASON_BAD_FORMAT && o.read == 137);
    o = download_all(&keys, &DEVICE, package, len, 137);
    CHECK(o.reason == DH_FW_REASON_BAD_FORMAT && o.read == 137);
    /* A Content-Length shorter than a header: nothing past it is taken, and it fails. */
    o = download_all(&keys, &DEVICE, package, len, 136);
    CHECK(o.reason == DH_FW_REASON_DOWNLOAD && o.read == 136 && !o.opened);
    o = download_all(&keys, &DEVICE, package, len, 1);
    CHECK(o.reason == DH_FW_REASON_DOWNLOAD && o.read == 1);
    o = download_all(&keys, &DEVICE, package, 0, 0);
    CHECK(o.reason == DH_FW_REASON_DOWNLOAD && o.read == 0);
    /* One byte of image stated: the manifest is read, and then the lengths don't add up. */
    o = download_all(&keys, &DEVICE, package, len, image_at + 1);
    CHECK(o.reason == DH_FW_REASON_BAD_FORMAT && o.read == image_at && !o.opened);

    /* Step 5: the signature, before the manifest is parsed. */
    for (size_t bit = 0; bit < DH_FW_SIGNATURE_LEN * 8; bit += 101) {
        len = build(package, &maker, manifest, mlen, image, image_len);
        package[DH_FW_SIGNED_HEADER_LEN + bit / 8] ^= (uint8_t)(1u << (bit % 8));
        o = download_all(&keys, &DEVICE, package, len, len);
        CHECK(o.reason == DH_FW_REASON_BAD_SIGNATURE && o.read == image_at && !o.opened);
    }
    for (size_t i = 0; i < mlen; i += 41) {
        len = build(package, &maker, manifest, mlen, image, image_len);
        package[DH_FW_HEADER_LEN + i] ^= 1;
        o = download_all(&keys, &DEVICE, package, len, len);
        CHECK(o.reason == DH_FW_REASON_BAD_SIGNATURE && o.read == image_at);
    }
    len = build(package, &maker, manifest, mlen, image, image_len);
    memset(package + DH_FW_SIGNED_HEADER_LEN, 0, DH_FW_SIGNATURE_LEN);
    o = download_all(&keys, &DEVICE, package, len, len);
    CHECK(o.reason == DH_FW_REASON_BAD_SIGNATURE);
    /* Signed without the fixed text. */
    len = build(package, &maker, manifest, mlen, image, image_len);
    sign_header(&maker, package, (const uint8_t *)manifest, mlen, false);
    o = download_all(&keys, &DEVICE, package, len, len);
    CHECK(o.reason == DH_FW_REASON_BAD_SIGNATURE);
    /* The header names one trusted key, and another trusted key signed it. */
    len = build(package, &maker, manifest, mlen, image, image_len);
    sign_header(&recovery, package, (const uint8_t *)manifest, mlen, true);
    o = download_all(&keys, &DEVICE, package, len, len);
    CHECK(o.reason == DH_FW_REASON_BAD_SIGNATURE);
    /* Another manifest's signature. */
    static uint8_t other[80000];
    size_t other_mlen = manifest_for(manifest, sizeof manifest, image, image_len, "1.2.1", 4, "");
    build(other, &maker, manifest, other_mlen, image, image_len);
    mlen = manifest_for(manifest, sizeof manifest, image, image_len, "1.2.0", 4, "");
    len = build(package, &maker, manifest, mlen, image, image_len);
    memcpy(package + DH_FW_SIGNED_HEADER_LEN, other + DH_FW_SIGNED_HEADER_LEN, DH_FW_SIGNATURE_LEN);
    o = download_all(&keys, &DEVICE, package, len, len);
    CHECK(o.reason == DH_FW_REASON_BAD_SIGNATURE);
    /* A manifest that isn't even JSON, unsigned: the signature is what is reported, because
     * nothing parses it first. */
    len = build(package, &maker, "not json at all", 15, image, image_len);
    package[DH_FW_SIGNED_HEADER_LEN + 5] ^= 1;
    o = download_all(&keys, &DEVICE, package, len, len);
    CHECK(o.reason == DH_FW_REASON_BAD_SIGNATURE && o.read == 137 + 15);
    /* manifest_len is signed: the same bytes with another length don't verify. */
    len = build(package, &maker, manifest, mlen, image, image_len);
    package[6] = (uint8_t)(package[6] - 1);
    o = download_all(&keys, &DEVICE, package, len, len);
    CHECK(o.reason == DH_FW_REASON_BAD_SIGNATURE && o.read == image_at - 1);

    /* Step 6: a signed manifest that breaks the rules. */
    static const char *broken_manifests[] = {
        "not json at all", "{}", "[]", "{\"device_type\":\"llm-usage-display\"}",
        "{\"device_type\":\"llm-usage-display\",\"device_type\":\"llm-usage-display\"}"};
    for (size_t i = 0; i < LEN(broken_manifests); i++) {
        size_t n = strlen(broken_manifests[i]);
        len = build(package, &maker, broken_manifests[i], n, image, image_len);
        o = download_all(&keys, &DEVICE, package, len, len);
        CHECK(o.reason == DH_FW_REASON_BAD_FORMAT && o.read == 137 + n && !o.opened);
    }
    mlen = manifest_for(manifest, sizeof manifest, image, image_len, "1.2", 4, "");
    len = build(package, &maker, manifest, mlen, image, image_len);
    CHECK(download_all(&keys, &DEVICE, package, len, len).reason == DH_FW_REASON_BAD_FORMAT);
    mlen = manifest_for(manifest, sizeof manifest, image, image_len, "1.2.0", 4, ",\"n\":1.5");
    len = build(package, &maker, manifest, mlen, image, image_len);
    CHECK(download_all(&keys, &DEVICE, package, len, len).reason == DH_FW_REASON_BAD_FORMAT);
    /* Unknown members are fine. */
    mlen = manifest_for(manifest, sizeof manifest, image, image_len, "1.2.0", 4,
                        ",\"n\":15,\"usb_install\":{\"chip\":\"esp32s3\"},\"later\":[null]");
    len = build(package, &maker, manifest, mlen, image, image_len);
    CHECK(download_all(&keys, &DEVICE, package, len, len).reason == DH_FW_REASON_NONE);

    /* Step 7, through a download. */
    dh_fw_device other_type = {"llm-usage-displaY", HARDWARE, 1u << 20};
    dh_fw_device other_board = {DEVICE_TYPE, "test-board-v2", 1u << 20};
    dh_fw_device small = {DEVICE_TYPE, HARDWARE, (uint32_t)image_len - 1};
    dh_fw_device exact = {DEVICE_TYPE, HARDWARE, (uint32_t)image_len};
    dh_fw_keys high = keys_of(&maker, &recovery, 5);
    mlen = manifest_for(manifest, sizeof manifest, image, image_len, "1.2.0", 4, "");
    len = build(package, &maker, manifest, mlen, image, image_len);
    image_at = DH_FW_HEADER_LEN + mlen;
    o = download_all(&keys, &other_type, package, len, len);
    CHECK(o.reason == DH_FW_REASON_WRONG_DEVICE_TYPE && o.read == image_at && !o.opened);
    o = download_all(&keys, &other_board, package, len, len);
    CHECK(o.reason == DH_FW_REASON_WRONG_HARDWARE && o.read == image_at && !o.opened);
    o = download_all(&high, &DEVICE, package, len, len);
    CHECK(o.reason == DH_FW_REASON_DOWNGRADE && o.read == image_at && !o.opened);
    o = download_all(&keys, &small, package, len, len);
    CHECK(o.reason == DH_FW_REASON_TOO_LARGE && o.read == image_at && !o.opened);
    o = download_all(&keys, &exact, package, len, len);
    CHECK(o.reason == DH_FW_REASON_NONE);
    /* Equal to the floor is fine; a key_set that drops a key at the floor isn't. */
    dh_fw_keys at_floor = keys_of(&maker, &recovery, 4);
    CHECK(download_all(&at_floor, &DEVICE, package, len, len).reason == DH_FW_REASON_NONE);
    char extra[400], hex[131];
    snprintf(extra, sizeof extra, ",\"key_set\":[\"%s\"]", key_hex(&maker.public, hex));
    mlen = manifest_for(manifest, sizeof manifest, image, image_len, "1.2.0", 4, extra);
    len = build(package, &maker, manifest, mlen, image, image_len);
    o = download_all(&at_floor, &DEVICE, package, len, len);
    CHECK(o.reason == DH_FW_REASON_DOWNGRADE && !o.opened);
    o = download_all(&keys, &DEVICE, package, len, len);
    CHECK(o.reason == DH_FW_REASON_NONE && o.manifest.has_key_set && o.manifest.key_count == 1);
    /* The Content-Length against the manifest's lengths, with a body that has the bytes. */
    mlen = manifest_for(manifest, sizeof manifest, image, image_len, "1.2.0", 4, "");
    len = build(package, &maker, manifest, mlen, image, image_len + 100);
    o = download_all(&keys, &DEVICE, package, len, len);
    CHECK(o.reason == DH_FW_REASON_BAD_FORMAT && o.read == image_at && !o.opened);
    o = download_all(&keys, &DEVICE, package, len, len - 101);
    CHECK(o.reason == DH_FW_REASON_BAD_FORMAT && o.read == image_at && !o.opened);

    /* Step 8: bytes after the image are never taken. */
    o = download_all(&keys, &DEVICE, package, len, len - 100);
    CHECK(o.reason == DH_FW_REASON_NONE && o.read == len - 100 && o.written == image_len);
    CHECK(memcmp(spare, image, image_len) == 0);
    /* One image byte changed, first, middle and last. */
    size_t spots[] = {0, image_len / 2, image_len - 1};
    for (size_t i = 0; i < LEN(spots); i++) {
        len = build(package, &maker, manifest, mlen, image, image_len);
        package[image_at + spots[i]] ^= 0x40;
        o = download_all(&keys, &DEVICE, package, len, len);
        CHECK(o.reason == DH_FW_REASON_IMAGE_HASH && o.opened && o.read == len);
    }
    /* The body ends early: in the header, the manifest, and the image. */
    len = build(package, &maker, manifest, mlen, image, image_len);
    size_t cuts[] = {0, 1, 136, 137, 138, image_at - 1, image_at, image_at + 1, len - 1};
    for (size_t i = 0; i < LEN(cuts); i++) {
        o = download_all(&keys, &DEVICE, package, cuts[i], len);
        CHECK(o.reason == DH_FW_REASON_DOWNLOAD && o.read == cuts[i]);
        CHECK(o.opened == (cuts[i] >= image_at));
        CHECK(o.written == (cuts[i] > image_at ? cuts[i] - image_at : 0));
    }

    /* What only the caller can see. */
    CHECK(dh_fw_download_begin(&dl, &keys, &DEVICE, len, true, len) == DH_FW_REASON_NONE);
    CHECK(!dh_fw_download_slot_open(&dl) && dh_fw_download_manifest(&dl) == NULL);
    CHECK(dh_fw_download_fail(&dl, DH_FW_REASON_FLASH) == DH_FW_REASON_FLASH);
    CHECK(dh_fw_download_fail(&dl, DH_FW_REASON_DOWNLOAD) == DH_FW_REASON_FLASH);
    CHECK(dh_fw_download_begin(&dl, &keys, &DEVICE, len, true, len) == DH_FW_REASON_NONE);
    CHECK(dh_fw_download_fail(&dl, DH_FW_REASON_BAD_FORMAT) == DH_FW_REASON_DOWNLOAD);
    /* esp_ota_end fails after the image verified. */
    CHECK(dh_fw_download_begin(&dl, &keys, &DEVICE, len, true, len) == DH_FW_REASON_NONE);
    dh_fw_push p = dh_fw_download_push(&dl, package, len);
    CHECK(p.step == DH_FW_STEP_OPEN_SLOT && p.used == image_at && p.write_len == 0);
    p = dh_fw_download_push(&dl, package + p.used, len - p.used);
    CHECK(p.step == DH_FW_STEP_FINISH && p.used == image_len && p.write_len == image_len &&
          p.write == package + image_at);
    CHECK(dh_fw_download_fail(&dl, DH_FW_REASON_FLASH) == DH_FW_REASON_FLASH);
    CHECK(dh_fw_download_push(&dl, package, len).step == DH_FW_STEP_ENDED);
    /* An empty piece changes nothing. */
    CHECK(dh_fw_download_begin(&dl, &keys, &DEVICE, len, true, len) == DH_FW_REASON_NONE);
    p = dh_fw_download_push(&dl, package, 0);
    CHECK(p.step == DH_FW_STEP_MORE && p.used == 0 && dh_fw_download_bytes_read(&dl) == 0);

    /* Time limits: nothing for 30 seconds, or 15 minutes in all. */
    CHECK(!dh_fw_download_overdue(100, 100, 100));
    CHECK(!dh_fw_download_overdue(100, 100, 129));
    CHECK(dh_fw_download_overdue(100, 100, 130));
    CHECK(!dh_fw_download_overdue(100, 990, 999));
    CHECK(dh_fw_download_overdue(100, 999, 1000));
    CHECK(!dh_fw_download_overdue(100, 100, 50)); /* a timer that went backwards waits */
}

/* A whole 8 MiB image, the largest there is, in pieces of an awkward size. */
static void test_largest_image(void)
{
    static uint8_t big[DH_FW_MAX_IMAGE_SIZE];
    static uint8_t big_package[DH_FW_HEADER_LEN + 512 + DH_FW_MAX_IMAGE_SIZE];
    uint32_t x = 99;
    for (size_t i = 0; i < sizeof big; i++) {
        x = x * 1664525u + 1013904223u;
        big[i] = (uint8_t)(x >> 24);
    }
    dh_fw_keys keys = keys_of(&maker, NULL, 0);
    dh_fw_device roomy = {DEVICE_TYPE, HARDWARE, DH_FW_MAX_IMAGE_SIZE};
    size_t mlen = manifest_for(manifest, 512, big, sizeof big, "3.0.0", 0, "");
    size_t len = build(big_package, &maker, manifest, mlen, big, sizeof big);
    outcome o = download(&keys, &roomy, big_package, len, len, true, len, 4099);
    CHECK(o.reason == DH_FW_REASON_NONE && o.read == len && o.written == sizeof big);
    CHECK(memcmp(spare, big, sizeof big) == 0);
    CHECK(sizeof dl < 16384); /* the download's state stays small whatever the image */
    big_package[len - 1] ^= 1;
    o = download(&keys, &roomy, big_package, len, len, true, len, 65536);
    CHECK(o.reason == DH_FW_REASON_IMAGE_HASH);
    /* One byte more than the largest image is a manifest that breaks the rules. */
    char hex[65];
    uint8_t digest[32];
    CHECK(dh_sha256_of(big, 16, digest));
    to_hex(digest, 32, hex);
    mlen = (size_t)snprintf(manifest, sizeof manifest,
                            "{\"device_type\":\"%s\",\"hardware_revisions\":[\"%s\"],"
                            "\"firmware_version\":\"3.0.0\",\"security_counter\":0,"
                            "\"image_size\":8388609,\"image_sha256\":\"%s\"}",
                            DEVICE_TYPE, HARDWARE, hex);
    len = build(big_package, &maker, manifest, mlen, big, 16);
    dh_fw_device roomier = {DEVICE_TYPE, HARDWARE, UINT32_MAX};
    uint64_t stated = (uint64_t)DH_FW_HEADER_LEN + mlen + 8388609u;
    o = download(&keys, &roomier, big_package, len, stated, true, stated, 97);
    CHECK(o.reason == DH_FW_REASON_BAD_FORMAT && !o.opened);
}

/* ---- The stored forms ---- */

static dh_fw_record sample_record(void)
{
    dh_fw_record r;
    dh_fw_record_init(&r);
    r.has_last_offer = true;
    strcpy(r.last_offer_id, "00112233445566778899aabbccddeeff");
    r.attempts_left = 2;
    r.credit_hours = 3;
    r.stage = DH_FW_STAGE_PENDING;
    r.pending_slot = 1;
    strcpy(r.pending_version, "1.2.0-rc.1");
    r.pending_security_counter = 0xdeadbeef;
    r.pending_has_key_set = true;
    r.pending_key_count = 2;
    r.pending_key_set[0] = maker.public;
    r.pending_key_set[1] = fresh.public;
    r.result_state = DH_FW_REPORT_REFUSED;
    r.result_reason = DH_FW_REASON_DOWNGRADE;
    strcpy(r.result_offer_id, "ffeeddccbbaa99887766554433221100");
    r.result_answered = true;
    return r;
}

static bool records_equal(const dh_fw_record *a, const dh_fw_record *b)
{
    uint8_t x[DH_FW_RECORD_BLOB_LEN], y[DH_FW_RECORD_BLOB_LEN];
    dh_fw_record_encode(a, x);
    dh_fw_record_encode(b, y);
    return memcmp(x, y, sizeof x) == 0;
}

static void test_stored_forms(void)
{
    uint8_t blob[DH_FW_KEYS_BLOB_LEN], again[DH_FW_KEYS_BLOB_LEN];
    dh_fw_keys keys = keys_of(&maker, &recovery, 0x01020304), back;
    dh_fw_keys_encode(&keys, blob);
    CHECK(blob[0] == 1 && blob[1] == 2 && blob[197] == 4 && blob[200] == 1);
    CHECK(dh_fw_keys_decode(&back, blob, sizeof blob));
    CHECK(back.key_count == 2 && back.security_floor == 0x01020304 &&
          memcmp(back.keys[0].bytes, maker.public.bytes, DH_FW_KEY_LEN) == 0 &&
          memcmp(back.keys[1].bytes, recovery.public.bytes, DH_FW_KEY_LEN) == 0);
    dh_fw_keys_encode(&back, again);
    CHECK(memcmp(blob, again, sizeof blob) == 0);
    /* Nothing stored yet, the wrong length, another format, too many keys. */
    CHECK(!dh_fw_keys_decode(&back, NULL, 0) && back.key_count == 0 && back.security_floor == 0);
    CHECK(!dh_fw_keys_decode(&back, blob, sizeof blob - 1));
    CHECK(!dh_fw_keys_decode(&back, blob, 0));
    blob[0] = 2;
    CHECK(!dh_fw_keys_decode(&back, blob, sizeof blob) && back.key_count == 0);
    blob[0] = 1;
    blob[1] = 4;
    CHECK(!dh_fw_keys_decode(&back, blob, sizeof blob) && back.key_count == 0);
    blob[1] = 3;
    CHECK(dh_fw_keys_decode(&back, blob, sizeof blob) && back.key_count == 3);
    /* A count the struct should never hold is stored as none. */
    keys.key_count = 200;
    dh_fw_keys_encode(&keys, blob);
    CHECK(dh_fw_keys_decode(&back, blob, sizeof blob) && back.key_count == 0);
    dh_fw_keys_init(&keys);
    dh_fw_keys_encode(&keys, blob);
    CHECK(dh_fw_keys_decode(&back, blob, sizeof blob) && back.key_count == 0);

    uint8_t rb[DH_FW_RECORD_BLOB_LEN], rb2[DH_FW_RECORD_BLOB_LEN];
    dh_fw_record rec, out;
    dh_fw_record_init(&rec);
    CHECK(rec.attempts_left == 5 && rec.credit_hours == 0 && !rec.has_last_offer &&
          rec.stage == DH_FW_STAGE_IDLE && rec.result_state == DH_FW_REPORT_NONE);
    dh_fw_record_encode(&rec, rb);
    CHECK(dh_fw_record_decode(&out, rb, sizeof rb) && records_equal(&rec, &out));
    rec = sample_record();
    dh_fw_record_encode(&rec, rb);
    CHECK(dh_fw_record_decode(&out, rb, sizeof rb));
    CHECK(out.has_last_offer && strcmp(out.last_offer_id, rec.last_offer_id) == 0 &&
          out.attempts_left == 2 && out.credit_hours == 3 && out.stage == DH_FW_STAGE_PENDING &&
          out.pending_slot == 1 && strcmp(out.pending_version, "1.2.0-rc.1") == 0 &&
          out.pending_security_counter == 0xdeadbeef && out.pending_has_key_set &&
          out.pending_key_count == 2 &&
          memcmp(out.pending_key_set[1].bytes, fresh.public.bytes, DH_FW_KEY_LEN) == 0 &&
          out.result_state == DH_FW_REPORT_REFUSED && out.result_reason == DH_FW_REASON_DOWNGRADE &&
          strcmp(out.result_offer_id, rec.result_offer_id) == 0 && out.result_answered);
    dh_fw_record_encode(&out, rb2);
    CHECK(memcmp(rb, rb2, sizeof rb) == 0);
    CHECK(!dh_fw_record_decode(&out, rb, sizeof rb - 1) && out.attempts_left == 5);
    CHECK(!dh_fw_record_decode(&out, NULL, 0) && out.attempts_left == 5);
    /* What isn't pending isn't kept. */
    rec.stage = DH_FW_STAGE_IDLE;
    dh_fw_record_encode(&rec, rb);
    CHECK(dh_fw_record_decode(&out, rb, sizeof rb) && out.pending_version[0] == '\0' &&
          !out.pending_has_key_set && out.pending_key_count == 0 &&
          out.pending_security_counter == 0);

    /* One byte wrong at a time. `at` is an offset in the stored record (see dh_update_state.c). */
    static const struct {
        size_t at;
        uint8_t value;
    } wrong[] = {{0, 0},    {0, 2},    {1, 0x08}, {1, 0x80}, {2, 'G'},  {2, 'A'},  {33, 0},
                 {36, 3},   {36, 255}, {38, 0},   {38, 49},  {39, 'v'}, {41, '.'}, {91, 0},
                 {91, 4},   {287, 1},  {287, 6},  {287, 0},  {288, 0},  {288, 9},  {288, 12},
                 {289, 'x'}, {320, 0}};
    rec = sample_record();
    for (size_t i = 0; i < LEN(wrong); i++) {
        dh_fw_record_encode(&rec, rb);
        rb[wrong[i].at] = wrong[i].value;
        CHECK(!dh_fw_record_decode(&out, rb, sizeof rb));
        CHECK(out.attempts_left == 5 && out.stage == DH_FW_STAGE_IDLE && !out.has_last_offer);
    }
    /* A stage with no offer to belong to. */
    dh_fw_record_encode(&rec, rb);
    rb[1] = (uint8_t)(rb[1] & ~1u);
    CHECK(!dh_fw_record_decode(&out, rb, sizeof rb));
    /* A key count without a key_set. */
    dh_fw_record_encode(&rec, rb);
    rb[1] = (uint8_t)(rb[1] & ~2u);
    CHECK(!dh_fw_record_decode(&out, rb, sizeof rb));
    /* No result, but a reason or an answer. */
    rec.result_state = DH_FW_REPORT_NONE;
    dh_fw_record_encode(&rec, rb);
    CHECK(dh_fw_record_decode(&out, rb, sizeof rb) && out.result_state == DH_FW_REPORT_NONE);
    rb[288] = 1;
    CHECK(!dh_fw_record_decode(&out, rb, sizeof rb));
    rb[288] = 0;
    rb[1] = (uint8_t)(rb[1] | 4u);
    CHECK(!dh_fw_record_decode(&out, rb, sizeof rb));
    /* Each final state with the reasons that go with it. */
    for (int state = 0; state <= 6; state++) {
        for (int reason = 0; reason <= 12; reason++) {
            rec = sample_record();
            dh_fw_record_encode(&rec, rb);
            rb[287] = (uint8_t)state;
            rb[288] = (uint8_t)reason;
            rb[1] = (uint8_t)(rb[1] & ~4u);
            bool ok = (state == DH_FW_REPORT_NONE && reason == 0) ||
                      (state == DH_FW_REPORT_INSTALLED && reason == 0) ||
                      (state == DH_FW_REPORT_ROLLED_BACK && reason == 0) ||
                      (state == DH_FW_REPORT_REFUSED && reason >= 1 && reason <= 8) ||
                      (state == DH_FW_REPORT_FAILED && reason >= 9 && reason <= 11);
            CHECK(dh_fw_record_decode(&out, rb, sizeof rb) == ok);
        }
    }
    /* A budget out of range reads as the nearest in range. */
    static const struct {
        uint8_t attempts, credit, want_attempts, want_credit;
    } budgets[] = {{9, 9, 5, 0}, {5, 3, 5, 0}, {2, 9, 2, 3}, {0, 4, 0, 3}, {255, 0, 5, 0},
                   {4, 3, 4, 3}, {0, 0, 0, 0}};
    for (size_t i = 0; i < LEN(budgets); i++) {
        dh_fw_record_init(&rec);
        dh_fw_record_encode(&rec, rb);
        rb[34] = budgets[i].attempts;
        rb[35] = budgets[i].credit;
        CHECK(dh_fw_record_decode(&out, rb, sizeof rb) &&
              out.attempts_left == budgets[i].want_attempts &&
              out.credit_hours == budgets[i].want_credit);
    }

    /* Whatever is in flash, reading it is safe, and what reads back is always in range and
     * stores to the same bytes again. */
    uint32_t x = 7;
    unsigned accepted = 0;
    for (int round = 0; round < 20000; round++) {
        rec = sample_record();
        dh_fw_record_encode(&rec, rb);
        /* A few bytes changed in a good record: most still read. */
        for (int k = 0; k < 1 + round % 4; k++) {
            x = x * 1664525u + 1013904223u;
            size_t at = (x >> 8) % sizeof rb;
            x = x * 1664525u + 1013904223u;
            rb[at] = round % 2 ? (uint8_t)(x >> 24) : (uint8_t)((x >> 24) % 6);
        }
        if (dh_fw_record_decode(&out, rb, sizeof rb)) {
            accepted++;
            CHECK(out.attempts_left <= 5 && out.credit_hours <= 3 && out.stage <= 2 &&
                  out.pending_key_count <= 3 && out.result_state <= DH_FW_REPORT_ROLLED_BACK &&
                  out.result_state != DH_FW_REPORT_DOWNLOADING &&
                  out.last_offer_id[32] == '\0' && out.result_offer_id[32] == '\0' &&
                  out.pending_version[48] == '\0');
            dh_fw_record back2;
            dh_fw_record_encode(&out, rb2);
            CHECK(dh_fw_record_decode(&back2, rb2, sizeof rb2) && records_equal(&out, &back2));
            /* And a start with it is safe. */
            dh_fw_session s;
            dh_fw_keys k2 = keys_of(&maker, NULL, 1);
            unsigned save = 0;
            (void)dh_fw_start(&s, &out, &k2, (uint8_t)(round & 1), (round & 2) != 0, &save);
            CHECK(k2.key_count >= 1 && k2.key_count <= 3 && k2.security_floor >= 1);
        }
    }
    CHECK(accepted > 1000);
}

/* ---- The budget and offers ---- */

#define OFFER_A "0000000000000000000000000000000a"
#define OFFER_B "0000000000000000000000000000000b"
#define OFFER_C "0000000000000000000000000000000c"

static void test_budget(void)
{
    dh_fw_session s;
    dh_fw_record rec;
    memset(&s, 0, sizeof s);
    dh_fw_record_init(&rec);
    /* Full: nothing is counted or saved, however long. */
    CHECK(dh_fw_tick(&s, &rec, 0) == 0);
    CHECK(dh_fw_tick(&s, &rec, 100 * 3600) == 0 && rec.attempts_left == 5);
    CHECK(dh_fw_seconds_to_next_attempt(&s, &rec, 100 * 3600) == 0);
    /* An attempt is spent at 100 h: the next hour is counted from then. */
    rec.attempts_left = 4;
    CHECK(dh_fw_tick(&s, &rec, 100 * 3600 + 3599) == 0 && rec.credit_hours == 0);
    CHECK(dh_fw_seconds_to_next_attempt(&s, &rec, 100 * 3600 + 3599) == 3 * 3600 + 1);
    CHECK(dh_fw_tick(&s, &rec, 101 * 3600) == DH_FW_SAVE_RECORD && rec.credit_hours == 1);
    CHECK(dh_fw_tick(&s, &rec, 101 * 3600) == 0);
    CHECK(dh_fw_tick(&s, &rec, 102 * 3600 - 1) == 0);
    CHECK(dh_fw_tick(&s, &rec, 102 * 3600) == DH_FW_SAVE_RECORD && rec.credit_hours == 2);
    CHECK(dh_fw_tick(&s, &rec, 103 * 3600) == DH_FW_SAVE_RECORD && rec.credit_hours == 3 &&
          rec.attempts_left == 4);
    CHECK(dh_fw_seconds_to_next_attempt(&s, &rec, 103 * 3600 + 600) == 3000);
    CHECK(dh_fw_tick(&s, &rec, 104 * 3600 - 1) == 0);
    CHECK(dh_fw_tick(&s, &rec, 104 * 3600) == DH_FW_SAVE_RECORD && rec.credit_hours == 0 &&
          rec.attempts_left == 5);
    /* Full again: time stops counting. */
    CHECK(dh_fw_tick(&s, &rec, 200 * 3600) == 0);
    rec.attempts_left = 4;
    CHECK(dh_fw_tick(&s, &rec, 200 * 3600 + 3599) == 0);

    /* From empty, called rarely: every hour that passed counts, in one save. */
    memset(&s, 0, sizeof s);
    dh_fw_record_init(&rec);
    rec.attempts_left = 0;
    CHECK(dh_fw_tick(&s, &rec, 3599) == 0);
    CHECK(dh_fw_seconds_to_next_attempt(&s, &rec, 3599) == 4 * 3600 - 3599);
    CHECK(dh_fw_tick(&s, &rec, 9 * 3600 + 5) == DH_FW_SAVE_RECORD && rec.attempts_left == 2 &&
          rec.credit_hours == 1);
    /* It stops at full and drops the rest. */
    CHECK(dh_fw_tick(&s, &rec, 1000 * 3600) == DH_FW_SAVE_RECORD && rec.attempts_left == 5 &&
          rec.credit_hours == 0);
    CHECK(dh_fw_tick(&s, &rec, 2000 * 3600) == 0);
    /* Three credit hours and four attempts: one more hour fills it. */
    memset(&s, 0, sizeof s);
    rec.attempts_left = 4;
    rec.credit_hours = 3;
    CHECK(dh_fw_tick(&s, &rec, 10 * 3600) == DH_FW_SAVE_RECORD && rec.attempts_left == 5 &&
          rec.credit_hours == 0);
    /* A record out of range is brought into range, and a timer that goes backwards waits. */
    memset(&s, 0, sizeof s);
    rec.attempts_left = 77;
    rec.credit_hours = 77;
    CHECK(dh_fw_tick(&s, &rec, 5) == 0 && rec.attempts_left == 5 && rec.credit_hours == 0);
    rec.attempts_left = 1;
    rec.credit_hours = 77;
    CHECK(dh_fw_tick(&s, &rec, 5) == 0 && rec.credit_hours == 3);
    s.counted_from_s = 5000;
    CHECK(dh_fw_tick(&s, &rec, 100) == 0 && rec.credit_hours == 3);
    CHECK(dh_fw_seconds_to_next_attempt(&s, &rec, 100) == 3600);
    CHECK(dh_fw_tick(&s, &rec, UINT64_MAX) == DH_FW_SAVE_RECORD && rec.attempts_left == 5);
}

static dh_fw_offer_decision offer(dh_fw_session *s, dh_fw_record *rec, const dh_fw_keys *keys,
                                  const char *id, uint64_t size, uint64_t uptime)
{
    return dh_fw_on_offer(s, rec, keys, &DEVICE, id, size, uptime);
}

static void test_offers(void)
{
    dh_fw_session s;
    dh_fw_record rec;
    dh_fw_keys keys = keys_of(&maker, NULL, 0), none = keys_of(NULL, NULL, 0);
    unsigned save;
    dh_fw_report rep;
    dh_fw_record_init(&rec);
    CHECK(dh_fw_start(&s, &rec, &keys, 0, false, &save) == DH_FW_START_NORMAL && save == 0);

    /* An offer_id that isn't 32 lower-case hex digits is no offer. */
    static const char *ids[] = {"", "0", "0000000000000000000000000000000",
                                "000000000000000000000000000000000",
                                "0000000000000000000000000000000A",
                                "0000000000000000000000000000000g",
                                "00000000-0000-0000-0000-00000000"};
    for (size_t i = 0; i < LEN(ids); i++) {
        dh_fw_offer_decision d = offer(&s, &rec, &keys, ids[i], 1000, 0);
        CHECK(d.action == DH_FW_OFFER_IGNORE && d.save == 0);
    }
    CHECK(offer(&s, &rec, &keys, NULL, 1000, 0).action == DH_FW_OFFER_IGNORE);
    CHECK(rec.attempts_left == 5 && !rec.has_last_offer);

    /* Five attempts, one for each new offer id; the one last acted on is ignored. */
    const char *five[] = {OFFER_A, OFFER_B, OFFER_A, OFFER_C, OFFER_A};
    for (int i = 0; i < 5; i++) {
        dh_fw_offer_decision d = offer(&s, &rec, &keys, five[i], 1000, 0);
        CHECK(d.action == DH_FW_OFFER_ATTEMPT && d.save == DH_FW_SAVE_RECORD);
        CHECK(rec.attempts_left == 4 - i && rec.stage == DH_FW_STAGE_ATTEMPT &&
              strcmp(rec.last_offer_id, five[i]) == 0);
        /* While it runs, nothing new is acted on, not even another offer. */
        CHECK(offer(&s, &rec, &keys, OFFER_C, 1000, 0).action == DH_FW_OFFER_BUSY);
        CHECK(offer(&s, &rec, &keys, five[i], 1000, 0).action == DH_FW_OFFER_BUSY);
        CHECK(rec.attempts_left == 4 - i);
        CHECK(dh_fw_attempt_ended(&rec, DH_FW_REASON_DOWNLOAD) == DH_FW_SAVE_RECORD);
        /* Its report must be answered before anything new. */
        CHECK(offer(&s, &rec, &keys, OFFER_B, 1000, 0).action == DH_FW_OFFER_BUSY);
        CHECK(dh_fw_report_next(&s, &rec, &rep) && rep.state == DH_FW_REPORT_FAILED &&
              rep.reason == DH_FW_REASON_DOWNLOAD && strcmp(rep.offer_id, five[i]) == 0);
        CHECK(dh_fw_report_answered(&s, &rec, &rep) == DH_FW_SAVE_RECORD);
        CHECK(!dh_fw_report_next(&s, &rec, &rep));
        CHECK(dh_fw_report_answered(&s, &rec, &rep) == 0);
        dh_fw_offer_decision same = offer(&s, &rec, &keys, five[i], 1000, 0);
        CHECK(same.action == DH_FW_OFFER_IGNORE && same.save == 0 && rec.attempts_left == 4 - i);
    }
    /* None left: wait, with nothing spent, saved or reported. */
    dh_fw_offer_decision d = offer(&s, &rec, &keys, OFFER_B, 1000, 100);
    CHECK(d.action == DH_FW_OFFER_WAIT && d.save == 0 && d.wait_seconds == 4 * 3600 - 100);
    CHECK(rec.attempts_left == 0 && strcmp(rec.last_offer_id, OFFER_A) == 0 &&
          rec.stage == DH_FW_STAGE_IDLE && !dh_fw_report_next(&s, &rec, &rep));
    /* Three hours 59 minutes 59 seconds: still waiting, with three credit hours saved. */
    d = offer(&s, &rec, &keys, OFFER_B, 1000, 4 * 3600 - 1);
    CHECK(d.action == DH_FW_OFFER_WAIT && d.save == DH_FW_SAVE_RECORD && d.wait_seconds == 1 &&
          rec.credit_hours == 3);
    /* Four hours: one attempt, spent at once. One save covers both. */
    d = offer(&s, &rec, &keys, OFFER_B, 1000, 4 * 3600);
    CHECK(d.action == DH_FW_OFFER_ATTEMPT && d.save == DH_FW_SAVE_RECORD &&
          rec.attempts_left == 0 && rec.credit_hours == 0);
    CHECK(dh_fw_attempt_ended(&rec, DH_FW_REASON_IMAGE_HASH) == DH_FW_SAVE_RECORD);
    CHECK(dh_fw_report_next(&s, &rec, &rep) && rep.state == DH_FW_REPORT_FAILED &&
          rep.reason == DH_FW_REASON_IMAGE_HASH);
    CHECK(dh_fw_report_answered(&s, &rec, &rep) == DH_FW_SAVE_RECORD);
    /* Credit hours carry on through an attempt. */
    CHECK(dh_fw_tick(&s, &rec, 5 * 3600) == DH_FW_SAVE_RECORD && rec.credit_hours == 1);

    /* Refused from the offer alone: nothing spent or saved, remembered until the next start,
     * and decided before the budget is looked at. */
    CHECK(rec.attempts_left == 0);
    d = offer(&s, &rec, &none, OFFER_C, 1000, 5 * 3600);
    CHECK(d.action == DH_FW_OFFER_REFUSE && d.reason == DH_FW_REASON_NO_TRUSTED_KEYS &&
          d.save == 0);
    CHECK(dh_fw_report_next(&s, &rec, &rep) && rep.state == DH_FW_REPORT_REFUSED &&
          rep.reason == DH_FW_REASON_NO_TRUSTED_KEYS && strcmp(rep.offer_id, OFFER_C) == 0);
    /* Until its report is answered, nothing new; afterwards the same offer is ignored. */
    CHECK(offer(&s, &rec, &none, OFFER_C, 1000, 5 * 3600).action == DH_FW_OFFER_BUSY);
    CHECK(dh_fw_report_answered(&s, &rec, &rep) == 0);
    CHECK(!dh_fw_report_next(&s, &rec, &rep));
    CHECK(offer(&s, &rec, &none, OFFER_C, 1000, 5 * 3600).action == DH_FW_OFFER_IGNORE);
    CHECK(offer(&s, &rec, &keys, OFFER_C, 1000, 5 * 3600).action == DH_FW_OFFER_IGNORE);
    uint64_t too_large = 137u + 4096u + DEVICE.slot_bytes + 1u;
    d = offer(&s, &rec, &keys, OFFER_A, too_large, 5 * 3600);
    CHECK(d.action == DH_FW_OFFER_REFUSE && d.reason == DH_FW_REASON_TOO_LARGE);
    CHECK(dh_fw_report_next(&s, &rec, &rep) && rep.reason == DH_FW_REASON_TOO_LARGE);
    (void)dh_fw_report_answered(&s, &rec, &rep);
    CHECK(strcmp(rec.last_offer_id, OFFER_B) == 0 && rec.attempts_left == 0);
    /* The next start forgets it. */
    dh_fw_record before = rec;
    CHECK(dh_fw_start(&s, &rec, &keys, 0, false, &save) == DH_FW_START_NORMAL && save == 0);
    CHECK(records_equal(&before, &rec));
    d = offer(&s, &rec, &keys, OFFER_A, too_large, 0);
    CHECK(d.action == DH_FW_OFFER_REFUSE);
    /* An unanswered refusal is forgotten at a start too. */
    CHECK(dh_fw_start(&s, &rec, &keys, 0, false, &save) == DH_FW_START_NORMAL);
    CHECK(!dh_fw_report_next(&s, &rec, &rep));

    /* Answering a report that isn't the one waiting changes nothing. */
    dh_fw_record_init(&rec);
    CHECK(dh_fw_start(&s, &rec, &keys, 0, false, &save) == DH_FW_START_NORMAL);
    CHECK(offer(&s, &rec, &keys, OFFER_A, 1000, 0).action == DH_FW_OFFER_ATTEMPT);
    CHECK(dh_fw_attempt_ended(&rec, DH_FW_REASON_BAD_SIGNATURE) == DH_FW_SAVE_RECORD);
    CHECK(dh_fw_report_next(&s, &rec, &rep) && rep.state == DH_FW_REPORT_REFUSED &&
          rep.reason == DH_FW_REASON_BAD_SIGNATURE);
    dh_fw_report wrong = rep;
    memcpy(wrong.offer_id, OFFER_B, 33);
    CHECK(dh_fw_report_answered(&s, &rec, &wrong) == 0);
    wrong = rep;
    wrong.state = DH_FW_REPORT_FAILED;
    CHECK(dh_fw_report_answered(&s, &rec, &wrong) == 0);
    wrong = rep;
    wrong.reason = DH_FW_REASON_BAD_FORMAT;
    CHECK(dh_fw_report_answered(&s, &rec, &wrong) == 0);
    CHECK(dh_fw_report_next(&s, &rec, &rep));
    CHECK(dh_fw_report_answered(&s, &rec, &rep) == DH_FW_SAVE_RECORD);
    /* A result is kept after it is answered, and no longer reported. */
    CHECK(rec.result_state == DH_FW_REPORT_REFUSED && rec.result_answered);

    /* Ending an attempt that isn't running does nothing; a reason that isn't one is `download`. */
    CHECK(dh_fw_attempt_ended(&rec, DH_FW_REASON_FLASH) == 0);
    dh_fw_manifest m = plain_manifest(1, 10);
    CHECK(dh_fw_attempt_installed(&rec, &m, 1) == 0 && rec.stage == DH_FW_STAGE_IDLE);
    CHECK(offer(&s, &rec, &keys, OFFER_B, 1000, 0).action == DH_FW_OFFER_ATTEMPT);
    CHECK(dh_fw_attempt_installed(&rec, NULL, 1) == 0 && rec.stage == DH_FW_STAGE_ATTEMPT);
    CHECK(dh_fw_attempt_ended(&rec, DH_FW_REASON_NONE) == DH_FW_SAVE_RECORD &&
          rec.result_reason == DH_FW_REASON_DOWNLOAD && rec.result_state == DH_FW_REPORT_FAILED);
    /* The result of an attempt never changes the budget. */
    CHECK(rec.attempts_left == 3);
}

/* ---- The trial boot ---- */

static void test_trial(void)
{
    dh_fw_session s;
    memset(&s, 0, sizeof s);
    CHECK(dh_fw_trial_tick(&s, 0) == DH_FW_TRIAL_NONE);
    CHECK(dh_fw_trial_tick(&s, 1000) == DH_FW_TRIAL_NONE);
    CHECK(dh_fw_trial_check_in(&s, 10, DH_FW_CHECK_IN_ANSWERED) == DH_FW_TRIAL_NONE);
    s.trial = true;
    CHECK(dh_fw_trial_tick(&s, 0) == DH_FW_TRIAL_WAIT);
    CHECK(dh_fw_trial_tick(&s, 299) == DH_FW_TRIAL_WAIT);
    CHECK(dh_fw_trial_tick(&s, 300) == DH_FW_TRIAL_ROLL_BACK);
    CHECK(dh_fw_trial_tick(&s, UINT64_MAX) == DH_FW_TRIAL_ROLL_BACK);
    CHECK(dh_fw_trial_check_in(&s, 0, DH_FW_CHECK_IN_FAILED) == DH_FW_TRIAL_WAIT);
    CHECK(dh_fw_trial_check_in(&s, 299, DH_FW_CHECK_IN_FAILED) == DH_FW_TRIAL_WAIT);
    CHECK(dh_fw_trial_check_in(&s, 0, DH_FW_CHECK_IN_ANSWERED) == DH_FW_TRIAL_MARK_GOOD);
    CHECK(dh_fw_trial_check_in(&s, 299, DH_FW_CHECK_IN_ANSWERED) == DH_FW_TRIAL_MARK_GOOD);
    CHECK(dh_fw_trial_check_in(&s, 299, DH_FW_CHECK_IN_SET_UP_AGAIN) == DH_FW_TRIAL_MARK_GOOD);
    /* An answer at 300 seconds or later is too late, whatever it is. */
    CHECK(dh_fw_trial_check_in(&s, 300, DH_FW_CHECK_IN_ANSWERED) == DH_FW_TRIAL_ROLL_BACK);
    CHECK(dh_fw_trial_check_in(&s, 300, DH_FW_CHECK_IN_SET_UP_AGAIN) == DH_FW_TRIAL_ROLL_BACK);
    CHECK(dh_fw_trial_check_in(&s, 301, DH_FW_CHECK_IN_FAILED) == DH_FW_TRIAL_ROLL_BACK);

    /* What counts as reaching the hub. */
    dh_response r;
    memset(&r, 0, sizeof r);
    r.outcome = DH_OUTCOME_SHOW;
    CHECK(dh_fw_check_in_of(&r) == DH_FW_CHECK_IN_ANSWERED);
    static const dh_outcome not_enough[] = {DH_OUTCOME_UNREADABLE, DH_OUTCOME_INCOMPATIBLE,
                                            DH_OUTCOME_RETRY, DH_OUTCOME_FAULT,
                                            DH_OUTCOME_REPROVISION};
    for (size_t i = 0; i < LEN(not_enough); i++) {
        r.outcome = not_enough[i];
        r.error_code = DH_ERROR_NONE;
        CHECK(dh_fw_check_in_of(&r) == DH_FW_CHECK_IN_FAILED);
    }
    for (int code = DH_ERROR_NONE; code <= DH_ERROR_UNRECOGNISED; code++) {
        r.outcome = DH_OUTCOME_REPROVISION;
        r.error_code = (dh_error_code)code;
        bool set_up_again = code == DH_ERROR_TOKEN_REVOKED || code == DH_ERROR_UNKNOWN_DEVICE;
        CHECK(dh_fw_check_in_of(&r) ==
              (set_up_again ? DH_FW_CHECK_IN_SET_UP_AGAIN : DH_FW_CHECK_IN_FAILED));
    }

    /* A trial boot the record knows nothing about (it was lost): marked good, nothing applied. */
    dh_fw_record rec;
    dh_fw_keys keys = keys_of(&maker, &recovery, 3);
    unsigned save = 99;
    dh_fw_report rep;
    dh_fw_record_init(&rec);
    CHECK(dh_fw_start(&s, &rec, &keys, 1, true, &save) == DH_FW_START_TRIAL && save == 0);
    CHECK(!dh_fw_report_next(&s, &rec, &rep));
    CHECK(dh_fw_marked_good(&s, &rec, &keys) == 0 && !s.trial);
    CHECK(keys.key_count == 2 && keys.security_floor == 3 && !dh_fw_report_next(&s, &rec, &rep));
    /* Not on trial: nothing to mark. */
    CHECK(dh_fw_marked_good(&s, &rec, &keys) == 0);
    /* A record about some other partition's image: cleared, not applied. */
    rec = sample_record();
    rec.result_state = DH_FW_REPORT_NONE;
    rec.result_reason = DH_FW_REASON_NONE;
    rec.result_answered = false;
    rec.pending_slot = 0;
    CHECK(dh_fw_start(&s, &rec, &keys, 1, true, &save) == DH_FW_START_TRIAL);
    CHECK(dh_fw_marked_good(&s, &rec, &keys) == DH_FW_SAVE_RECORD);
    CHECK(rec.stage == DH_FW_STAGE_IDLE && keys.key_count == 2 && keys.security_floor == 3 &&
          memcmp(keys.keys[1].bytes, recovery.public.bytes, DH_FW_KEY_LEN) == 0);

    /* Marked good: the floor only ever rises, and the keys change only with a key_set. */
    static const struct {
        uint32_t floor, counter, want;
    } floors[] = {{3, 4, 4}, {3, 3, 3}, {5, 4, 5}, {0, 4294967295u, 4294967295u}};
    for (size_t i = 0; i < LEN(floors); i++) {
        keys = keys_of(&maker, &recovery, floors[i].floor);
        rec = sample_record();
        rec.pending_has_key_set = false;
        rec.pending_key_count = 0;
        rec.pending_security_counter = floors[i].counter;
        CHECK(dh_fw_start(&s, &rec, &keys, 1, true, &save) == DH_FW_START_TRIAL);
        CHECK(dh_fw_marked_good(&s, &rec, &keys) == (DH_FW_SAVE_KEYS | DH_FW_SAVE_RECORD));
        CHECK(keys.security_floor == floors[i].want && keys.key_count == 2 &&
              memcmp(keys.keys[0].bytes, maker.public.bytes, DH_FW_KEY_LEN) == 0 &&
              memcmp(keys.keys[1].bytes, recovery.public.bytes, DH_FW_KEY_LEN) == 0);
        CHECK(rec.stage == DH_FW_STAGE_IDLE && rec.pending_version[0] == '\0' &&
              rec.attempts_left == 2);
        CHECK(dh_fw_report_next(&s, &rec, &rep) && rep.state == DH_FW_REPORT_INSTALLED &&
              rep.reason == DH_FW_REASON_NONE &&
              strcmp(rep.offer_id, "00112233445566778899aabbccddeeff") == 0);
    }
    /* A key_set replaces the whole set, here three keys with one. */
    keys = keys_of(&maker, &recovery, 3);
    keys.keys[keys.key_count++] = stranger.public;
    rec = sample_record();
    rec.pending_key_count = 1;
    rec.pending_key_set[0] = fresh.public;
    CHECK(dh_fw_start(&s, &rec, &keys, 1, true, &save) == DH_FW_START_TRIAL);
    CHECK(dh_fw_marked_good(&s, &rec, &keys) == (DH_FW_SAVE_KEYS | DH_FW_SAVE_RECORD));
    CHECK(keys.key_count == 1 && memcmp(keys.keys[0].bytes, fresh.public.bytes, 65) == 0);
    static const uint8_t zeros[DH_FW_KEY_LEN];
    CHECK(memcmp(keys.keys[1].bytes, zeros, 65) == 0 && memcmp(keys.keys[2].bytes, zeros, 65) == 0);
}

/* ---- A power cut at every save point ---- */

/* Flash, as the firmware uses it: only ever the stored forms, written only when told to. */
static struct {
    uint8_t keys[DH_FW_KEYS_BLOB_LEN];
    uint8_t record[DH_FW_RECORD_BLOB_LEN];
} nvs;

static void nvs_save(const dh_fw_keys *keys, const dh_fw_record *rec, unsigned what)
{
    if (what & DH_FW_SAVE_KEYS) {
        dh_fw_keys_encode(keys, nvs.keys);
    }
    if (what & DH_FW_SAVE_RECORD) {
        dh_fw_record_encode(rec, nvs.record);
    }
}

static dh_fw_start_result power_on(dh_fw_session *s, dh_fw_record *rec, dh_fw_keys *keys,
                                   uint8_t slot, bool on_trial, unsigned *save)
{
    CHECK(dh_fw_keys_decode(keys, nvs.keys, sizeof nvs.keys));
    CHECK(dh_fw_record_decode(rec, nvs.record, sizeof nvs.record));
    return dh_fw_start(s, rec, keys, slot, on_trial, save);
}

static void test_power_cuts(void)
{
    dh_fw_session s;
    dh_fw_record rec;
    dh_fw_keys keys, old_keys = keys_of(&maker, &recovery, 3);
    dh_fw_report rep;
    unsigned save;
    dh_fw_manifest m = plain_manifest(7, 1000);
    strcpy(m.firmware_version, "2.0.0");
    m.has_key_set = true;
    m.key_count = 2;
    m.key_set[0] = recovery.public;
    m.key_set[1] = fresh.public;

    /* A fresh display. */
    keys = old_keys;
    dh_fw_record_init(&rec);
    nvs_save(&keys, &rec, DH_FW_SAVE_KEYS | DH_FW_SAVE_RECORD);

    /* 1. An attempt is decided, and the power goes before the record is saved: nothing was
     * requested, so nothing is spent and the offer is acted on afresh. */
    CHECK(power_on(&s, &rec, &keys, 0, false, &save) == DH_FW_START_NORMAL && save == 0);
    dh_fw_offer_decision d = offer(&s, &rec, &keys, OFFER_A, 5000, 10);
    CHECK(d.action == DH_FW_OFFER_ATTEMPT && d.save == DH_FW_SAVE_RECORD);
    CHECK(power_on(&s, &rec, &keys, 0, false, &save) == DH_FW_START_NORMAL && save == 0);
    CHECK(rec.attempts_left == 5 && !rec.has_last_offer);

    /* 2. Saved, then the power goes during the download: the attempt stays spent, the old
     * firmware reports `failed` with `download`, and the same offer is not tried again. */
    d = offer(&s, &rec, &keys, OFFER_A, 5000, 10);
    nvs_save(&keys, &rec, d.save);
    CHECK(power_on(&s, &rec, &keys, 0, false, &save) == DH_FW_START_INTERRUPTED &&
          save == DH_FW_SAVE_RECORD);
    /* 3. ... and the power goes again before that is saved: the same answer next time. */
    CHECK(power_on(&s, &rec, &keys, 0, false, &save) == DH_FW_START_INTERRUPTED);
    nvs_save(&keys, &rec, save);
    CHECK(power_on(&s, &rec, &keys, 0, false, &save) == DH_FW_START_NORMAL && save == 0);
    CHECK(rec.attempts_left == 4 && rec.stage == DH_FW_STAGE_IDLE);
    CHECK(dh_fw_report_next(&s, &rec, &rep) && rep.state == DH_FW_REPORT_FAILED &&
          rep.reason == DH_FW_REASON_DOWNLOAD && strcmp(rep.offer_id, OFFER_A) == 0);
    /* 4. The hub answers, and the power goes before that is saved: it is reported again. */
    CHECK(dh_fw_report_answered(&s, &rec, &rep) == DH_FW_SAVE_RECORD);
    CHECK(power_on(&s, &rec, &keys, 0, false, &save) == DH_FW_START_NORMAL);
    CHECK(dh_fw_report_next(&s, &rec, &rep) && rep.state == DH_FW_REPORT_FAILED);
    nvs_save(&keys, &rec, dh_fw_report_answered(&s, &rec, &rep));
    CHECK(power_on(&s, &rec, &keys, 0, false, &save) == DH_FW_START_NORMAL);
    CHECK(!dh_fw_report_next(&s, &rec, &rep));
    CHECK(offer(&s, &rec, &keys, OFFER_A, 5000, 10).action == DH_FW_OFFER_IGNORE);

    /* 5. A refusal is decided, and the power goes before it is saved: it reads as an attempt
     * cut short. The attempt is spent either way. */
    d = offer(&s, &rec, &keys, OFFER_B, 5000, 10);
    CHECK(d.action == DH_FW_OFFER_ATTEMPT);
    nvs_save(&keys, &rec, d.save);
    CHECK(dh_fw_attempt_ended(&rec, DH_FW_REASON_WRONG_HARDWARE) == DH_FW_SAVE_RECORD);
    CHECK(power_on(&s, &rec, &keys, 0, false, &save) == DH_FW_START_INTERRUPTED);
    nvs_save(&keys, &rec, save);
    CHECK(rec.attempts_left == 3 && rec.result_reason == DH_FW_REASON_DOWNLOAD);
    CHECK(dh_fw_report_next(&s, &rec, &rep));
    nvs_save(&keys, &rec, dh_fw_report_answered(&s, &rec, &rep));

    /* 6. The image is written and verified, and the power goes before the record is saved:
     * the spare partition was never set to boot, so it is an attempt cut short. */
    d = offer(&s, &rec, &keys, OFFER_C, 5000, 10);
    nvs_save(&keys, &rec, d.save);
    CHECK(dh_fw_attempt_installed(&rec, &m, 1) == DH_FW_SAVE_RECORD);
    CHECK(power_on(&s, &rec, &keys, 0, false, &save) == DH_FW_START_INTERRUPTED);
    nvs_save(&keys, &rec, save);
    CHECK(dh_fw_report_next(&s, &rec, &rep));
    nvs_save(&keys, &rec, dh_fw_report_answered(&s, &rec, &rep));
    CHECK(rec.attempts_left == 2);

    /* 7. Saved, and the power goes before the partition is set to boot: the old firmware
     * starts, finds a release it didn't finish, and reports `rolled_back`. Keys and floor are
     * untouched, and the pending key_set is dropped. */
    d = offer(&s, &rec, &keys, OFFER_A, 5000, 10);
    CHECK(d.action == DH_FW_OFFER_ATTEMPT);
    nvs_save(&keys, &rec, d.save);
    nvs_save(&keys, &rec, dh_fw_attempt_installed(&rec, &m, 1));
    CHECK(rec.stage == DH_FW_STAGE_PENDING && rec.pending_has_key_set);
    CHECK(power_on(&s, &rec, &keys, 0, false, &save) == DH_FW_START_ROLLED_BACK &&
          save == DH_FW_SAVE_RECORD);
    CHECK(rec.stage == DH_FW_STAGE_IDLE && !rec.pending_has_key_set &&
          rec.pending_key_count == 0);
    /* 8. ... the power goes before that is saved: the same again. */
    CHECK(power_on(&s, &rec, &keys, 0, false, &save) == DH_FW_START_ROLLED_BACK);
    nvs_save(&keys, &rec, save);
    CHECK(power_on(&s, &rec, &keys, 0, false, &save) == DH_FW_START_NORMAL && save == 0);
    CHECK(keys.key_count == 2 && keys.security_floor == 3 &&
          memcmp(keys.keys[0].bytes, maker.public.bytes, DH_FW_KEY_LEN) == 0);
    CHECK(dh_fw_report_next(&s, &rec, &rep) && rep.state == DH_FW_REPORT_ROLLED_BACK &&
          rep.reason == DH_FW_REASON_NONE && strcmp(rep.offer_id, OFFER_A) == 0);
    nvs_save(&keys, &rec, dh_fw_report_answered(&s, &rec, &rep));
    CHECK(rec.attempts_left == 1);

    /* 9. Set to boot and restarted. On the trial boot nothing is saved, reported or acted on;
     * the power goes during it, and the bootloader goes back. */
    d = offer(&s, &rec, &keys, OFFER_B, 5000, 10);
    CHECK(d.action == DH_FW_OFFER_ATTEMPT);
    nvs_save(&keys, &rec, d.save);
    nvs_save(&keys, &rec, dh_fw_attempt_installed(&rec, &m, 1));
    uint8_t before_trial[DH_FW_RECORD_BLOB_LEN], during_trial[DH_FW_RECORD_BLOB_LEN];
    memcpy(before_trial, nvs.record, sizeof before_trial);
    CHECK(power_on(&s, &rec, &keys, 1, true, &save) == DH_FW_START_TRIAL && save == 0);
    CHECK(!dh_fw_report_next(&s, &rec, &rep));
    CHECK(offer(&s, &rec, &keys, OFFER_C, 5000, 20).action == DH_FW_OFFER_BUSY);
    CHECK(dh_fw_trial_check_in(&s, 20, DH_FW_CHECK_IN_FAILED) == DH_FW_TRIAL_WAIT);
    dh_fw_record_encode(&rec, during_trial);
    CHECK(memcmp(before_trial, during_trial, sizeof before_trial) == 0);
    CHECK(keys.key_count == 2 && keys.security_floor == 3);
    CHECK(power_on(&s, &rec, &keys, 0, false, &save) == DH_FW_START_ROLLED_BACK);
    nvs_save(&keys, &rec, save);
    CHECK(keys.security_floor == 3 && keys.key_count == 2);
    CHECK(dh_fw_report_next(&s, &rec, &rep) && rep.state == DH_FW_REPORT_ROLLED_BACK);
    nvs_save(&keys, &rec, dh_fw_report_answered(&s, &rec, &rep));
    CHECK(rec.attempts_left == 0);

    /* A USB bundle refills the budget, for the last run. */
    CHECK(dh_fw_bundle_apply(&rec, &keys, NULL, &save) == DH_FW_BUNDLE_OK &&
          save == DH_FW_SAVE_RECORD);
    nvs_save(&keys, &rec, save);

    /* 10. The trial reaches the hub and the image is marked good; then the power goes before
     * anything is saved. The new firmware starts again, no longer on trial, finds its own
     * release still pending, and finishes the job. */
    d = offer(&s, &rec, &keys, OFFER_C, 5000, 10);
    CHECK(d.action == DH_FW_OFFER_ATTEMPT);
    nvs_save(&keys, &rec, d.save);
    nvs_save(&keys, &rec, dh_fw_attempt_installed(&rec, &m, 1));
    CHECK(power_on(&s, &rec, &keys, 1, true, &save) == DH_FW_START_TRIAL);
    CHECK(dh_fw_trial_check_in(&s, 30, DH_FW_CHECK_IN_ANSWERED) == DH_FW_TRIAL_MARK_GOOD);
    CHECK(dh_fw_marked_good(&s, &rec, &keys) == (DH_FW_SAVE_KEYS | DH_FW_SAVE_RECORD));
    CHECK(power_on(&s, &rec, &keys, 1, false, &save) == DH_FW_START_INSTALLED &&
          save == (DH_FW_SAVE_KEYS | DH_FW_SAVE_RECORD));
    CHECK(keys.security_floor == 7 && keys.key_count == 2 &&
          memcmp(keys.keys[0].bytes, recovery.public.bytes, DH_FW_KEY_LEN) == 0 &&
          memcmp(keys.keys[1].bytes, fresh.public.bytes, DH_FW_KEY_LEN) == 0);
    /* 11. The keys are saved, and the power goes before the record: the same again, and the
     * keys come out the same. */
    nvs_save(&keys, &rec, DH_FW_SAVE_KEYS);
    uint8_t keys_once[DH_FW_KEYS_BLOB_LEN];
    memcpy(keys_once, nvs.keys, sizeof keys_once);
    CHECK(power_on(&s, &rec, &keys, 1, false, &save) == DH_FW_START_INSTALLED &&
          save == (DH_FW_SAVE_KEYS | DH_FW_SAVE_RECORD));
    nvs_save(&keys, &rec, save);
    CHECK(memcmp(keys_once, nvs.keys, sizeof keys_once) == 0);
    /* 12. Both saved: an ordinary start, with `installed` to report. */
    CHECK(power_on(&s, &rec, &keys, 1, false, &save) == DH_FW_START_NORMAL && save == 0);
    CHECK(rec.stage == DH_FW_STAGE_IDLE && keys.security_floor == 7);
    CHECK(dh_fw_report_next(&s, &rec, &rep) && rep.state == DH_FW_REPORT_INSTALLED &&
          strcmp(rep.offer_id, OFFER_C) == 0);
    nvs_save(&keys, &rec, dh_fw_report_answered(&s, &rec, &rep));
    /* The result never changed the budget: five after the bundle, one spent. */
    CHECK(rec.attempts_left == 4);
    /* The retired key is refused from now on, and the old release is a downgrade. */
    const size_t image_len = 300;
    size_t mlen = manifest_for(manifest, sizeof manifest, image, image_len, "1.9.0", 6, "");
    size_t len = build(package, &maker, manifest, mlen, image, image_len);
    CHECK(download_all(&keys, &DEVICE, package, len, len).reason == DH_FW_REASON_UNTRUSTED_KEY);
    len = build(package, &recovery, manifest, mlen, image, image_len);
    CHECK(download_all(&keys, &DEVICE, package, len, len).reason == DH_FW_REASON_DOWNGRADE);

    /* 13. A credit hour is counted, and the power goes before it is saved: it is lost, and so
     * is the hour. A restart can only lengthen the wait. */
    CHECK(power_on(&s, &rec, &keys, 1, false, &save) == DH_FW_START_NORMAL);
    CHECK(dh_fw_tick(&s, &rec, 3600) == DH_FW_SAVE_RECORD && rec.credit_hours == 1);
    CHECK(power_on(&s, &rec, &keys, 1, false, &save) == DH_FW_START_NORMAL);
    CHECK(rec.credit_hours == 0 && rec.attempts_left == 4);
    CHECK(dh_fw_tick(&s, &rec, 3599) == 0);

    /* A record that says an attempt is running with no offer to report against (which nothing
     * here writes) is cleared rather than reported. */
    dh_fw_record_init(&rec);
    rec.stage = DH_FW_STAGE_PENDING;
    CHECK(dh_fw_start(&s, &rec, &keys, 0, false, &save) == DH_FW_START_NORMAL &&
          save == DH_FW_SAVE_RECORD && rec.stage == DH_FW_STAGE_IDLE);
    CHECK(!dh_fw_report_next(&s, &rec, &rep));
}

/* ---- The bundle ---- */

static bool bundle_parses(const char *json, dh_fw_bundle *out)
{
    dh_json_doc doc;
    dh_json_value root;
    if (dh_json_parse(&doc, (const uint8_t *)json, strlen(json), NULL, &root) != DH_JSON_OK) {
        fprintf(stderr, "a test bundle isn't JSON: %s\n", json);
        exit(1);
    }
    return dh_fw_bundle_parse(&doc, root, out);
}

static void test_bundle(void)
{
    char hex[4][131], text[1024];
    key_hex(&maker.public, hex[0]);
    key_hex(&recovery.public, hex[1]);
    key_hex(&fresh.public, hex[2]);
    key_hex(&stranger.public, hex[3]);
    dh_fw_bundle b;

    snprintf(text, sizeof text, "{\"trusted_keys\":[\"%s\",\"%s\"],\"security_counter\":3,\"x\":1}",
             hex[0], hex[1]);
    CHECK(bundle_parses(text, &b) && b.key_count == 2 && b.security_counter == 3 &&
          memcmp(b.keys[1].bytes, recovery.public.bytes, DH_FW_KEY_LEN) == 0);
    snprintf(text, sizeof text, "{\"security_counter\":4294967295,\"trusted_keys\":[\"%s\"]}", hex[0]);
    CHECK(bundle_parses(text, &b) && b.key_count == 1 && b.security_counter == 4294967295u);
    static const char *counters[] = {"4294967296", "-1", "1.0", "1e0", "\"3\"", "null", "[3]"};
    for (size_t i = 0; i < LEN(counters); i++) {
        snprintf(text, sizeof text, "{\"trusted_keys\":[\"%s\"],\"security_counter\":%s}", hex[0],
                 counters[i]);
        CHECK(!bundle_parses(text, &b) && b.key_count == 0);
    }
    snprintf(text, sizeof text, "{\"trusted_keys\":[\"%s\"]}", hex[0]);
    CHECK(!bundle_parses(text, &b));
    CHECK(!bundle_parses("{\"security_counter\":3}", &b));
    CHECK(!bundle_parses("{\"trusted_keys\":[],\"security_counter\":3}", &b));
    CHECK(!bundle_parses("{\"trusted_keys\":null,\"security_counter\":3}", &b));
    CHECK(!bundle_parses("{\"trusted_keys\":[\"04\"],\"security_counter\":3}", &b));
    CHECK(!bundle_parses("{\"trusted_keys\":[7],\"security_counter\":3}", &b));
    CHECK(!bundle_parses("null", &b));
    CHECK(!bundle_parses("[]", &b));
    CHECK(!bundle_parses("\"x\"", &b));
    snprintf(text, sizeof text,
             "{\"trusted_keys\":[\"%s\",\"%s\",\"%s\",\"%s\"],\"security_counter\":3}", hex[0],
             hex[1], hex[2], hex[3]);
    CHECK(!bundle_parses(text, &b));
    snprintf(text, sizeof text, "{\"trusted_keys\":[\"%s\",\"%s\"],\"security_counter\":3}", hex[0],
             hex[0]);
    CHECK(!bundle_parses(text, &b));
    /* A member the reference knows, twice. */
    snprintf(text, sizeof text,
             "{\"trusted_keys\":[\"%s\"],\"security_counter\":3,\"security_counter\":3}", hex[0]);
    CHECK(!bundle_parses(text, &b));
    /* Not starting 04, and upper-case. */
    char altered[131];
    memcpy(altered, hex[0], sizeof altered);
    altered[1] = '3';
    snprintf(text, sizeof text, "{\"trusted_keys\":[\"%s\"],\"security_counter\":3}", altered);
    CHECK(!bundle_parses(text, &b));
    memcpy(altered, hex[0], sizeof altered);
    for (size_t i = 0; i < 130; i++) {
        if (altered[i] >= 'a' && altered[i] <= 'f') {
            altered[i] = (char)(altered[i] - 'a' + 'A');
            break;
        }
    }
    snprintf(text, sizeof text, "{\"trusted_keys\":[\"%s\"],\"security_counter\":3}", altered);
    CHECK(!bundle_parses(text, &b));
    /* A key that isn't a point still parses: whether it is one is the next question. */
    memcpy(altered, hex[0], sizeof altered);
    altered[129] = altered[129] == '0' ? '1' : '0';
    snprintf(text, sizeof text, "{\"trusted_keys\":[\"%s\",\"%s\"],\"security_counter\":3}", hex[1],
             altered);
    CHECK(bundle_parses(text, &b) && b.key_count == 2);
    dh_fw_keys keys = keys_of(NULL, NULL, 0), same = keys_of(&recovery, NULL, 9);
    dh_fw_record rec;
    unsigned save = 99;
    dh_fw_record_init(&rec);
    rec.attempts_left = 1;
    rec.credit_hours = 2;
    CHECK(dh_fw_bundle_check(&keys, &b) == DH_FW_BUNDLE_INVALID);
    /* It is invalid before it is locked: the points are checked first. */
    CHECK(dh_fw_bundle_check(&same, &b) == DH_FW_BUNDLE_INVALID);
    CHECK(dh_fw_bundle_apply(&rec, &keys, &b, &save) == DH_FW_BUNDLE_INVALID && save == 0);
    CHECK(keys.key_count == 0 && keys.security_floor == 0 && rec.attempts_left == 1 &&
          rec.credit_hours == 2);

    /* A display with no keys takes the bundle's, and its counter as the floor. */
    memset(&b, 0, sizeof b);
    b.key_count = 3;
    b.keys[0] = maker.public;
    b.keys[1] = recovery.public;
    b.keys[2] = fresh.public;
    b.security_counter = 6;
    CHECK(dh_fw_bundle_check(&keys, &b) == DH_FW_BUNDLE_OK && keys.key_count == 0);
    CHECK(dh_fw_bundle_apply(&rec, &keys, &b, &save) == DH_FW_BUNDLE_OK &&
          save == (DH_FW_SAVE_KEYS | DH_FW_SAVE_RECORD));
    CHECK(keys.key_count == 3 && keys.security_floor == 6 &&
          memcmp(keys.keys[2].bytes, fresh.public.bytes, DH_FW_KEY_LEN) == 0);
    CHECK(rec.attempts_left == 5 && rec.credit_hours == 0);
    /* The same set in another order: accepted, and nothing changes or is saved, floor included. */
    b.keys[0] = fresh.public;
    b.keys[2] = maker.public;
    b.security_counter = 99;
    CHECK(dh_fw_bundle_apply(&rec, &keys, &b, &save) == DH_FW_BUNDLE_OK && save == 0);
    CHECK(keys.security_floor == 6 &&
          memcmp(keys.keys[0].bytes, maker.public.bytes, DH_FW_KEY_LEN) == 0);
    /* No firmware member: accepted, and only the budget moves. */
    rec.attempts_left = 0;
    CHECK(dh_fw_bundle_check(&keys, NULL) == DH_FW_BUNDLE_OK);
    CHECK(dh_fw_bundle_apply(&rec, &keys, NULL, &save) == DH_FW_BUNDLE_OK &&
          save == DH_FW_SAVE_RECORD && rec.attempts_left == 5 && keys.key_count == 3);
    /* Credit hours alone are a change too. */
    rec.attempts_left = 5;
    rec.credit_hours = 1;
    CHECK(dh_fw_bundle_apply(&rec, &keys, NULL, &save) == DH_FW_BUNDLE_OK &&
          save == DH_FW_SAVE_RECORD && rec.credit_hours == 0);
    /* Fewer, more, or other keys: locked, and nothing changes, not even the budget. */
    rec.attempts_left = 2;
    dh_fw_bundle fewer = b, other = b;
    fewer.key_count = 2;
    other.keys[1] = stranger.public;
    CHECK(dh_fw_bundle_apply(&rec, &keys, &fewer, &save) == DH_FW_BUNDLE_KEYS_LOCKED && save == 0);
    CHECK(dh_fw_bundle_apply(&rec, &keys, &other, &save) == DH_FW_BUNDLE_KEYS_LOCKED && save == 0);
    CHECK(rec.attempts_left == 2 && keys.key_count == 3 && keys.security_floor == 6);
    dh_fw_keys two = keys_of(&maker, &recovery, 3);
    CHECK(dh_fw_bundle_check(&two, &b) == DH_FW_BUNDLE_KEYS_LOCKED); /* the bundle has more */
    dh_fw_keys one = keys_of(&maker, NULL, 3);
    dh_fw_bundle stranger_only;
    memset(&stranger_only, 0, sizeof stranger_only);
    stranger_only.key_count = 1;
    stranger_only.keys[0] = stranger.public;
    CHECK(dh_fw_bundle_check(&one, &stranger_only) == DH_FW_BUNDLE_KEYS_LOCKED);
    /* A count outside 1 to 3, or a repeat, however the bundle was read. */
    dh_fw_bundle bad = b;
    bad.key_count = 0;
    CHECK(dh_fw_bundle_check(&keys, &bad) == DH_FW_BUNDLE_INVALID);
    bad.key_count = 4;
    CHECK(dh_fw_bundle_check(&keys, &bad) == DH_FW_BUNDLE_INVALID);
    bad = b;
    bad.keys[2] = bad.keys[0];
    CHECK(dh_fw_bundle_check(&keys, &bad) == DH_FW_BUNDLE_INVALID);
    dh_fw_keys empty = keys_of(NULL, NULL, 0);
    CHECK(dh_fw_bundle_check(&empty, &bad) == DH_FW_BUNDLE_INVALID);
}

int main(void)
{
    make_signer(&maker);
    make_signer(&recovery);
    make_signer(&fresh);
    make_signer(&stranger);
    fill_image();
    test_crypto();
    test_names();
    test_versions();
    test_header();
    test_manifest();
    test_manifest_check();
    test_download();
    test_largest_image();
    test_stored_forms();
    test_budget();
    test_offers();
    test_trial();
    test_power_cuts();
    test_bundle();
    printf("update: %d checks, %d failed\n", checks, failures);
    /* So a leak checker sees only what these tests leaked. */
    mbedtls_psa_crypto_free();
    return failures ? 1 : 0;
}
