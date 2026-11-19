/*
 * Fuzzes everything in the signed-update core that reads bytes without needing a signature
 * first: the manifest reader, the header check, a bundle's firmware member, the stored forms of
 * the keys and the update record, and step 7's checks on whatever manifest comes out. It checks
 * that what is accepted is within every bound the rest of the core relies on.
 */
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

#include "dh_update.h"

#define CHECK(c) \
    do { \
        if (!(c)) { \
            abort(); \
        } \
    } while (0)

static dh_text_scratch scratch;
static dh_fw_manifest manifest;

static size_t bounded_length(const char *s, size_t room)
{
    size_t n = 0;
    while (n < room && s[n] != '\0') {
        n++;
    }
    return n;
}

static void check_text(const char *s, size_t room, size_t max_chars)
{
    size_t n = bounded_length(s, room);
    CHECK(n < room);
    CHECK(dh_text_check((const uint8_t *)s, n, max_chars, &scratch) == DH_TEXT_OK);
}

static void check_manifest(const uint8_t *data, size_t size)
{
    if (!dh_fw_manifest_parse(data, size, &manifest, &scratch)) {
        return;
    }
    CHECK(size >= DH_FW_MIN_MANIFEST_LEN && size <= DH_FW_MAX_MANIFEST_LEN);
    check_text(manifest.device_type, sizeof manifest.device_type, DH_FW_MAX_DEVICE_FIELD_CHARS);
    CHECK(manifest.hardware_revision_count >= 1 &&
          manifest.hardware_revision_count <= DH_FW_MAX_HARDWARE_REVISIONS);
    for (size_t i = 0; i < manifest.hardware_revision_count; i++) {
        check_text(manifest.hardware_revisions[i], sizeof manifest.hardware_revisions[i],
                   DH_FW_MAX_DEVICE_FIELD_CHARS);
        for (size_t k = 0; k < i; k++) {
            CHECK(strcmp(manifest.hardware_revisions[i], manifest.hardware_revisions[k]) != 0);
        }
    }
    size_t version_len = bounded_length(manifest.firmware_version, sizeof manifest.firmware_version);
    CHECK(version_len < sizeof manifest.firmware_version);
    CHECK(dh_fw_version_valid(manifest.firmware_version, version_len));
    CHECK(manifest.image_size >= 1 && manifest.image_size <= DH_FW_MAX_IMAGE_SIZE);
    if (manifest.has_summary) {
        check_text(manifest.summary, sizeof manifest.summary, DH_FW_MAX_SUMMARY_CHARS);
    }
    CHECK(manifest.key_count <= DH_FW_MAX_KEYS);
    CHECK(manifest.has_key_set ? manifest.key_count >= 1 : manifest.key_count == 0);
    for (uint8_t i = 0; i < manifest.key_count; i++) {
        CHECK(dh_p256_key_valid(manifest.key_set[i].bytes));
        for (uint8_t k = 0; k < i; k++) {
            CHECK(memcmp(manifest.key_set[i].bytes, manifest.key_set[k].bytes, DH_FW_KEY_LEN) != 0);
        }
    }

    /* Step 7 for a display that is what the manifest asks for, and for one that isn't. */
    dh_fw_keys keys;
    dh_fw_keys_init(&keys);
    keys.key_count = 1;
    memset(keys.keys[0].bytes, 0x04, DH_FW_KEY_LEN);
    keys.security_floor = data[size - 1];
    dh_fw_device device = {manifest.device_type, manifest.hardware_revisions[0], 1u << 20};
    uint64_t length = (uint64_t)DH_FW_HEADER_LEN + size + manifest.image_size;
    dh_fw_reason reason = dh_fw_manifest_check(&manifest, &keys, &device, (uint16_t)size, length);
    if (reason == DH_FW_REASON_NONE) {
        CHECK(manifest.security_counter >= keys.security_floor);
        CHECK(manifest.image_size <= device.slot_bytes);
        /* The display's one key isn't a point, so no key_set can keep it. */
        CHECK(!manifest.has_key_set || manifest.security_counter > keys.security_floor);
    } else {
        CHECK(reason == DH_FW_REASON_DOWNGRADE || reason == DH_FW_REASON_TOO_LARGE);
    }
    CHECK(dh_fw_manifest_check(&manifest, &keys, &device, (uint16_t)size, length + 1) !=
          DH_FW_REASON_NONE);
    device.device_type = "\x01";
    CHECK(dh_fw_manifest_check(&manifest, &keys, &device, (uint16_t)size, length) ==
          DH_FW_REASON_WRONG_DEVICE_TYPE);
}

static void check_bundle(const uint8_t *data, size_t size)
{
    dh_json_doc doc;
    dh_json_value root;
    dh_fw_bundle bundle;
    if (dh_json_parse(&doc, data, size, NULL, &root) != DH_JSON_OK ||
        !dh_fw_bundle_parse(&doc, root, &bundle)) {
        return;
    }
    CHECK(bundle.key_count >= 1 && bundle.key_count <= DH_FW_MAX_KEYS);
    dh_fw_keys keys;
    dh_fw_record record;
    unsigned save = 0;
    dh_fw_keys_init(&keys);
    dh_fw_record_init(&record);
    record.attempts_left = 0;
    dh_fw_bundle_answer answer = dh_fw_bundle_apply(&record, &keys, &bundle, &save);
    CHECK(answer == DH_FW_BUNDLE_OK || answer == DH_FW_BUNDLE_INVALID);
    if (answer == DH_FW_BUNDLE_OK) {
        CHECK(save == (DH_FW_SAVE_KEYS | DH_FW_SAVE_RECORD));
        CHECK(keys.key_count == bundle.key_count && record.attempts_left == DH_FW_FULL_BUDGET);
        CHECK(keys.security_floor == bundle.security_counter);
        /* The same bundle again names the same set. */
        CHECK(dh_fw_bundle_check(&keys, &bundle) == DH_FW_BUNDLE_OK);
    } else {
        CHECK(save == 0 && keys.key_count == 0 && record.attempts_left == 0);
    }
}

