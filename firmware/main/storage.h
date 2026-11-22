// What the display keeps in flash, in encrypted NVS (docs/firmware-handoff.md, "Storage and NVS").
//
// Two NVS partitions, so a factory reset can erase one, every sector of it, and leave the other:
//
//   nvs_setup  dh_setup  "setup"     the setup from the USB bundle (hub hosts, port, trust
//                                    anchors, the enrolment secret until it is used, Wi-Fi), as
//                                    dh_setup_encode() writes it
//                        "device"    the device ID and token the hub gave it (dh_device_encode)
//                        "snapshot"  the last good snapshot, as the hub sent it (at most 32 KiB)
//   nvs        dh_fw     "keys"      the trusted firmware keys and the security floor
//                                    (dh_fw_keys_encode)
//                        "record"    the update record, with the attempt budget
//                                    (dh_fw_record_encode)
//              dh_reset  "wiping"    there only while a factory reset's erase is under way
//
// A factory reset erases the whole of nvs_setup, in flash itself, and never erases anything in
// nvs (storage.c says why a partition and not a namespace). Both are encrypted with the same
// storage key.
//
// A NEW BOARD has no storage key, so nothing can be stored on it and encrypted NVS isn't
// started. It reads as working and not set up, with no firmware keys and a new update record.
// Its key is made, permanently, when its first setup arrives and has passed every check
// (the store's `prepare`; storage_key.h says why not sooner).
//
// The keys, the record and the session live in memory between writes (storage_fw.c, which also
// has every call that changes them for an update). Anything that reads or changes them, or calls
// into dh_update.h or dh_usb.h with them, must hold storage_lock().
// Nothing here logs a secret: only counts and yes/no facts.
//
// THE SETUP'S GENERATION. The task that talks to the hub (hub.c) works from a copy of the setup,
// and a request can be in flight for many seconds. Meanwhile a factory reset may erase the setup,
// and a new bundle may even arrive. So every setup has a number, which changes whenever one is
// stored or erased, and everything that task writes names the number of the setup it belongs to.
// A write for a setup that has gone is refused: a token from the old hub can never be stored
// beside a new setup, and nothing comes back after a factory reset.

#pragma once

#include <stdbool.h>
#include <stdint.h>

#include "dh_hub.h"
#include "dh_update.h"
#include "dh_usb.h"
#include "efuse_report.h"
#include "esp_err.h"
#include "storage_fw.h"
#include "storage_key.h"

