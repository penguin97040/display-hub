/*
 * The provisioning bundle and its stored form. See dh_setup.h.
 *
 * The rules are Bundle::problems() in crates/hub-protocol/src/provisioning.rs, with the firmware
 * member's from crates/hub-protocol/src/firmware.rs, checked in the same order. What the reference
 * can't read into its Bundle type at all is DH_BUNDLE_UNREADABLE here, so that the caller ignores
 * the line as the reference does: a member that is missing, repeated or of the wrong type, a
 * `version` or `security_counter` that isn't a plain integer fitting 32 bits, a `port` that isn't
 * one fitting 16 bits, and an `expires_at` that isn't a real date in the wire form. As in the
 * reference, `wifi` and `firmware` may each be left out or null, and unknown members are ignored.
 *
 * The size rule is exact: the reference counts the bundle as it writes it out again, which is the
 * same members in a fixed order, each string escaped serde_json's way, so the length is worked out
 * here without writing anything.
 *
 * Where this differs from the reference:
 *  - only the first broken rule is named, in fixed words, where the reference lists them all;
 *  - an object written as an array of its members in order, which serde also accepts, is
 *    unreadable here.
 */
#include "dh_setup.h"

#include <string.h>

#include "dh_text.h"
#include "dh_time.h"

#define LIT(s) s, (sizeof s - 1u)

_Static_assert(DH_SETUP_BLOB_MAX ==
                   1u + 1u + DH_SETUP_MAX_HOSTS * (1u + DH_SETUP_MAX_HOST_BYTES) + 2u + 1u +
                       DH_SETUP_MAX_ANCHORS * (2u + DH_SETUP_MAX_PEM_BYTES + DH_SHA256_LEN) + 1u +
                       DH_SETUP_MAX_SECRET_BYTES + DH_SETUP_TIMESTAMP_BYTES + 1u + 1u +
                       DH_SETUP_MAX_SSID_BYTES + 1u + DH_SETUP_MAX_PASSWORD_BYTES,
               "DH_SETUP_BLOB_MAX must be the longest stored form");

static const char PEM_START[] = "-----BEGIN CERTIFICATE-----";
#define PEM_START_LEN (sizeof PEM_START - 1u)

#define SETUP_FORMAT 1u

void dh_wipe(void *p, size_t n)
{
    volatile uint8_t *b = (volatile uint8_t *)p;
    while (n-- > 0) {
        *b++ = 0;
    }
}

void dh_setup_wipe(dh_setup *setup)
{
    dh_wipe(setup, sizeof *setup);
}

/* ---- The rules, on plain bytes ---- */

static bool is_utf8(const char *s, size_t n)
{
    size_t i = 0;
    while (i < n) {
        uint32_t cp;
        size_t len = dh_utf8_decode((const uint8_t *)s + i, n - i, &cp);
        if (len == 0) {
            return false;
        }
        i += len;
    }
    return true;
}

/* A plain host name or address: 1 to 253 bytes with no white space or control character (Rust's
 * char::is_whitespace and char::is_control). */
static bool host_ok(const char *s, size_t n)
{
    if (n == 0 || n > DH_SETUP_MAX_HOST_BYTES) {
        return false;
    }
    size_t i = 0;
    while (i < n) {
        uint32_t cp;
        size_t len = dh_utf8_decode((const uint8_t *)s + i, n - i, &cp);
        if (len == 0 || dh_char_is_whitespace(cp) || cp < 0x20 || (cp >= 0x7F && cp <= 0x9F)) {
            return false;
        }
        i += len;
    }
    return true;
}

static bool pem_ok(const char *s, size_t n)
{
    return n >= PEM_START_LEN && n <= DH_SETUP_MAX_PEM_BYTES &&
           memcmp(s, PEM_START, PEM_START_LEN) == 0;
}

static bool password_ok(size_t n)
{
    return n == 0 || (n >= DH_SETUP_MIN_PASSWORD_BYTES && n <= DH_SETUP_MAX_PASSWORD_BYTES);
}

static bool timestamp_ok(const char *s, size_t n)
{
    int64_t t;
    return n == DH_SETUP_TIMESTAMP_BYTES && dh_time_parse(s, n, &t);
}