static void check_stored(const uint8_t *data, size_t size)
{
    dh_fw_keys keys;
    if (dh_fw_keys_decode(&keys, data, size)) {
        uint8_t again[DH_FW_KEYS_BLOB_LEN];
        dh_fw_keys back;
        CHECK(keys.key_count <= DH_FW_MAX_KEYS);
        dh_fw_keys_encode(&keys, again);
        CHECK(dh_fw_keys_decode(&back, again, sizeof again) && back.key_count == keys.key_count &&
              back.security_floor == keys.security_floor);
    } else {
        CHECK(keys.key_count == 0 && keys.security_floor == 0);
    }
    dh_fw_record record;
    if (!dh_fw_record_decode(&record, data, size)) {
        CHECK(record.attempts_left == DH_FW_FULL_BUDGET && record.stage == DH_FW_STAGE_IDLE);
        return;
    }
    CHECK(record.attempts_left <= DH_FW_FULL_BUDGET);
    CHECK(record.credit_hours < DH_FW_CREDIT_HOURS_PER_ATTEMPT);
    CHECK(record.attempts_left < DH_FW_FULL_BUDGET || record.credit_hours == 0);
    CHECK(record.pending_key_count <= DH_FW_MAX_KEYS);
    CHECK(bounded_length(record.last_offer_id, sizeof record.last_offer_id) <= DH_OFFER_ID_HEX_LEN);
    CHECK(bounded_length(record.result_offer_id, sizeof record.result_offer_id) <=
          DH_OFFER_ID_HEX_LEN);
    CHECK(bounded_length(record.pending_version, sizeof record.pending_version) <=
          DH_FW_MAX_VERSION_BYTES);
    uint8_t first[DH_FW_RECORD_BLOB_LEN], second[DH_FW_RECORD_BLOB_LEN];
    dh_fw_record back;
    dh_fw_record_encode(&record, first);
    CHECK(dh_fw_record_decode(&back, first, sizeof first));
    dh_fw_record_encode(&back, second);
    CHECK(memcmp(first, second, sizeof first) == 0);

    /* Whatever the record says, a start and what follows keep the keys and budget in range. */
    dh_fw_session session;
    unsigned save = 0;
    dh_fw_keys_init(&keys);
    keys.key_count = 1;
    keys.security_floor = 5;
    uint8_t budget = record.attempts_left;
    uint8_t mix = 0;
    for (size_t i = 0; i < size; i++) {
        mix = (uint8_t)(mix ^ data[i]);
    }
    dh_fw_start_result started =
        dh_fw_start(&session, &record, &keys, mix & 1, (mix & 2) != 0, &save);
    CHECK(record.attempts_left == budget && keys.security_floor >= 5);
    CHECK(keys.key_count >= 1 && keys.key_count <= DH_FW_MAX_KEYS);
    CHECK((started == DH_FW_START_TRIAL) == session.trial);
    dh_fw_report report;
    if (dh_fw_report_next(&session, &record, &report)) {
        CHECK(!session.trial && report.state != DH_FW_REPORT_NONE &&
              report.state != DH_FW_REPORT_DOWNLOADING);
        CHECK(dh_fw_report_answered(&session, &record, &report) == DH_FW_SAVE_RECORD);
        CHECK(!dh_fw_report_next(&session, &record, &report));
    }
    static const dh_fw_device device = {"d", "h", 65536};
    dh_fw_offer_decision d = dh_fw_on_offer(&session, &record, &keys, &device,
                                            "0123456789abcdef0123456789abcdef", 1000, mix);
    CHECK(record.attempts_left <= budget);
    CHECK(d.action != DH_FW_OFFER_ATTEMPT ||
          (record.attempts_left + 1 == budget && record.stage == DH_FW_STAGE_ATTEMPT));
    (void)dh_fw_marked_good(&session, &record, &keys);
    CHECK(keys.key_count >= 1 && keys.key_count <= DH_FW_MAX_KEYS && keys.security_floor >= 5);
    /* However long it runs, the budget stops at full. */
    (void)dh_fw_tick(&session, &record, UINT64_MAX);
    CHECK(record.attempts_left == DH_FW_FULL_BUDGET && record.credit_hours == 0);
}

int LLVMFuzzerTestOneInput(const uint8_t *data, size_t size)
{
    check_manifest(data, size);
    check_bundle(data, size);
    if (size >= DH_FW_HEADER_LEN) {
        uint16_t len = 0;
        if (dh_fw_header_check(data, &len)) {
            CHECK(len >= DH_FW_MIN_MANIFEST_LEN && len <= DH_FW_MAX_MANIFEST_LEN);
        }
    }
    if (size == DH_FW_KEYS_BLOB_LEN || size == DH_FW_RECORD_BLOB_LEN) {
        check_stored(data, size);
    }
    (void)dh_fw_version_valid((const char *)data, size);
    return 0;
}
