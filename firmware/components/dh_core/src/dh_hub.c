/*
 * Talking to the hub once the display is set up. See dh_hub.h.
 *
 * Where this follows the reference display rather than the documents, or differs from it:
 *  - any 2xx status with a readable body is a successful enrolment, and any 2xx a heartbeat
 *    taken, as crates/hub-client reads them (a hub answers 201 and 204);
 *  - an enrolment response that repeats `device_id` or `device_token` is unreadable, as it is to
 *    serde; unknown members are ignored;
 *  - the ID and token are taken as any printable ASCII without spaces, up to 64 and 128 bytes.
 *    The reference takes any string; the schema is narrower (`d-` and 12 hex digits, `dhd_` and
 *    43 characters). A token is sent back in a header, so nothing wider than this is safe;
 *  - an error answer over 8 KiB counts as having no body, where the reference fails the read;
 *  - when no address gives an answer, the problem shown is the last address's, as in the
 *    reference (so a wrong certificate at the first address and silence at the second reads
 *    "can't reach the hub"). Within one address, a wrong certificate from either anchor counts
 *    as a wrong certificate.
 */
#include "dh_hub.h"

#include <string.h>

#include "dh_json.h"
#include "dh_writer.h"

#define LIT(s) s, (sizeof s - 1u)

#define DEVICE_FORMAT 1u

/* ---- The requests ---- */

static void put_device(dh_writer *w, const dh_device_info *info)
{
    dh_put_char(w, '{');
    dh_put_device_fields(w, info->device_type, info->firmware_version, info->hardware_revision);
    (void)dh_put_firmware_member(w, info->keys, info->record, info->slot_bytes);
    dh_put_char(w, '}');
}

/* Ends a body. Returns its length, or 0 (and a wiped buffer) if it didn't fit. */
static size_t finish(dh_writer *w)
{
    if (w->full || w->cap == 0) {
        /* What was written so far may hold part of a secret. */
        dh_wipe(w->buf, w->cap);
        return 0;
    }
    w->buf[w->len] = '\0';
    return w->len;
}

size_t dh_enrol_request(const dh_device_info *info, const char *secret, size_t secret_len,
                        char *out, size_t cap)
{
    dh_writer w = {out, cap, 0, false};
    if (secret_len == 0 || secret_len > DH_SETUP_MAX_SECRET_BYTES) {
        w.full = true;
        return finish(&w);
    }
    dh_put(&w, "{\"enrolment_secret\":");
    dh_put_utf8(&w, secret, secret_len);
    dh_put(&w, ",\"device\":");
    put_device(&w, info);
    dh_put_char(&w, '}');
    return finish(&w);
}

size_t dh_heartbeat_request(const dh_device_info *info, char *out, size_t cap)
{
    dh_writer w = {out, cap, 0, false};
    put_device(&w, info);
    return finish(&w);
}

/* ---- The device ID and token ---- */

/* Printable ASCII with no space: safe in a log line (the ID) and in a header (the token). */
static bool plain(const char *s, size_t n)
{
    for (size_t i = 0; i < n; i++) {
        unsigned char c = (unsigned char)s[i];
        if (c < 0x21 || c > 0x7E) {
            return false;
        }
    }
    return true;
}

bool dh_device_valid(const dh_device *device)
{
    return device->id_len >= 1 && device->id_len <= DH_DEVICE_MAX_ID_BYTES &&
           device->token_len >= 1 && device->token_len <= DH_DEVICE_MAX_TOKEN_BYTES &&
           plain(device->id, device->id_len) && plain(device->token, device->token_len) &&
           device->id[device->id_len] == '\0' && device->token[device->token_len] == '\0';
}

void dh_device_wipe(dh_device *device)
{
    dh_wipe(device, sizeof *device);
}

size_t dh_device_encode(const dh_device *device, uint8_t out[DH_DEVICE_BLOB_MAX])
{
    if (!dh_device_valid(device)) {
        return 0;
    }
    size_t n = 0;
    out[n++] = DEVICE_FORMAT;
    out[n++] = device->id_len;
    memcpy(out + n, device->id, device->id_len);
    n += device->id_len;
    out[n++] = device->token_len;
    memcpy(out + n, device->token, device->token_len);
    n += device->token_len;
    return n;
}

