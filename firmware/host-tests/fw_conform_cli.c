/*
 * Runs firmware update vectors (protocol/v1/firmware/README.md) through dh_core's update core
 * (and, for the provision vectors, the whole USB handler in dh_usb.h) and prints what it did as
 * each vector's "expect" object, one JSON line per file. run_conformance.py compares them.
 *
 *   dh_fw_conform <piece> <vector.json>...
 *
 * <piece> is how many bytes of a download's body are pushed at a time.
 *
 * It behaves as firmware must: the keys and the update record are only ever "in flash" as the
 * blobs dh_fw_keys_encode() and dh_fw_record_encode() make, written only when a function says to
 * save, and read back at every restart. So a vector also fails if a save point is missing.
 */
#include <inttypes.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "dh_json.h"
#include "dh_snapshot.h"
#include "dh_update.h"
#include "dh_usb.h"

#define LIT(s) s, (sizeof s - 1u)

static const char *current_file = "";

static _Noreturn void die(const char *what)
{
    fprintf(stderr, "%s: %s\n", current_file, what);
    exit(1);
}

static uint8_t *read_file(const char *path, size_t *len)
{
    FILE *f = fopen(path, "rb");
    if (!f) {
        return NULL;
    }
    size_t cap = 8192, n = 0;
    uint8_t *buf = malloc(cap);
    while (buf) {
        size_t got = fread(buf + n, 1, cap - n, f);
        n += got;
        if (got == 0) {
            break;
        }
        if (n == cap) {
            cap *= 2;
            uint8_t *bigger = realloc(buf, cap);
            if (!bigger) {
                free(buf);
            }
            buf = bigger;
        }
    }
    fclose(f);
    *len = n;
    return buf;
}

/* ---- Reading the vector ---- */

static dh_json_doc doc;

static dh_json_value get(dh_json_value obj, const char *name)
{
    dh_json_value v = 0;
    if (dh_json_object_find(&doc, obj, name, strlen(name), &v) != 1) {
        fprintf(stderr, "%s: no member %s\n", current_file, name);
        exit(1);
    }
    return v;
}

static bool is_null(dh_json_value v)
{
    return dh_json_type_of(&doc, v) == DH_JSON_NULL;
}

static uint64_t get_u64(dh_json_value obj, const char *name)
{
    uint64_t n = 0;
    if (!dh_json_number_u64(&doc, get(obj, name), &n)) {
        die("a number isn't a whole number");
    }
    return n;
}

static void copy(dh_json_value v, char *out, size_t cap)
{
    size_t len;
    if (dh_json_type_of(&doc, v) != DH_JSON_STRING ||
        !dh_json_string_copy(&doc, v, out, cap, &len)) {
        die("a string is missing or too long");
    }
}

static void get_string(dh_json_value obj, const char *name, char *out, size_t cap)
{
    copy(get(obj, name), out, cap);
}

static int hex_digit(char c)
{
    return c >= '0' && c <= '9' ? c - '0' : c >= 'a' && c <= 'f' ? c - 'a' + 10 : -1;
}

static void unhex(const char *s, uint8_t *out, size_t n)
{
    for (size_t i = 0; i < n; i++) {
        int hi = hex_digit(s[2 * i]);
        int lo = hi < 0 ? -1 : hex_digit(s[2 * i + 1]);
        if (lo < 0) {
            die("not lower-case hex");
        }
        out[i] = (uint8_t)((hi << 4) | lo);
    }
}

/* The vector's keys, by name. */
#define MAX_NAMED 8
static struct {
    char name[32];
    char hex[2 * DH_FW_KEY_LEN + 1];
    dh_fw_key key;
} named[MAX_NAMED];
static size_t named_count;

static void read_named_keys(dh_json_value root)
{
    dh_json_iter it;
    dh_json_value k, v;
    named_count = 0;
    if (!dh_json_object_begin(&doc, get(root, "keys"), &it)) {
        die("keys isn't an object");
    }
    while (dh_json_object_next(&doc, &it, &k, &v)) {
        if (named_count == MAX_NAMED) {
            die("too many keys");
        }
        copy(k, named[named_count].name, sizeof named[0].name);
        copy(v, named[named_count].hex, sizeof named[0].hex);
        if (strlen(named[named_count].hex) != 2 * DH_FW_KEY_LEN) {
            die("a key isn't 130 hex digits");
        }
        unhex(named[named_count].hex, named[named_count].key.bytes, DH_FW_KEY_LEN);
        named_count++;
    }
}

