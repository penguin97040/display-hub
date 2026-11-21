// The storage key: the secret in eFuse key block KEY5 that encrypted NVS hangs from.
//
// The firmware keeps the Wi-Fi password and the device token in encrypted NVS, with ESP-IDF's
// HMAC scheme: the encryption keys are worked out at start-up by the chip's HMAC unit from a
// secret key held in eFuse key block KEY5 (docs/firmware-handoff.md, "Storage and NVS"). This
// file looks at KEY5 before NVS is started, and NVS is started only when it is safe to:
//
// - KEY5 holds an HMAC_UP key that is read-protected, write-protected and has its purpose
//   locked, as ESP-IDF's own key generation leaves it: encrypted NVS is started with it. Nothing
//   is burnt.
// - KEY5 is empty and nothing else about the chip is unexpected: this is a new board. Nothing
//   is burnt at start-up, and NVS isn't started: nothing is stored yet, so there is nothing to
//   read. The key is made when the first setup arrives over USB, just before that setup's first
//   write (storage_key_make): ESP-IDF's own key generation makes a random key and burns it into
//   KEY5, for good. That is the intended behaviour, and the console says so plainly, before and
//   after.
// - Anything else (KEY5 has another purpose, isn't empty, or holds an HMAC_UP key that isn't
//   protected; a key block reads back as damaged; or one of the conditions below fails): NVS is
//   NOT started and nothing is burnt. The display shows a storage error and refuses setup.
//
// WHY NOT AT START-UP. ESP-IDF burns the key block first and block 0 (the purpose and the
// protection bits) second (efuse/esp32s3/esp_efuse_utility.c:103-104). A reset between the two
// leaves KEY5 holding a key with no purpose and no protection, which the checks here refuse for
// good. Just after an install is when a reset is most likely: the app opening the USB port can
// reset the chip. When a `provision` arrives the port is open and the app is waiting for the
// answer, so the burn is made then and no sooner.
//
// A KEY THAT WAS NEVER FINISHED (KEY5 not empty, purpose USER, unprotected) is not adopted, and
// no other key block is used in its place. It can't be told apart from a key someone loaded
// into a second-hand board and kept a copy of, and with that copy they could decrypt everything
// the display stores. It can't be finished either: a key block can only be programmed once (its
// error-check symbols are burnt with it), so more random bits can't be added on top. Using
// another empty block was looked at and left out: ESP-IDF's HMAC scheme would take a key id at
// run time (nvs_sec_provider_register_hmac), but every start would then have to choose between
// blocks from eFuse readings alone, which is one more way to pick a key somebody else planted,
// and none of it could be tried without using up real boards. So the state is reported, in
// words that say what happened (status->unfinished), and the board can't be set up with this
// firmware.
//
// The only function here that can change an eFuse is storage_key_make().

#pragma once

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#include "efuse_report.h"
#include "esp_err.h"

#ifdef __cplusplus
extern "C" {
#endif

// The eFuse key block the storage key lives in: KEY5, as CONFIG_NVS_SEC_HMAC_EFUSE_KEY_ID says.
#define STORAGE_KEY_BLOCK 5

typedef enum {
    STORAGE_KEY_PRESENT, // KEY5 holds a protected HMAC_UP key: start encrypted NVS with it
    STORAGE_KEY_EMPTY,   // KEY5 is empty and everything else is as expected: a key may be made
    STORAGE_KEY_BLOCKED, // something isn't as expected: don't start NVS, and burn nothing
} storage_key_state_t;

#define STORAGE_KEY_MAX_BLOCKERS 12

typedef struct {
    storage_key_state_t state;
    // Why storage can't be used, in plain words. Empty unless the state is BLOCKED.
    const char *blockers[STORAGE_KEY_MAX_BLOCKERS];
    int blocker_count;
    // BLOCKED because KEY5 holds a key that was never given its purpose and protection (see the
    // top of this file). The screen and the USB reply say so in words of their own.
    bool unfinished;
} storage_key_status_t;

// Works out the state from an eFuse report. Read-only.
void storage_key_assess(const efuse_report_t *efuse, storage_key_status_t *status);

// Makes the storage key and burns it into KEY5, PERMANENTLY. Only for a state of
// STORAGE_KEY_EMPTY: it reads the eFuses again itself, before and after, logs both, and refuses
// unless they still say so. `efuse` and `status` are brought up to date. ESP_OK only if KEY5
// then holds a usable key (STORAGE_KEY_PRESENT). *burn_tried says whether ESP-IDF's key
// generation, the one call that can burn, was reached: false means it declined before that,
// with certainty that no eFuse was touched; true with an error means an eFuse may have been,
// whatever the readings say. Call it only when a setup is about to be written (storage.c,
// store_prepare).
esp_err_t storage_key_make(efuse_report_t *efuse, storage_key_status_t *status,
                           bool *burn_tried);

// Starts encrypted NVS on one NVS partition, by its name in the partition table, with the keys
// the HMAC unit works out from KEY5. Only for a state of STORAGE_KEY_PRESENT. It can't make a
// key and never erases anything.
esp_err_t storage_key_start_partition(const storage_key_status_t *status, const char *partition);

// True if this run burnt the key and the burn changed eFuse bits other than KEY5's own (key,
// purpose and protection). Reported by storage_log() every minute.
bool storage_key_burn_changed_more(void);

// Prints the state to the console.
void storage_key_log(const storage_key_status_t *status);

#ifdef __cplusplus
}
#endif
