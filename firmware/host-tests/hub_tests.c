/*
 * Unit tests for talking to the hub (dh_hub): the enrolment request and the heartbeat, the
 * enrolment response and error answers, the device ID and token and their stored form, which
 * address and anchor to try next, when a heartbeat is due and when a snapshot is worth saving.
 * The heartbeat cases are the reference display's own (tools/display-simulator/src/main.rs).
 */
#include <inttypes.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "dh_hub.h"
#include "dh_json.h"

static int checks, failures;

#define CHECK(c) \
    do { \
        checks++; \
        if (!(c)) { \
            failures++; \
            fprintf(stderr, "%s:%d: CHECK(%s) failed\n", __FILE__, __LINE__, #c); \
        } \
    } while (0)

/* A failure found inside a loop, reported once with its own words. */
#define FAIL(what) \
    do { \
        checks++; \
        failures++; \
        fprintf(stderr, "%s:%d: %s\n", __FILE__, __LINE__, what); \
    } while (0)

#define LEN(a) (sizeof(a) / sizeof((a)[0]))
#define LIT(s) s, (sizeof s - 1u)
#define BODY(s) (const uint8_t *)(s), (sizeof s - 1u)

/* A public key from protocol/v1/firmware's vectors, and its id (the SHA-256 of its 65 bytes). */
#define MAKER \
    "04795c666d0cdda58bfb27c60b936bba8ad0c8801218dcb6d5f50a73c00a9904229fa366102a70677d0ca541" \
    "0d6300b8273e102bba8920cef822483339734b7eb3"

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

static uint8_t file_buf[65536];

static size_t read_example(const char *name)
{
    char path[1024];
    snprintf(path, sizeof path, "%s/examples/device/%s", DH_PROTOCOL_DIR, name);
    FILE *f = fopen(path, "rb");
    if (f == NULL) {
        fprintf(stderr, "can't read %s\n", path);
        exit(1);
    }
    size_t n = fread(file_buf, 1, sizeof file_buf, f);
    fclose(f);
    return n;
}

/* Whether member `name` of obj is there once, as a string equal to want. */
static bool member_is(const dh_json_doc *doc, dh_json_value obj, const char *name,
                      const char *want)
{
    dh_json_value v;
    return dh_json_object_find(doc, obj, name, strlen(name), &v) == 1 &&
           dh_json_type_of(doc, v) == DH_JSON_STRING &&
           dh_json_string_equals(doc, v, want, strlen(want));
}

static bool member_number(const dh_json_doc *doc, dh_json_value obj, const char *name,
                          uint64_t want)
{
    dh_json_value v;
    uint64_t got = 0;
    return dh_json_object_find(doc, obj, name, strlen(name), &v) == 1 &&
           dh_json_type_of(doc, v) == DH_JSON_NUMBER && dh_json_number_u64(doc, v, &got) &&
           got == want;
}

static const dh_device_info PLAIN_INFO = {
    .device_type = "llm-usage-display",
    .firmware_version = "1.0.0",
    .hardware_revision = "waveshare-amoled-1.8-v2",
};

static void test_requests(void)
{
    char out[DH_HUB_REQUEST_MAX];

    /* The protocol's own example, written compactly. */
    size_t n = dh_enrol_request(&PLAIN_INFO, LIT("q3H7cX0f2pS9L1mB8vN4wA"), out, sizeof out);
    const char *want =
        "{\"enrolment_secret\":\"q3H7cX0f2pS9L1mB8vN4wA\",\"device\":{"
        "\"device_type\":\"llm-usage-display\",\"firmware_version\":\"1.0.0\","
        "\"hardware_revision\":\"waveshare-amoled-1.8-v2\",\"protocol_version\":1}}";
    CHECK(n == strlen(want) && strcmp(out, want) == 0);

    n = dh_heartbeat_request(&PLAIN_INFO, out, sizeof out);
    want = "{\"device_type\":\"llm-usage-display\",\"firmware_version\":\"1.0.0\","
           "\"hardware_revision\":\"waveshare-amoled-1.8-v2\",\"protocol_version\":1}";
    CHECK(n == strlen(want) && strcmp(out, want) == 0);

    /* With the firmware member, as a display that takes signed updates sends it. */
    dh_fw_keys keys;
    dh_fw_record record;
    dh_fw_keys_init(&keys);
    dh_fw_record_init(&record);
    keys.key_count = 1;
    unhex(MAKER, keys.keys[0].bytes, DH_FW_KEY_LEN);
    keys.security_floor = 3;
    record.attempts_left = 4;
    char id[DH_PACKAGE_ID_HEX_LEN + 1];
    CHECK(dh_fw_key_id_hex(&keys.keys[0], id));
    dh_device_info info = PLAIN_INFO;
    info.slot_bytes = 6291456;
    info.keys = &keys;
    info.record = &record;
    n = dh_heartbeat_request(&info, out, sizeof out);
    dh_json_doc doc;
    dh_json_value root = 0, fw = 0, ids = 0, first = 0;
    dh_json_iter it;
    dh_json_options strict = {0, 0, DH_JSON_NO_DUPLICATES};
    CHECK(n > 0 && dh_json_parse(&doc, (const uint8_t *)out, n, &strict, &root) == DH_JSON_OK);
    CHECK(member_is(&doc, root, "device_type", "llm-usage-display"));
    CHECK(member_number(&doc, root, "protocol_version", 1));
    CHECK(dh_json_object_find(&doc, root, LIT("firmware"), &fw) == 1);
    CHECK(member_number(&doc, fw, "security_floor", 3));
    CHECK(member_number(&doc, fw, "slot_bytes", 6291456));
    CHECK(member_number(&doc, fw, "attempts_left", 4));
    CHECK(dh_json_object_find(&doc, fw, LIT("key_ids"), &ids) == 1 &&
          dh_json_array_length(&doc, ids) == 1 && dh_json_array_begin(&doc, ids, &it) &&
          dh_json_array_next(&doc, &it, &first) &&
          dh_json_string_equals(&doc, first, id, DH_PACKAGE_ID_HEX_LEN));

    /* The same member in an enrolment request, inside `device`. */
    n = dh_enrol_request(&info, LIT("s"), out, sizeof out);
    dh_json_value device = 0;
    CHECK(n > 0 && dh_json_parse(&doc, (const uint8_t *)out, n, &strict, &root) == DH_JSON_OK);
    CHECK(member_is(&doc, root, "enrolment_secret", "s"));
    CHECK(dh_json_object_find(&doc, root, LIT("device"), &device) == 1 &&
          dh_json_object_find(&doc, device, LIT("firmware"), &fw) == 1);

    /* No keys is still a firmware member, with an empty list. A record that makes no sense, or
     * no record at all, leaves the member out. */
    keys.key_count = 0;
    n = dh_heartbeat_request(&info, out, sizeof out);
    CHECK(n > 0 && strstr(out, "\"firmware\":{\"key_ids\":[],") != NULL);
    record.attempts_left = 6;
    n = dh_heartbeat_request(&info, out, sizeof out);
    CHECK(n > 0 && strstr(out, "firmware\"") == NULL);
    info.record = NULL;
    n = dh_heartbeat_request(&info, out, sizeof out);
    CHECK(n > 0 && strstr(out, "firmware\"") == NULL);

    /* A secret may be any text: quotes, a backslash, control characters, a NUL, and characters
     * of several bytes all come back out of the JSON as they went in. */
    static const char odd[] = "a\"b\\c\n\x01\x7f\0z \xc3\xa9\xf0\x9f\x98\x80";
    n = dh_enrol_request(&PLAIN_INFO, odd, sizeof odd - 1, out, sizeof out);
    dh_json_value secret = 0;
    CHECK(n > 0 && dh_json_parse(&doc, (const uint8_t *)out, n, &strict, &root) == DH_JSON_OK);
    CHECK(dh_json_object_find(&doc, root, LIT("enrolment_secret"), &secret) == 1 &&
          dh_json_string_equals(&doc, secret, odd, sizeof odd - 1));

    /* The longest of everything fits, with room to spare. */
    char longest[DH_SETUP_MAX_SECRET_BYTES];
    memset(longest, 0x01, sizeof longest);
    char field[49];
    memset(field, 'x', 48);
    field[48] = '\0';
    dh_device_info big = {field, field, field, UINT32_MAX, &keys, &record};
    keys.key_count = 3;
    unhex(MAKER, keys.keys[1].bytes, DH_FW_KEY_LEN);
    unhex(MAKER, keys.keys[2].bytes, DH_FW_KEY_LEN);
    keys.security_floor = UINT32_MAX;
    record.attempts_left = 5;
    n = dh_enrol_request(&big, longest, sizeof longest, out, sizeof out);
    CHECK(n > 0 && n < DH_HUB_REQUEST_MAX - 64 && out[n] == '\0');
    CHECK(dh_json_parse(&doc, (const uint8_t *)out, n, NULL, &root) == DH_JSON_OK);

    /* Too small a buffer gives nothing, and leaves none of the secret behind. */
    for (size_t cap = 0; cap <= n; cap++) {
        memset(out, 'Z', sizeof out);
        size_t got = dh_enrol_request(&big, longest, sizeof longest, out, cap);
        bool clean = true;
        for (size_t i = 0; i < cap; i++) {
            clean = clean && out[i] == '\0';
        }
        if (got != 0 || !clean || out[cap] != 'Z') {
            FAIL("a body that doesn't fit must give 0 and a wiped buffer");
            break;
        }
    }
    CHECK(dh_enrol_request(&big, longest, sizeof longest, out, n + 1) == n);
    /* No secret, or one longer than a bundle can hold, is never sent. */
    CHECK(dh_enrol_request(&PLAIN_INFO, "", 0, out, sizeof out) == 0);
    CHECK(dh_enrol_request(&PLAIN_INFO, longest, 65, out, sizeof out) == 0);
}

static void test_enrol_response(void)
{
    dh_device device;
    dh_enrol_answer a;

    /* The protocol's example. */
    size_t n = read_example("enrol-response.json");
    a = dh_read_enrol_response(201, file_buf, n, &device);
    CHECK(a.enrolled && dh_device_valid(&device));
    CHECK(strcmp(device.id, "d-3fa91c07b2e4") == 0 && device.id_len == 14);
    CHECK(strcmp(device.token, "dhd_EXAMPLEexampleEXAMPLEexampleEXAMPLEexample0") == 0 &&
          device.token_len == 47);

    /* Any success status, as the reference reads it; unknown members are ignored. */
    static const int good[] = {200, 201, 204, 299};
    for (size_t i = 0; i < LEN(good); i++) {
        a = dh_read_enrol_response(
            good[i], BODY("{\"later\":[1,{}],\"device_id\":\"d-1\",\"device_token\":\"t\"}"),
            &device);
        CHECK(a.enrolled && device.id_len == 3 && device.token_len == 1);
    }

    /* Bodies that aren't an enrolment response: nothing is kept. */
    static const struct {
        const char *body;
        dh_unreadable why;
    } bad[] = {
        {"", DH_UNREADABLE_NOT_JSON},
        {"<html>", DH_UNREADABLE_NOT_JSON},
        {"{\"device_id\":\"d-1\",\"device_token\":\"t\"} x", DH_UNREADABLE_NOT_JSON},
        {"[]", DH_UNREADABLE_MALFORMED},
        {"null", DH_UNREADABLE_MALFORMED},
        {"{}", DH_UNREADABLE_MALFORMED},
        {"{\"device_id\":\"d-1\"}", DH_UNREADABLE_MALFORMED},
        {"{\"device_token\":\"t\"}", DH_UNREADABLE_MALFORMED},
        {"{\"device_id\":1,\"device_token\":\"t\"}", DH_UNREADABLE_MALFORMED},
        {"{\"device_id\":\"d-1\",\"device_token\":null}", DH_UNREADABLE_MALFORMED},
        {"{\"device_id\":\"d-1\",\"device_id\":\"d-1\",\"device_token\":\"t\"}",
         DH_UNREADABLE_MALFORMED},
        {"{\"device_id\":\"d-1\",\"device_token\":\"t\",\"device_token\":\"t\"}",
         DH_UNREADABLE_MALFORMED},
        {"{\"device_id\":\"\",\"device_token\":\"t\"}", DH_UNREADABLE_MALFORMED},
        {"{\"device_id\":\"d-1\",\"device_token\":\"\"}", DH_UNREADABLE_MALFORMED},
        /* Not plain ASCII without spaces: a space, a line break (a header could be split with
         * one), a NUL, DEL and a character beyond ASCII. */
        {"{\"device_id\":\"d 1\",\"device_token\":\"t\"}", DH_UNREADABLE_INVALID},
        {"{\"device_id\":\"d-1\",\"device_token\":\"a b\"}", DH_UNREADABLE_INVALID},
        {"{\"device_id\":\"d-1\",\"device_token\":\"a\\r\\nX-Evil: 1\"}", DH_UNREADABLE_INVALID},
        {"{\"device_id\":\"d-1\",\"device_token\":\"a\\u0000b\"}", DH_UNREADABLE_INVALID},
        {"{\"device_id\":\"d-1\",\"device_token\":\"a\\u007f\"}", DH_UNREADABLE_INVALID},
        {"{\"device_id\":\"d-\xc3\xa9\",\"device_token\":\"t\"}", DH_UNREADABLE_INVALID},
    };
    for (size_t i = 0; i < LEN(bad); i++) {
        memset(&device, 0x55, sizeof device);
        a = dh_read_enrol_response(201, (const uint8_t *)bad[i].body, strlen(bad[i].body),
                                   &device);
        static const dh_device wiped;
        bool ok = !a.enrolled && a.response.outcome == DH_OUTCOME_UNREADABLE &&
                  a.response.next_poll == DH_NEXT_BACKOFF && a.response.unreadable == bad[i].why &&
                  memcmp(&device, &wiped, sizeof device) == 0;
        if (!ok) {
            fprintf(stderr, "enrol response %zu: %s\n", i, bad[i].body);
        }
        CHECK(ok);
    }

    /* The longest ID and token are taken, and one byte more isn't. */
    char body[512];
    char id[DH_DEVICE_MAX_ID_BYTES + 2], token[DH_DEVICE_MAX_TOKEN_BYTES + 2];
    memset(id, 'i', sizeof id);
    memset(token, '~', sizeof token);
    for (int extra_id = 0; extra_id <= 1; extra_id++) {
        for (int extra_token = 0; extra_token <= 1; extra_token++) {
            int len = snprintf(body, sizeof body, "{\"device_id\":\"%.*s\",\"device_token\":\"%.*s\"}",
                               (int)DH_DEVICE_MAX_ID_BYTES + extra_id, id,
                               (int)DH_DEVICE_MAX_TOKEN_BYTES + extra_token, token);
            a = dh_read_enrol_response(201, (const uint8_t *)body, (size_t)len, &device);
            CHECK(a.enrolled == (extra_id == 0 && extra_token == 0));
        }
    }

    /* A body over the limit is refused before it is parsed, even if it would read. */
    static uint8_t big[DH_MAX_HUB_ANSWER_BYTES + 1];
    static const char start[] = "{\"device_id\":\"d-1\",\"device_token\":\"t\"}";
    memset(big, ' ', sizeof big);
    memcpy(big, start, sizeof start - 1);
    a = dh_read_enrol_response(201, big, DH_MAX_HUB_ANSWER_BYTES, &device);
    CHECK(a.enrolled);
    a = dh_read_enrol_response(201, big, DH_MAX_HUB_ANSWER_BYTES + 1, &device);
    CHECK(!a.enrolled && a.response.unreadable == DH_UNREADABLE_TOO_LARGE && device.id_len == 0);

    /* A refusal: the two a hub gives at this route, and the rest of the groups. */
    static const struct {
        int status;
        const char *code;
        dh_outcome outcome;
        dh_next_poll next;
        dh_error_code error;
    } errors[] = {
        {409, "enrolment_used", DH_OUTCOME_REPROVISION, DH_NEXT_HOURLY, DH_ERROR_ENROLMENT_USED},
        {401, "enrolment_expired", DH_OUTCOME_REPROVISION, DH_NEXT_HOURLY,
         DH_ERROR_ENROLMENT_EXPIRED},
        {401, "token_revoked", DH_OUTCOME_REPROVISION, DH_NEXT_HOURLY, DH_ERROR_TOKEN_REVOKED},
        {401, "unknown_device", DH_OUTCOME_REPROVISION, DH_NEXT_HOURLY, DH_ERROR_UNKNOWN_DEVICE},
        {426, "incompatible_version", DH_OUTCOME_INCOMPATIBLE, DH_NEXT_HOURLY,
         DH_ERROR_INCOMPATIBLE_VERSION},
        {429, "rate_limited", DH_OUTCOME_RETRY, DH_NEXT_BACKOFF, DH_ERROR_RATE_LIMITED},
        {503, "unavailable", DH_OUTCOME_RETRY, DH_NEXT_BACKOFF, DH_ERROR_UNAVAILABLE},
        {418, "a_code_from_a_newer_hub", DH_OUTCOME_RETRY, DH_NEXT_BACKOFF, DH_ERROR_UNRECOGNISED},
        {401, "unauthorised", DH_OUTCOME_FAULT, DH_NEXT_BACKOFF, DH_ERROR_UNAUTHORISED},
        {403, "forbidden", DH_OUTCOME_FAULT, DH_NEXT_BACKOFF, DH_ERROR_FORBIDDEN},
        {400, "bad_request", DH_OUTCOME_FAULT, DH_NEXT_BACKOFF, DH_ERROR_BAD_REQUEST},
        {413, "payload_too_large", DH_OUTCOME_FAULT, DH_NEXT_BACKOFF, DH_ERROR_PAYLOAD_TOO_LARGE},
        {404, "not_found", DH_OUTCOME_FAULT, DH_NEXT_BACKOFF, DH_ERROR_NOT_FOUND},
        {409, "conflict", DH_OUTCOME_FAULT, DH_NEXT_BACKOFF, DH_ERROR_CONFLICT},
    };
    for (size_t i = 0; i < LEN(errors); i++) {
        int len = snprintf(body, sizeof body, "{\"error\":{\"code\":\"%s\",\"message\":\"m\"}}",
                           errors[i].code);
        memset(&device, 0x55, sizeof device);
        a = dh_read_enrol_response(errors[i].status, (const uint8_t *)body, (size_t)len, &device);
        CHECK(!a.enrolled && a.response.outcome == errors[i].outcome &&
              a.response.next_poll == errors[i].next && a.response.error_code == errors[i].error &&
              device.id_len == 0 && device.token_len == 0);
        /* The same from any route, and the same as a snapshot request's error. */
        dh_response r = dh_read_error_response(errors[i].status, (const uint8_t *)body, (size_t)len);
        CHECK(r.outcome == errors[i].outcome && r.error_code == errors[i].error);
    }

    /* A status outside 2xx is never success, whatever the body says. */
    static const int not_success[] = {0, 100, 199, 300, 302, 401, 500, -1};
    for (size_t i = 0; i < LEN(not_success); i++) {
        a = dh_read_enrol_response(not_success[i],
                                   BODY("{\"device_id\":\"d-1\",\"device_token\":\"t\"}"), &device);
        CHECK(!a.enrolled && a.response.outcome != DH_OUTCOME_SHOW && device.token_len == 0);
    }
}

static void test_error_response(void)
{
    /* The protocol's example. */
    size_t n = read_example("error-revoked.json");
    dh_response r = dh_read_error_response(401, file_buf, n);
    CHECK(r.outcome == DH_OUTCOME_REPROVISION && r.error_code == DH_ERROR_TOKEN_REVOKED &&
          r.next_poll == DH_NEXT_HOURLY);

    /* No body in the error form: a server error passes, anything else is a fault. A redirect is
     * never followed, so it lands here too. */
    r = dh_read_error_response(503, BODY("<html>Service unavailable</html>"));
    CHECK(r.outcome == DH_OUTCOME_RETRY && r.error_code == DH_ERROR_NONE);
    r = dh_read_error_response(500, NULL, 0);
    CHECK(r.outcome == DH_OUTCOME_RETRY);
    r = dh_read_error_response(404, BODY("Not found"));
    CHECK(r.outcome == DH_OUTCOME_FAULT && r.next_poll == DH_NEXT_BACKOFF);
    r = dh_read_error_response(302, NULL, 0);
    CHECK(r.outcome == DH_OUTCOME_FAULT);
    r = dh_read_error_response(401, BODY("{\"error\":{\"code\":\"token_revoked\"}}"));
    CHECK(r.outcome == DH_OUTCOME_FAULT && r.error_code == DH_ERROR_NONE);
    /* Even a 200 is never "show" here: this isn't the snapshot route. */
    r = dh_read_error_response(200, BODY("{}"));
    CHECK(r.outcome == DH_OUTCOME_FAULT);

    /* At the limit the body is read; a byte over, it counts as no body. */
    static uint8_t big[DH_MAX_HUB_ANSWER_BYTES + 1];
    static const char start[] = "{\"error\":{\"code\":\"token_revoked\",\"message\":\"m\"}}";
    memset(big, ' ', sizeof big);
    memcpy(big, start, sizeof start - 1);
    r = dh_read_error_response(401, big, DH_MAX_HUB_ANSWER_BYTES);
    CHECK(r.outcome == DH_OUTCOME_REPROVISION);
    r = dh_read_error_response(401, big, DH_MAX_HUB_ANSWER_BYTES + 1);
    CHECK(r.outcome == DH_OUTCOME_FAULT && r.error_code == DH_ERROR_NONE);
    r = dh_read_error_response(502, big, DH_MAX_HUB_ANSWER_BYTES + 1);
    CHECK(r.outcome == DH_OUTCOME_RETRY);

    CHECK(dh_heartbeat_taken(204) && dh_heartbeat_taken(200) && dh_heartbeat_taken(299));
    CHECK(!dh_heartbeat_taken(199) && !dh_heartbeat_taken(300) && !dh_heartbeat_taken(401) &&
          !dh_heartbeat_taken(0));
}

static void test_device(void)
{
    dh_device device, back;
    uint8_t blob[DH_DEVICE_BLOB_MAX + 1];
    static const dh_device wiped;

    dh_read_enrol_response(201,
                           BODY("{\"device_id\":\"d-3fa91c07b2e4\",\"device_token\":"
                                "\"dhd_EXAMPLEexampleEXAMPLEexampleEXAMPLEexample0\"}"),
                           &device);
    size_t n = dh_device_encode(&device, blob);
    CHECK(n == 3 + 14 + 47);
    CHECK(dh_device_decode(&back, blob, n) && memcmp(&back, &device, sizeof device) == 0);

    /* Every shorter length, one byte more, another format and a bad character are refused, and
     * leave nothing behind. */
    for (size_t cut = 0; cut < n; cut++) {
        memset(&back, 0x55, sizeof back);
        if (dh_device_decode(&back, blob, cut) || memcmp(&back, &wiped, sizeof back) != 0) {
            FAIL("a cut-short device blob must be refused");
            break;
        }
    }
    blob[n] = 'x';
    CHECK(!dh_device_decode(&back, blob, n + 1));
    blob[0] = 2;
    CHECK(!dh_device_decode(&back, blob, n));
    blob[0] = 1;
    blob[5] = ' ';
    CHECK(!dh_device_decode(&back, blob, n) && memcmp(&back, &wiped, sizeof back) == 0);
    blob[5] = 'f';
    blob[n - 1] = '\n';
    CHECK(!dh_device_decode(&back, blob, n));
    /* Empty texts. */
    static const uint8_t no_id[] = {1, 0, 1, 't'}, no_token[] = {1, 1, 'i', 0};
    CHECK(!dh_device_decode(&back, no_id, sizeof no_id));
    CHECK(!dh_device_decode(&back, no_token, sizeof no_token));

    /* The longest of each. */
    memset(&device, 0, sizeof device);
    device.id_len = DH_DEVICE_MAX_ID_BYTES;
    memset(device.id, 'a', DH_DEVICE_MAX_ID_BYTES);
    device.token_len = DH_DEVICE_MAX_TOKEN_BYTES;
    memset(device.token, 'b', DH_DEVICE_MAX_TOKEN_BYTES);
    CHECK(dh_device_valid(&device));
    n = dh_device_encode(&device, blob);
    CHECK(n == DH_DEVICE_BLOB_MAX && dh_device_decode(&back, blob, n) &&
          memcmp(&back, &device, sizeof device) == 0);

    char header[DH_AUTHORIZATION_MAX + 1];
    memset(header, 'Z', sizeof header);
    CHECK(dh_authorization(&device, header, DH_AUTHORIZATION_MAX) == 7 + DH_DEVICE_MAX_TOKEN_BYTES);
    CHECK(strncmp(header, "Bearer bbbb", 11) == 0 && header[DH_AUTHORIZATION_MAX - 1] == '\0' &&
          header[DH_AUTHORIZATION_MAX] == 'Z');
    CHECK(dh_authorization(&device, header, DH_AUTHORIZATION_MAX - 1) == 0 && header[0] == '\0');

    /* A device that breaks a rule is never encoded or put in a header. */
    device.token[3] = ' ';
    CHECK(!dh_device_valid(&device) && dh_device_encode(&device, blob) == 0);
    CHECK(dh_authorization(&device, header, sizeof header) == 0 && header[0] == '\0');
    device.token[3] = 'b';
    device.token_len = 0;
    CHECK(!dh_device_valid(&device) && dh_device_encode(&device, blob) == 0);
    device.token_len = DH_DEVICE_MAX_TOKEN_BYTES;
    device.id_len = DH_DEVICE_MAX_ID_BYTES + 1;
    CHECK(!dh_device_valid(&device));

    dh_device_wipe(&device);
    CHECK(memcmp(&device, &wiped, sizeof device) == 0);
}

/* Runs a walk where try number i (from 0) gives results[i], and writes the tries made as
 * "host.anchor " into out. */
static dh_problem walk(uint8_t hosts, uint8_t anchors, const dh_try_result *results, size_t count,
                       char *out, size_t cap)
{
    dh_hub_walk w;
    uint8_t host = 0xFF, anchor = 0xFF;
    size_t tries = 0, at = 0;
    out[0] = '\0';
    dh_hub_walk_begin(&w, hosts, anchors);
    while (dh_hub_walk_next(&w, &host, &anchor)) {
        if (tries >= count || host >= hosts || anchor >= anchors) {
            snprintf(out, cap, "ran on");
            return DH_PROBLEM_NONE;
        }
        at += (size_t)snprintf(out + at, cap - at, "%u.%u ", (unsigned)host, (unsigned)anchor);
        dh_hub_walk_tried(&w, results[tries++]);
    }
    /* Once it has ended it stays ended. */
    CHECK(!dh_hub_walk_next(&w, &host, &anchor));
    return dh_hub_walk_problem(&w);
}

static void test_walk(void)
{
    char tries[128];
    const dh_try_result N = DH_TRY_NO_ANSWER, W = DH_TRY_WRONG_HUB, A = DH_TRY_ANSWERED;

    /* The first address answers: nothing else is tried. */
    CHECK(walk(4, 2, (dh_try_result[]){A}, 1, tries, sizeof tries) == DH_PROBLEM_NONE);
    CHECK(strcmp(tries, "0.0 ") == 0);
    /* No answer: straight on to the next address, without trying the other anchor. */
    CHECK(walk(3, 2, (dh_try_result[]){N, N, A}, 3, tries, sizeof tries) == DH_PROBLEM_NONE);
    CHECK(strcmp(tries, "0.0 1.0 2.0 ") == 0);
    /* A certificate that doesn't match: the other anchor, at the same address. */
    CHECK(walk(2, 2, (dh_try_result[]){W, A}, 2, tries, sizeof tries) == DH_PROBLEM_NONE);
    CHECK(strcmp(tries, "0.0 0.1 ") == 0);
    /* Neither anchor matches: the next address, from the first anchor again. */
    CHECK(walk(2, 2, (dh_try_result[]){W, W, W, A}, 4, tries, sizeof tries) == DH_PROBLEM_NONE);
    CHECK(strcmp(tries, "0.0 0.1 1.0 1.1 ") == 0);

    /* Nothing answers anywhere. */
    CHECK(walk(4, 2, (dh_try_result[]){N, N, N, N}, 4, tries, sizeof tries) ==
          DH_PROBLEM_UNREACHABLE);
    CHECK(strcmp(tries, "0.0 1.0 2.0 3.0 ") == 0);
    /* Something else answers everywhere. */
    CHECK(walk(2, 1, (dh_try_result[]){W, W}, 2, tries, sizeof tries) == DH_PROBLEM_WRONG_HUB);
    CHECK(strcmp(tries, "0.0 1.0 ") == 0);
    /* The last address decides, as in the reference. */
    CHECK(walk(2, 1, (dh_try_result[]){W, N}, 2, tries, sizeof tries) == DH_PROBLEM_UNREACHABLE);
    CHECK(walk(2, 1, (dh_try_result[]){N, W}, 2, tries, sizeof tries) == DH_PROBLEM_WRONG_HUB);
    /* Within one address, a mismatch isn't forgotten because the second anchor's try got no
     * answer. */
    CHECK(walk(1, 2, (dh_try_result[]){W, N}, 2, tries, sizeof tries) == DH_PROBLEM_WRONG_HUB);
    CHECK(strcmp(tries, "0.0 0.1 ") == 0);

    /* Nothing to try. */
    CHECK(walk(0, 2, NULL, 0, tries, sizeof tries) == DH_PROBLEM_UNREACHABLE && tries[0] == '\0');
    CHECK(walk(2, 0, NULL, 0, tries, sizeof tries) == DH_PROBLEM_UNREACHABLE && tries[0] == '\0');

    /* A result with no try outstanding changes nothing, and asking twice gives the same try. */
    dh_hub_walk w;
    uint8_t host, anchor;
    dh_hub_walk_begin(&w, 2, 1);
    dh_hub_walk_tried(&w, DH_TRY_ANSWERED);
    CHECK(dh_hub_walk_problem(&w) == DH_PROBLEM_UNREACHABLE);
    CHECK(dh_hub_walk_next(&w, &host, &anchor) && host == 0 && anchor == 0);
    CHECK(dh_hub_walk_next(&w, &host, &anchor) && host == 0 && anchor == 0);
    dh_hub_walk_tried(&w, DH_TRY_NO_ANSWER);
    dh_hub_walk_tried(&w, DH_TRY_NO_ANSWER);
    CHECK(dh_hub_walk_next(&w, &host, &anchor) && host == 1 && anchor == 0);
}

/* The reference display's own cases (tools/display-simulator/src/main.rs, mod tests). */
static void test_heartbeats(void)
{
    dh_heartbeat_state h = {0};
    CHECK(dh_heartbeat_due(&h, 0, 5)); /* the first poll */
    dh_heartbeat_sent(&h, 0, 5, true);
    CHECK(!dh_heartbeat_due(&h, 300, 5));
    /* A change goes at once. Taken, the next polls don't repeat it. */
    CHECK(dh_heartbeat_due(&h, 300, 4));
    dh_heartbeat_sent(&h, 300, 4, true);
    for (uint64_t poll = 2; poll < 12; poll++) {
        CHECK(!dh_heartbeat_due(&h, 300 * poll, 4));
    }
    /* The hourly one carries it again. */
    CHECK(!dh_heartbeat_due(&h, 300 + 3599, 4));
    CHECK(dh_heartbeat_due(&h, 300 + 3600, 4));

    /* A change the hub didn't take is tried again after five minutes. */
    h = (dh_heartbeat_state){0};
    dh_heartbeat_sent(&h, 0, 5, true);
    CHECK(dh_heartbeat_due(&h, 60, 4));
    dh_heartbeat_sent(&h, 60, 4, false);
    static const uint64_t soon[] = {75, 120, 300, 359};
    for (size_t i = 0; i < LEN(soon); i++) {
        CHECK(!dh_heartbeat_due(&h, soon[i], 4));
    }
    CHECK(dh_heartbeat_due(&h, 360, 4));
    dh_heartbeat_sent(&h, 360, 4, false);
    CHECK(!dh_heartbeat_due(&h, 420, 4));
    CHECK(dh_heartbeat_due(&h, 660, 4));
    /* Taken: that stops it, until the hourly one. */
    dh_heartbeat_sent(&h, 660, 4, true);
    static const uint64_t later[] = {720, 960, 1260, 3600};
    for (size_t i = 0; i < LEN(later); i++) {
        CHECK(!dh_heartbeat_due(&h, later[i], 4));
    }
    CHECK(dh_heartbeat_due(&h, 660 + 3600, 4));

    /* A new change isn't held back by an earlier one the hub didn't take. */
    h = (dh_heartbeat_state){0};
    dh_heartbeat_sent(&h, 0, 5, true);
    dh_heartbeat_sent(&h, 60, 4, false);
    CHECK(!dh_heartbeat_due(&h, 120, 4)); /* the same value waits */
    CHECK(dh_heartbeat_due(&h, 120, 3));
    dh_heartbeat_sent(&h, 120, 3, false);
    CHECK(!dh_heartbeat_due(&h, 180, 3));
    CHECK(dh_heartbeat_due(&h, 180, 4)); /* differs from both the taken and the failed value */
    CHECK(!dh_heartbeat_due(&h, 180, 5)); /* back to what the hub last took */
    CHECK(dh_heartbeat_due(&h, 420, 3));

    /* The first one after start-up, not taken: again after five minutes, not at every poll. */
    h = (dh_heartbeat_state){0};
    dh_heartbeat_sent(&h, 10, 5, false);
    CHECK(!dh_heartbeat_due(&h, 40, 5) && !dh_heartbeat_due(&h, 309, 5));
    CHECK(dh_heartbeat_due(&h, 310, 5));
}

static void test_saving(void)
{
#define SNAPSHOT(at, title) \
    "{\"protocol_version\":1,\"message_type\":\"dashboard.snapshot\",\"generated_at\":\"" at \
    "\",\"refresh_after_seconds\":300,\"pages\":[{\"id\":\"a\",\"title\":\"" title "\"}]}"
    /* Only the time it was built differs. */
    CHECK(dh_snapshot_bodies_match(BODY(SNAPSHOT("2026-11-10T20:00:00Z", "One")),
                                   BODY(SNAPSHOT("2026-11-10T20:05:00Z", "One"))));
    CHECK(dh_snapshot_bodies_match(BODY(SNAPSHOT("2026-11-10T20:00:00Z", "One")),
                                   BODY(SNAPSHOT("2026-11-10T20:00:00Z", "One"))));
    /* Something else differs, before it or after it. */
    CHECK(!dh_snapshot_bodies_match(BODY(SNAPSHOT("2026-11-10T20:00:00Z", "One")),
                                    BODY(SNAPSHOT("2026-11-10T20:00:00Z", "Two"))));
    CHECK(!dh_snapshot_bodies_match(
        BODY(SNAPSHOT("2026-11-10T20:00:00Z", "One")),
        BODY(" " SNAPSHOT("2026-11-10T20:00:00Z", "One"))));
    CHECK(!dh_snapshot_bodies_match(
        BODY(SNAPSHOT("2026-11-10T20:00:00Z", "One")),
        BODY(SNAPSHOT("2026-11-10T20:00:00Z", "One") " ")));
    /* A page's own times are not the snapshot's. */
    CHECK(!dh_snapshot_bodies_match(
        BODY("{\"generated_at\":\"a\",\"pages\":[{\"generated_at\":\"x\"}]}"),
        BODY("{\"generated_at\":\"a\",\"pages\":[{\"generated_at\":\"y\"}]}")));
    /* The value may be any length; the rest must still line up. */
    CHECK(dh_snapshot_bodies_match(BODY("{\"generated_at\":\"a\",\"x\":1}"),
                                   BODY("{\"generated_at\":\"longer\",\"x\":1}")));
    /* Bodies a display wouldn't have accepted never match, even themselves. */
    CHECK(!dh_snapshot_bodies_match(BODY("{\"x\":1}"), BODY("{\"x\":1}")));
    CHECK(!dh_snapshot_bodies_match(BODY("{\"generated_at\":1}"), BODY("{\"generated_at\":1}")));
    CHECK(!dh_snapshot_bodies_match(BODY("{\"generated_at\":\"a\",\"generated_at\":\"a\"}"),
                                    BODY("{\"generated_at\":\"a\",\"generated_at\":\"a\"}")));
    CHECK(!dh_snapshot_bodies_match(BODY("not json"), BODY("not json")));
    CHECK(!dh_snapshot_bodies_match(NULL, 0, BODY("{\"generated_at\":\"a\"}")));
#undef SNAPSHOT

    /* Nothing saved yet: save. */
    CHECK(dh_snapshot_save_due(false, false, false, 0, 5));
    /* The same as what is saved: never. */
    CHECK(!dh_snapshot_save_due(true, true, false, 0, 5));
    CHECK(!dh_snapshot_save_due(true, true, true, 0, 999999));
    /* Different from what was saved before this start: save once... */
    CHECK(dh_snapshot_save_due(true, false, false, 0, 5));
    /* ...and then at most hourly. */
    CHECK(!dh_snapshot_save_due(true, false, true, 5, 5 + 3599));
    CHECK(dh_snapshot_save_due(true, false, true, 5, 5 + 3600));
    /* A save that failed left nothing: still at most hourly, so a flash that refuses writes
     * isn't hammered. */
    CHECK(!dh_snapshot_save_due(false, false, true, 5, 65));
    CHECK(dh_snapshot_save_due(false, false, true, 5, 3605));
}

static uint32_t half(void *ctx)
{
    (void)ctx;
    return 0x80000000u;
}

static void test_settings_problem(void)
{
    /* Settings that can't be used wait an hour, as the reference does, and say so. */
    dh_poll_state st = {0, DH_PROBLEM_NONE};
    CHECK(dh_poll_failed(&st, DH_PROBLEM_SETTINGS, half, NULL) == 3600000);
    CHECK(st.problem == DH_PROBLEM_SETTINGS && st.failures == 1);
    CHECK(strcmp(dh_problem_headline(DH_PROBLEM_SETTINGS), "This display's settings are wrong") ==
          0);
    CHECK(dh_poll_failed(&st, DH_PROBLEM_WRONG_HUB, half, NULL) == 30000);
    CHECK(strcmp(dh_problem_headline(DH_PROBLEM_WRONG_HUB),
                 "Something else is answering as the hub") == 0);
}

int main(void)
{
    test_requests();
    test_enrol_response();
    test_error_response();
    test_device();
    test_walk();
    test_heartbeats();
    test_saving();
    test_settings_problem();
    printf("hub: %d checks, %d failed\n", checks, failures);
    return failures ? 1 : 0;
}