/* A key given by name, or written out in full as 130 hex digits. */
static const char *key_hex(dh_json_value v)
{
    static char text[2 * DH_FW_KEY_LEN + 1];
    copy(v, text, sizeof text);
    for (size_t i = 0; i < named_count; i++) {
        if (strcmp(named[i].name, text) == 0) {
            return named[i].hex;
        }
    }
    if (strlen(text) != 2 * DH_FW_KEY_LEN) {
        die("a key name isn't in keys");
    }
    return text;
}

static uint8_t read_key_list(dh_json_value list, dh_fw_key out[DH_FW_MAX_KEYS])
{
    dh_json_iter it;
    dh_json_value v;
    uint8_t n = 0;
    if (!dh_json_array_begin(&doc, list, &it)) {
        die("a key list isn't an array");
    }
    while (dh_json_array_next(&doc, &it, &v)) {
        if (n == DH_FW_MAX_KEYS) {
            die("more than three keys");
        }
        unhex(key_hex(v), out[n].bytes, DH_FW_KEY_LEN);
        n++;
    }
    return n;
}

/* ---- Printing ---- */

static void put_name_or_null(const char *s)
{
    if (s) {
        printf("\"%s\"", s);
    } else {
        fputs("null", stdout);
    }
}

static void put_key_names(const dh_fw_keys *keys)
{
    putchar('[');
    for (uint8_t i = 0; i < keys->key_count; i++) {
        const char *name = NULL;
        for (size_t k = 0; k < named_count; k++) {
            if (memcmp(named[k].key.bytes, keys->keys[i].bytes, DH_FW_KEY_LEN) == 0) {
                name = named[k].name;
            }
        }
        if (!name) {
            die("the display holds a key the vector doesn't name");
        }
        printf("%s\"%s\"", i ? "," : "", name);
    }
    putchar(']');
}

/* ---- The display: flash, and what is in memory ---- */

static struct {
    uint8_t keys[DH_FW_KEYS_BLOB_LEN];
    uint8_t record[DH_FW_RECORD_BLOB_LEN];
} flash;
static dh_fw_keys keys;
static dh_fw_record record;
static dh_fw_session session;
static uint8_t running_slot;
/* Writes of the record that a function asked for. */
static unsigned record_saves;

static void save(unsigned what)
{
    if (what & DH_FW_SAVE_KEYS) {
        dh_fw_keys_encode(&keys, flash.keys);
    }
    if (what & DH_FW_SAVE_RECORD) {
        dh_fw_record_encode(&record, flash.record);
        record_saves++;
    }
}

/* What a factory leaves, or a test sets up: straight to flash. */
static void format_flash(void)
{
    dh_fw_keys_encode(&keys, flash.keys);
    dh_fw_record_encode(&record, flash.record);
    record_saves = 0;
}

static void load(void)
{
    if (!dh_fw_keys_decode(&keys, flash.keys, sizeof flash.keys) ||
        !dh_fw_record_decode(&record, flash.record, sizeof flash.record)) {
        die("what was saved doesn't read back");
    }
}

/* A start: everything in memory is gone, and flash is read again. */
static dh_fw_start_result restart(uint8_t slot, bool on_trial)
{
    unsigned what = 0;
    memset(&keys, 0xee, sizeof keys);
    memset(&record, 0xee, sizeof record);
    load();
    running_slot = slot;
    dh_fw_start_result r = dh_fw_start(&session, &record, &keys, slot, on_trial, &what);
    save(what);
    return r;
}

/* Sends whatever final report is waiting, and has the hub answer it. Returns it. */
static dh_fw_report report_and_answer(void)
{
    dh_fw_report r;
    if (dh_fw_report_next(&session, &record, &r)) {
        save(dh_fw_report_answered(&session, &record, &r));
    }
    return r;
}

/* ---- package ---- */

static dh_fw_download download;

