/*
 * Tests for the firmware's own part in a signed update, on a computer: main/update.c (reading a
 * download, writing the spare partition, the trial boot's clock) and main/storage_fw.c (the
 * saves, in their order), over the stand-in for the board in glue/. The firmware vectors run
 * through the same code as dh_fw_conform_glue; these are the cases a vector can't describe: the
 * two time limits (also against a hub that trickles bytes), flash calls and saves that fail, a
 * partition that can't be set to boot, a setup that goes mid-download, a refusal made from the
 * offer alone, an image that was given up (which must be left unable to start), the trial
 * boot's five minutes against a check-in, and a start whose image state can't be read.
 *
 * The package is the one in protocol/v1/firmware/package-01-valid.json: version 1.2.0, security
 * counter 4, for a display that trusts `maker` and `recovery` with a floor of 3.
 */
#include <inttypes.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "dh_hub.h"
#include "dh_json.h"
#include "dh_update.h"
#include "esp_err.h"
#include "esp_ota_ops.h"
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

/* The setup goes (a factory reset) now, or once the display's clock reaches stop_at_us. */
static bool stop_now;
static int64_t stop_at_us;
static bool stop(void)
{
    return stop_now || (stop_at_us != 0 && glue.now_us >= stop_at_us);
}

/* How long before now the download's request was made. */
static int64_t asked_ago_us;

/* Which restart the last receive() asked for. */
static update_restart_t restart;

static dh_fw_reason receive(void)
{
    glue.keep_next_free = true;
    const dh_fw_reason reason = update_receive(NULL, &DEVICE, form_len, true, form_len,
                                               (glue.now_us - asked_ago_us) / 1000, stop,
                                               &restart);
    /* A restart is asked for exactly when no reason is given. */
    CHECK((reason == DH_FW_REASON_NONE) == (restart != UPDATE_NO_RESTART));
    return reason;
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
    /* Nothing is ever erased but the first sector of the spare partition, once it is closed. */
    CHECK(!glue.erased_elsewhere);
    /* One read never asks for more than 256 bytes (update.c, "How long one read can take"). */
    CHECK(!glue.read_past_the_form && glue.most_asked <= 256);
}

/* ---- A download ---- */