bool dh_setup_valid(const dh_setup *s)
{
    if (s->host_count < 1 || s->host_count > DH_SETUP_MAX_HOSTS || s->port == 0 ||
        s->anchor_count < 1 || s->anchor_count > DH_SETUP_MAX_ANCHORS ||
        s->secret_len > DH_SETUP_MAX_SECRET_BYTES || !is_utf8(s->secret, s->secret_len) ||
        s->expires_at[DH_SETUP_TIMESTAMP_BYTES] != '\0' ||
        !timestamp_ok(s->expires_at, DH_SETUP_TIMESTAMP_BYTES)) {
        return false;
    }
    for (uint8_t i = 0; i < s->host_count; i++) {
        if (!host_ok(s->hosts[i].text, s->hosts[i].len)) {
            return false;
        }
    }
    for (uint8_t i = 0; i < s->anchor_count; i++) {
        const dh_setup_anchor *a = &s->anchors[i];
        if (!pem_ok(a->pem, a->pem_len) || !is_utf8(a->pem, a->pem_len)) {
            return false;
        }
    }
    if (!s->has_wifi) {
        return s->ssid_len == 0 && s->password_len == 0;
    }
    return s->ssid_len >= 1 && s->ssid_len <= DH_SETUP_MAX_SSID_BYTES &&
           is_utf8(s->ssid, s->ssid_len) && password_ok(s->password_len) &&
           is_utf8(s->password, s->password_len);
}

/* ---- Reading a bundle ---- */

/* The member's value, if the object has it exactly once. */
static bool one(const dh_json_doc *doc, dh_json_value obj, const char *name, size_t n,
                dh_json_value *v)
{
    return dh_json_object_find(doc, obj, name, n, v) == 1;
}

static bool one_of_type(const dh_json_doc *doc, dh_json_value obj, const char *name, size_t n,
                        dh_json_type type, dh_json_value *v)
{
    return one(doc, obj, name, n, v) && dh_json_type_of(doc, *v) == type;
}

/* A member that may be left out or null. False if it is repeated, or is anything but null or an
 * object. *present says whether *v is an object to read. */
static bool optional_object(const dh_json_doc *doc, dh_json_value obj, const char *name, size_t n,
                            bool *present, dh_json_value *v)
{
    *present = false;
    size_t count = dh_json_object_find(doc, obj, name, n, v);
    if (count == 0) {
        return true;
    }
    if (count != 1) {
        return false;
    }
    dh_json_type type = dh_json_type_of(doc, *v);
    *present = type == DH_JSON_OBJECT;
    return type == DH_JSON_OBJECT || type == DH_JSON_NULL;
}

/* A plain unsigned integer of at most max. */
static bool one_integer(const dh_json_doc *doc, dh_json_value obj, const char *name, size_t n,
                        uint64_t max, uint64_t *out)
{
    dh_json_value v;
    return one_of_type(doc, obj, name, n, DH_JSON_NUMBER, &v) &&
           dh_json_number_u64(doc, v, out) && *out <= max;
}

/* Whether v is an array whose items all have the one type. */
static bool array_of(const dh_json_doc *doc, dh_json_value v, dh_json_type type)
{
    dh_json_iter it;
    dh_json_value item;
    if (!dh_json_array_begin(doc, v, &it)) {
        return false;
    }
    while (dh_json_array_next(doc, &it, &item)) {
        if (dh_json_type_of(doc, item) != type) {
            return false;
        }
    }
    return true;
}

static size_t decimal_digits(uint64_t n)
{
    size_t digits = 1;
    while (n >= 10) {
        n /= 10;
        digits++;
    }
    return digits;
}

static int hex_digit(char c)
{
    if (c >= '0' && c <= '9') {
        return c - '0';
    }
    if (c >= 'a' && c <= 'f') {
        return c - 'a' + 10;
    }
    return -1;
}

/* The values of a bundle that has the right shape. */
typedef struct {
    dh_json_value format, hosts, server_name, anchors, secret, expires_at;
    uint64_t version, port;
    bool has_wifi, has_firmware;
    dh_json_value ssid, password, firmware, trusted_keys;
    uint64_t security_counter;
} bundle_shape;

