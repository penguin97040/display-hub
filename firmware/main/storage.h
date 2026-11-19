// What the display keeps in flash, in encrypted NVS (docs/firmware-handoff.md, "Storage and NVS").
//
// Two namespaces, so a factory reset can erase one and leave the other:
//
//   dh_setup  "setup"   the setup from the USB bundle (hub hosts, port, trust anchors, the
//                       enrolment secret, Wi-Fi), as dh_setup_encode() writes it. Later
//                       milestones add the device ID, token and saved snapshot here.
//   dh_fw     "keys"    the trusted firmware keys and the security floor (dh_fw_keys_encode)
//             "record"  the update record, with the attempt budget (dh_fw_record_encode)
//
// A factory reset erases all of dh_setup and never touches dh_fw.
//
// The keys, the record and the session live here, in memory, between writes. Anything that reads
// or changes them, or calls into dh_update.h or dh_usb.h with them, must hold storage_lock().
// Nothing here logs a secret: only counts and yes/no facts.

#pragma once

#include <stdbool.h>
#include <stdint.h>

#include "dh_update.h"
#include "dh_usb.h"
#include "efuse_report.h"
#include "esp_err.h"
#include "storage_key.h"

#ifdef __cplusplus
extern "C" {
#endif

typedef enum {
    STORAGE_WORKING,
    // The storage key can't be used (storage_key.h): NVS was never started.
    STORAGE_NO_KEY,
    // Encrypted NVS didn't start, or what it holds couldn't be read.
    STORAGE_FAILED,
} storage_state_t;

// What is known about the stored setup, without any of its secrets.
typedef struct {
    storage_state_t state;
    bool set_up;           // a setup is held: the display refuses another until it is reset
    bool setup_unreadable; // one is held, but it isn't in a form this firmware reads
    uint8_t hub_hosts;     // how many addresses it has for the hub
    bool wifi_set;         // whether it came with Wi-Fi details
} storage_facts_t;

// Starts storage: looks at the storage key, starts encrypted NVS (making the key on a new
// board's first start), and loads the firmware keys, the update record and the setup. Call it
// once, before the USB task starts. `efuse` is the reading taken at start-up, and is brought up to
// date if a key was made. It never erases the NVS partition.
void storage_start(efuse_report_t *efuse);

// Take this around every use of the things below. It is recursive.
void storage_lock(void);
void storage_unlock(void);

storage_facts_t storage_facts(void);
// Why storage isn't working, for the screen and the log.
const storage_key_status_t *storage_key_status(void);

// For dh_usb.h: NULL while storage isn't working.
const dh_setup_store *storage_store(void);
dh_fw_keys *storage_fw_keys(void);
dh_fw_record *storage_fw_record(void);
// The size of the spare app partition, 0 if there is none.
uint32_t storage_spare_slot_bytes(void);

// Prints the facts to the console.
void storage_log(void);

#ifdef __cplusplus
}
#endif