static void test_install(void)
{
    static const size_t pieces[] = {1, 137, 4096};
    for (size_t i = 0; i < 2 * sizeof pieces / sizeof pieces[0]; i++) {
        fresh_display();
        take_offer();
        glue.piece = pieces[i / 2];
        /* Bytes that come in one run, and bytes that come in bursts with pauses between. */
        glue.pauses = i % 2 == 1;
        CHECK(receive() == DH_FW_REASON_NONE && restart == UPDATE_RESTART_INTO_NEW);
        check_tidy();
        const dh_fw_manifest *m = dh_fw_download_manifest(glue.download);
        CHECK(m != NULL && strcmp(m->firmware_version, "1.2.0") == 0);
        /* Opened for writes in order, written with exactly the image, checked, and set to
         * boot only once the record was in flash. */
        CHECK(glue.begun && glue.sequential && glue.ended && !glue.aborted && glue.set_to_boot);
        CHECK(glue.spare_spoilt == 0);
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
    /* An image that was given up can't be started: its first sector is erased, once. A
     * partition that was never opened is left as it was. */
    CHECK(glue.spare_spoilt == (opened ? 1u : 0u));
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

    /* A hub that trickles on purpose: every read is kept open as long as it can be, a byte
     * just inside each wait. One read can't be cut short, so the limits are overshot, but by
     * no more than one read: 256 bytes of 50 ms each, 12.8 seconds. */
    fresh_display();
    take_offer();
    glue.piece = 256;
    glue.read_takes_us_per_byte = 49999;
    /* Finding the hub took 880 of the 900 seconds, so the limit falls inside the second read. */
    glue.now_us = 880 * 1000000LL;
    asked_ago_us = 880 * 1000000LL;
    CHECK(receive() == DH_FW_REASON_DOWNLOAD);
    CHECK(glue.now_us >= 900 * 1000000LL && glue.now_us < 900 * 1000000LL + 12800000LL);
    CHECK(glue.handed == 512);
    asked_ago_us = 0;
    check_ended(DH_FW_REASON_DOWNLOAD, glue.begun);

    /* The same hub, and a factory reset part-way: seen within one read. */
    fresh_display();
    take_offer();
    glue.piece = 256;
    glue.read_takes_us_per_byte = 49999;
    stop_at_us = 20 * 1000000LL;
    CHECK(receive() == DH_FW_REASON_DOWNLOAD);
    CHECK(glue.now_us >= 20 * 1000000LL && glue.now_us < 20 * 1000000LL + 12800000LL);
    stop_at_us = 0;
    check_ended(DH_FW_REASON_DOWNLOAD, glue.begun);

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

/* An image that was given up is left unable to start (update.c, spoil_spare). */
static void test_abandoned_image(void)
{
    static uint8_t bad[sizeof form];
    uint8_t start[16];

    /* The whole image arrives and doesn't match the manifest's hash: what was written could be
     * a complete app that nobody signed. Its first sector is erased, after the partition is
     * closed, and reads back as blank. */
    fresh_display();
    take_offer();
    memcpy(bad, form, form_len);
    bad[form_len - 1] ^= 0x01u;
    glue.body = bad;
    /* In bursts, so that most of the image is written before its last piece is refused. */
    glue.piece = 64;
    glue.pauses = true;
    CHECK(receive() == DH_FW_REASON_IMAGE_HASH);
    CHECK(glue.aborted && glue.spare_spoilt == 1 && glue.written_when_spoilt > 0);
    CHECK(esp_partition_read(esp_ota_get_next_update_partition(NULL), 0, start, sizeof start) ==
          ESP_OK);
    for (size_t i = 0; i < sizeof start; i++) {
        CHECK(start[i] == 0xff);
    }
    check_ended(DH_FW_REASON_IMAGE_HASH, true);

    /* The erase fails: the update still ends as it would have, and nothing else is touched. */
    fresh_display();
    take_offer();
    glue.body = bad;
    glue.erase_fails = true;
    CHECK(receive() == DH_FW_REASON_IMAGE_HASH);
    CHECK(glue.aborted && glue.spare_spoilt == 0 && !glue.set_to_boot);
    check_tidy();

    /* The record of the release can't be saved: the image is whole and genuine, but it must
     * never start without its record, so it is spoilt too. */
    fresh_display();
    take_offer();
    glue.record_write_fails = true;
    CHECK(receive() == DH_FW_REASON_FLASH);
    CHECK(glue.ended && !glue.set_to_boot && glue.spare_spoilt == 1);
    check_tidy();

    /* It can't be set to boot, and the display still starts the firmware it is running. */
    fresh_display();
    take_offer();
    glue.set_boot_fails = true;
    CHECK(receive() == DH_FW_REASON_FLASH && restart == UPDATE_NO_RESTART);
    CHECK(glue.spare_spoilt == 1);
    check_tidy();

    /* Whenever a restart is asked for, the image may be the one that starts: never spoilt. */
    fresh_display();
    take_offer();
    glue.set_boot_fails = true;
    glue.set_boot_lands = true;
    CHECK(receive() == DH_FW_REASON_NONE && restart == UPDATE_RESTART_INTO_NEW);
    CHECK(glue.spare_spoilt == 0);
    fresh_display();
    take_offer();
    glue.set_boot_fails = true;
    glue.boot_unreadable = true;
    CHECK(receive() == DH_FW_REASON_NONE && restart == UPDATE_RESTART_TO_SETTLE);
    CHECK(glue.spare_spoilt == 0);
    check_tidy();

    /* The OTA data names the spare partition as the one to start next (it can't be read as
     * the running one): left alone. */
    fresh_display();
    take_offer();
    glue.body = bad;
    glue.set_boot_fails = true; /* only so that set_boot_lands is looked at */
    glue.set_boot_lands = true;
    CHECK(receive() == DH_FW_REASON_IMAGE_HASH);
    CHECK(glue.aborted && glue.spare_spoilt == 0);
    check_tidy();

    /* ESP-IDF gone wrong, naming the running partition as the spare one and letting it be
     * opened: nothing of the running partition is ever erased. */
    fresh_display();
    take_offer();
    glue.body = bad;
    glue.spare_is_running = true;
    CHECK(receive() == DH_FW_REASON_IMAGE_HASH);
    CHECK(glue.begun && glue.aborted && glue.spare_spoilt == 0);
    check_tidy();
}

/* After a write failed, nothing about updates is done until the display restarts. */
static void check_stopped(const char *writes_so_far)
{
    dh_fw_offer_decision decision;
    dh_fw_report report;
    CHECK(storage_fw_facts().stopped);
    /* Later writes would work, and none is made. */
    glue.keys_write_fails = false;
    glue.record_write_fails = false;
    CHECK(!storage_fw_on_offer(&DEVICE, "000000000000000000000000000003e9", form_len, &decision));
    CHECK(decision.action == DH_FW_OFFER_IGNORE);
    CHECK(!storage_fw_report_next(&report));
    storage_fw_attempt_ended(DH_FW_REASON_FLASH);
    storage_fw_report_answered(&report);
    glue.now_us += 5 * 3600 * 1000000LL;
    storage_fw_tick();
    CHECK(!storage_fw_attempt_installed(NULL, 1));
    CHECK(!storage_fw_report_next(&report));
    CHECK(strcmp(glue.writes, writes_so_far) == 0);
    check_tidy();
}

static void test_failed_saves(void)
{
    dh_fw_offer_decision decision;
    dh_fw_report report;

    /* The spent attempt can't be saved: nothing may be requested, and update work stops. */
    fresh_display();
    glue.record_write_fails = true;
    CHECK(!storage_fw_facts().stopped);
    CHECK(!storage_fw_on_offer(&DEVICE, OFFER, form_len, &decision));
    CHECK(decision.action == DH_FW_OFFER_IGNORE && storage_fw_attempts_left() == 5);
    check_stopped("");
    /* Flash was never written, so the restart starts clean and takes the offer. */
    CHECK(glue_restart(0, false) && !storage_fw_facts().stopped);
    CHECK(storage_fw_on_offer(&DEVICE, OFFER, form_len, &decision));
    CHECK(decision.action == DH_FW_OFFER_ATTEMPT && strcmp(glue.writes, "r") == 0);

    /* The same, but the write that "failed" reached flash (committed, then read back wrong).
     * This run does nothing more; the next start finds a spent attempt and reports it as cut
     * short. Memory was never trusted to say which record flash holds. */
    fresh_display();
    glue.record_write_fails = true;
    glue.failed_write_lands = true;
    CHECK(!storage_fw_on_offer(&DEVICE, OFFER, form_len, &decision));
    CHECK(flash_record().stage == DH_FW_STAGE_ATTEMPT && flash_record().attempts_left == 4);
    check_stopped("");
    CHECK(glue_restart(0, false));
    CHECK(storage_fw_report_next(&report) && report.state == DH_FW_REPORT_FAILED &&
          report.reason == DH_FW_REASON_DOWNLOAD && storage_fw_attempts_left() == 4);

    /* The image is written and checked, but the record can't be saved: the partition is never
     * set to boot, nothing is reported, and the next start settles the attempt. */
    fresh_display();
    take_offer();
    glue.record_write_fails = true;
    CHECK(receive() == DH_FW_REASON_FLASH);
    CHECK(glue.ended && !glue.set_to_boot);
    CHECK(flash_record().stage == DH_FW_STAGE_ATTEMPT);
    check_stopped("r");
    CHECK(glue_restart(0, false));
    CHECK(storage_fw_report_next(&report) && report.state == DH_FW_REPORT_FAILED &&
          report.reason == DH_FW_REASON_DOWNLOAD);

    /* The same, with the failed write in flash after all: the record says a release is waiting
     * in a partition that was never set to boot, which the next start reports as gone back. */
    fresh_display();
    take_offer();
    glue.record_write_fails = true;
    glue.failed_write_lands = true;
    CHECK(receive() == DH_FW_REASON_FLASH);
    CHECK(!glue.set_to_boot && flash_record().stage == DH_FW_STAGE_PENDING);
    check_stopped("r");
    CHECK(glue_restart(0, false));
    CHECK(storage_fw_report_next(&report) && report.state == DH_FW_REPORT_ROLLED_BACK);
    CHECK(flash_keys().security_floor == 3);

    /* A result that can't be saved isn't reported in this run. */
    fresh_display();
    take_offer();
    glue.body_len = 500;
    CHECK(receive() == DH_FW_REASON_DOWNLOAD);
    glue.record_write_fails = true;
    storage_fw_attempt_ended(DH_FW_REASON_DOWNLOAD);
    CHECK(!storage_fw_report_next(&report));
    check_stopped("r");

    /* A write that fails somewhere else (setup over USB) stops update work too. */
    fresh_display();
    storage_fw_write_failed();
    check_stopped("");

    /* Storage that isn't working takes no offer and gives no report. */
    fresh_display();
    glue.storage_unusable = true;
    CHECK(!storage_fw_on_offer(&DEVICE, OFFER, form_len, &decision));
    CHECK(!storage_fw_report_next(&report) && glue.writes[0] == '\0');
}

/* Uses up the copy storage_fw.c keeps for putting the record back, as if something else had
 * changed the record between the save and the partition being set to boot. */
static void record_changes_behind_its_back(void)
{
    (void)storage_fw_attempt_not_set_to_boot();
    storage_fw_record()->stage = DH_FW_STAGE_PENDING;
}

/* esp_ota_set_boot_partition() fails after the record was saved. */
static void test_not_set_to_boot(void)
{
    dh_fw_report report;

    /* The OTA data still names the running firmware: the update ends as failed with `flash`,
     * with no restart, and that is what is saved and reported. */
    fresh_display();
    take_offer();
    glue.set_boot_fails = true;
    CHECK(receive() == DH_FW_REASON_FLASH);
    CHECK(!glue.set_to_boot && strcmp(glue.writes, "rr") == 0);
    CHECK(flash_record().stage == DH_FW_STAGE_PENDING);
    CHECK(storage_fw_record()->stage == DH_FW_STAGE_ATTEMPT &&
          storage_fw_record()->pending_version[0] == '\0');
    check_ended(DH_FW_REASON_FLASH, true);
    CHECK(flash_keys().security_floor == 3);
    CHECK(storage_fw_facts().last_result == DH_FW_REPORT_FAILED);

    /* A power cut before that result was saved is the "saved, but not set to boot" case. */
    fresh_display();
    take_offer();
    glue.set_boot_fails = true;
    CHECK(receive() == DH_FW_REASON_FLASH);
    CHECK(glue_restart(0, false));
    CHECK(storage_fw_report_next(&report) && report.state == DH_FW_REPORT_ROLLED_BACK);

    /* The write landed and was reported as failed: the OTA data names the new firmware, so
     * this is an install. Restart, with the record of the release kept for its trial boot. */
    fresh_display();
    take_offer();
    glue.set_boot_fails = true;
    glue.set_boot_lands = true;
    CHECK(receive() == DH_FW_REASON_NONE && restart == UPDATE_RESTART_INTO_NEW);
    CHECK(flash_record().stage == DH_FW_STAGE_PENDING && strcmp(glue.writes, "rr") == 0);
    CHECK(glue_restart(1, true) && storage_fw_on_trial());

    /* The OTA data can't be read: the record is left as it is and the display restarts, so
     * whichever firmware starts settles it. */
    fresh_display();
    take_offer();
    glue.set_boot_fails = true;
    glue.boot_unreadable = true;
    CHECK(receive() == DH_FW_REASON_NONE && restart == UPDATE_RESTART_TO_SETTLE);
    CHECK(flash_record().stage == DH_FW_STAGE_PENDING && strcmp(glue.writes, "rr") == 0);
    CHECK(glue_restart(0, false));
    CHECK(storage_fw_report_next(&report) && report.state == DH_FW_REPORT_ROLLED_BACK);

    /* The OTA data still names the running firmware, but the record in memory is no longer the
     * one this attempt saved, so it can't be put back and ended here. The display restarts, and
     * the start settles it from flash: nothing new is saved before that. */
    fresh_display();
    take_offer();
    glue.set_boot_fails = true;
    glue.on_set_boot = record_changes_behind_its_back;
    /* Not a restart into the new firmware, and it mustn't be called one. */
    CHECK(receive() == DH_FW_REASON_NONE && restart == UPDATE_RESTART_TO_SETTLE);
    CHECK(!glue.set_to_boot && strcmp(glue.writes, "rr") == 0);
    CHECK(flash_record().stage == DH_FW_STAGE_PENDING);
    CHECK(glue_restart(0, false));
    CHECK(storage_fw_report_next(&report) && report.state == DH_FW_REPORT_ROLLED_BACK);
    CHECK(flash_keys().security_floor == 3);

    /* Nothing to put back when no release is waiting. */
    fresh_display();
    CHECK(!storage_fw_attempt_not_set_to_boot());
    take_offer();
    CHECK(!storage_fw_attempt_not_set_to_boot());
    CHECK(storage_fw_record()->stage == DH_FW_STAGE_ATTEMPT);
}

/* A final report the hub answers but never takes. */
static void test_reports_not_taken(void)
{
    dh_fw_offer_decision decision;
    dh_fw_report report, other;

    fresh_display();
    take_offer();
    storage_fw_attempt_ended(DH_FW_REASON_IMAGE_HASH);
    CHECK(strcmp(glue.writes, "rr") == 0);
    CHECK(storage_fw_report_next(&report));
    /* Nine answers that don't settle it: it goes on waiting, nothing is written, and no new
     * offer is acted on meanwhile. Requests that got no answer are never counted (the caller
     * doesn't call this for them). */
    for (unsigned i = 1; i < DH_FW_REPORT_MAX_REFUSALS; i++) {
        CHECK(!storage_fw_report_not_taken(&report));
        CHECK(storage_fw_report_next(&other) && other.state == DH_FW_REPORT_FAILED);
    }
    CHECK(strcmp(glue.writes, "rr") == 0 && !flash_record().result_answered);
    CHECK(storage_fw_on_offer(&DEVICE, "000000000000000000000000000003e9", form_len, &decision));
    CHECK(decision.action == DH_FW_OFFER_BUSY);
    /* A report that isn't the one waiting changes nothing. */
    other = report;
    other.state = DH_FW_REPORT_ROLLED_BACK;
    other.reason = DH_FW_REASON_NONE;
    CHECK(!storage_fw_report_not_taken(&other));
    CHECK(storage_fw_report_next(&other));
    /* The tenth: given up, saved as settled, and the next offer is taken. */
    CHECK(storage_fw_report_not_taken(&report));
    CHECK(strcmp(glue.writes, "rrr") == 0 && flash_record().result_answered);
    CHECK(!storage_fw_report_next(&other));
    CHECK(flash_record().result_state == DH_FW_REPORT_FAILED);
    CHECK(storage_fw_on_offer(&DEVICE, "000000000000000000000000000003e9", form_len, &decision));
    CHECK(decision.action == DH_FW_OFFER_ATTEMPT);

    /* The count is kept in memory: a restart starts it again, and the report still waits. */
    fresh_display();
    take_offer();
    storage_fw_attempt_ended(DH_FW_REASON_IMAGE_HASH);
    CHECK(storage_fw_report_next(&report));
    for (unsigned i = 1; i < DH_FW_REPORT_MAX_REFUSALS; i++) {
        CHECK(!storage_fw_report_not_taken(&report));
    }
    CHECK(glue_restart(0, false));
    CHECK(storage_fw_report_next(&report));
    for (unsigned i = 1; i < DH_FW_REPORT_MAX_REFUSALS; i++) {
        CHECK(!storage_fw_report_not_taken(&report));
    }
    CHECK(storage_fw_report_not_taken(&report) && !storage_fw_report_next(&other));

    /* An answer that settles it in between ends the count: the next report starts from none. */
    fresh_display();
    take_offer();
    storage_fw_attempt_ended(DH_FW_REASON_IMAGE_HASH);
    CHECK(storage_fw_report_next(&report));
    for (unsigned i = 1; i < DH_FW_REPORT_MAX_REFUSALS; i++) {
        CHECK(!storage_fw_report_not_taken(&report));
    }
    storage_fw_report_answered(&report);
    CHECK(storage_fw_on_offer(&DEVICE, "000000000000000000000000000003e9", form_len, &decision));
    CHECK(decision.action == DH_FW_OFFER_ATTEMPT);
    storage_fw_attempt_ended(DH_FW_REASON_DOWNLOAD);
    CHECK(storage_fw_report_next(&report));
    for (unsigned i = 1; i < DH_FW_REPORT_MAX_REFUSALS; i++) {
        CHECK(!storage_fw_report_not_taken(&report));
    }
    CHECK(storage_fw_report_next(&other));
    CHECK(storage_fw_report_not_taken(&report));

    /* A refusal made from the offer alone is given up the same way, with nothing to write. */
    dh_fw_keys none;
    dh_fw_record record;
    dh_fw_keys_init(&none);
    dh_fw_record_init(&record);
    glue_reset();
    glue_start(&none, &record, 0, false);
    CHECK(storage_fw_on_offer(&DEVICE, OFFER, form_len, &decision));
    CHECK(decision.action == DH_FW_OFFER_REFUSE && storage_fw_report_next(&report));
    for (unsigned i = 1; i < DH_FW_REPORT_MAX_REFUSALS; i++) {
        CHECK(!storage_fw_report_not_taken(&report));
    }
    CHECK(storage_fw_report_not_taken(&report));
    CHECK(!storage_fw_report_next(&other) && glue.writes[0] == '\0');

    /* Once update work has stopped, nothing is counted or settled. */
    fresh_display();
    take_offer();
    storage_fw_attempt_ended(DH_FW_REASON_IMAGE_HASH);
    CHECK(storage_fw_report_next(&report));
    storage_fw_write_failed();
    for (unsigned i = 0; i < 2 * DH_FW_REPORT_MAX_REFUSALS; i++) {
        CHECK(!storage_fw_report_not_taken(&report));
    }
    CHECK(strcmp(glue.writes, "rr") == 0 && !flash_record().result_answered);
}

/* Whoever asked is told, once, when update work stops. */
static unsigned told_stopped;
static void on_stopped(void)
{
    told_stopped++;
    /* It is called with storage's lock held. */
    CHECK(glue.lock_depth > 0);
}

static void test_told_when_stopped(void)
{
    dh_fw_offer_decision decision;
    storage_fw_on_stopped(on_stopped);

    /* A failed write on the update's own path. */
    fresh_display();
    told_stopped = 0;
    glue.record_write_fails = true;
    CHECK(!storage_fw_on_offer(&DEVICE, OFFER, form_len, &decision));
    CHECK(told_stopped == 1 && storage_fw_facts().stopped);
    /* Later calls don't say it again. */
    storage_lock();
    storage_fw_write_failed();
    storage_unlock();
    storage_fw_attempt_ended(DH_FW_REASON_FLASH);
    CHECK(told_stopped == 1);

    /* A failed write somewhere else (setup over USB), on a display that isn't set up. */
    fresh_display();
    told_stopped = 0;
    storage_lock();
    storage_fw_write_failed();
    storage_unlock();
    CHECK(told_stopped == 1 && storage_fw_stopped());

    /* A restart starts clean, and a write that works says nothing. */
    told_stopped = 0;
    CHECK(glue_restart(0, false) && !storage_fw_stopped());
    take_offer();
    CHECK(told_stopped == 0);
    storage_fw_on_stopped(NULL);
}

/* What the screen is told. */
static void test_facts(void)
{
    dh_fw_offer_decision decision;
    dh_fw_report report;
    dh_fw_keys none;
    dh_fw_record record;

    /* A display with no keys refuses from the offer alone: nothing is written, and the refusal
     * is the last result all the same. */
    dh_fw_keys_init(&none);
    dh_fw_record_init(&record);
    glue_reset();
    glue_start(&none, &record, 0, false);
    CHECK(storage_fw_facts().last_result == DH_FW_REPORT_NONE);
    CHECK(storage_fw_on_offer(&DEVICE, OFFER, form_len, &decision));
    CHECK(decision.action == DH_FW_OFFER_REFUSE && glue.writes[0] == '\0');
    CHECK(storage_fw_facts().last_result == DH_FW_REPORT_REFUSED &&
          storage_fw_facts().last_reason == DH_FW_REASON_NO_TRUSTED_KEYS);
    /* It stays the last result once the hub has the report, and is gone after a restart. */
    CHECK(storage_fw_report_next(&report) && report.reason == DH_FW_REASON_NO_TRUSTED_KEYS);
    storage_fw_report_answered(&report);
    CHECK(!storage_fw_report_next(&report));
    CHECK(storage_fw_facts().last_result == DH_FW_REPORT_REFUSED);
    CHECK(glue_restart(0, false) && storage_fw_facts().last_result == DH_FW_REPORT_NONE);

    /* Too large for the display, then an offer that is attempted and fails: the later one is
     * the last result. */
    fresh_display();
    CHECK(storage_fw_on_offer(&DEVICE, OFFER, 137u + 4096u + 65537u, &decision));
    CHECK(decision.action == DH_FW_OFFER_REFUSE);
    CHECK(storage_fw_facts().last_result == DH_FW_REPORT_REFUSED &&
          storage_fw_facts().last_reason == DH_FW_REASON_TOO_LARGE);
    CHECK(storage_fw_report_next(&report));
    storage_fw_report_answered(&report);
    CHECK(storage_fw_on_offer(&DEVICE, "000000000000000000000000000003e9", form_len, &decision));
    CHECK(decision.action == DH_FW_OFFER_ATTEMPT);
    storage_fw_attempt_ended(DH_FW_REASON_IMAGE_HASH);
    CHECK(storage_fw_facts().last_result == DH_FW_REPORT_FAILED &&
          storage_fw_facts().last_reason == DH_FW_REASON_IMAGE_HASH);
    CHECK(storage_fw_facts().attempts_left == 4 && !storage_fw_facts().on_trial);

    /* Setup over USB changes the keys or the record: a refusal remembered from before is
     * forgotten, on the screen and for the offer, which is then looked at afresh. */
    glue_reset();
    glue_start(&none, &record, 0, false);
    CHECK(storage_fw_on_offer(&DEVICE, OFFER, form_len, &decision));
    CHECK(decision.action == DH_FW_OFFER_REFUSE);
    /* Nothing new is acted on while its report waits, and the offer itself is remembered. */
    CHECK(storage_fw_on_offer(&DEVICE, OFFER, form_len, &decision));
    CHECK(decision.action == DH_FW_OFFER_BUSY);
    CHECK(storage_fw_report_next(&report));
    storage_fw_report_answered(&report);
    CHECK(storage_fw_on_offer(&DEVICE, OFFER, form_len, &decision));
    CHECK(decision.action == DH_FW_OFFER_IGNORE);
    CHECK(storage_fw_facts().last_result == DH_FW_REPORT_REFUSED);
    /* A bundle is accepted: the display takes its keys (dh_usb.h writes them and takes them
     * into memory). */
    storage_lock();
    *storage_fw_keys() = display_keys;
    storage_fw_changed_by_setup();
    storage_unlock();
    CHECK(storage_fw_facts().last_result == DH_FW_REPORT_NONE);
    CHECK(!storage_fw_report_next(&report));
    /* The same when the refusal's report was still waiting: it is dropped on purpose, because
     * the offer is looked at again with the new keys and that outcome is what gets reported. */
    glue_reset();
    glue_start(&none, &record, 0, false);
    CHECK(storage_fw_on_offer(&DEVICE, OFFER, form_len, &decision));
    CHECK(decision.action == DH_FW_OFFER_REFUSE && storage_fw_report_next(&report));
    storage_lock();
    *storage_fw_keys() = display_keys;
    storage_fw_changed_by_setup();
    storage_unlock();
    CHECK(!storage_fw_report_next(&report));
    CHECK(storage_fw_on_offer(&DEVICE, OFFER, form_len, &decision));
    CHECK(decision.action == DH_FW_OFFER_ATTEMPT);
    glue_reset();
    glue_start(&none, &record, 0, false);
    CHECK(storage_fw_on_offer(&DEVICE, OFFER, form_len, &decision));
    CHECK(storage_fw_report_next(&report));
    storage_fw_report_answered(&report);
    storage_lock();
    *storage_fw_keys() = display_keys;
    storage_fw_changed_by_setup();
    storage_unlock();
    CHECK(storage_fw_on_offer(&DEVICE, OFFER, form_len, &decision));
    CHECK(decision.action == DH_FW_OFFER_ATTEMPT);

    /* What an enrolment or a heartbeat may say about firmware: the keys and the record while
     * they are known to match flash, and nothing once a write has failed. */
    const dh_fw_keys *keys = NULL;
    const dh_fw_record *rec = NULL;
    fresh_display();
    storage_lock();
    CHECK(storage_fw_reportable(&keys, &rec) && keys == storage_fw_keys() &&
          rec == storage_fw_record() && !storage_fw_stopped());
    storage_unlock();
    char body[DH_HUB_REQUEST_MAX];
    dh_device_info info = {"llm-usage-display", "1.1.0", "simulated", 65536, keys, rec};
    CHECK(dh_heartbeat_request(&info, body, sizeof body) > 0 && strstr(body, "\"firmware\":{"));
    glue.record_write_fails = true;
    CHECK(!storage_fw_on_offer(&DEVICE, OFFER, form_len, &decision));
    storage_lock();
    CHECK(!storage_fw_reportable(&keys, &rec) && keys == NULL && rec == NULL &&
          storage_fw_stopped());
    storage_unlock();
    info.keys = keys;
    info.record = rec;
    CHECK(dh_heartbeat_request(&info, body, sizeof body) > 0 && !strstr(body, "firmware\""));
    CHECK(dh_enrol_request(&info, "s", 1, body, sizeof body) > 0 && !strstr(body, "firmware\""));
    /* Storage that isn't working at all: the same. */
    fresh_display();
    glue.storage_unusable = true;
    storage_lock();
    CHECK(!storage_fw_reportable(&keys, &rec) && keys == NULL && rec == NULL);
    storage_unlock();
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

    /* Marked good, but the keys can't be saved: the record is never written ahead of them,
     * nothing is reported, and update work stops. The next start finds the release waiting in
     * the running partition, no longer on trial, and finishes the job. */
    start_trial();
    glue.keys_write_fails = true;
    CHECK(update_trial_check_in(DH_FW_CHECK_IN_ANSWERED) && glue.marked_valid == 1);
    CHECK(flash_record().stage == DH_FW_STAGE_PENDING && flash_keys().security_floor == 3);
    check_stopped("");
    CHECK(glue_restart(1, false));
    CHECK(strcmp(glue.writes, "kr") == 0 && flash_keys().security_floor == 4);
    CHECK(storage_fw_report_next(&report) && report.state == DH_FW_REPORT_INSTALLED);

    /* The keys are saved but the record isn't: the same. */
    start_trial();
    glue.record_write_fails = true;
    CHECK(update_trial_check_in(DH_FW_CHECK_IN_ANSWERED));
    check_stopped("k");
    forget_writes();
    CHECK(glue_restart(1, false));
    CHECK(flash_keys().security_floor == 4 && flash_record().stage == DH_FW_STAGE_IDLE);
    CHECK(storage_fw_report_next(&report) && report.state == DH_FW_REPORT_INSTALLED);

    /* Nothing to go back to: the clock stops, and the display carries on, on trial. It is the
     * only firmware that works, so the next answer from the hub marks it good, however late. */
    start_trial();
    glue.roll_back_error = ESP_ERR_OTA_ROLLBACK_FAILED;
    CHECK(!storage_fw_facts().no_way_back);
    CHECK(glue_run_task() && glue.task_ended && glue.restarts == 0 && glue.rolled_back == 0);
    CHECK(storage_fw_on_trial() && storage_fw_facts().no_way_back && glue.writes[0] == '\0');
    glue.now_us = 3600 * 1000000LL;
    CHECK(!update_trial_check_in(DH_FW_CHECK_IN_FAILED) && glue.marked_valid == 0);
    CHECK(!storage_fw_report_next(&report));
    CHECK(update_trial_check_in(DH_FW_CHECK_IN_ANSWERED) && glue.marked_valid == 1);
    CHECK(strcmp(glue.writes, "kr") == 0 && glue.writes_at_marked_valid == 0);
    CHECK(!storage_fw_on_trial() && !storage_fw_facts().no_way_back);
    CHECK(flash_keys().security_floor == 4);
    CHECK(storage_fw_report_next(&report) && report.state == DH_FW_REPORT_INSTALLED);
    /* A late answer only counts when there was nothing to go back to. */
    start_trial();
    glue.now_us = 3600 * 1000000LL;
    CHECK(!update_trial_check_in(DH_FW_CHECK_IN_ANSWERED) && glue.marked_valid == 0);

    /* The OTA data can't be written: a restart does the same job. */
    start_trial();
    glue.roll_back_error = ESP_FAIL;
    CHECK(glue_run_task() && glue.restarts == 1 && glue.writes[0] == '\0');
    CHECK(!glue.mutex_misused);
}

/* Installs the package and restarts into it with the bootloader's record of the image
 * unreadable (or saying anything but "on trial" or "marked good"). */
static void start_unknown(uint8_t running_slot)
{
    fresh_display();
    take_offer();
    CHECK(receive() == DH_FW_REASON_NONE);
    forget_writes();
    glue.image_unknown = true;
    CHECK(glue_restart(running_slot, false));
}

/* "On trial" fails closed (storage_fw.h): with a release waiting in the running partition, only
 * an image that reads back as marked good is taken as marked good. */
static void test_image_state_unknown(void)
{
    dh_fw_report report;

    /* Unknown, with the release waiting in the partition that is running: a trial boot. Nothing
     * of the release is applied, nothing is written and nothing is reported. */
    start_unknown(1);
    CHECK(storage_fw_on_trial() && storage_image_on_trial() && storage_fw_facts().on_trial);
    CHECK(glue.writes[0] == '\0' && flash_keys().security_floor == 3);
    CHECK(flash_record().stage == DH_FW_STAGE_PENDING && !storage_fw_report_next(&report));
    /* Its clock runs, and a check-in marks it good: only then the floor, the keys, the record. */
    update_trial_start();
    CHECK(glue.task_made);
    CHECK(update_trial_check_in(DH_FW_CHECK_IN_ANSWERED));
    CHECK(glue.marked_valid == 1 && glue.writes_at_marked_valid == 0);
    CHECK(strcmp(glue.writes, "kr") == 0 && flash_keys().security_floor == 4);
    CHECK(storage_fw_report_next(&report) && report.state == DH_FW_REPORT_INSTALLED);
    check_tidy();

    /* The same start with no check-in: back to the version before at 300 seconds, with nothing
     * of the release ever applied. */
    start_unknown(1);
    update_trial_start();
    CHECK(glue_run_task());
    CHECK(glue.rolled_back == 1 && glue.rolled_back_at_us == 300 * 1000000LL);
    CHECK(glue.writes[0] == '\0' && flash_keys().security_floor == 3);

    /* Read back as marked good: the last start ended between marking and saving, and this one
     * finishes the job (as before). */
    fresh_display();
    take_offer();
    CHECK(receive() == DH_FW_REASON_NONE);
    forget_writes();
    CHECK(glue_restart(1, false));
    CHECK(!storage_fw_on_trial() && !storage_image_on_trial());
    CHECK(strcmp(glue.writes, "kr") == 0 && flash_keys().security_floor == 4);

    /* Unknown, in the other partition: the version before, which settles it as rolled back. */
    start_unknown(0);
    CHECK(!storage_fw_on_trial() && !storage_image_on_trial());
    CHECK(flash_keys().security_floor == 3);
    CHECK(storage_fw_report_next(&report) && report.state == DH_FW_REPORT_ROLLED_BACK);

    /* Unknown, with no release waiting (a first install reads this way): an ordinary start. */
    fresh_display();
    glue.image_unknown = true;
    CHECK(glue_restart(0, false));
    CHECK(!storage_fw_on_trial() && !storage_image_on_trial() && glue.writes[0] == '\0');
    update_trial_start();
    CHECK(!glue.task_made);
}

int main(void)
{
    load_package();
    test_install();
    test_time_limits();
    test_flash_failures();
    test_abandoned_image();
    test_failed_saves();
    test_not_set_to_boot();
    test_reports_not_taken();
    test_told_when_stopped();
    test_facts();
    test_trial();
    test_image_state_unknown();
    glue_reset();
    printf("update glue tests: %d checks, %d failures\n", checks, failures);
    return failures == 0 ? 0 : 1;
}