bool dh_device_decode(dh_device *out, const uint8_t *blob, size_t len)
{
    dh_device_wipe(out);
    if (len < 3 || blob[0] != DEVICE_FORMAT) {
        return false;
    }
    size_t id_len = blob[1];
    if (id_len > DH_DEVICE_MAX_ID_BYTES || len < 3 + id_len) {
        return false;
    }
    size_t token_len = blob[2 + id_len];
    /* Every byte must be accounted for. */
    if (token_len > DH_DEVICE_MAX_TOKEN_BYTES || len != 3 + id_len + token_len) {
        return false;
    }
    out->id_len = (uint8_t)id_len;
    memcpy(out->id, blob + 2, id_len);
    out->token_len = (uint8_t)token_len;
    memcpy(out->token, blob + 3 + id_len, token_len);
    if (!dh_device_valid(out)) {
        dh_device_wipe(out);
        return false;
    }
    return true;
}

size_t dh_authorization(const dh_device *device, char *out, size_t cap)
{
    static const char PREFIX[] = "Bearer ";
    const size_t prefix = sizeof PREFIX - 1u;
    if (cap > 0) {
        out[0] = '\0';
    }
    if (!dh_device_valid(device) || cap < prefix + device->token_len + 1u) {
        return 0;
    }
    memcpy(out, PREFIX, prefix);
    memcpy(out + prefix, device->token, device->token_len);
    out[prefix + device->token_len] = '\0';
    return prefix + device->token_len;
}

/* ---- The answers ---- */

static dh_response unreadable(dh_unreadable why, int json_status)
{
    dh_response r;
    memset(&r, 0, sizeof r);
    r.outcome = DH_OUTCOME_UNREADABLE;
    r.next_poll = DH_NEXT_BACKOFF;
    r.error_code = DH_ERROR_NONE;
    r.unreadable = why;
    r.json_status = json_status;
    return r;
}

/* One member that must be there exactly once, as a string of 1 to max bytes. */
static bool read_text(const dh_json_doc *doc, dh_json_value obj, const char *name, size_t name_len,
                      char *out, size_t max, uint8_t *out_len)
{
    dh_json_value v;
    size_t len = 0;
    if (dh_json_object_find(doc, obj, name, name_len, &v) != 1 ||
        dh_json_type_of(doc, v) != DH_JSON_STRING ||
        !dh_json_string_copy(doc, v, out, max + 1u, &len) || len == 0) {
        return false;
    }
    *out_len = (uint8_t)len;
    return true;
}

dh_enrol_answer dh_read_enrol_response(int http_status, const uint8_t *body, size_t len,
                                       dh_device *out)
{
    dh_enrol_answer answer;
    memset(&answer, 0, sizeof answer);
    dh_device_wipe(out);

    if (http_status < 200 || http_status > 299) {
        answer.response = dh_read_error_response(http_status, body, len);
        return answer;
    }
    if (len > DH_MAX_HUB_ANSWER_BYTES) {
        answer.response = unreadable(DH_UNREADABLE_TOO_LARGE, DH_JSON_OK);
        return answer;
    }
    dh_json_doc doc;
    dh_json_value root;
    dh_json_options options = {DH_MAX_HUB_ANSWER_BYTES, 0, 0};
    dh_json_status status = dh_json_parse(&doc, body, len, &options, &root);
    if (status != DH_JSON_OK) {
        answer.response = unreadable(DH_UNREADABLE_NOT_JSON, (int)status);
        return answer;
    }
    if (dh_json_type_of(&doc, root) != DH_JSON_OBJECT ||
        !read_text(&doc, root, LIT("device_id"), out->id, DH_DEVICE_MAX_ID_BYTES, &out->id_len) ||
        !read_text(&doc, root, LIT("device_token"), out->token, DH_DEVICE_MAX_TOKEN_BYTES,
                   &out->token_len)) {
        dh_device_wipe(out);
        answer.response = unreadable(DH_UNREADABLE_MALFORMED, DH_JSON_OK);
        return answer;
    }
    if (!dh_device_valid(out)) {
        dh_device_wipe(out);
        answer.response = unreadable(DH_UNREADABLE_INVALID, DH_JSON_OK);
        return answer;
    }
    answer.enrolled = true;
    answer.response.outcome = DH_OUTCOME_SHOW;
    answer.response.next_poll = DH_NEXT_REFRESH;
    return answer;
}

bool dh_heartbeat_taken(int http_status)
{
    return http_status >= 200 && http_status <= 299;
}

/* ---- Which address to try ---- */

