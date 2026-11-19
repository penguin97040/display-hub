/*
 * The package format and one download: "The package" and "Checks before installing" in
 * docs/firmware-updates.md, following crates/firmware-package (header.rs, manifest.rs, json.rs,
 * display.rs) and the reference display's installer (tools/display-simulator,
 * src/firmware/install.rs).
 *
 * Where the documents leave a detail open, this does what the reference does:
 *  - a firmware version's numbers are plain decimal, with no leading zero, and fit in 32 bits;
 *  - a number anywhere in the manifest, an ignored member included, must fit in 64 bits;
 *  - an optional member that is present must have its proper type (null isn't "absent"), except
 *    usb_install, whose contents a display doesn't read at all.
 *
 * One thing is stricter than the reference: a header whose manifest and at least one byte of
 * image can't fit in the Content-Length is refused (bad_format) as soon as the header's key is
 * found to be trusted, instead of reading a manifest that the answer says isn't all there.
 */
#include "dh_update.h"

#include <string.h>

#define LIT(s) s, (sizeof s - 1u)

/* The fixed text at the front of the signed bytes, with its zero byte. */
static const uint8_t SIGNATURE_CONTEXT[24] = "display-hub.firmware.v1";

enum {
    PHASE_HEADER = 0,
    PHASE_MANIFEST,
    PHASE_IMAGE,
    PHASE_COMPLETE,
    PHASE_ENDED,
};

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

/* Exactly n bytes from 2 * n lower-case hex digits. */
static bool hex_decode(const char *s, size_t digits, uint8_t *out, size_t n)
{
    if (digits != n * 2u) {
        return false;
    }
    for (size_t i = 0; i < n; i++) {
        int hi = hex_digit(s[2u * i]);
        int lo = hex_digit(s[2u * i + 1u]);
        if (hi < 0 || lo < 0) {
            return false;
        }
        out[i] = (uint8_t)((hi << 4) | lo);
    }
    return true;
}

static bool is_digit(char c)
{
    return c >= '0' && c <= '9';
}