static bool read_shape(const dh_json_doc *doc, dh_json_value bundle, bundle_shape *s)
{
    dh_json_value hub, enrolment, wifi, v;
    if (dh_json_type_of(doc, bundle) != DH_JSON_OBJECT ||
        !one_of_type(doc, bundle, LIT("format"), DH_JSON_STRING, &s->format) ||
        !one_integer(doc, bundle, LIT("version"), UINT32_MAX, &s->version) ||
        !one_of_type(doc, bundle, LIT("hub"), DH_JSON_OBJECT, &hub) ||
        !one_of_type(doc, bundle, LIT("enrolment"), DH_JSON_OBJECT, &enrolment) ||
        !optional_object(doc, bundle, LIT("wifi"), &s->has_wifi, &wifi) ||
        !optional_object(doc, bundle, LIT("firmware"), &s->has_firmware, &s->firmware)) {
        return false;
    }
    if (!one(doc, hub, LIT("hosts"), &s->hosts) || !array_of(doc, s->hosts, DH_JSON_STRING) ||
        !one_integer(doc, hub, LIT("port"), UINT16_MAX, &s->port) ||
        !one_of_type(doc, hub, LIT("server_name"), DH_JSON_STRING, &s->server_name) ||
        !one(doc, hub, LIT("trust_anchors"), &s->anchors) ||
        !array_of(doc, s->anchors, DH_JSON_OBJECT)) {
        return false;
    }
    dh_json_iter it;
    dh_json_value anchor;
    dh_json_array_begin(doc, s->anchors, &it);
    while (dh_json_array_next(doc, &it, &anchor)) {
        if (!one_of_type(doc, anchor, LIT("certificate_pem"), DH_JSON_STRING, &v) ||
            !one_of_type(doc, anchor, LIT("spki_sha256"), DH_JSON_STRING, &v)) {
            return false;
        }
    }
    char when[DH_SETUP_TIMESTAMP_BYTES + 1u];
    size_t when_len = 0;
    if (!one_of_type(doc, enrolment, LIT("secret"), DH_JSON_STRING, &s->secret) ||
        !one_of_type(doc, enrolment, LIT("expires_at"), DH_JSON_STRING, &s->expires_at) ||
        !dh_json_string_copy(doc, s->expires_at, when, sizeof when, &when_len) ||
        !timestamp_ok(when, when_len)) {
        return false;
    }
    if (s->has_wifi && (!one_of_type(doc, wifi, LIT("ssid"), DH_JSON_STRING, &s->ssid) ||
                        !one_of_type(doc, wifi, LIT("password"), DH_JSON_STRING, &s->password))) {
        return false;
    }
    if (s->has_firmware &&
        (!one(doc, s->firmware, LIT("trusted_keys"), &s->trusted_keys) ||
         !array_of(doc, s->trusted_keys, DH_JSON_STRING) ||
         !one_integer(doc, s->firmware, LIT("security_counter"), UINT32_MAX,
                      &s->security_counter))) {
        return false;
    }
    return true;
}

/* The length of the bundle as serde_json writes the reference's Bundle type out: the members it
 * knows, in its order, compactly, with `wifi` as null when absent and `firmware` left out when
 * absent. A timestamp is written back in the same 20 characters. */
