// The storage key: the secret in eFuse key block KEY5 that encrypted NVS hangs from.
//
// The firmware keeps the Wi-Fi password and the device token in encrypted NVS, with ESP-IDF's
// HMAC scheme: the encryption keys are worked out at start-up by the chip's HMAC unit from a
// secret key held in eFuse key block KEY5 (docs/firmware-handoff.md, "Storage and NVS"). This
// file looks at KEY5 before NVS is started, and starts NVS only when it is safe to:
//
// - KEY5 holds an HMAC_UP key: encrypted NVS is started with it. Nothing is burnt.
// - KEY5 is empty and nothing else about the chip is unexpected: this is a new board's first
//   start. ESP-IDF's own key generation makes a random key and burns it into KEY5, for good.
//   That is the intended behaviour, and the console says so plainly, before and after.
// - Anything else (KEY5 has another purpose or isn't empty, a key block reads back as damaged,
//   or one of the conditions below fails): NVS is NOT started and nothing is burnt. The display
//   shows a storage error and refuses setup.
//
// The only function here that can change an eFuse is storage_key_start_nvs(), and only in the
// second case.

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
    STORAGE_KEY_PRESENT, // KEY5 holds an HMAC_UP key: start encrypted NVS with it
    STORAGE_KEY_EMPTY,   // KEY5 is empty and everything else is as expected: a key may be made
    STORAGE_KEY_BLOCKED, // something isn't as expected: don't start NVS, and burn nothing
} storage_key_state_t;

#define STORAGE_KEY_MAX_BLOCKERS 12

typedef struct {
    storage_key_state_t state;
    // Why storage can't be used, in plain words. Empty unless the state is BLOCKED.
    const char *blockers[STORAGE_KEY_MAX_BLOCKERS];
    int blocker_count;
} storage_key_status_t;

// Works out the state from an eFuse report. Read-only.
void storage_key_assess(const efuse_report_t *efuse, storage_key_status_t *status);

// Starts encrypted NVS on the default partition, as the state allows (see the top of this file).
// With an empty KEY5 it first makes and burns the key, PERMANENTLY, reading the eFuses again
// before and after and logging both. `efuse` and `status` are brought up to date, and
// `*key_made` says whether a key was burnt during this call. It never erases the NVS partition.
// Returns ESP_OK if encrypted NVS is running.
esp_err_t storage_key_start_nvs(efuse_report_t *efuse, storage_key_status_t *status,
                                bool *key_made);

// True if this start burnt the key and the burn changed eFuse bits other than KEY5's own (key,
// purpose and protection). Reported by storage_log() every minute.
bool storage_key_burn_changed_more(void);

// Prints the state to the console.
void storage_key_log(const storage_key_status_t *status);

#ifdef __cplusplus
}
#endif