static void run_package(dh_json_value root, size_t piece)
{
    dh_json_value display = get(root, "display"), offer = get(root, "offer"),
                  response = get(root, "response");
    char device_type[200], hardware_revision[200], offer_id[64], after_body[16];
    get_string(display, "device_type", device_type, sizeof device_type);
    get_string(display, "hardware_revision", hardware_revision, sizeof hardware_revision);
    get_string(offer, "offer_id", offer_id, sizeof offer_id);
    get_string(response, "after_body", after_body, sizeof after_body);
    if (strcmp(after_body, "ends") != 0 && strcmp(after_body, "stalls") != 0) {
        die("after_body is neither ends nor stalls");
    }
    dh_fw_device device = {device_type, hardware_revision, (uint32_t)get_u64(display, "slot_bytes")};
    uint64_t offer_size = get_u64(offer, "size");
    dh_json_value cl = get(response, "content_length");
    bool has_length = !is_null(cl);
    uint64_t content_length = has_length ? get_u64(response, "content_length") : 0;

    dh_json_value body_hex = get(response, "body_hex");
    size_t hex_len = dh_json_string_length(&doc, body_hex);
    char *hex = malloc(hex_len + 1);
    size_t body_len = hex_len / 2;
    uint8_t *body = malloc(body_len ? body_len : 1);
    uint8_t *written = malloc(body_len ? body_len : 1);
    if (!hex || !body || !written || hex_len % 2 != 0) {
        die("body_hex can't be read");
    }
    copy(body_hex, hex, hex_len + 1);
    unhex(hex, body, body_len);
    free(hex);

    dh_fw_keys_init(&keys);
    keys.key_count = read_key_list(get(display, "trusted_keys"), keys.keys);
    keys.security_floor = (uint32_t)get_u64(display, "security_floor");
    dh_fw_record_init(&record);
    format_flash();
    restart(0, false);

    const char *reports[3];
    size_t report_count = 0;
    bool requested = false, begun = false, finished = false;
    size_t written_len = 0;
    uint64_t bytes_read = 0;

    dh_fw_offer_decision decision =
        dh_fw_on_offer(&session, &record, &keys, &device, offer_id, offer_size, 0);
    save(decision.save);
    if (decision.action == DH_FW_OFFER_ATTEMPT) {
        requested = true;
        dh_fw_reason reason = dh_fw_download_begin(&download, &keys, &device, offer_size,
                                                   has_length, content_length);
        size_t at = 0;
        bool over = reason != DH_FW_REASON_NONE;
        while (!over) {
            if (at == body_len) {
                /* Whether it ends or stalls, nothing more comes: the display gives up. */
                reason = dh_fw_download_fail(&download, DH_FW_REASON_DOWNLOAD);
                break;
            }
            size_t n = body_len - at < piece ? body_len - at : piece;
            /* A copy of exactly the piece, so the sanitisers see a read past its end. */
            uint8_t *exact = malloc(n);
            if (!exact) {
                die("out of memory");
            }
            memcpy(exact, body + at, n);
            size_t used = 0;
            while (used < n && !over) {
                dh_fw_push p = dh_fw_download_push(&download, exact + used, n - used);
                if (p.used > n - used || (p.step == DH_FW_STEP_MORE && p.used != n - used)) {
                    die("a push took the wrong amount");
                }
                if (p.write_len) {
                    if (!begun || written_len + p.write_len > body_len) {
                        die("image bytes to write before the slot was opened, or too many");
                    }
                    memcpy(written + written_len, p.write, p.write_len);
                    written_len += p.write_len;
                }
                used += p.used;
                switch (p.step) {
                case DH_FW_STEP_MORE:
                    break;
                case DH_FW_STEP_OPEN_SLOT:
                    begun = true;
                    reports[report_count++] = "downloading";
                    break;
                case DH_FW_STEP_FINISH:
                    finished = true;
                    over = true;
                    break;
                case DH_FW_STEP_ENDED:
                    reason = p.reason;
                    over = true;
                    break;
                }
            }
            free(exact);
            at += used;
        }
        bytes_read = dh_fw_download_bytes_read(&download);
        if (begun != dh_fw_download_slot_open(&download)) {
            die("slot_open disagrees with the steps");
        }
        if (finished) {
            /* What was written must be the image, whole and in order. */
            const dh_fw_manifest *m = dh_fw_download_manifest(&download);
            size_t image_at = (size_t)bytes_read - m->image_size;
            if (written_len != m->image_size || memcmp(written, body + image_at, written_len) != 0) {
                die("what was written isn't the image");
            }
            save(dh_fw_attempt_installed(&record, m, 1));
        } else {
            save(dh_fw_attempt_ended(&record, reason));
        }
    } else if (decision.action != DH_FW_OFFER_REFUSE) {
        die("the offer was neither attempted nor refused");
    }

    char new_version[DH_FW_MAX_VERSION_BYTES + 1] = "";
    if (finished) {
        /* The restart, the trial boot, a check-in, and marked good. */
        if (restart(1, true) != DH_FW_START_TRIAL) {
            die("the new firmware didn't start on trial");
        }
        dh_fw_report none;
        if (dh_fw_report_next(&session, &record, &none)) {
            die("a report on a trial boot");
        }
        memcpy(new_version, record.pending_version, sizeof new_version);
        if (dh_fw_trial_check_in(&session, 10, DH_FW_CHECK_IN_ANSWERED) != DH_FW_TRIAL_MARK_GOOD) {
            die("a check-in didn't mark the trial good");
        }
        save(dh_fw_marked_good(&session, &record, &keys));
        restart(1, false);
    }
    dh_fw_report final = report_and_answer();
    if (final.state == DH_FW_REPORT_NONE) {
        die("no final report");
    }
    reports[report_count++] = dh_fw_report_state_name(final.state);
    load();

    printf("{\"state\":\"%s\",\"reason\":", dh_fw_report_state_name(final.state));
    put_name_or_null(dh_fw_reason_name(final.reason));
    fputs(",\"reports\":[", stdout);
    for (size_t i = 0; i < report_count; i++) {
        printf("%s\"%s\"", i ? "," : "", reports[i]);
    }
    printf("],\"requests_download\":%s,\"body_bytes_read\":%" PRIu64 ",\"spare_slot\":\"%s\"",
           requested ? "true" : "false", bytes_read,
           !begun ? "untouched" : finished ? "set_to_boot" : "abandoned");
    fputs(",\"after_marked_good\":", stdout);
    if (finished) {
        printf("{\"firmware_version\":\"%s\",\"security_floor\":%" PRIu32 ",\"trusted_keys\":",
               new_version, keys.security_floor);
        put_key_names(&keys);
        putchar('}');
    } else {
        fputs("null", stdout);
    }
    fputs("}\n", stdout);
    free(body);
    free(written);
}

