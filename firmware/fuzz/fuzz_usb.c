/*
 * Fuzzes setup over USB: the line reader, the handler and the bundle rules behind it, and the
 * setup's stored form, against a store that can be told to fail.
 *
 * The first byte of the input chooses what the rest is and how the display starts:
 *   bits 0-1  0: bytes from the port, fed to the line reader in pieces (the second byte is the
 *                piece size) and each line to the handler
 *             1: a bundle, sent as one `provision` line
 *             2: a stored setup, for dh_setup_decode()
 *             3: as 1, with the bundle as the `firmware` member of an otherwise good bundle
 *   bit 2     the display already trusts a firmware key
 *   bit 3     the display is already set up
 *   bits 4-6  the write of the keys, the record or the setup fails
 *   bit 7     storage isn't working at all
 *
 * It checks that every reply is one line of plain JSON that fits its buffer, that a setup is
 * stored only by `provisioned` and is then exactly what the handler read, that anything else
 * leaves the scratch wiped and the stored setup alone, and that what is in memory always matches
 * what was written.
 */
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

#include "dh_setup.h"
#include "dh_usb.h"
#include "dh_usb_line.h"

#define CHECK(c) \
    do { \
        if (!(c)) { \
            abort(); \
        } \
    } while (0)

/* A real public key ("maker" in protocol/v1/firmware's vectors). */
static const uint8_t MAKER[DH_FW_KEY_LEN] = {
    0x04, 0x79, 0x5c, 0x66, 0x6d, 0x0c, 0xdd, 0xa5, 0x8b, 0xfb, 0x27, 0xc6, 0x0b,
    0x93, 0x6b, 0xba, 0x8a, 0xd0, 0xc8, 0x80, 0x12, 0x18, 0xdc, 0xb6, 0xd5, 0xf5,
    0x0a, 0x73, 0xc0, 0x0a, 0x99, 0x04, 0x22, 0x9f, 0xa3, 0x66, 0x10, 0x2a, 0x70,
    0x67, 0x7d, 0x0c, 0xa5, 0x41, 0x0d, 0x63, 0x00, 0xb8, 0x27, 0x3e, 0x10, 0x2b,
    0xba, 0x89, 0x20, 0xce, 0xf8, 0x22, 0x48, 0x33, 0x39, 0x73, 0x4b, 0x7e, 0xb3,
};

static struct {
    bool has_setup;
    size_t setup_len;
    uint8_t setup[DH_SETUP_BLOB_MAX];
    uint8_t keys[DH_FW_KEYS_BLOB_LEN];
    uint8_t record[DH_FW_RECORD_BLOB_LEN];
    bool fail_keys, fail_record, fail_setup;
    unsigned setup_writes;
} flash;

static dh_held store_held(void *ctx)
{
    (void)ctx;
    return flash.has_setup ? DH_HELD_SETUP : DH_HELD_NOTHING;
}

static bool store_save_keys(void *ctx, const dh_fw_keys *keys)
{
    (void)ctx;
    if (flash.fail_keys) {
        return false;
    }
    dh_fw_keys_encode(keys, flash.keys);
    return true;
}

static bool store_save_record(void *ctx, const dh_fw_record *record)
{
    (void)ctx;
    if (flash.fail_record) {
        return false;
    }
    dh_fw_record_encode(record, flash.record);
    return true;
}

static bool store_save_setup(void *ctx, const dh_setup *setup)
{
    (void)ctx;
    flash.setup_writes++;
    if (flash.fail_setup) {
        return false;
    }
    /* The handler must never ask for a setup to be stored that breaks a rule. */
    CHECK(dh_setup_valid(setup));
    flash.setup_len = dh_setup_encode(setup, flash.setup);
    CHECK(flash.setup_len > 0);
    flash.has_setup = true;
    return true;
}

static bool store_erase_setup(void *ctx)
{
    (void)ctx;
    flash.has_setup = false;
    return true;
}

static const dh_setup_store store = {NULL,           store_held,       store_save_keys,
                                     store_save_record, store_save_setup, store_erase_setup,
                                     NULL};

static dh_fw_keys keys;
static dh_fw_record record;
static dh_setup work, back;
static const dh_setup zero;
static dh_usb_display display;
static dh_usb_line_reader reader;
static char reply[DH_USB_REPLY_MAX + 16];
static uint8_t line_buf[DH_MAX_USB_LINE_BYTES + 512];