#ifdef __cplusplus
extern "C" {
#endif

typedef enum {
    STORAGE_WORKING,
    // The storage key can't be used (storage_key.h): NVS isn't started.
    STORAGE_NO_KEY,
    // The stored data itself can't be used (a flash layout without the setup partition, or a
    // main NVS partition that NVS says must be erased): only Install firmware puts it right.
    STORAGE_FAILED,
    // Storage is out of use for this run, for a reason that may pass: it didn't start or
    // couldn't be read (a read that failed, no memory, a storage key that may just have been
    // burnt), or it was working until a write failed and couldn't be undone. A restart is the
    // first thing to try; Install firmware only if it keeps happening. `stop` in the facts
    // says which, and whether anything of the setup was erased.
    STORAGE_NOT_STARTED,
} storage_state_t;

// With STORAGE_NOT_STARTED: why, and what became of the setup partition in this run. The
// screen says "nothing was erased" only for the first.
typedef enum {
    // It didn't start, or couldn't be read. Nothing was erased.
    STORAGE_STOP_NOT_STARTED,
    // An erase of the setup partition began and couldn't finish (to finish a factory reset,
    // because NVS said the partition couldn't be used as it was, or for a reset asked for
    // while storage was out of use). Its mark is in flash, so the next start finishes it. (Or
    // the mark couldn't be read after a write to it failed, so it may be: storage.c,
    // erase_setup_partition.)
    STORAGE_STOP_ERASE_UNFINISHED,
    // The setup partition was erased, for one of the same reasons, and storage is out of use
    // all the same.
    STORAGE_STOP_ERASED,
    // Storage was working until a write failed and what it left couldn't be erased. Nothing
    // more is read or written until a restart has read what is really there.
    STORAGE_STOP_WRITE,
} storage_stop_t;

// What is known about the stored setup, without any of its secrets.
typedef struct {
    storage_state_t state;
    storage_stop_t stop;   // with STORAGE_NOT_STARTED: why
    bool set_up;           // a setup is held: the display refuses another until it is reset
    bool setup_unreadable; // one is held, but it isn't in a form this firmware reads
    uint8_t hub_hosts;     // how many addresses it has for the hub
    bool wifi_set;         // whether it came with Wi-Fi details
    bool enrolled;         // a device ID and token are held
    uint32_t snapshot_bytes; // the size of the saved snapshot, 0 for none
    // A factory reset began erasing the setup and couldn't finish (or couldn't take its mark
    // out). No setup is taken until the reset has been done again, or the display restarted:
    // the next start finishes the erase, because the reset's mark is in flash. It is set with
    // that mark in place, or where the mark may be there and can't be read: the setup is out
    // of use either way, so nothing is in use that the next start may erase. (A reset that
    // couldn't even begin, with the mark known not to be in flash, leaves the setup as it was
    // and sets nothing here.)
    bool reset_unfinished;
} storage_facts_t;

// Starts storage: looks at the storage key and, if there is one, starts encrypted NVS and loads
// the firmware keys, the update record and the setup. Nothing is burnt here. Call it once,
// before the USB task starts. `efuse` is the reading taken at start-up; storage keeps a copy,
// and brings it up to date if a key is made later (storage_efuse_report). It never erases the
// main NVS partition; the setup partition is erased only to finish a factory reset that was cut
// short, or if NVS says it can't be used as it is (no free pages, or a newer NVS's format).
void storage_start(const efuse_report_t *efuse);

// Take this around every use of the things below. It is recursive.
void storage_lock(void);
void storage_unlock(void);

storage_facts_t storage_facts(void);
// Why storage isn't working, for the screen and the log, and the eFuse reading as it is now.
// Both are copies taken under the lock: the USB task rewrites them when it makes a new board's
// storage key.
storage_key_status_t storage_key_status(void);
void storage_efuse_report(efuse_report_t *out);

// How a factory reset ended.
typedef enum {
    // The setup partition is blank (or this is a new board, which has never stored anything).
    STORAGE_RESET_DONE,
    // The erase began and couldn't finish, or its mark may be in flash and can't be taken out
    // or read. Another reset, or a restart with the mark in flash, finishes it.
    // (`reset_unfinished` in the facts, while storage is working.)
    STORAGE_RESET_UNFINISHED,
    // Nothing was erased, the mark isn't in flash, and the setup is in use as it was.
    STORAGE_RESET_NOT_STARTED,
    // Storage isn't working and NVS never started in this run, or the flash layout has no
    // setup partition, so nothing can be erased safely. Only Install firmware erases the
    // display then.
    STORAGE_RESET_NO_STORAGE,
} storage_reset_t;

// The factory reset (the BOOT button): erases the setup partition, every sector of it, and
// nothing else. It works whenever NVS is running on the main partition, where the reset's mark
// is kept: also when storage is out of use for this run after a read or a write that failed
// (the setup is erased, and storage stays out of use until a restart). Takes the lock itself.
storage_reset_t storage_factory_reset(void);
// Whether storage_factory_reset() could erase anything: false only where it would answer
// STORAGE_RESET_NO_STORAGE. Takes the lock itself.
bool storage_reset_possible(void);

// For dh_usb.h: NULL while storage isn't working.
const dh_setup_store *storage_store(void);
// For dh_usb.h's storage_why: words for the app when a bundle is refused for a reason worth
// telling apart (a storage key that was never finished, or couldn't be made just now; a
// factory reset that didn't finish). NULL for the usual words. Call it with the lock held.
const char *storage_usb_words(void);
// The size of the spare app partition, 0 if there is none.
uint32_t storage_spare_slot_bytes(void);

// ---- For the task that talks to the hub (hub.c). Each takes the lock itself. ----

// The number of the setup that is held now. It changes whenever a setup is stored or erased.
uint32_t storage_setup_generation(void);

// Copies the stored setup out, with its generation. False if there is none, it can't be read or
// storage isn't working. It holds secrets: wipe it (dh_setup_wipe) when done.
bool storage_load_setup(dh_setup *out, uint32_t *generation);

typedef enum {
    STORAGE_HELD,
    STORAGE_NOT_HELD,
    // Something is stored but can't be read, or the setup it belonged to has gone.
    STORAGE_UNREADABLE,
} storage_held_t;

// The device ID and token. Wipe the copy (dh_device_wipe) when done.
storage_held_t storage_load_device(uint32_t generation, dh_device *out);
// Stores them, in one write that is read back. False if the write failed or the setup has gone.
bool storage_save_device(uint32_t generation, const dh_device *device);

// Rewrites the stored setup without its enrolment secret. Call it once enrolment has succeeded
// (after storage_save_device) or been refused. If the write fails the old setup stays.
bool storage_clear_secret(uint32_t generation);

// The saved snapshot: copies it into out and returns its length, or 0 if there is none, it is
// longer than cap or it can't be read.
size_t storage_load_snapshot(uint32_t generation, uint8_t *out, size_t cap);
// Saves a snapshot body of 1 to 32768 bytes. The buffer is used to read it back: it holds the
// same bytes afterwards if this returns true, and anything otherwise. A failed save leaves no
// saved snapshot, never part of one.
bool storage_save_snapshot(uint32_t generation, uint8_t *body, size_t len);

// What storage keeps for signed updates, and the calls that change it, are in storage_fw.h.

// Prints the facts to the console.
void storage_log(void);

#ifdef __cplusplus
}
#endif