/* ---- offer ---- */

static void put_budget(void)
{
    printf("{\"attempts_left\":%u,\"credit_hours\":%u,\"last_offer_id\":", record.attempts_left,
           record.credit_hours);
    put_name_or_null(record.has_last_offer ? record.last_offer_id : NULL);
    putchar('}');
}

static void run_offer(dh_json_value root)
{
    dh_json_value before = get(root, "before");
    static const dh_fw_device device = {"llm-usage-display", "simulated", 65536};
    dh_fw_keys_init(&keys);
    keys.key_count = 1;
    keys.keys[0] = named[0].key;
    dh_fw_record_init(&record);
    record.attempts_left = (uint8_t)get_u64(before, "attempts_left");
    record.credit_hours = (uint8_t)get_u64(before, "credit_hours");
    if (!is_null(get(before, "last_offer_id"))) {
        record.has_last_offer = true;
        get_string(before, "last_offer_id", record.last_offer_id, sizeof record.last_offer_id);
    }
    format_flash();
    restart(0, false);
    uint64_t uptime = 0;
    /* Saves of the budget: the ones a credit hour, an attempt or a refill asks for. */
    unsigned budget_saves = 0;

    fputs("{\"actions\":[", stdout);
    size_t actions = 0;
    dh_json_iter it;
    dh_json_value e;
    if (!dh_json_array_begin(&doc, get(root, "events"), &it)) {
        die("events isn't an array");
    }
    while (dh_json_array_next(&doc, &it, &e)) {
        char event[32];
        get_string(e, "event", event, sizeof event);
        if (strcmp(event, "running") == 0) {
            /* A display looks at its timer often; here, once a minute. */
            for (uint64_t m = get_u64(e, "minutes"); m > 0; m--) {
                uptime += 60;
                unsigned what = dh_fw_tick(&session, &record, uptime);
                save(what);
                budget_saves += what & DH_FW_SAVE_RECORD ? 1 : 0;
            }
        } else if (strcmp(event, "restart") == 0) {
            restart(running_slot, false);
            uptime = 0;
        } else if (strcmp(event, "usb_bundle") == 0) {
            unsigned what = 0;
            if (dh_fw_bundle_apply(&record, &keys, NULL, &what) != DH_FW_BUNDLE_OK) {
                die("a bundle without keys was refused");
            }
            save(what);
            budget_saves += what & DH_FW_SAVE_RECORD ? 1 : 0;
        } else if (strcmp(event, "clock_moves") == 0) {
            /* Nothing here takes a date or the hub's time, so there is nothing to move. */
        } else if (strcmp(event, "offer") == 0) {
            char offer_id[64], outcome[32];
            get_string(e, "offer_id", offer_id, sizeof offer_id);
            get_string(e, "outcome", outcome, sizeof outcome);
            bool early = strcmp(outcome, "refused_before_request") == 0;
            /* An offer refused from the offer alone is one that is too large. */
            uint64_t size = early ? (uint64_t)DH_MAX_FIRMWARE_UPDATE_BYTES : 1000;
            (void)report_and_answer();
            dh_fw_offer_decision d =
                dh_fw_on_offer(&session, &record, &keys, &device, offer_id, size, uptime);
            save(d.save);
            budget_saves += d.save & DH_FW_SAVE_RECORD ? 1 : 0;
            const char *action = "busy";
            switch (d.action) {
            case DH_FW_OFFER_IGNORE:
                action = "ignore";
                break;
            case DH_FW_OFFER_REFUSE:
                action = "refuse_without_request";
                break;
            case DH_FW_OFFER_WAIT:
                action = "wait";
                break;
            case DH_FW_OFFER_BUSY:
                break;
            case DH_FW_OFFER_ATTEMPT: {
                action = "attempt";
                /* The attempt runs its course. Its result never changes the budget. */
                dh_fw_manifest m;
                memset(&m, 0, sizeof m);
                memcpy(m.firmware_version, "9.9.9", 6);
                uint8_t spare = (uint8_t)(1 - running_slot);
                if (strcmp(outcome, "installed") == 0) {
                    save(dh_fw_attempt_installed(&record, &m, spare));
                    restart(spare, true);
                    if (dh_fw_trial_check_in(&session, 5, DH_FW_CHECK_IN_ANSWERED) !=
                        DH_FW_TRIAL_MARK_GOOD) {
                        die("the trial wasn't marked good");
                    }
                    save(dh_fw_marked_good(&session, &record, &keys));
                    uptime = 5;
                } else if (strcmp(outcome, "rolled_back") == 0) {
                    save(dh_fw_attempt_installed(&record, &m, spare));
                    if (restart(running_slot, false) != DH_FW_START_ROLLED_BACK) {
                        die("the old firmware didn't see the rollback");
                    }
                    uptime = 0;
                } else if (strcmp(outcome, "refused") == 0) {
                    save(dh_fw_attempt_ended(&record, DH_FW_REASON_BAD_SIGNATURE));
                } else if (strcmp(outcome, "failed") == 0) {
                    save(dh_fw_attempt_ended(&record, DH_FW_REASON_DOWNLOAD));
                } else {
                    die("an attempt can't end that way");
                }
                break;
            }
            }
            printf("%s\"%s\"", actions++ ? "," : "", action);
        } else {
            die("an event this runner doesn't know");
        }
    }
    /* What flash holds afterwards. */
    load();
    printf("],\"saves\":%u,\"after\":", budget_saves);
    put_budget();
    fputs("}\n", stdout);
}