static size_t serialised_size(const dh_json_doc *doc, const bundle_shape *s)
{
    dh_json_iter it;
    dh_json_value v, m;
    size_t n = sizeof "{\"format\":,\"version\":,\"hub\":{\"hosts\":[],\"port\":,\"server_name\":"
                      ",\"trust_anchors\":[]},\"enrolment\":{\"secret\":,\"expires_at\":},"
                      "\"wifi\":}" -
               1u;
    n += dh_json_serde_string_length(doc, s->format) + decimal_digits(s->version) +
         decimal_digits(s->port) + dh_json_serde_string_length(doc, s->server_name) +
         dh_json_serde_string_length(doc, s->secret) + DH_SETUP_TIMESTAMP_BYTES + 2u;

    size_t count = 0;
    dh_json_array_begin(doc, s->hosts, &it);
    while (dh_json_array_next(doc, &it, &v)) {
        n += dh_json_serde_string_length(doc, v);
        count++;
    }
    n += count > 0 ? count - 1u : 0u; /* the commas between them */

    count = 0;
    dh_json_array_begin(doc, s->anchors, &it);
    while (dh_json_array_next(doc, &it, &v)) {
        n += sizeof "{\"certificate_pem\":,\"spki_sha256\":}" - 1u;
        dh_json_object_find(doc, v, LIT("certificate_pem"), &m);
        n += dh_json_serde_string_length(doc, m);
        dh_json_object_find(doc, v, LIT("spki_sha256"), &m);
        n += dh_json_serde_string_length(doc, m);
        count++;
    }
    n += count > 0 ? count - 1u : 0u;

    if (s->has_wifi) {
        n += sizeof "{\"ssid\":,\"password\":}" - 1u;
        n += dh_json_serde_string_length(doc, s->ssid) +
             dh_json_serde_string_length(doc, s->password);
    } else {
        n += sizeof "null" - 1u;
    }

    if (s->has_firmware) {
        n += sizeof ",\"firmware\":{\"trusted_keys\":[],\"security_counter\":}" - 1u;
        n += decimal_digits(s->security_counter);
        count = 0;
        dh_json_array_begin(doc, s->trusted_keys, &it);
        while (dh_json_array_next(doc, &it, &v)) {
            n += dh_json_serde_string_length(doc, v);
            count++;
        }
        n += count > 0 ? count - 1u : 0u;
    }
    return n;
}

/* The first rule the bundle breaks, in the reference's order, or NULL. Fills *out and *firmware
 * as it goes; they are only good to use if this returns NULL. */
static const char *first_problem(const dh_json_doc *doc, const bundle_shape *s, dh_setup *out,
                                 dh_fw_bundle *firmware)
{
    dh_json_iter it;
    dh_json_value v, m;
    size_t len = 0;

    if (!dh_json_string_equals(doc, s->format, LIT(DH_BUNDLE_FORMAT)) ||
        s->version != DH_BUNDLE_VERSION) {
        return "not a " DH_BUNDLE_FORMAT " version 1 bundle";
    }

    size_t hosts = dh_json_array_length(doc, s->hosts);
    if (hosts < 1 || hosts > DH_SETUP_MAX_HOSTS) {
        return "1-4 hub hosts";
    }
    out->host_count = (uint8_t)hosts;
    dh_json_array_begin(doc, s->hosts, &it);
    for (size_t i = 0; dh_json_array_next(doc, &it, &v); i++) {
        dh_setup_host *h = &out->hosts[i];
        if (!dh_json_string_copy(doc, v, h->text, sizeof h->text, &len) ||
            !host_ok(h->text, len)) {
            return "hub hosts must be plain host names or addresses";
        }
        h->len = (uint8_t)len;
    }

    if (s->port == 0) {
        return "hub port must not be 0";
    }
    out->port = (uint16_t)s->port;
    if (!dh_json_string_equals(doc, s->server_name, LIT(DH_HUB_SERVER_NAME))) {
        return "server_name must be " DH_HUB_SERVER_NAME;
    }

    size_t anchors = dh_json_array_length(doc, s->anchors);
    if (anchors < 1 || anchors > DH_SETUP_MAX_ANCHORS) {
        return "1-2 trust anchors";
    }
    out->anchor_count = (uint8_t)anchors;
    dh_json_array_begin(doc, s->anchors, &it);
    for (size_t i = 0; dh_json_array_next(doc, &it, &v); i++) {
        dh_setup_anchor *a = &out->anchors[i];
        dh_json_object_find(doc, v, LIT("certificate_pem"), &m);
        if (!dh_json_string_copy(doc, m, a->pem, sizeof a->pem, &len) || !pem_ok(a->pem, len)) {
            return "trust anchors must be PEM certificates";
        }
        a->pem_len = (uint16_t)len;
        char hex[DH_SHA256_LEN * 2u + 1u];
        dh_json_object_find(doc, v, LIT("spki_sha256"), &m);
        bool ok = dh_json_string_copy(doc, m, hex, sizeof hex, &len) && len == DH_SHA256_LEN * 2u;
        for (size_t k = 0; ok && k < DH_SHA256_LEN; k++) {
            int hi = hex_digit(hex[2u * k]), lo = hex_digit(hex[2u * k + 1u]);
            ok = hi >= 0 && lo >= 0;
            a->spki_sha256[k] = (uint8_t)(ok ? (hi << 4) | lo : 0);
        }
        if (!ok) {
            return "spki_sha256 must be 64 lower-case hex digits";
        }
    }

    if (!dh_json_string_copy(doc, s->secret, out->secret, sizeof out->secret, &len) ||
        len == 0) {
        return "enrolment secret missing or too long";
    }
    out->secret_len = (uint8_t)len;
    /* The shape check has already proved this is 20 characters of a real date. */
    dh_json_string_copy(doc, s->expires_at, out->expires_at, sizeof out->expires_at, &len);

    out->has_wifi = s->has_wifi;
    if (s->has_wifi) {
        /* 802.11's limits: a name of up to 32 bytes, a WPA2 passphrase of 8 to 63. */
        if (!dh_json_string_copy(doc, s->ssid, out->ssid, sizeof out->ssid, &len) || len == 0) {
            return "Wi-Fi network name must be 1-32 bytes";
        }
        out->ssid_len = (uint8_t)len;
        if (!dh_json_string_copy(doc, s->password, out->password, sizeof out->password, &len) ||
            !password_ok(len)) {
            return "Wi-Fi password must be empty (open network) or 8-63 characters";
        }
        out->password_len = (uint8_t)len;
    }

    if (s->has_firmware && !dh_fw_bundle_parse(doc, s->firmware, firmware)) {
        return "firmware.trusted_keys must be 1-3 different keys, each 130 lower-case hex "
               "digits starting 04";
    }

    if (serialised_size(doc, s) > DH_MAX_BUNDLE_BYTES) {
        return "bundle larger than 12288 bytes";
    }
    return NULL;
}

