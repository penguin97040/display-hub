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

// ---- For the tasks that install updates ----
//
// Each of these takes the lock itself, calls the update core with the keys, the record and the
// session, and writes what the core says to save before it returns: the keys first, then the
// record. A write that failed is tried again before any later one, in the same order, so the
// record never reaches flash ahead of keys it depends on.

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

// What to do about an offer (dh_fw_on_offer). False if the record couldn't be saved: nothing
// was spent, nothing may be requested, and the offer is looked at again at the next poll. With
// DH_FW_OFFER_ATTEMPT and true, the spent attempt and the offer id are in flash.
bool storage_fw_on_offer(const dh_fw_device *device, const char *offer_id, uint64_t size,
                         dh_fw_offer_decision *decision);
// Readies a download with a copy of the trusted keys (dh_fw_download_begin).
dh_fw_reason storage_fw_download_begin(dh_fw_download *download, const dh_fw_device *device,
                                       uint64_t offer_size, bool has_content_length,
                                       uint64_t content_length);
// The attempt was refused or failed (dh_fw_attempt_ended). If the record can't be saved, the
// next start reads the attempt as cut short, and the write is tried again before then.
void storage_fw_attempt_ended(dh_fw_reason reason);
// The image is written and verified (dh_fw_attempt_installed). True only once the record is in
// flash: only then may the spare partition be set to boot.
bool storage_fw_attempt_installed(const dh_fw_manifest *manifest, uint8_t slot);
// A check-in on a trial boot (dh_fw_trial_check_in), by the display's running time.
dh_fw_trial_step storage_fw_trial_check_in(dh_fw_check_in answer);
// The running image has just been marked good (dh_fw_marked_good). True once the keys and then
// the record are in flash; until then no report is given out (storage_fw_report_next).
bool storage_fw_marked_good(void);
// The final report waiting for an answer, if any (dh_fw_report_next). None while a save is
// still owed: a report must never run ahead of what is in flash.
bool storage_fw_report_next(dh_fw_report *out);
// The hub answered a report (dh_fw_report_answered).
void storage_fw_report_answered(const dh_fw_report *sent);

// What the screen says about updates.
typedef struct {
    bool on_trial;                  // new firmware, not yet marked good
    dh_fw_report_state last_result; // the last update's final result, DH_FW_REPORT_NONE for none
    dh_fw_reason last_reason;
    uint8_t attempts_left;
} storage_fw_facts_t;
storage_fw_facts_t storage_fw_facts(void);

#ifdef __cplusplus
}
#endif