/* ---- trial ---- */

static dh_snapshot snapshot;
static dh_text_scratch scratch;

static dh_fw_check_in check_in_of(const char *answer)
{
    if (strcmp(answer, "success") == 0) {
        return DH_FW_CHECK_IN_ANSWERED;
    }
    if (strcmp(answer, "no_answer") == 0) {
        return DH_FW_CHECK_IN_FAILED;
    }
    /* The hub's error answer, read as the snapshot reader reads it. */
    char body[128];
    int n = snprintf(body, sizeof body, "{\"error\":{\"code\":\"%s\",\"message\":\"m\"}}", answer);
    int status = strcmp(answer, "unavailable") == 0 ? 503 : 401;
    dh_response r =
        dh_read_response(status, (const uint8_t *)body, (size_t)n, NULL, &snapshot, &scratch);
    return dh_fw_check_in_of(&r);
}

static void run_trial(dh_json_value root)
{
    dh_json_value before = get(root, "before"), update = get(before, "update"),
                  trial = get(root, "trial");
    char old_version[64], new_version[64];
    get_string(before, "firmware_version", old_version, sizeof old_version);
    get_string(update, "firmware_version", new_version, sizeof new_version);

    dh_fw_keys_init(&keys);
    keys.key_count = read_key_list(get(before, "trusted_keys"), keys.keys);
    keys.security_floor = (uint32_t)get_u64(before, "security_floor");
    /* The record the previous firmware saved before it set the new image to boot. */
    dh_fw_record_init(&record);
    record.attempts_left = 4;
    record.has_last_offer = true;
    memcpy(record.last_offer_id, "000000000000000000000000000003e8", DH_OFFER_ID_HEX_LEN + 1);
    record.stage = DH_FW_STAGE_ATTEMPT;
    dh_fw_manifest m;
    memset(&m, 0, sizeof m);
    if (strlen(new_version) > DH_FW_MAX_VERSION_BYTES) {
        die("the version is too long");
    }
    memcpy(m.firmware_version, new_version, strlen(new_version) + 1);
    m.security_counter = (uint32_t)get_u64(update, "security_counter");
    if (!is_null(get(update, "key_set"))) {
        m.has_key_set = true;
        m.key_count = read_key_list(get(update, "key_set"), m.key_set);
    }
    if (dh_fw_attempt_installed(&record, &m, 1) != DH_FW_SAVE_RECORD) {
        die("the record didn't take the update");
    }
    format_flash();

    bool starts = dh_json_type_of(&doc, get(trial, "starts")) == DH_JSON_TRUE;
    bool has_restart = !is_null(get(trial, "restart_at_seconds"));
    uint64_t restart_at = has_restart ? get_u64(trial, "restart_at_seconds") : 0;

    bool good = false;
    uint64_t at = 0;
    if (starts) {
        if (restart(1, true) != DH_FW_START_TRIAL) {
            die("the new firmware didn't start on trial");
        }
        /* Second by second, as a display's own timer runs: a restart, then time passing, then
         * any check-in's answer. */
        for (bool over = false; !over; at++) {
            if (at > 100000) {
                die("the trial never ended");
            }
            if (has_restart && restart_at == at) {
                break;
            }
            if (dh_fw_trial_tick(&session, at) == DH_FW_TRIAL_ROLL_BACK) {
                break;
            }
            dh_json_iter it;
            dh_json_value c;
            if (!dh_json_array_begin(&doc, get(trial, "check_ins"), &it)) {
                die("check_ins isn't an array");
            }
            while (!over && dh_json_array_next(&doc, &it, &c)) {
                char answer[32];
                if (get_u64(c, "at_seconds") != at) {
                    continue;
                }
                get_string(c, "answer", answer, sizeof answer);
                /* On trial, nothing may be reported and no offer acted on. */
                dh_fw_report none;
                static const dh_fw_device device = {"llm-usage-display", "simulated", 65536};
                if (dh_fw_report_next(&session, &record, &none) ||
                    dh_fw_on_offer(&session, &record, &keys, &device,
                                   "00000000000000000000000000000abc", 1000, at)
                            .action != DH_FW_OFFER_BUSY) {
                    die("a trial boot reported, or acted on an offer");
                }
                if (dh_fw_trial_check_in(&session, at, check_in_of(answer)) ==
                    DH_FW_TRIAL_MARK_GOOD) {
                    save(dh_fw_marked_good(&session, &record, &keys));
                    good = true;
                    over = true;
                }
            }
            if (over) {
                break;
            }
        }
    }
    if (!good) {
        /* The bootloader goes back to the previous image, which finds the record unfinished. */
        if (restart(0, false) != DH_FW_START_ROLLED_BACK) {
            die("the old firmware didn't see the rollback");
        }
    }
    dh_fw_report final = report_and_answer();
    load();
    printf("{\"outcome\":\"%s\",\"at_seconds\":%" PRIu64 ",\"report\":", good ? "marked_good" : "rolled_back", at);
    put_name_or_null(dh_fw_report_state_name(final.state));
    printf(",\"firmware_version\":\"%s\",\"security_floor\":%" PRIu32 ",\"trusted_keys\":",
           good ? new_version : old_version, keys.security_floor);
    put_key_names(&keys);
    fputs("}\n", stdout);
}

