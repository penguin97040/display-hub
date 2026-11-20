// What storage keeps for signed updates (dh_update.h): the trusted keys with the security floor,
// the update record, and this start's session. storage.c holds them and writes them; this is how
// the tasks that install updates (hub.c, update.c) use them. It is a header of its own, with
// nothing from ESP-IDF in it, so the host tests can build update.c against a stand-in.

#pragma once

#include <stdbool.h>
#include <stdint.h>

#include "dh_update.h"

#ifdef __cplusplus
extern "C" {
#endif

// ---- What storage.c provides (the host tests provide a stand-in) ----

// The lock around everything storage keeps (storage.h). It is recursive.
void storage_lock(void);
void storage_unlock(void);
// Whether encrypted NVS is working, so the keys and the record were read and can be written.
bool storage_fw_usable(void);
// Each writes one blob in one write and reads it back. Call them with the lock held.
bool storage_fw_write_keys(const dh_fw_keys *keys);
bool storage_fw_write_record(const dh_fw_record *record);

// ---- The keys and the record themselves ----

// In memory between writes. Anything that reads or changes them, or calls into dh_update.h or
// dh_usb.h with them, must hold storage_lock().
dh_fw_keys *storage_fw_keys(void);
dh_fw_record *storage_fw_record(void);

// Call once at start-up, with the lock held or before any other task runs, once the keys and
// the record have been loaded into the two above: settles whatever the last run left
// (dh_fw_start) and saves what that changes. running_slot is the update core's number for the
// app partition that is running (0 for ota_0), and on_trial the bootloader's word for it.
void storage_fw_settle_start(uint8_t running_slot, bool on_trial);

// Prints how things stand to the console. Call it with the lock held.
void storage_fw_log(void);

// A write of the keys or the record failed somewhere else (setup over USB writes them through
// storage.c). See "When a write fails" below.
void storage_fw_write_failed(void);

// ---- For the tasks that install updates ----
//
// Each of these takes the lock itself, calls the update core with the keys, the record and the
// session, and writes what the core says to save before it returns: the keys first, then the
// record, so the record never reaches flash ahead of keys it depends on.
//
// WHEN A WRITE FAILS. A write that is reported as failed may still have reached flash (one that
// was committed and then read back wrong, say), so from then on nobody knows which record flash
// holds. Nothing is tried again and nothing is guessed: all update work stops for the rest of
// this run. No offer is acted on, nothing more is saved, and no report is given out, because
// each of those depends on what is in flash. The pages, the polls and the heartbeat carry on.
// The next start reads flash again and settles whatever it finds (dh_fw_start), which every
// power-cut case already covers. The record is never erased to "clean up": a missing record
// reads as a new one, with a full attempt budget.

// Whether the bootloader started this image on trial (ESP_OTA_IMG_PENDING_VERIFY), whatever
// became of storage.
bool storage_image_on_trial(void);

// Counts running time towards the update attempt budget (dh_fw_tick) and saves the record when
// it changes. Call it every minute or so.
void storage_fw_tick(void);
// The attempts left, for a heartbeat.
uint8_t storage_fw_attempts_left(void);
// Whether this is new firmware on its trial boot, which sends no heartbeat (dh_update.h).
bool storage_fw_on_trial(void);

// What to do about an offer (dh_fw_on_offer). False if nothing may be done about it: storage
// isn't working, update work has stopped, or the record couldn't be saved just now (nothing was
// spent as far as memory goes, and nothing may be requested). With DH_FW_OFFER_ATTEMPT and
// true, the spent attempt and the offer id are in flash.
bool storage_fw_on_offer(const dh_fw_device *device, const char *offer_id, uint64_t size,
                         dh_fw_offer_decision *decision);
// Readies a download with a copy of the trusted keys (dh_fw_download_begin).
dh_fw_reason storage_fw_download_begin(dh_fw_download *download, const dh_fw_device *device,
                                       uint64_t offer_size, bool has_content_length,
                                       uint64_t content_length);
// The attempt was refused or failed (dh_fw_attempt_ended). If the record can't be saved, the
// next start reads the attempt as cut short.
void storage_fw_attempt_ended(dh_fw_reason reason);
// The image is written and verified (dh_fw_attempt_installed). True only once the record is in
// flash: only then may the spare partition be set to boot.
bool storage_fw_attempt_installed(const dh_fw_manifest *manifest, uint8_t slot);
// After storage_fw_attempt_installed(): the spare partition could not be set to boot, and the
// caller has made sure the bootloader will still start the running one. In memory, the record
// goes back to an attempt that is still running, so the caller can end it as failed with
// `flash` (storage_fw_attempt_ended), which is then written over the "waiting for its trial
// boot" record in flash. A power cut before that write is the "saved, but not set to boot" case.
// False, with nothing changed, if there is no such attempt.
bool storage_fw_attempt_not_set_to_boot(void);
// A check-in on a trial boot (dh_fw_trial_check_in), by the display's running time.
dh_fw_trial_step storage_fw_trial_check_in(dh_fw_check_in answer);
// On a trial boot whose five minutes ran out: there turned out to be no other firmware to go
// back to, so this one carries on. From now on the five minutes no longer count against a
// check-in: the first answer from the hub marks it good, since it is the only image that works.
void storage_fw_trial_no_way_back(void);
// The running image has just been marked good (dh_fw_marked_good). True once the keys and then
// the record are in flash; otherwise no report is given out, and the next start finishes the job.
bool storage_fw_marked_good(void);
// The final report waiting for an answer, if any (dh_fw_report_next). None once update work
// has stopped: a report must never run ahead of what is in flash.
bool storage_fw_report_next(dh_fw_report *out);
// The hub's answer settled a report (dh_fw_report_answered): it isn't sent again.
void storage_fw_report_answered(const dh_fw_report *sent);

// What the screen says about updates.
typedef struct {
    bool on_trial;       // new firmware, not yet marked good
    bool no_way_back;    // on trial past its five minutes, with nothing to go back to
    bool stopped;        // a write failed: no update work until the display restarts
    // How the last update ended, DH_FW_REPORT_NONE for none: the record's result, or a refusal
    // made from an offer alone since this start, whichever came later.
    dh_fw_report_state last_result;
    dh_fw_reason last_reason;
    uint8_t attempts_left;
} storage_fw_facts_t;
storage_fw_facts_t storage_fw_facts(void);

#ifdef __cplusplus
}
#endif