bool dh_fw_version_valid(const char *s, size_t n)
{
    if (n == 0 || n > DH_FW_MAX_VERSION_BYTES) {
        return false;
    }
    size_t i = 0;
    for (unsigned part = 0; part < 3; part++) {
        size_t start = i;
        uint64_t value = 0;
        while (i < n && is_digit(s[i])) {
            /* Checked as it grows, so it never gets near the end of 64 bits. */
            value = value * 10u + (uint64_t)(s[i] - '0');
            if (value > UINT32_MAX) {
                return false;
            }
            i++;
        }
        size_t digits = i - start;
        if (digits == 0 || (digits > 1 && s[start] == '0')) {
            return false;
        }
        if (part < 2) {
            if (i >= n || s[i] != '.') {
                return false;
            }
            i++;
        }
    }
    if (i == n) {
        return true;
    }
    if (s[i] != '-' || i + 1 == n) {
        return false;
    }
    for (i++; i < n; i++) {
        char c = s[i];
        bool ok = is_digit(c) || (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') || c == '.' ||
                  c == '-';
        if (!ok) {
            return false;
        }
    }
    return true;
}

/* Whether every number in a manifest that has parsed (so every number is a run of digits) fits
 * in 64 bits. */
static bool numbers_fit(const uint8_t *json, size_t len)
{
    static const char MAX[] = "18446744073709551615";
    const size_t max_digits = sizeof MAX - 1u;
    bool in_string = false;
    size_t i = 0;
    while (i < len) {
        uint8_t c = json[i];
        if (in_string) {
            if (c == '\\') {
                i += 2; /* the escaped character is never a quote that ends the string */
                continue;
            }
            if (c == '"') {
                in_string = false;
            }
            i++;
            continue;
        }
        if (c == '"') {
            in_string = true;
            i++;
            continue;
        }
        if (c < '0' || c > '9') {
            i++;
            continue;
        }
        size_t start = i;
        while (i < len && json[i] >= '0' && json[i] <= '9') {
            i++;
        }
        size_t digits = i - start;
        if (digits > max_digits ||
            (digits == max_digits && memcmp(json + start, MAX, max_digits) > 0)) {
            return false;
        }
    }
    return true;
}

/* The member's value, if the object has it. With DH_JSON_NO_DUPLICATES there is at most one. */
static bool member(const dh_json_doc *doc, dh_json_value obj, const char *name, size_t n,
                   dh_json_value *v)
{
    return dh_json_object_find(doc, obj, name, n, v) == 1;
}

/* Copies a string value into buf (cap bytes, with its NUL). False for anything but a string
 * that fits. */
static bool copy_string(const dh_json_doc *doc, dh_json_value v, char *buf, size_t cap,
                        size_t *len)
{
    if (dh_json_type_of(doc, v) != DH_JSON_STRING) {
        return false;
    }
    return dh_json_string_copy(doc, v, buf, cap, len);
}

/* A string value that follows the text rules, with at most max_chars characters. Text that
 * passes holds no NUL, so buf can be used as a C string afterwards. */
static bool copy_text(const dh_json_doc *doc, dh_json_value v, char *buf, size_t cap,
                      size_t max_chars, dh_text_scratch *scratch)
{
    size_t len = 0;
    return copy_string(doc, v, buf, cap, &len) &&
           dh_text_check((const uint8_t *)buf, len, max_chars, scratch) == DH_TEXT_OK;
}

static bool read_u64(const dh_json_doc *doc, dh_json_value v, uint64_t *out)
{
    return dh_json_type_of(doc, v) == DH_JSON_NUMBER && dh_json_number_u64(doc, v, out);
}

/* A string value of exactly n * 2 lower-case hex digits, as n bytes. */
static bool read_hex(const dh_json_doc *doc, dh_json_value v, uint8_t *out, size_t n)
{
    char text[DH_FW_KEY_LEN * 2u + 1u];
    size_t len = 0;
    if (n > DH_FW_KEY_LEN || !copy_string(doc, v, text, n * 2u + 1u, &len)) {
        return false;
    }
    return hex_decode(text, len, out, n);
}

static bool read_hardware_revisions(const dh_json_doc *doc, dh_json_value v, dh_fw_manifest *out,
                                    dh_text_scratch *scratch)
{
    dh_json_iter it;
    dh_json_value item;
    if (!dh_json_array_begin(doc, v, &it)) {
        return false;
    }
    size_t count = 0;
    while (dh_json_array_next(doc, &it, &item)) {
        if (count >= DH_FW_MAX_HARDWARE_REVISIONS) {
            return false;
        }
        char *slot = out->hardware_revisions[count];
        if (!copy_text(doc, item, slot, sizeof out->hardware_revisions[count],
                       DH_FW_MAX_DEVICE_FIELD_CHARS, scratch)) {
            return false;
        }
        for (size_t i = 0; i < count; i++) {
            if (strcmp(out->hardware_revisions[i], slot) == 0) {
                return false;
            }
        }
        count++;
    }
    out->hardware_revision_count = count;
    return count >= 1;
}

/* 1 to 3 public keys as hex, each a valid point, with no repeats. */
static bool read_keys(const dh_json_doc *doc, dh_json_value v, dh_fw_key keys[DH_FW_MAX_KEYS],
                      uint8_t *key_count, bool check_points)
{
    dh_json_iter it;
    dh_json_value item;
    if (!dh_json_array_begin(doc, v, &it)) {
        return false;
    }
    uint8_t count = 0;
    while (dh_json_array_next(doc, &it, &item)) {
        if (count >= DH_FW_MAX_KEYS) {
            return false;
        }
        dh_fw_key *key = &keys[count];
        if (!read_hex(doc, item, key->bytes, DH_FW_KEY_LEN)) {
            return false;
        }
        if (check_points && !dh_p256_key_valid(key->bytes)) {
            return false;
        }
        for (uint8_t i = 0; i < count; i++) {
            if (memcmp(keys[i].bytes, key->bytes, DH_FW_KEY_LEN) == 0) {
                return false;
            }
        }
        count++;
    }
    *key_count = count;
    return count >= 1;
}

bool dh_fw_manifest_parse(const uint8_t *json, size_t len, dh_fw_manifest *out,
                          dh_text_scratch *scratch)
{
    memset(out, 0, sizeof *out);
    if (len > DH_FW_MAX_MANIFEST_LEN) {
        return false;
    }
    /* One object, UTF-8 with no byte-order mark (a mark isn't JSON, so the reader refuses it),
     * no member name twice at any level, whole numbers only, at most four levels. */
    dh_json_doc doc;
    dh_json_value root, v;
    dh_json_options opts = {DH_FW_MAX_MANIFEST_LEN, DH_FW_MAX_JSON_DEPTH,
                            DH_JSON_NO_DUPLICATES | DH_JSON_PLAIN_INTEGERS};
    if (dh_json_parse(&doc, json, len, &opts, &root) != DH_JSON_OK ||
        dh_json_type_of(&doc, root) != DH_JSON_OBJECT || !numbers_fit(json, len)) {
        return false;
    }

    if (!member(&doc, root, LIT("device_type"), &v) ||
        !copy_text(&doc, v, out->device_type, sizeof out->device_type,
                   DH_FW_MAX_DEVICE_FIELD_CHARS, scratch)) {
        return false;
    }
    if (!member(&doc, root, LIT("hardware_revisions"), &v) ||
        !read_hardware_revisions(&doc, v, out, scratch)) {
        return false;
    }
    size_t version_len = 0;
    if (!member(&doc, root, LIT("firmware_version"), &v) ||
        !copy_string(&doc, v, out->firmware_version, sizeof out->firmware_version,
                     &version_len) ||
        !dh_fw_version_valid(out->firmware_version, version_len)) {
        return false;
    }
    uint64_t n = 0;
    if (!member(&doc, root, LIT("security_counter"), &v) || !read_u64(&doc, v, &n) ||
        n > UINT32_MAX) {
        return false;
    }
    out->security_counter = (uint32_t)n;
    if (!member(&doc, root, LIT("image_size"), &v) || !read_u64(&doc, v, &n) || n < 1 ||
        n > DH_FW_MAX_IMAGE_SIZE) {
        return false;
    }
    out->image_size = (uint32_t)n;
    if (!member(&doc, root, LIT("image_sha256"), &v) ||
        !read_hex(&doc, v, out->image_sha256, DH_SHA256_LEN)) {
        return false;
    }
    if (member(&doc, root, LIT("summary"), &v)) {
        if (!copy_text(&doc, v, out->summary, sizeof out->summary, DH_FW_MAX_SUMMARY_CHARS,
                       scratch)) {
            return false;
        }
        out->has_summary = true;
    }
    if (member(&doc, root, LIT("key_set"), &v)) {
        if (!read_keys(&doc, v, out->key_set, &out->key_count, true)) {
            return false;
        }
        out->has_key_set = true;
    }
    /* usb_install is for the app; a display ignores it, like any member it doesn't know. */
    return true;
}

bool dh_fw_bundle_parse(const dh_json_doc *doc, dh_json_value firmware, dh_fw_bundle *out)
{
    memset(out, 0, sizeof *out);
    dh_json_value v;
    uint64_t n = 0;
    if (dh_json_type_of(doc, firmware) != DH_JSON_OBJECT ||
        !member(doc, firmware, LIT("trusted_keys"), &v) ||
        !read_keys(doc, v, out->keys, &out->key_count, false) ||
        !member(doc, firmware, LIT("security_counter"), &v) ||
        !read_u64(doc, v, &n) || n > UINT32_MAX) {
        memset(out, 0, sizeof *out);
        return false;
    }
    for (uint8_t i = 0; i < out->key_count; i++) {
        if (out->keys[i].bytes[0] != 0x04) {
            memset(out, 0, sizeof *out);
            return false;
        }
    }
    out->security_counter = (uint32_t)n;
    return true;
}

bool dh_fw_header_check(const uint8_t header[DH_FW_HEADER_LEN], uint16_t *manifest_len)
{
    if (memcmp(header, "DHFW", 4) != 0 || header[4] != 1 || header[5] != 1) {
        return false;
    }
    unsigned len = (unsigned)header[6] | ((unsigned)header[7] << 8);
    if (len < DH_FW_MIN_MANIFEST_LEN || len > DH_FW_MAX_MANIFEST_LEN) {
        return false;
    }
    *manifest_len = (uint16_t)len;
    return true;
}

bool dh_fw_signature_valid(const dh_fw_key *stored_key, const uint8_t header[DH_FW_HEADER_LEN],
                           const uint8_t *manifest, size_t manifest_len)
{
    uint8_t digest[DH_SHA256_LEN];
    dh_sha256 h;
    dh_sha256_begin(&h);
    dh_sha256_update(&h, SIGNATURE_CONTEXT, sizeof SIGNATURE_CONTEXT);
    dh_sha256_update(&h, header, DH_FW_SIGNED_HEADER_LEN);
    dh_sha256_update(&h, manifest, manifest_len);
    if (!dh_sha256_finish(&h, digest)) {
        return false;
    }
    return dh_p256_verify(stored_key->bytes, digest, header + DH_FW_SIGNED_HEADER_LEN);
}

static uint8_t key_count_of(const dh_fw_keys *keys)
{
    /* A count out of range can't come from dh_fw_keys_decode(); read it as no keys at all. */
    return keys->key_count <= DH_FW_MAX_KEYS ? keys->key_count : 0;
}

dh_fw_reason dh_fw_offer_check(const dh_fw_keys *keys, const dh_fw_device *device,
                               uint64_t offer_size)
{
    if (key_count_of(keys) == 0) {
        return DH_FW_REASON_NO_TRUSTED_KEYS;
    }
    uint64_t limit = (uint64_t)DH_FW_HEADER_LEN + DH_FW_MAX_MANIFEST_LEN + device->slot_bytes;
    if (offer_size > limit) {
        return DH_FW_REASON_TOO_LARGE;
    }
    return DH_FW_REASON_NONE;
}

static bool same_text(const char *mine, const char *theirs)
{
    return mine != NULL && strcmp(mine, theirs) == 0;
}

dh_fw_reason dh_fw_manifest_check(const dh_fw_manifest *m, const dh_fw_keys *keys,
                                  const dh_fw_device *device, uint16_t manifest_len,
                                  uint64_t content_length)
{
    if (!same_text(device->device_type, m->device_type)) {
        return DH_FW_REASON_WRONG_DEVICE_TYPE;
    }
    bool listed = false;
    for (size_t i = 0; i < m->hardware_revision_count && i < DH_FW_MAX_HARDWARE_REVISIONS; i++) {
        if (same_text(device->hardware_revision, m->hardware_revisions[i])) {
            listed = true;
        }
    }
    if (!listed) {
        return DH_FW_REASON_WRONG_HARDWARE;
    }
    if (m->security_counter < keys->security_floor) {
        return DH_FW_REASON_DOWNGRADE;
    }
    /* A key_set that leaves out a key the display trusts retires that key, and needs a counter
     * above the floor. Without that, a replay at the same counter could bring a key back. */
    if (m->has_key_set && m->security_counter <= keys->security_floor) {
        for (uint8_t i = 0; i < key_count_of(keys); i++) {
            bool kept = false;
            for (uint8_t k = 0; k < m->key_count && k < DH_FW_MAX_KEYS; k++) {
                if (memcmp(keys->keys[i].bytes, m->key_set[k].bytes, DH_FW_KEY_LEN) == 0) {
                    kept = true;
                }
            }
            if (!kept) {
                return DH_FW_REASON_DOWNGRADE;
            }
        }
    }
    if (m->image_size > device->slot_bytes) {
        return DH_FW_REASON_TOO_LARGE;
    }
    if ((uint64_t)DH_FW_HEADER_LEN + manifest_len + m->image_size != content_length) {
        return DH_FW_REASON_BAD_FORMAT;
    }
    return DH_FW_REASON_NONE;
}

static void end(dh_fw_download *d, dh_fw_reason reason)
{
    dh_sha256_abandon(&d->hash);
    d->phase = PHASE_ENDED;
    d->ended = reason;
}

dh_fw_reason dh_fw_download_begin(dh_fw_download *d, const dh_fw_keys *keys,
                                  const dh_fw_device *device, uint64_t offer_size,
                                  bool has_content_length, uint64_t content_length)
{
    memset(d, 0, sizeof *d);
    d->device = *device;
    d->keys = *keys;
    d->keys.key_count = key_count_of(keys);
    dh_fw_reason reason = dh_fw_offer_check(&d->keys, device, offer_size);
    if (reason == DH_FW_REASON_NONE && (!has_content_length || content_length != offer_size)) {
        reason = DH_FW_REASON_TOO_LARGE;
    }
    if (reason != DH_FW_REASON_NONE) {
        end(d, reason);
        return reason;
    }
    d->content_length = content_length;
    d->phase = PHASE_HEADER;
    return DH_FW_REASON_NONE;
}

static size_t smaller(size_t a, size_t b)
{
    return a < b ? a : b;
}

/* Steps 3 and 4, and whether the manifest can fit in what the answer says is coming. */
static dh_fw_reason check_header(dh_fw_download *d)
{
    if (d->keys.key_count == 0) {
        return DH_FW_REASON_NO_TRUSTED_KEYS;
    }
    if (!dh_fw_header_check(d->header, &d->manifest_len)) {
        return DH_FW_REASON_BAD_FORMAT;
    }
    bool found = false;
    for (uint8_t i = 0; i < d->keys.key_count && !found; i++) {
        if (memcmp(d->keys.keys[i].bytes, d->header + 8, DH_FW_KEY_LEN) == 0) {
            d->key_index = i;
            found = true;
        }
    }
    if (!found) {
        return DH_FW_REASON_UNTRUSTED_KEY;
    }
    if ((uint64_t)DH_FW_HEADER_LEN + d->manifest_len + 1u > d->content_length) {
        return DH_FW_REASON_BAD_FORMAT;
    }
    return DH_FW_REASON_NONE;
}

/* Steps 5, 6 and 7: the signature, with the display's stored copy of the key, before anything
 * parses the manifest; then the manifest; then whether it is for this display. */
static dh_fw_reason check_manifest(dh_fw_download *d)
{
    if (!dh_fw_signature_valid(&d->keys.keys[d->key_index], d->header, d->manifest_bytes,
                               d->manifest_len)) {
        return DH_FW_REASON_BAD_SIGNATURE;
    }
    if (!dh_fw_manifest_parse(d->manifest_bytes, d->manifest_len, &d->manifest, &d->scratch)) {
        return DH_FW_REASON_BAD_FORMAT;
    }
    return dh_fw_manifest_check(&d->manifest, &d->keys, &d->device, d->manifest_len,
                                d->content_length);
}

dh_fw_push dh_fw_download_push(dh_fw_download *d, const uint8_t *piece, size_t len)
{
    dh_fw_push r = {DH_FW_STEP_MORE, DH_FW_REASON_NONE, 0, NULL, 0};
    while (d->phase != PHASE_ENDED && d->phase != PHASE_COMPLETE && r.used < len) {
        size_t rest = len - r.used;
        if (d->phase == PHASE_HEADER) {
            /* A Content-Length shorter than a header: there is nothing more to take. */
            uint64_t stated = d->content_length - d->read;
            if (stated == 0) {
                end(d, DH_FW_REASON_DOWNLOAD);
                break;
            }
            size_t take = smaller(DH_FW_HEADER_LEN - d->have, rest);
            if ((uint64_t)take > stated) {
                take = (size_t)stated;
            }
            memcpy(d->header + d->have, piece + r.used, take);
            d->have += take;
            d->read += take;
            r.used += take;
            if (d->have < DH_FW_HEADER_LEN) {
                continue;
            }
            dh_fw_reason reason = check_header(d);
            if (reason != DH_FW_REASON_NONE) {
                end(d, reason);
                break;
            }
            d->phase = PHASE_MANIFEST;
            d->have = 0;
        } else if (d->phase == PHASE_MANIFEST) {
            /* check_header() made sure the whole manifest is within the Content-Length. */
            size_t take = smaller((size_t)d->manifest_len - d->have, rest);
            memcpy(d->manifest_bytes + d->have, piece + r.used, take);
            d->have += take;
            d->read += take;
            r.used += take;
            if (d->have < d->manifest_len) {
                continue;
            }
            dh_fw_reason reason = check_manifest(d);
            if (reason != DH_FW_REASON_NONE) {
                end(d, reason);
                break;
            }
            /* Step 8. Only now may the spare partition be opened. */
            d->phase = PHASE_IMAGE;
            d->image_left = d->manifest.image_size;
            d->slot_open = true;
            dh_sha256_begin(&d->hash);
            r.step = DH_FW_STEP_OPEN_SLOT;
            return r;
        } else {
            /* dh_fw_manifest_check() made sure the image ends exactly at the Content-Length. */
            size_t take = smaller(d->image_left, rest);
            dh_sha256_update(&d->hash, piece + r.used, take);
            r.write = piece + r.used;
            r.write_len = take;
            d->image_left -= (uint32_t)take;
            d->read += take;
            r.used += take;
            if (d->image_left > 0) {
                continue;
            }
            uint8_t digest[DH_SHA256_LEN];
            if (!dh_sha256_finish(&d->hash, digest) ||
                memcmp(digest, d->manifest.image_sha256, DH_SHA256_LEN) != 0) {
                /* The last bytes needn't be written: the partition is about to be abandoned. */
                r.write = NULL;
                r.write_len = 0;
                end(d, DH_FW_REASON_IMAGE_HASH);
                break;
            }
            d->phase = PHASE_COMPLETE;
        }
    }
    if (d->phase == PHASE_ENDED) {
        r.step = DH_FW_STEP_ENDED;
        r.reason = d->ended;
    } else if (d->phase == PHASE_COMPLETE) {
        r.step = DH_FW_STEP_FINISH;
    }
    return r;
}

dh_fw_reason dh_fw_download_fail(dh_fw_download *d, dh_fw_reason why)
{
    if (d->phase != PHASE_ENDED) {
        end(d, why == DH_FW_REASON_FLASH ? DH_FW_REASON_FLASH : DH_FW_REASON_DOWNLOAD);
    }
    return d->ended;
}

bool dh_fw_download_slot_open(const dh_fw_download *d)
{
    return d->slot_open;
}

const dh_fw_manifest *dh_fw_download_manifest(const dh_fw_download *d)
{
    return d->slot_open ? &d->manifest : NULL;
}

uint64_t dh_fw_download_bytes_read(const dh_fw_download *d)
{
    return d->read;
}

bool dh_fw_download_overdue(uint64_t started_s, uint64_t last_byte_s, uint64_t now_s)
{
    uint64_t quiet = now_s > last_byte_s ? now_s - last_byte_s : 0;
    uint64_t total = now_s > started_s ? now_s - started_s : 0;
    return quiet >= DH_FW_DOWNLOAD_STALL_SECONDS || total >= DH_FW_DOWNLOAD_TIMEOUT_SECONDS;
}