/* ---- provision ---- */

/* The whole USB handler (dh_usb.h), with a store that keeps everything "in flash": the keys and
 * the record as their blobs, as above, and the setup as its own. */

static struct {
    bool held;
    size_t len;
    uint8_t blob[DH_SETUP_BLOB_MAX];
} flash_setup;

static dh_held store_held(void *ctx)
{
    (void)ctx;
    return flash_setup.held ? DH_HELD_SETUP : DH_HELD_NOTHING;
}

static bool store_save_keys(void *ctx, const dh_fw_keys *k)
{
    (void)ctx;
    dh_fw_keys_encode(k, flash.keys);
    return true;
}

static bool store_save_record(void *ctx, const dh_fw_record *r)
{
    (void)ctx;
    dh_fw_record_encode(r, flash.record);
    record_saves++;
    return true;
}

static bool store_save_setup(void *ctx, const dh_setup *s)
{
    (void)ctx;
    flash_setup.len = dh_setup_encode(s, flash_setup.blob);
    flash_setup.held = flash_setup.len > 0;
    return flash_setup.held;
}

static bool store_erase_setup(void *ctx)
{
    (void)ctx;
    memset(&flash_setup, 0, sizeof flash_setup);
    return true;
}

static const dh_setup_store store = {NULL,           store_held,       store_save_keys,
                                     store_save_record, store_save_setup, store_erase_setup};
