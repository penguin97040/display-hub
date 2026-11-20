/*
 * Tests for the firmware's own part in a signed update, on a computer: main/update.c (reading a
 * download, writing the spare partition, the trial boot's clock) and main/storage_fw.c (the
 * saves, in their order), over the stand-in for the board in glue/. The firmware vectors run
 * through the same code as dh_fw_conform_glue; these are the cases a vector can't describe: the
 * two time limits, flash calls and saves that fail, a setup that goes mid-download, and the
 * trial boot's five minutes against a check-in.
 *
 * The package is the one in protocol/v1/firmware/package-01-valid.json: version 1.2.0, security
 * counter 4, for a display that trusts `maker` and `recovery` with a floor of 3.
 */
#include <inttypes.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "dh_json.h"
#include "dh_update.h"
#include "esp_err.h"
#include "glue.h"
#include "storage_fw.h"
#include "update.h"

static int checks, failures;

#define CHECK(c) \
    do { \
        checks++; \
        if (!(c)) { \
            failures++; \
            fprintf(stderr, "%s:%d: CHECK(%s) failed\n", __FILE__, __LINE__, #c); \
        } \
    } while (0)

#define LIT(s) s, (sizeof s - 1u)

static const char OFFER[] = "000000000000000000000000000003e8";
static const dh_fw_device DEVICE = {"llm-usage-display", "simulated", 65536};

/* The valid package's update form, and the keys of the display it is for. */
static uint8_t form[4096];
static size_t form_len;
static dh_fw_keys display_keys;

static _Noreturn void die(const char *what)
{
    fprintf(stderr, "update_glue_tests: %s\n", what);
    exit(1);
}

static void unhex(const char *hex, uint8_t *out, size_t n)
{
    for (size_t i = 0; i < n; i++) {
        unsigned v = 0;
        if (sscanf(hex + 2 * i, "%2x", &v) != 1) {
            die("bad hex in the vector");
        }
        out[i] = (uint8_t)v;
    }
}

static void read_key(const dh_json_doc *doc, dh_json_value named, const char *name, dh_fw_key *out)
{
    dh_json_value v;
    char hex[2 * DH_FW_KEY_LEN + 1];
    size_t len = 0;
    if (dh_json_object_find(doc, named, name, strlen(name), &v) != 1 ||
        !dh_json_string_copy(doc, v, hex, sizeof hex, &len) || len != 2 * DH_FW_KEY_LEN) {
        die("the vector has no such key");
    }
    unhex(hex, out->bytes, DH_FW_KEY_LEN);
}

static void load_package(void)
{
    static uint8_t file[65536];
    static char hex[2 * sizeof form + 1];
    FILE *f = fopen(DH_PROTOCOL_DIR "/firmware/package-01-valid.json", "rb");
    if (f == NULL) {
        die("can't read package-01-valid.json");
    }
    size_t n = fread(file, 1, sizeof file, f);
    fclose(f);
    dh_json_doc doc;
    dh_json_value root = 0, response = 0, body = 0, named = 0;
    size_t hex_len = 0;
    if (dh_json_parse(&doc, file, n, NULL, &root) != DH_JSON_OK ||
        dh_json_object_find(&doc, root, LIT("response"), &response) != 1 ||
        dh_json_object_find(&doc, response, LIT("body_hex"), &body) != 1 ||
        !dh_json_string_copy(&doc, body, hex, sizeof hex, &hex_len) || hex_len % 2 != 0 ||
        dh_json_object_find(&doc, root, LIT("keys"), &named) != 1) {
        die("package-01-valid.json isn't the vector these tests were written for");
    }
    form_len = hex_len / 2;
    unhex(hex, form, form_len);
    dh_fw_keys_init(&display_keys);
    display_keys.key_count = 2;
    read_key(&doc, named, "maker", &display_keys.keys[0]);
    read_key(&doc, named, "recovery", &display_keys.keys[1]);
    display_keys.security_floor = 3;
}

/* A display that trusts the package's signer and has never tried an update, just started. */
static void fresh_display(void)
{
    dh_fw_record record;
    dh_fw_record_init(&record);
    glue_reset();
    glue_start(&display_keys, &record, 0, false);
    glue.body = form;
    glue.body_len = form_len;
    glue.may_read = form_len;
}

/* Takes the offer, which must spend an attempt and save it. */
static void take_offer(void)
{
    dh_fw_offer_decision decision;
    CHECK(storage_fw_on_offer(&DEVICE, OFFER, form_len, &decision));
    CHECK(decision.action == DH_FW_OFFER_ATTEMPT);
    CHECK(strcmp(glue.writes, "r") == 0);
}

static bool stop_now;
static bool stop(void)
{
    return stop_now;
}

static dh_fw_reason receive(void)
{
    glue.keep_next_free = true;
    return update_receive(NULL, &DEVICE, form_len, true, form_len, glue.now_us / 1000, stop);
}

/* What is in flash, read as a restart would read it. */
static dh_fw_record flash_record(void)
{
    dh_fw_record r;
    CHECK(dh_fw_record_decode(&r, glue.flash_record, sizeof glue.flash_record));
    return r;
}

static dh_fw_keys flash_keys(void)
{
    dh_fw_keys k;
    CHECK(dh_fw_keys_decode(&k, glue.flash_keys, sizeof glue.flash_keys));
    return k;
}

/* Nothing odd happened to the partition, the lock or the connection. */
static void check_tidy(void)
{
    CHECK(!glue.wrote_while_closed && !glue.lock_misused && !glue.mutex_misused);
    CHECK(glue.lock_depth == 0 && !glue.open);
    CHECK(!glue.read_past_the_form && glue.most_asked <= 4096);
}

/* ---- A download ---- */

static void test_install(void)
{
    static const size_t pieces[] = {1, 137, 4096};
    for (size_t i = 0; i < sizeof pieces / sizeof pieces[0]; i++) {
        fresh_display();
        take_offer();
        glue.piece = pieces[i];
        CHECK(receive() == DH_FW_REASON_NONE);
        check_tidy();
        const dh_fw_manifest *m = dh_fw_download_manifest(glue.download);
        CHECK(m != NULL && strcmp(m->firmware_version, "1.2.0") == 0);
        /* Opened for writes in order, written with exactly the image, checked, and set to
         * boot only once the record was in flash. */
        CHECK(glue.begun && glue.sequential && glue.ended && !glue.aborted && glue.set_to_boot);
        CHECK(m != NULL && glue.written_len == m->image_size &&
              memcmp(glue.written, form + form_len - glue.written_len, glue.written_len) == 0);
        CHECK(glue.handed == form_len);
        CHECK(strcmp(glue.writes, "rr") == 0 && glue.record_writes_at_set_boot == 2);
        dh_fw_record r = flash_record();
        CHECK(r.stage == DH_FW_STAGE_PENDING && r.pending_slot == 1 &&
              strcmp(r.pending_version, "1.2.0") == 0 && r.pending_security_counter == 4 &&
              r.attempts_left == 4 && strcmp(r.last_offer_id, OFFER) == 0);
        /* The keys and the floor don't move until the trial boot is marked good. */
        CHECK(glue.keys_writes == 0 && flash_keys().security_floor == 3);
        /* Nothing is reported from here: the display restarts. */
        dh_fw_report report;
        CHECK(!storage_fw_report_next(&report));
    }
}

/* What a download that ended must leave: nothing set to boot, the partition abandoned if it was
 * opened, and, once the attempt is ended, a result in flash to report. */
static void check_ended(dh_fw_reason reason, bool opened)
{
    check_tidy();
    CHECK(!glue.set_to_boot && glue.begun == opened);
    CHECK(!opened || glue.aborted || glue.ended);
    storage_fw_attempt_ended(reason);
    dh_fw_record r = flash_record();
    CHECK(r.stage == DH_FW_STAGE_IDLE && r.result_state == dh_fw_reason_state(reason) &&
          r.result_reason == reason && !r.result_answered && r.attempts_left == 4);
    dh_fw_report report;
    CHECK(storage_fw_report_next(&report) && report.state == dh_fw_reason_state(reason) &&
          report.reason == reason && strcmp(report.offer_id, OFFER) == 0);
    /* Any answer settles it, and that is saved. */
    storage_fw_report_answered(&report);
    CHECK(flash_record().result_answered && !storage_fw_report_next(&report));
}

static void test_time_limits(void)
{
    /* Nothing for 30 seconds, part-way through the image: given up, by the display's clock. */
    fresh_display();
    take_offer();
    glue.body_len = form_len - 100;
    glue.stalls = true;
    CHECK(receive() == DH_FW_REASON_DOWNLOAD);
    CHECK(glue.now_us >= 30 * 1000000LL && glue.now_us < 40 * 1000000LL);
    CHECK(glue.written_len > 0);
    check_ended(DH_FW_REASON_DOWNLOAD, true);

    /* Nothing at all after the answer's headers. */
    fresh_display();
    take_offer();
    glue.body_len = 0;
    glue.stalls = true;
    CHECK(receive() == DH_FW_REASON_DOWNLOAD);
    CHECK(glue.now_us >= 30 * 1000000LL && glue.now_us < 40 * 1000000LL);
    check_ended(DH_FW_REASON_DOWNLOAD, false);

    /* A byte every 20 seconds never stalls for 30, but 15 minutes in all is the limit. */
    fresh_display();
    take_offer();
    glue.piece = 1;
    glue.read_takes_us = 20 * 1000000LL;
    CHECK(receive() == DH_FW_REASON_DOWNLOAD);
    CHECK(glue.now_us >= 900 * 1000000LL && glue.now_us < 940 * 1000000LL);
    CHECK(glue.handed >= 44 && glue.handed <= 46);
    check_ended(DH_FW_REASON_DOWNLOAD, false);

    /* The same pace is fine for a download that is done within the 15 minutes. */
    fresh_display();
    take_offer();
    glue.piece = 32;
    glue.read_takes_us = 20 * 1000000LL;
    CHECK(receive() == DH_FW_REASON_NONE);
    CHECK(glue.set_to_boot);

    /* The connection ends early. */
    fresh_display();
    take_offer();
    glue.body_len = 500;
    CHECK(receive() == DH_FW_REASON_DOWNLOAD);
    check_ended(DH_FW_REASON_DOWNLOAD, true);

    /* The setup goes (a factory reset) part-way: given up at the next piece. */
    fresh_display();
    take_offer();
    stop_now = true;
    CHECK(receive() == DH_FW_REASON_DOWNLOAD);
    CHECK(glue.handed == 0);
    stop_now = false;
    check_ended(DH_FW_REASON_DOWNLOAD, false);
}

static void test_flash_failures(void)
{
    /* The spare partition won't open: nothing is written. */
    fresh_display();
    take_offer();
    glue.begin_fails = true;
    CHECK(receive() == DH_FW_REASON_FLASH);
    CHECK(glue.written_len == 0);
    check_ended(DH_FW_REASON_FLASH, false);

    /* A write fails part-way: abandoned. */
    fresh_display();
    take_offer();
    glue.piece = 64;
    glue.write_fails_after = 200;
    CHECK(receive() == DH_FW_REASON_FLASH);
    CHECK(glue.written_len <= 200 && glue.aborted);
    check_ended(DH_FW_REASON_FLASH, true);

    /* What was written isn't a valid app (esp_ota_end says so). */
    fresh_display();
    take_offer();
    glue.end_fails = true;
    CHECK(receive() == DH_FW_REASON_FLASH);
    CHECK(glue.ended && !glue.aborted);
    check_ended(DH_FW_REASON_FLASH, true);

    /* A piece that isn't in internal memory is never handed to the flash (update.c says why). */
    fresh_display();
    take_offer();
    glue.piece_is_external = true;
    CHECK(receive() == DH_FW_REASON_FLASH);
    CHECK(glue.written_len == 0 && glue.aborted);
    check_ended(DH_FW_REASON_FLASH, true);
}

static void test_failed_saves(void)
{
    dh_fw_offer_decision decision;
    dh_fw_report report;

    /* The spent attempt can't be saved: there is no attempt, and nothing may be requested. */
    fresh_display();
    glue.record_write_fails = true;
    CHECK(!storage_fw_on_offer(&DEVICE, OFFER, form_len, &decision));
    CHECK(glue.writes[0] == '\0' && storage_fw_attempts_left() == 5);
    CHECK(storage_fw_record()->stage == DH_FW_STAGE_IDLE && !storage_fw_record()->has_last_offer);
    /* The same offer is taken at the next poll, once the write works. */
    glue.record_write_fails = false;
    CHECK(storage_fw_on_offer(&DEVICE, OFFER, form_len, &decision));
    CHECK(decision.action == DH_FW_OFFER_ATTEMPT && storage_fw_attempts_left() == 4);
    CHECK(strcmp(glue.writes, "r") == 0);

    /* The image is written and checked, but the record can't be saved: the partition is never
     * set to boot, and the attempt ends as failed with `flash`. */
    fresh_display();
    take_offer();
    glue.record_write_fails = true;
    CHECK(receive() == DH_FW_REASON_FLASH);
    CHECK(glue.ended && !glue.set_to_boot && strcmp(glue.writes, "r") == 0);
    CHECK(flash_record().stage == DH_FW_STAGE_ATTEMPT);
    CHECK(storage_fw_record()->stage == DH_FW_STAGE_ATTEMPT);
    /* The result can't be saved either, so it isn't reported yet... */
    storage_fw_attempt_ended(DH_FW_REASON_FLASH);
    CHECK(!storage_fw_report_next(&report));
    CHECK(!storage_fw_on_offer(&DEVICE, "000000000000000000000000000003e9", form_len, &decision));
    /* ...and a restart now reads the attempt as cut short, as the rules say. */
    dh_fw_record cut = flash_record();
    CHECK(cut.stage == DH_FW_STAGE_ATTEMPT);
    /* Once the write works, the result is saved before it is reported. */
    glue.record_write_fails = false;
    CHECK(storage_fw_report_next(&report) && report.state == DH_FW_REPORT_FAILED &&
          report.reason == DH_FW_REASON_FLASH);
    CHECK(strcmp(glue.writes, "rr") == 0 && flash_record().result_reason == DH_FW_REASON_FLASH);
    check_tidy();

    /* The partition can't be set to boot after the record was saved: the display restarts all
     * the same, and the same firmware then reports that the update didn't start. */
    fresh_display();
    take_offer();
    glue.set_boot_fails = true;
    CHECK(receive() == DH_FW_REASON_NONE);
    CHECK(!glue.set_to_boot && flash_record().stage == DH_FW_STAGE_PENDING);
    CHECK(glue_restart(0, false));
    CHECK(storage_fw_report_next(&report) && report.state == DH_FW_REPORT_ROLLED_BACK);
    CHECK(flash_keys().security_floor == 3);

    /* Storage that isn't working takes no offer and gives no report. */
    fresh_display();
    glue.storage_unusable = true;
    CHECK(!storage_fw_on_offer(&DEVICE, OFFER, form_len, &decision));
    CHECK(!storage_fw_report_next(&report) && glue.writes[0] == '\0');
}

/* ---- The trial boot ---- */

/* Starts the count of writes to flash again. */
static void forget_writes(void)
{
    memset(glue.writes, 0, sizeof glue.writes);
    glue.keys_writes = 0;
    glue.record_writes = 0;
}

static void check_in(void *arg)
{
    bool *marked = arg;
    *marked = update_trial_check_in(DH_FW_CHECK_IN_ANSWERED);
}

/* Installs the package and restarts into it, on trial, with the trial's clock started. */
static void start_trial(void)
{
    fresh_display();
    take_offer();
    CHECK(receive() == DH_FW_REASON_NONE);
    forget_writes();
    CHECK(glue_restart(1, true));
    CHECK(storage_fw_on_trial());
    update_trial_start();
    CHECK(glue.task_made);
    /* Nothing is written by starting on trial, and nothing is reported. */
    dh_fw_report report;
    CHECK(glue.writes[0] == '\0' && !storage_fw_report_next(&report));
}

static void test_trial(void)
{
    dh_fw_report report;
    dh_fw_offer_decision decision;
    bool marked = false;

    /* Not a trial boot: no task, and a check-in does nothing. */
    fresh_display();
    update_trial_start();
    CHECK(!glue.task_made && !update_trial_check_in(DH_FW_CHECK_IN_ANSWERED));
    CHECK(glue.marked_valid == 0);

    /* The hub answers: marked good first, then the keys, then the record, then the report. */
    start_trial();
    CHECK(!update_trial_check_in(DH_FW_CHECK_IN_FAILED) && glue.marked_valid == 0);
    CHECK(!storage_fw_on_offer(&DEVICE, "000000000000000000000000000003e9", form_len, &decision) ||
          decision.action == DH_FW_OFFER_BUSY);
    glue.now_us = 299 * 1000000LL;
    CHECK(update_trial_check_in(DH_FW_CHECK_IN_ANSWERED));
    CHECK(glue.marked_valid == 1 && glue.writes_at_marked_valid == 0);
    CHECK(strcmp(glue.writes, "kr") == 0);
    CHECK(flash_keys().security_floor == 4 && flash_record().stage == DH_FW_STAGE_IDLE);
    CHECK(!storage_fw_on_trial());
    CHECK(storage_fw_report_next(&report) && report.state == DH_FW_REPORT_INSTALLED &&
          strcmp(report.offer_id, OFFER) == 0);
    /* A second answer changes nothing, and the clock's task ends without going back. */
    CHECK(!update_trial_check_in(DH_FW_CHECK_IN_ANSWERED) && glue.marked_valid == 1);
    CHECK(glue_run_task() && glue.task_ended && glue.rolled_back == 0 && glue.restarts == 0);
    check_tidy();

    /* "Set up again" counts as reaching the hub. */
    start_trial();
    CHECK(update_trial_check_in(DH_FW_CHECK_IN_SET_UP_AGAIN) && glue.marked_valid == 1);

    /* No answer in five minutes: back to the version before, at 300 seconds by the display's
     * own clock, with nothing saved. */
    start_trial();
    CHECK(glue_run_task());
    CHECK(glue.rolled_back == 1 && glue.restarts == 1 && glue.rolled_back_at_us == 300 * 1000000LL);
    CHECK(glue.writes[0] == '\0' && glue.marked_valid == 0);
    /* The version before settles it: the release's floor is never applied, and it reports. */
    CHECK(glue_restart(0, false));
    CHECK(flash_keys().security_floor == 3);
    CHECK(storage_fw_report_next(&report) && report.state == DH_FW_REPORT_ROLLED_BACK);

    /* An answer that comes at 300 seconds is too late: the image is never marked good. */
    start_trial();
    glue.now_us = 300 * 1000000LL;
    CHECK(!update_trial_check_in(DH_FW_CHECK_IN_ANSWERED) && glue.marked_valid == 0);
    CHECK(glue_run_task() && glue.rolled_back == 1);

    /* The image can't be marked good: nothing is applied, and the display restarts so the
     * bootloader goes back. */
    start_trial();
    glue.mark_valid_error = ESP_FAIL;
    CHECK(glue_run_until_restart(check_in, &marked));
    CHECK(glue.restarts == 1 && glue.writes[0] == '\0' && glue.marked_valid == 0);

    /* Marked good, but the keys can't be saved: the record isn't written ahead of them, and
     * nothing is reported until both are in flash. */
    start_trial();
    glue.keys_write_fails = true;
    CHECK(update_trial_check_in(DH_FW_CHECK_IN_ANSWERED) && glue.marked_valid == 1);
    CHECK(glue.writes[0] == '\0' && !storage_fw_report_next(&report));
    CHECK(flash_record().stage == DH_FW_STAGE_PENDING && flash_keys().security_floor == 3);
    glue.keys_write_fails = false;
    CHECK(storage_fw_report_next(&report) && report.state == DH_FW_REPORT_INSTALLED);
    CHECK(strcmp(glue.writes, "kr") == 0 && flash_keys().security_floor == 4);

    /* The keys are saved but the record isn't, and the display restarts: the next start finds
     * the release waiting in the running partition, no longer on trial, and finishes the job. */
    start_trial();
    glue.record_write_fails = true;
    CHECK(update_trial_check_in(DH_FW_CHECK_IN_ANSWERED));
    CHECK(strcmp(glue.writes, "k") == 0 && !storage_fw_report_next(&report));
    glue.record_write_fails = false;
    forget_writes();
    CHECK(glue_restart(1, false));
    CHECK(flash_keys().security_floor == 4 && flash_record().stage == DH_FW_STAGE_IDLE);
    CHECK(storage_fw_report_next(&report) && report.state == DH_FW_REPORT_INSTALLED);

    /* Nothing to go back to: the task stops asking, and the display carries on, on trial. */
    start_trial();
    glue.roll_back_error = ESP_ERR_OTA_ROLLBACK_FAILED;
    CHECK(glue_run_task() && glue.task_ended && glue.restarts == 0 && glue.rolled_back == 0);
    CHECK(storage_fw_on_trial() && glue.writes[0] == '\0');

    /* The OTA data can't be written: a restart does the same job. */
    start_trial();
    glue.roll_back_error = ESP_FAIL;
    CHECK(glue_run_task() && glue.restarts == 1 && glue.writes[0] == '\0');
    CHECK(!glue.mutex_misused);
}

int main(void)
{
    load_package();
    test_install();
    test_time_limits();
    test_flash_failures();
    test_failed_saves();
    test_trial();
    glue_reset();
    printf("update glue tests: %d checks, %d failures\n", checks, failures);
    return failures == 0 ? 0 : 1;
}
