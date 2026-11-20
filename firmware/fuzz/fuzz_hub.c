/*
 * Fuzzes what a display reads from the hub outside a snapshot, and what it keeps of it (dh_hub).
 * The first byte chooses what the rest is:
 *
 *   0  the body of an enrolment response, with a success status;
 *   1  the body of an error answer; the next two bytes are the status;
 *   2  a stored device ID and token;
 *   3  an enrolment secret, sent in a request;
 *   4  two snapshot bodies to compare; the next two bytes are the first one's length.
 *
 * It checks that whatever is accepted keeps every rule the rest of the firmware relies on: an ID
 * and token of plain ASCII within their lengths, a stored form that reads back the same, a
 * request that is JSON holding the secret it was given, and error answers grouped the same way
 * here as for a snapshot request.
 */
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

#include "dh_hub.h"
#include "dh_json.h"

#define CHECK(c) \
    do { \
        if (!(c)) { \
            abort(); \
        } \
    } while (0)

static void check_wiped(const dh_device *device)
{
    static const dh_device wiped;
    CHECK(memcmp(device, &wiped, sizeof wiped) == 0);
}

static void check_device(const dh_device *device)
{
    CHECK(dh_device_valid(device));
    CHECK(device->id_len >= 1 && device->id_len <= DH_DEVICE_MAX_ID_BYTES);
    CHECK(device->token_len >= 1 && device->token_len <= DH_DEVICE_MAX_TOKEN_BYTES);
    CHECK(strlen(device->id) == device->id_len && strlen(device->token) == device->token_len);
    for (size_t i = 0; i < device->token_len; i++) {
        CHECK(device->token[i] > ' ' && device->token[i] < 0x7F);
    }
    for (size_t i = 0; i < device->id_len; i++) {
        CHECK(device->id[i] > ' ' && device->id[i] < 0x7F);
    }

    /* The stored form reads back the same, and the header is the token and nothing else. */
    uint8_t blob[DH_DEVICE_BLOB_MAX];
    dh_device back;
    size_t n = dh_device_encode(device, blob);
    CHECK(n == 3u + device->id_len + device->token_len);
    CHECK(dh_device_decode(&back, blob, n));
    CHECK(memcmp(&back, device, sizeof back) == 0);
    char header[DH_AUTHORIZATION_MAX];
    size_t h = dh_authorization(device, header, sizeof header);
    CHECK(h == 7u + device->token_len && header[h] == '\0');
    CHECK(memcmp(header, "Bearer ", 7) == 0 && strcmp(header + 7, device->token) == 0);
}

static void enrol_response(const uint8_t *data, size_t size)
{
    dh_device device;
    memset(&device, 0xA5, sizeof device);
    dh_enrol_answer a = dh_read_enrol_response(201, data, size, &device);
    if (a.enrolled) {
        CHECK(size <= DH_MAX_HUB_ANSWER_BYTES);
        check_device(&device);
    } else {
        CHECK(a.response.outcome == DH_OUTCOME_UNREADABLE);
        CHECK(a.response.unreadable != DH_UNREADABLE_NONE);
        check_wiped(&device);
    }
}

static void error_response(const uint8_t *data, size_t size)
{
    if (size < 2) {
        return;
    }
    int status = (data[0] << 8) | data[1];
    data += 2;
    size -= 2;
    dh_response r = dh_read_error_response(status, data, size);
    CHECK(r.outcome != DH_OUTCOME_SHOW && r.outcome != DH_OUTCOME_UNREADABLE);
    CHECK(r.unreadable == DH_UNREADABLE_NONE);
    if (r.error_code == DH_ERROR_NONE) {
        CHECK(r.outcome == (status >= 500 ? DH_OUTCOME_RETRY : DH_OUTCOME_FAULT));
    }
    if (status != 200 && size <= DH_MAX_HUB_ANSWER_BYTES) {
        /* Grouped exactly as a snapshot request's error is. */
        static dh_snapshot snapshot;
        static dh_text_scratch scratch;
        dh_response same = dh_read_response(status, data, size, NULL, &snapshot, &scratch);
        CHECK(same.outcome == r.outcome && same.next_poll == r.next_poll &&
              same.error_code == r.error_code);
    }
    /* A refusal of an enrolment is the same answer, and keeps nothing. */
    if (status < 200 || status > 299) {
        dh_device device;
        memset(&device, 0xA5, sizeof device);
        dh_enrol_answer a = dh_read_enrol_response(status, data, size, &device);
        CHECK(!a.enrolled && a.response.outcome == r.outcome &&
              a.response.error_code == r.error_code);
        check_wiped(&device);
    }
}