static dh_setup work;

/* Everything in a bundle but its firmware member. */
static const char BUNDLE_START[] =
    "{\"type\":\"provision\",\"bundle\":{\"format\":\"display-hub.provisioning\",\"version\":1,"
    "\"hub\":{\"hosts\":[\"hub.local\",\"192.168.1.20\"],\"port\":8443,"
    "\"server_name\":\"hub.display-hub.internal\",\"trust_anchors\":[{\"certificate_pem\":"
    "\"-----BEGIN CERTIFICATE-----\\nMIIB\\n-----END CERTIFICATE-----\\n\",\"spki_sha256\":"
    "\"9a54396d429aa58fc9ac78413a9e84a0b7a45f125256f91697c977c2acbfde88\"}]},"
    "\"enrolment\":{\"secret\":\"q3H7cX0f2pS9L1mB8vN4wA\",\"expires_at\":\"2026-11-10T20:10:00Z\"},"
    "\"wifi\":{\"ssid\":\"Example network\",\"password\":\"example-passphrase\"}";

/* Sends one `provision` line through the handler and returns the answer's name. bundle_firmware
 * is the vector's member, or a null value for a bundle without one. */
static const char *send_bundle(dh_json_value bundle_firmware)
{
    static char line[2048], reply[DH_USB_REPLY_MAX];
    size_t n = (size_t)snprintf(line, sizeof line, "%s", BUNDLE_START);
    if (!is_null(bundle_firmware)) {
        /* The member as it would arrive: key names written out as hex. */
        n += (size_t)snprintf(line + n, sizeof line - n, ",\"firmware\":{\"trusted_keys\":[");
        dh_json_iter it;
        dh_json_value k;
        bool first = true;
        if (!dh_json_array_begin(&doc, get(bundle_firmware, "trusted_keys"), &it)) {
            die("trusted_keys isn't an array");
        }
        while (dh_json_array_next(&doc, &it, &k)) {
            n += (size_t)snprintf(line + n, sizeof line - n, "%s\"%s\"", first ? "" : ",",
                                  key_hex(k));
            first = false;
            if (n > sizeof line - 200) {
                die("the bundle is too long for this runner");
            }
        }
        n += (size_t)snprintf(line + n, sizeof line - n, "],\"security_counter\":%" PRIu64 "}",
                              get_u64(bundle_firmware, "security_counter"));
    }
    n += (size_t)snprintf(line + n, sizeof line - n, "}}");

    const dh_usb_display display = {"display-hub-waveshare-amoled-1.8", "0.1.0",
                                    "waveshare-amoled-1.8-v2", 6291456, &store, &keys, &record,
                                    &work};
    dh_usb_answer a = dh_usb_handle_line(&display, (const uint8_t *)line, n, reply, sizeof reply);
    /* The reply line must say the same as the answer. */
    char want[DH_USB_REPLY_MAX];
    switch (a.outcome) {
    case DH_USB_PROVISIONED:
        if (strcmp(reply, "{\"type\":\"provisioned\"}\n") != 0 || !flash_setup.held) {
            die("provisioned, but the reply or the stored setup is wrong");
        }
        return "provisioned";
    case DH_USB_REFUSED:
        snprintf(want, sizeof want, "{\"type\":\"error\",\"code\":\"%s\",\"message\":\"",
                 dh_usb_error_name(a.error));
        if (strncmp(reply, want, strlen(want)) != 0 || flash_setup.held) {
            die("refused, but the reply is wrong or a setup was stored");
        }
        return dh_usb_error_name(a.error);
    default:
        die("the handler didn't answer a bundle");
    }
}

