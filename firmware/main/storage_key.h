// The storage key check (milestone D2).
//
// The official firmware will keep the Wi-Fi password and the device token in encrypted NVS, with
// ESP-IDF's HMAC scheme: the encryption keys are worked out at start-up by the chip's HMAC unit
// from a secret key held in eFuse key block KEY5 (docs/firmware-handoff.md, "Storage and NVS").
// Putting that key there is permanent. This build does it once, on a real board, only after two
// deliberate taps on the screen, and then checks what Espressif's pages don't say: that the key
// can't be read back, and that the board can still be reflashed over USB.
//
// The only function here that can change an eFuse is storage_key_burn(). Everything else reads.

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
    STORAGE_KEY_CAN_BURN, // KEY5 is empty and everything else is as expected: offer the burn
    STORAGE_KEY_BLOCKED,  // no key yet, and something isn't as expected: don't offer it
    STORAGE_KEY_PRESENT,  // KEY5 holds an HMAC_UP key: run the checks
} storage_key_state_t;

#define STORAGE_KEY_MAX_BLOCKERS 12

typedef struct {
    storage_key_state_t state;
    // Why the burn isn't offered, in plain words. Empty unless the state is BLOCKED.
    const char *blockers[STORAGE_KEY_MAX_BLOCKERS];
    int blocker_count;
} storage_key_status_t;

typedef enum {
    CHECK_NOT_RUN = 0,
    CHECK_PASS,
    CHECK_FAIL,
} check_result_t;

typedef struct {
    bool ran;

    // The key itself.
    check_result_t key_purpose;    // KEY5's purpose is HMAC_UP
    check_result_t key_protected;  // KEY5 is read-protected and write-protected, purpose locked
    check_result_t key_unreadable; // reading KEY5 gives all zeros
    bool key_readable;             // the opposite, for the report: true is a failed check

    // Encrypted NVS.
    check_result_t nvs_started;    // the HMAC scheme gave keys and encrypted NVS started
    esp_err_t nvs_error;           // why not, if it didn't
    check_result_t stored_data;    // the count and marker from the last start read back correctly
    bool first_start;              // nothing was stored yet: the first start since an erase
    uint32_t boot_count;           // after adding this start
    check_result_t written_data;   // this start's count and marker were written and read back
    check_result_t raw_flash;      // the marker's plain bytes are nowhere in the raw NVS partition
    bool plain_text_found;         // the marker or its names were found there: not encrypted
    bool scanner_works;            // the search found the marker in a test buffer
    uint32_t raw_bytes_used;       // bytes of the raw partition that aren't blank (0xFF)

    // Only when the key was burnt during this start.
    check_result_t burn_changed_only_key; // nothing but KEY5's own eFuse bits changed

    bool all_passed;
} storage_key_checks_t;

// Works out the state from an eFuse report. Read-only.
void storage_key_assess(const efuse_report_t *efuse, storage_key_status_t *status);

// Burns the storage key into KEY5. PERMANENT. Call it only after the confirmation on the screen.
// It reads the eFuses itself, before and after, logs both, and refuses if the state isn't
// STORAGE_KEY_CAN_BURN. `before` and `after` are filled in for the caller. Returns ESP_OK if the
// key was burnt.
esp_err_t storage_key_burn(efuse_report_t *before, efuse_report_t *after);

// Fills in `checks->burn_changed_only_key` from the two readings around a burn.
void storage_key_check_burn(const efuse_report_t *before, const efuse_report_t *after,
                            storage_key_checks_t *checks);

// Runs the checks. Call it once per start, and only when the state is STORAGE_KEY_PRESENT: it
// never makes a key. It writes to the NVS partition (a count and a marker in a namespace of its
// own). Keeps what storage_key_check_burn() filled in.
void storage_key_run_checks(const efuse_report_t *efuse, storage_key_checks_t *checks);

// Prints the state and the checks to the console.
void storage_key_log(const storage_key_status_t *status, const storage_key_checks_t *checks);

#ifdef __cplusplus
}
#endif