static void stored_device(const uint8_t *data, size_t size)
{
    dh_device device;
    memset(&device, 0xA5, sizeof device);
    if (dh_device_decode(&device, data, size)) {
        check_device(&device);
        uint8_t blob[DH_DEVICE_BLOB_MAX];
        CHECK(dh_device_encode(&device, blob) == size && memcmp(blob, data, size) == 0);
    } else {
        check_wiped(&device);
    }
}

static void request(const uint8_t *data, size_t size)
{
    static const dh_device_info info = {
        .device_type = "display-hub-waveshare-amoled-1.8",
        .firmware_version = "0.1.0",
        .hardware_revision = "waveshare-amoled-1.8-v2",
    };
    char out[DH_HUB_REQUEST_MAX + 1];
    out[DH_HUB_REQUEST_MAX] = 'Z';
    size_t n = dh_enrol_request(&info, (const char *)data, size, out, DH_HUB_REQUEST_MAX);
    CHECK(out[DH_HUB_REQUEST_MAX] == 'Z');
    if (size == 0 || size > DH_SETUP_MAX_SECRET_BYTES) {
        CHECK(n == 0);
        return;
    }
    /* A secret a bundle could hold always fits. */
    CHECK(n > 0 && n < DH_HUB_REQUEST_MAX && out[n] == '\0' && strlen(out) == n);
    dh_json_doc doc;
    dh_json_value root, secret;
    if (dh_json_parse(&doc, (const uint8_t *)out, n, NULL, &root) == DH_JSON_OK) {
        CHECK(dh_json_object_find(&doc, root, "enrolment_secret", 16, &secret) == 1);
        CHECK(dh_json_string_equals(&doc, secret, (const char *)data, size));
    }
    /* Otherwise the secret wasn't UTF-8, which a stored setup never holds. */
}

static void bodies(const uint8_t *data, size_t size)
{
    if (size < 2) {
        return;
    }
    size_t a_len = ((size_t)data[0] << 8) | data[1];
    data += 2;
    size -= 2;
    if (a_len > size) {
        a_len = size;
    }
    const uint8_t *a = data, *b = data + a_len;
    size_t b_len = size - a_len;
    bool ab = dh_snapshot_bodies_match(a, a_len, b, b_len);
    CHECK(ab == dh_snapshot_bodies_match(b, b_len, a, a_len));
    if (ab) {
        /* Each then matches itself too. */
        CHECK(dh_snapshot_bodies_match(a, a_len, a, a_len));
        CHECK(dh_snapshot_bodies_match(b, b_len, b, b_len));
    }
    if (a_len == b_len && a_len > 0 && memcmp(a, b, a_len) == 0) {
        CHECK(ab == dh_snapshot_bodies_match(a, a_len, a, a_len));
    }
}

int LLVMFuzzerTestOneInput(const uint8_t *data, size_t size)
{
    if (size < 1) {
        return 0;
    }
    uint8_t kind = data[0];
    data++;
    size--;
    switch (kind % 5u) {
    case 0:
        enrol_response(data, size);
        break;
    case 1:
        error_response(data, size);
        break;
    case 2:
        stored_device(data, size);
        break;
    case 3:
        request(data, size);
        break;
    default:
        bodies(data, size);
        break;
    }
    return 0;
}