void dh_hub_walk_begin(dh_hub_walk *w, uint8_t host_count, uint8_t anchor_count)
{
    memset(w, 0, sizeof *w);
    w->host_count = host_count;
    w->anchor_count = anchor_count;
    w->done = host_count == 0 || anchor_count == 0;
    w->last = DH_PROBLEM_UNREACHABLE;
}

bool dh_hub_walk_next(dh_hub_walk *w, uint8_t *host, uint8_t *anchor)
{
    if (w->done) {
        return false;
    }
    w->waiting = true;
    *host = w->host;
    *anchor = w->anchor;
    return true;
}

void dh_hub_walk_tried(dh_hub_walk *w, dh_try_result result)
{
    if (w->done || !w->waiting) {
        return;
    }
    w->waiting = false;
    switch (result) {
    case DH_TRY_ANSWERED:
        w->answered = true;
        w->done = true;
        w->last = DH_PROBLEM_NONE;
        return;
    case DH_TRY_WRONG_HUB:
        w->last = DH_PROBLEM_WRONG_HUB;
        if (++w->anchor < w->anchor_count) {
            return;
        }
        break;
    case DH_TRY_NO_ANSWER:
    default:
        /* If an earlier anchor found something else answering here, that is what this address
         * is remembered for. */
        w->last = w->anchor > 0 ? DH_PROBLEM_WRONG_HUB : DH_PROBLEM_UNREACHABLE;
        break;
    }
    w->anchor = 0;
    if (++w->host >= w->host_count) {
        w->done = true;
    }
}

dh_problem dh_hub_walk_problem(const dh_hub_walk *w)
{
    return w->answered ? DH_PROBLEM_NONE : w->last;
}

/* ---- When a heartbeat is due ---- */

bool dh_heartbeat_due(const dh_heartbeat_state *st, uint64_t now_s, uint8_t attempts_left)
{
    if (!st->sent_any || now_s - st->last_s >= DH_HEARTBEAT_EVERY_SECONDS) {
        return true;
    }
    bool held_back = st->failed && st->failed_attempts == attempts_left &&
                     now_s - st->failed_s < DH_HEARTBEAT_RETRY_SECONDS;
    bool changed = !st->has_taken || st->taken_attempts != attempts_left;
    return changed && !held_back;
}

void dh_heartbeat_sent(dh_heartbeat_state *st, uint64_t now_s, uint8_t attempts_left, bool taken)
{
    st->sent_any = true;
    st->last_s = now_s;
    if (taken) {
        st->has_taken = true;
        st->taken_attempts = attempts_left;
        st->failed = false;
    } else {
        st->failed = true;
        st->failed_s = now_s;
        st->failed_attempts = attempts_left;
    }
}

/* ---- Saving the last good snapshot ---- */

/* Where the value of the top-level `generated_at` is, if the body is an object with exactly one,
 * a string. */
static bool generated_at_of(const uint8_t *body, size_t len, size_t *from, size_t *to)
{
    dh_json_doc doc;
    dh_json_value root, v;
    dh_json_options options = {DH_MAX_SNAPSHOT_BYTES, 0, 0};
    if (dh_json_parse(&doc, body, len, &options, &root) != DH_JSON_OK ||
        dh_json_type_of(&doc, root) != DH_JSON_OBJECT ||
        dh_json_object_find(&doc, root, LIT("generated_at"), &v) != 1 ||
        dh_json_type_of(&doc, v) != DH_JSON_STRING) {
        return false;
    }
    *from = v;
    *to = dh_json_end(&doc, v);
    return true;
}

bool dh_snapshot_bodies_match(const uint8_t *a, size_t a_len, const uint8_t *b, size_t b_len)
{
    size_t a_from, a_to, b_from, b_to;
    if (!generated_at_of(a, a_len, &a_from, &a_to) || !generated_at_of(b, b_len, &b_from, &b_to)) {
        return false;
    }
    /* The same bytes before the value and the same bytes after it. */
    return a_from == b_from && a_len - a_to == b_len - b_to && memcmp(a, b, a_from) == 0 &&
           memcmp(a + a_to, b + b_to, a_len - a_to) == 0;
}

bool dh_snapshot_save_due(bool has_saved, bool matches_saved, bool written_since_start,
                          uint64_t saved_s, uint64_t now_s)
{
    if (has_saved && matches_saved) {
        return false;
    }
    return !written_since_start || now_s - saved_s >= DH_SNAPSHOT_SAVE_EVERY_SECONDS;
}
