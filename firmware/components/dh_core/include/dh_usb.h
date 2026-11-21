/*
 * The display's end of setup over USB (docs/protocol-v1.md, "Over USB"; docs/firmware-handoff.md,
 * "Waiting for setup"), following the reference display (tools/display-simulator/src/usb.rs).
 *
 * dh_usb_line.h splits the port's bytes into lines. dh_usb_handle_line() takes one line, decides
 * what it means for this display, stores what must be stored through the caller's
 * dh_setup_store, and writes the reply line. Nothing here touches flash, the port or a clock, and
 * nothing here locks anything: the caller makes sure only one task at a time is in these functions
 * or in dh_update.h's with the same keys and record.
 *
 * A `provision` is settled in this order, and the first that applies is the answer:
 *
 *   1. storage isn't working, or can't say what it holds       storage_failed
 *   2. a setup or a device token is already held                already_provisioned
 *   3. the bundle breaks a rule (dh_bundle_read)                invalid_bundle
 *   4. one of its firmware keys isn't a valid P-256 point       invalid_bundle
 *   5. the display trusts a different set of firmware keys      firmware_keys_locked
 *   6. a write fails                                            storage_failed
 *   7. otherwise                                                provisioned
 *
 * Nothing is changed before step 6. There the store is first told that a write is coming (its
 * `prepare`, if it has one), and then the writes are made in this order: the firmware keys
 * (only for a display that had none), the update record (only if the attempt budget wasn't full
 * already) and, last, the setup itself. So a failed write never leaves a setup behind: the
 * display still reads as not set up, and the bundle can be sent again.
 */
#ifndef DH_USB_H
#define DH_USB_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#include "dh_setup.h"
#include "dh_update.h"

#ifdef __cplusplus
extern "C" {
#endif

/* Room for the longest reply, with its newline and a NUL. */
#define DH_USB_REPLY_MAX 768u

/* Whether the display already holds a setup or a device token. */
typedef enum {
    DH_HELD_NOTHING,
    DH_HELD_SETUP,
    /* The store couldn't be read. Never guess: a bundle is refused with storage_failed. */
    DH_HELD_UNKNOWN,
    /* Nothing is held, but a write failed earlier in this run, so what is stored isn't known
     * until the display has restarted. A bundle is refused with storage_failed too (the
     * protocol has no other code for it), with a message that says to restart the display and
     * try again. */
    DH_HELD_RESTART_NEEDED,
} dh_held;

/* Where the firmware keeps things: encrypted NVS on the chip, a fake in the host tests. Each save
 * is one write that either happens whole or not at all, and answers false if it failed. */
typedef struct {
    void *ctx;
    dh_held (*held)(void *ctx);
    bool (*save_keys)(void *ctx, const dh_fw_keys *keys);
    bool (*save_record)(void *ctx, const dh_fw_record *record);
    bool (*save_setup)(void *ctx, const dh_setup *setup);
    /* A factory reset: erases the setup (and the device ID, token and saved snapshot, once there
     * are any). It must leave the firmware keys and the update record alone. */
    bool (*erase_setup)(void *ctx);
    /* NULL, or what to do just before the first write of a bundle that has passed every check.
     * It is called once for each such bundle and for no other, so whatever a store must do
     * before it can keep anything, and should not do for a line that will be refused anyway, goes
     * here (on the chip: making a new board's storage key, which is permanent). False refuses
     * the bundle with storage_failed, with nothing written. */
    bool (*prepare)(void *ctx);
} dh_setup_store;

/* What the display knows about itself, and what it holds. The strings must be plain text that
 * outlives the call. */
typedef struct {
    const char *device_type;
    const char *firmware_version;
    const char *hardware_revision;
    /* The size of the spare app partition, for `hello`. */
    uint32_t slot_bytes;
    /* NULL when storage isn't working: `hello` then says `provisioned: false` and leaves out the
     * `firmware` member, and every bundle is refused with storage_failed. */
    const dh_setup_store *store;
    /* The trusted keys and the update record, as loaded and after dh_fw_start(). They are changed
     * only once the matching write has succeeded. Unused when store is NULL. */
    dh_fw_keys *keys;
    dh_fw_record *record;
    /* Scratch for the bundle being read. After DH_USB_PROVISIONED it holds the setup that was
     * stored; after anything else it is wiped. */
    dh_setup *work;
    /* True: `hello` leaves the `firmware` member out although the keys and the record are
     * there, because they may not be what is stored (a write failed earlier in this run). It
     * changes nothing about how a bundle is handled. */
    bool withhold_firmware;
    /* NULL, or a function that says why storage isn't working, for the reply's message when a
     * bundle is refused with storage_failed because the store is missing, can't say what it
     * holds, or couldn't get ready (`prepare`). It is asked at that moment, so it can speak of
     * what `prepare` has just found. It answers one fixed sentence of at most 200 bytes that is
     * safe to log, or NULL for the usual words (also used if the sentence is empty or longer). */
    const char *(*storage_why)(void);
} dh_usb_display;

typedef enum {
    /* Not a message for a display: answer nothing. */
    DH_USB_IGNORED,
    /* A `hello`, answered with the display's own. */
    DH_USB_HELLO,
    /* A bundle was stored. */
    DH_USB_PROVISIONED,
    /* A bundle was refused: `error` says how, and `why` says more. */
    DH_USB_REFUSED,
} dh_usb_outcome;

typedef enum {
    DH_USB_ERROR_NONE = 0,
    DH_USB_ERROR_INVALID_BUNDLE,
    DH_USB_ERROR_ALREADY_PROVISIONED,
    DH_USB_ERROR_STORAGE_FAILED,
    DH_USB_ERROR_FIRMWARE_KEYS_LOCKED,
} dh_usb_error;

/* The wire name: "invalid_bundle" and so on. NULL for DH_USB_ERROR_NONE. */
const char *dh_usb_error_name(dh_usb_error e);

typedef struct {
    dh_usb_outcome outcome;
    dh_usb_error error; /* for DH_USB_REFUSED */
    /* For DH_USB_REFUSED: the reply's message. Fixed words, never anything from the bundle, so it
     * is safe to log. NULL otherwise. */
    const char *why;
    /* The bytes of the reply in the caller's buffer, newline included; 0 for no reply. */
    size_t reply_len;
    /* For DH_USB_PROVISIONED: whether the display took the bundle's firmware keys. */
    bool keys_taken;
} dh_usb_answer;

/* Handles one line from dh_usb_line_feed() (DH_LINE_JSON). The reply, if any, is written to
 * reply[0..cap) as one line ending in "\n", with a NUL after it; cap must be at least
 * DH_USB_REPLY_MAX. The line may hold secrets: wipe the reader's buffer afterwards
 * (dh_usb_line_wipe). */
dh_usb_answer dh_usb_handle_line(const dh_usb_display *d, const uint8_t *line, size_t len,
                                 char *reply, size_t cap);

/* Writes the display's `hello` line into out[0..cap) and returns its length, or 0 if it doesn't
 * fit. */
size_t dh_usb_hello(const dh_usb_display *d, char *out, size_t cap);

/* A factory reset: erases the setup through the store and nothing else. The firmware keys, the
 * security floor and the update record are never passed to it, so they can't change. False if
 * storage isn't working or the erase failed. */
bool dh_usb_factory_reset(const dh_usb_display *d);

#ifdef __cplusplus
}
#endif

#endif