static void check_memory_matches_flash(void)
{
    uint8_t k[DH_FW_KEYS_BLOB_LEN], r[DH_FW_RECORD_BLOB_LEN];
    dh_fw_keys_encode(&keys, k);
    dh_fw_record_encode(&record, r);
    CHECK(memcmp(k, flash.keys, sizeof k) == 0);
    CHECK(memcmp(r, flash.record, sizeof r) == 0);
}

static void handle(const uint8_t *line, size_t len)
{
    bool was_set_up = flash.has_setup;
    unsigned writes_before = flash.setup_writes;
    uint8_t setup_before[DH_SHA256_LEN], setup_after[DH_SHA256_LEN];
    CHECK(dh_sha256_of(flash.setup, sizeof flash.setup, setup_before));
    uint8_t key_count_before = keys.key_count;

    /* A copy of exactly the line's length, so a read past its end is seen. */
    uint8_t *exact = malloc(len ? len : 1);
    CHECK(exact != NULL);
    memcpy(exact, line, len);
    memset(reply, 0x7e, sizeof reply);
    dh_usb_answer a = dh_usb_handle_line(&display, exact, len, reply, DH_USB_REPLY_MAX);
    free(exact);

    /* The reply: nothing, or one line of plain ASCII JSON within the buffer. */
    for (size_t i = DH_USB_REPLY_MAX; i < sizeof reply; i++) {
        CHECK(reply[i] == 0x7e);
    }
    CHECK((a.reply_len == 0) == (a.outcome == DH_USB_IGNORED));
    CHECK(a.reply_len < DH_USB_REPLY_MAX && reply[a.reply_len] == '\0');
    if (a.reply_len > 0) {
        CHECK(reply[0] == '{' && reply[a.reply_len - 1] == '\n');
        for (size_t i = 0; i + 1 < a.reply_len; i++) {
            CHECK(reply[i] >= 0x20 && reply[i] < 0x7f);
        }
        dh_json_doc doc;
        dh_json_value root;
        CHECK(dh_json_parse(&doc, (const uint8_t *)reply, a.reply_len, NULL, &root) == DH_JSON_OK);
        CHECK(dh_json_type_of(&doc, root) == DH_JSON_OBJECT);
    }

    CHECK(dh_sha256_of(flash.setup, sizeof flash.setup, setup_after));
    switch (a.outcome) {
    case DH_USB_PROVISIONED:
        /* Only a display that wasn't set up, with working storage, and after its last write. */
        CHECK(!was_set_up && display.store != NULL && flash.has_setup);
        CHECK(!flash.fail_setup && flash.setup_writes == writes_before + 1);
        CHECK(dh_setup_valid(&work));
        CHECK(dh_setup_decode(&back, flash.setup, flash.setup_len));
        CHECK(memcmp(&back, &work, sizeof back) == 0);
        CHECK(a.error == DH_USB_ERROR_NONE && a.why == NULL);
        CHECK(record.attempts_left == DH_FW_FULL_BUDGET && record.credit_hours == 0);
        CHECK(a.keys_taken == (key_count_before == 0 && keys.key_count > 0));
        break;
    case DH_USB_REFUSED:
        CHECK(a.error != DH_USB_ERROR_NONE && dh_usb_error_name(a.error) != NULL);
        CHECK(a.why != NULL && strlen(a.why) <= 200);
        /* Refused because it is already set up exactly when it was. */
        if (display.store != NULL) {
            CHECK((a.error == DH_USB_ERROR_ALREADY_PROVISIONED) == was_set_up);
        } else {
            CHECK(a.error == DH_USB_ERROR_STORAGE_FAILED);
        }
        /* fall through */
    default:
        CHECK(flash.has_setup == was_set_up);
        CHECK(memcmp(setup_before, setup_after, sizeof setup_before) == 0);
        CHECK(memcmp(&work, &zero, sizeof zero) == 0);
        /* Keys are only ever taken by a display that had none. */
        CHECK(key_count_before == 0 || keys.key_count == key_count_before);
        break;
    }
    if (a.outcome != DH_USB_REFUSED || a.error != DH_USB_ERROR_STORAGE_FAILED) {
        /* Nothing but an accepted bundle, or one that failed part-way, writes anything. */
        CHECK(a.outcome == DH_USB_PROVISIONED || flash.setup_writes == writes_before);
    }
    if (display.store != NULL) {
        check_memory_matches_flash();
    }
    dh_setup_wipe(&work);
}

