// Installing a signed update, and the trial boot that follows one (docs/firmware-updates.md,
// "Checks before installing" and "The trial boot"; dh_core's README, "Signed updates").
//
// What is genuine, what fits this display and when to give up are all decided by dh_core
// (dh_update.h). This file is the hardware's part: the spare app partition, the bootloader's
// record of which image to start, and the clock that sends new firmware back after five minutes.
// storage.c makes the saves the update core asks for, in its order.
//
// Nothing here logs a token, an address or anything from the setup.

#pragma once

#include <stdbool.h>
#include <stdint.h>

#include "dh_update.h"
#include "hub_http.h"

#ifdef __cplusplus
extern "C" {
#endif

// Call once, after storage_start(). If the bootloader started this image on trial, this starts
// the task that sends the display back to the previous firmware when five minutes of running
// have passed, unless update_trial_check_in() marks the image good first. It measures with
// esp_timer_get_time(), from the start of this boot, and depends on no other task: not the
// network, the screen or storage.
void update_trial_start(void);

// On a trial boot, what an authenticated request to the hub got (dh_fw_check_in_of). On success,
// or an answer that only setting the display up again would change, the running image is marked
// good, and then the release's security floor and keys are saved, and then its record. True if
// this call marked it good. False, and nothing done, when the display isn't on trial, the
// answer doesn't count, or the five minutes are up. (If they ran out and there turned out to be
// no other firmware to go back to, the five minutes no longer count: the next answer marks this
// one good, since it is the only image that works.)
bool update_trial_check_in(dh_fw_check_in answer);

// Reads a download to its end, checking it as it arrives (dh_fw_download_push): nothing is
// written to flash before the manifest has passed every check, and no byte past the update form
// is read. `download` is an open 200 answer, `has_length` and `length` what it said of its
// Content-Length, and `started_ms` when the request was made (esp_timer_get_time() / 1000): the
// download is given up when it sends nothing for 30 seconds or takes 15 minutes in all, or when
// `stop` says the setup has gone.
//
// Returns DH_FW_REASON_NONE when the display must restart now, and *restart says which restart
// it is. Otherwise *restart is UPDATE_NO_RESTART and the return is the reason the attempt ended,
// with nothing set to boot; the caller ends the attempt (storage_fw_attempt_ended) and reports
// it. That includes the spare partition failing to be set to boot while the running one is
// still the one that starts: `flash`, and no restart. The caller closes the download either way.
typedef enum {
    UPDATE_NO_RESTART,
    // The image is written and verified, the record is saved and the spare partition is set to
    // boot: the new firmware starts next, on trial.
    UPDATE_RESTART_INTO_NEW,
    // The record is saved, but the spare partition couldn't be set to boot and the attempt
    // couldn't be ended here (update.c, boot_not_set). The firmware that is running starts
    // again, or which one starts can't be told; either way the start settles the record.
    UPDATE_RESTART_TO_SETTLE,
} update_restart_t;

dh_fw_reason update_receive(hub_download_t *download, const dh_fw_device *device,
                            uint64_t offer_size, bool has_length, uint64_t length,
                            int64_t started_ms, bool (*stop)(void), update_restart_t *restart);

#ifdef __cplusplus
}
#endif