dh_bundle_result dh_bundle_read(const dh_json_doc *doc, dh_json_value bundle, dh_setup *out,
                                bool *has_firmware, dh_fw_bundle *firmware, const char **problem)
{
    bundle_shape shape;
    memset(&shape, 0, sizeof shape);
    dh_setup_wipe(out);
    memset(firmware, 0, sizeof *firmware);
    *has_firmware = false;
    *problem = NULL;

    if (!read_shape(doc, bundle, &shape)) {
        return DH_BUNDLE_UNREADABLE;
    }
    *problem = first_problem(doc, &shape, out, firmware);
    if (*problem != NULL) {
        dh_setup_wipe(out);
        memset(firmware, 0, sizeof *firmware);
        return DH_BUNDLE_INVALID;
    }
    *has_firmware = shape.has_firmware;
    return DH_BUNDLE_OK;
}

/* ---- The stored form ---- */

/* Format, then: the host count and each host as a length byte and its bytes; the port
 * (little-endian); the anchor count and each anchor as a two-byte length (little-endian), the PEM
 * and the 32-byte SPKI hash; the secret as a length byte and its bytes; the 20 bytes of
 * expires_at; whether there is Wi-Fi (0 or 1) and, if so, the SSID and the password, each as a
 * length byte and its bytes. */

size_t dh_setup_encode(const dh_setup *s, uint8_t out[DH_SETUP_BLOB_MAX])
{
    if (!dh_setup_valid(s)) {
        return 0;
    }
    size_t n = 0;
    out[n++] = SETUP_FORMAT;
    out[n++] = s->host_count;
    for (uint8_t i = 0; i < s->host_count; i++) {
        out[n++] = s->hosts[i].len;
        memcpy(out + n, s->hosts[i].text, s->hosts[i].len);
        n += s->hosts[i].len;
    }
    out[n++] = (uint8_t)(s->port & 0xffu);
    out[n++] = (uint8_t)(s->port >> 8);
    out[n++] = s->anchor_count;
    for (uint8_t i = 0; i < s->anchor_count; i++) {
        const dh_setup_anchor *a = &s->anchors[i];
        out[n++] = (uint8_t)(a->pem_len & 0xffu);
        out[n++] = (uint8_t)(a->pem_len >> 8);
        memcpy(out + n, a->pem, a->pem_len);
        n += a->pem_len;
        memcpy(out + n, a->spki_sha256, DH_SHA256_LEN);
        n += DH_SHA256_LEN;
    }
    out[n++] = s->secret_len;
    memcpy(out + n, s->secret, s->secret_len);
    n += s->secret_len;
    memcpy(out + n, s->expires_at, DH_SETUP_TIMESTAMP_BYTES);
    n += DH_SETUP_TIMESTAMP_BYTES;
    out[n++] = s->has_wifi ? 1 : 0;
    if (s->has_wifi) {
        out[n++] = s->ssid_len;
        memcpy(out + n, s->ssid, s->ssid_len);
        n += s->ssid_len;
        out[n++] = s->password_len;
        memcpy(out + n, s->password, s->password_len);
        n += s->password_len;
    }
    return n;
}