static void check_stored_form(const uint8_t *data, size_t size)
{
    static uint8_t again[DH_SETUP_BLOB_MAX];
    uint8_t *exact = malloc(size ? size : 1);
    CHECK(exact != NULL);
    memcpy(exact, data, size);
    if (dh_setup_decode(&back, exact, size)) {
        CHECK(dh_setup_valid(&back));
        CHECK(dh_setup_encode(&back, again) == size);
        CHECK(memcmp(again, exact, size) == 0);
    } else {
        CHECK(memcmp(&back, &zero, sizeof zero) == 0);
    }
    free(exact);
}

int LLVMFuzzerTestOneInput(const uint8_t *data, size_t size)
{
    if (size < 2) {
        return 0;
    }
    uint8_t how = data[0], piece_size = data[1];
    data += 2;
    size -= 2;
    unsigned mode = how & 3u;
    if (mode == 2) {
        check_stored_form(data, size);
        return 0;
    }

    /* The display, as the first byte says. */
    memset(&flash, 0, sizeof flash);
    dh_fw_keys_init(&keys);
    dh_fw_record_init(&record);
    if (how & 0x04u) {
        keys.key_count = 1;
        memcpy(keys.keys[0].bytes, MAKER, DH_FW_KEY_LEN);
        keys.security_floor = 3;
        record.attempts_left = 1;
        record.credit_hours = 2;
    }
    dh_fw_keys_encode(&keys, flash.keys);
    dh_fw_record_encode(&record, flash.record);
    flash.has_setup = (how & 0x08u) != 0;
    flash.fail_keys = (how & 0x10u) != 0;
    flash.fail_record = (how & 0x20u) != 0;
    flash.fail_setup = (how & 0x40u) != 0;
    bool working = (how & 0x80u) == 0;
    display = (dh_usb_display){"display-hub-waveshare-amoled-1.8",
                               "0.1.0",
                               "waveshare-amoled-1.8-v2",
                               6291456,
                               working ? &store : NULL,
                               working ? &keys : NULL,
                               working ? &record : NULL,
                               &work, false, NULL};
    dh_setup_wipe(&work);

    if (mode == 1 || mode == 3) {
        static const char START[] = "{\"type\":\"provision\",\"bundle\":";
        static const char GOOD[] =
            "{\"format\":\"display-hub.provisioning\",\"version\":1,\"hub\":{\"hosts\":[\"h\"],"
            "\"port\":8443,\"server_name\":\"hub.display-hub.internal\",\"trust_anchors\":["
            "{\"certificate_pem\":\"-----BEGIN CERTIFICATE-----\",\"spki_sha256\":"
            "\"9a54396d429aa58fc9ac78413a9e84a0b7a45f125256f91697c977c2acbfde88\"}]},"
            "\"enrolment\":{\"secret\":\"s\",\"expires_at\":\"2026-11-10T20:10:00Z\"},"
            "\"wifi\":null,\"firmware\":";
        size_t n = 0;
        memcpy(line_buf, START, sizeof START - 1);
        n += sizeof START - 1;
        if (mode == 3) {
            memcpy(line_buf + n, GOOD, sizeof GOOD - 1);
            n += sizeof GOOD - 1;
        }
        if (size > DH_MAX_USB_LINE_BYTES) {
            size = DH_MAX_USB_LINE_BYTES;
        }
        memcpy(line_buf + n, data, size);
        n += size;
        if (mode == 3) {
            line_buf[n++] = '}';
        }
        line_buf[n++] = '}';
        handle(line_buf, n);
        return 0;
    }

    /* Bytes from the port, in pieces. */
    size_t piece = piece_size ? piece_size : 1;
    dh_usb_line_init(&reader);
    for (size_t at = 0; at < size;) {
        size_t n = size - at < piece ? size - at : piece;
        const uint8_t *p = data + at;
        size_t left = n;
        while (left > 0) {
            size_t used = 0, len = 0;
            const uint8_t *line = NULL;
            dh_line_event e = dh_usb_line_feed(&reader, p, left, &used, &line, &len);
            CHECK(used <= left && (used > 0 || e != DH_LINE_NONE));
            p += used;
            left -= used;
            if (e == DH_LINE_JSON) {
                CHECK(len > 0 && len < DH_MAX_USB_LINE_BYTES && line[0] == '{');
                handle(line, len);
                dh_usb_line_wipe(&reader);
            }
        }
        at += n;
    }
    return 0;
}