static void run_provision(dh_json_value root)
{
    dh_json_value before = get(root, "before"), action = get(root, "action");
    char what[32];
    get_string(action, "action", what, sizeof what);
    dh_fw_keys_init(&keys);
    keys.key_count = read_key_list(get(before, "trusted_keys"), keys.keys);
    keys.security_floor = (uint32_t)get_u64(before, "security_floor");
    dh_fw_record_init(&record);
    record.attempts_left = 2;
    format_flash();
    memset(&flash_setup, 0, sizeof flash_setup);
    restart(0, false);

    const char *answer = NULL;
    if (strcmp(what, "factory_reset") == 0) {
        /* Set the display up first, so there is a setup for the reset to erase. A bundle without
         * a firmware member is accepted whatever keys the display has, and changes none. */
        dh_json_value none = get(action, "bundle_firmware");
        if (!is_null(none) || strcmp(send_bundle(none), "provisioned") != 0) {
            die("couldn't set the display up before the factory reset");
        }
        const dh_usb_display display = {"display-hub-waveshare-amoled-1.8", "0.1.0",
                                        "waveshare-amoled-1.8-v2", 6291456, &store, &keys,
                                        &record, &work};
        if (!dh_usb_factory_reset(&display) || flash_setup.held) {
            die("the factory reset left the setup behind");
        }
        restart(0, false);
        /* The reset leaves the record alone: the budget is as accepting the bundle left it. */
        if (record.attempts_left != DH_FW_FULL_BUDGET) {
            die("the budget after the factory reset is wrong");
        }
    } else if (strcmp(what, "bundle") == 0) {
        answer = send_bundle(get(action, "bundle_firmware"));
        /* Accepting any bundle refills the budget; refusing one changes nothing. Read it back
         * from flash, as a restart would. */
        restart(0, false);
        if (record.attempts_left != (strcmp(answer, "provisioned") == 0 ? DH_FW_FULL_BUDGET : 2u)) {
            die("the budget after the bundle is wrong");
        }
    } else {
        die("action is neither bundle nor factory_reset");
    }
    load();
    fputs("{\"answer\":", stdout);
    put_name_or_null(answer);
    fputs(",\"trusted_keys\":", stdout);
    put_key_names(&keys);
    printf(",\"security_floor\":%" PRIu32 "}\n", keys.security_floor);
}

static void run_vector(const char *path, size_t piece)
{
    current_file = path;
    size_t len = 0;
    uint8_t *file = read_file(path, &len);
    dh_json_value root = 0;
    if (!file || dh_json_parse(&doc, file, len, NULL, &root) != DH_JSON_OK) {
        die("can't read the vector");
    }
    char kind[32];
    read_named_keys(root);
    get_string(root, "kind", kind, sizeof kind);
    if (strcmp(kind, "package") == 0) {
        run_package(root, piece);
    } else if (strcmp(kind, "offer") == 0) {
        run_offer(root);
    } else if (strcmp(kind, "trial") == 0) {
        run_trial(root);
    } else if (strcmp(kind, "provision") == 0) {
        run_provision(root);
    } else {
        die("a kind this runner doesn't know");
    }
    free(file);
}

int main(int argc, char **argv)
{
    if (argc < 3) {
        fprintf(stderr, "usage: dh_fw_conform <piece> <vector.json>...\n");
        return 2;
    }
    size_t piece = (size_t)strtoul(argv[1], NULL, 10);
    if (piece == 0) {
        fprintf(stderr, "the piece size must be at least 1\n");
        return 2;
    }
    for (int i = 2; i < argc; i++) {
        run_vector(argv[i], piece);
    }
    return 0;
}