/* A cursor over a blob that never reads past its end. */
typedef struct {
    const uint8_t *p;
    size_t left;
    bool bad;
} reader;

static uint8_t take_u8(reader *r)
{
    if (r->left < 1) {
        r->bad = true;
        return 0;
    }
    r->left--;
    return *r->p++;
}

/* Copies n bytes into out (cap bytes) and adds a NUL. */
static void take_bytes(reader *r, void *out, size_t n, size_t cap)
{
    if (r->bad || n >= cap || r->left < n) {
        r->bad = true;
        return;
    }
    memcpy(out, r->p, n);
    ((uint8_t *)out)[n] = 0;
    r->p += n;
    r->left -= n;
}

bool dh_setup_decode(dh_setup *out, const uint8_t *blob, size_t len)
{
    dh_setup_wipe(out);
    if (blob == NULL || len < 2 || len > DH_SETUP_BLOB_MAX) {
        return false;
    }
    reader r = {blob, len, false};
    if (take_u8(&r) != SETUP_FORMAT) {
        return false;
    }
    out->host_count = take_u8(&r);
    if (out->host_count < 1 || out->host_count > DH_SETUP_MAX_HOSTS) {
        r.bad = true;
    }
    for (uint8_t i = 0; !r.bad && i < out->host_count; i++) {
        out->hosts[i].len = take_u8(&r);
        take_bytes(&r, out->hosts[i].text, out->hosts[i].len, sizeof out->hosts[i].text);
    }
    uint8_t lo = take_u8(&r);
    out->port = (uint16_t)(lo | ((unsigned)take_u8(&r) << 8));
    out->anchor_count = take_u8(&r);
    if (out->anchor_count < 1 || out->anchor_count > DH_SETUP_MAX_ANCHORS) {
        r.bad = true;
    }
    for (uint8_t i = 0; !r.bad && i < out->anchor_count; i++) {
        dh_setup_anchor *a = &out->anchors[i];
        lo = take_u8(&r);
        a->pem_len = (uint16_t)(lo | ((unsigned)take_u8(&r) << 8));
        take_bytes(&r, a->pem, a->pem_len, sizeof a->pem);
        /* Zeroed: take_bytes() writes nothing once the blob has been found bad, and the copy
         * below is made all the same (*out is wiped before a bad blob is refused). */
        uint8_t hash[DH_SHA256_LEN + 1u] = {0};
        take_bytes(&r, hash, DH_SHA256_LEN, sizeof hash);
        memcpy(a->spki_sha256, hash, DH_SHA256_LEN);
    }
    out->secret_len = take_u8(&r);
    take_bytes(&r, out->secret, out->secret_len, sizeof out->secret);
    take_bytes(&r, out->expires_at, DH_SETUP_TIMESTAMP_BYTES, sizeof out->expires_at);
    uint8_t wifi = take_u8(&r);
    out->has_wifi = wifi == 1;
    if (wifi > 1) {
        r.bad = true;
    }
    if (!r.bad && out->has_wifi) {
        out->ssid_len = take_u8(&r);
        take_bytes(&r, out->ssid, out->ssid_len, sizeof out->ssid);
        out->password_len = take_u8(&r);
        take_bytes(&r, out->password, out->password_len, sizeof out->password);
    }
    /* Every byte must be accounted for, and every field must keep the bundle's rules. */
    if (r.bad || r.left != 0 || !dh_setup_valid(out)) {
        dh_setup_wipe(out);
        return false;
    }
    return true;
}
