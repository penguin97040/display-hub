/*
 * Signed firmware updates, the display's side (docs/firmware-updates.md, "What a display must
 * do"), following the reference display (tools/display-simulator, src/firmware) and
 * crates/firmware-package.
 *
 * Nothing here reads a clock, the network or flash. The caller holds two small things that are
 * kept in flash (the trusted keys with the security floor, and the update record), one that lives
 * in memory for a single start (the session) and, while a download runs, the download itself.
 * Every function works on what it is passed, and time is always the display's own running time
 * in whole seconds since this start (esp_timer_get_time() / 1000000), never a date or the hub's
 * clock.
 *
 * Nothing here locks anything. Polls, a download and the USB port may each run on a task of their
 * own, so the caller must make sure only one of them at a time is inside these functions with the
 * same keys, record and session (one mutex around them is enough).
 *
 * SAVING. A function that changes something kept in flash says so with DH_FW_SAVE_KEYS and
 * DH_FW_SAVE_RECORD, in its return value or a `save` member. The caller writes what is named
 * (the keys first when both are) before it does anything else, and in particular before the
 * step the function's comment says the save must come before. Each save is one write of one
 * blob (dh_fw_keys_encode, dh_fw_record_encode), so a power cut leaves either the old state or
 * the new one, and every point between two saves is safe to restart from: dh_fw_start() reads
 * what was left and settles it. If a write fails, don't go on to the step it guards.
 *
 * One update, start to finish:
 *
 *   dh_fw_start()             at every start, before anything else here
 *   dh_fw_tick()              now and then (a minute apart is plenty): earns attempts back
 *   dh_fw_on_offer()          for the offer in a snapshot; on DH_FW_OFFER_ATTEMPT, save, then
 *                             request the download
 *   dh_fw_download_begin()    with the answer's Content-Length
 *   dh_fw_download_push()     with each piece of the body, doing what each result says
 *   dh_fw_attempt_ended()     if it was refused or failed; save, then report
 *   dh_fw_attempt_installed() once esp_ota_end() has passed; save, THEN
 *                             esp_ota_set_boot_partition() and restart
 *   dh_fw_trial_check_in()    on the new firmware's trial boot, for each answer from the hub
 *   dh_fw_marked_good()       after esp_ota_mark_app_valid_cancel_rollback(); save, then report
 *   dh_fw_report_next()       what to report, sent again after later polls until an answer
 *                             settles it; then dh_fw_report_answered()
 */
#ifndef DH_UPDATE_H
#define DH_UPDATE_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#include "dh_crypto.h"
#include "dh_json.h"
#include "dh_limits.h"
#include "dh_snapshot.h"
#include "dh_text.h"

#ifdef __cplusplus
extern "C" {
#endif

/* ---- The package format ("The package") ---- */

#define DH_FW_HEADER_LEN 137u
/* Header bytes 0 to 72, which the signature covers: everything before the signature. */
#define DH_FW_SIGNED_HEADER_LEN 73u
#define DH_FW_MIN_MANIFEST_LEN 2u
#define DH_FW_MAX_MANIFEST_LEN 4096u
/* How deeply the manifest's JSON may nest; the manifest object itself is the first level. */
#define DH_FW_MAX_JSON_DEPTH 4u
#define DH_FW_MAX_IMAGE_SIZE (8u * 1024u * 1024u)
#define DH_FW_KEY_LEN DH_P256_KEY_LEN
#define DH_FW_SIGNATURE_LEN DH_P256_SIGNATURE_LEN
/* Trusted keys on a display, and keys in a key_set. */
#define DH_FW_MAX_KEYS 3u
#define DH_FW_MAX_HARDWARE_REVISIONS 8u
/* Characters in device_type and in one hardware revision. */
#define DH_FW_MAX_DEVICE_FIELD_CHARS 48u
/* Bytes: a firmware version is ASCII. */
#define DH_FW_MAX_VERSION_BYTES 48u
#define DH_FW_MAX_SUMMARY_CHARS 280u
/* Room for so many characters of UTF-8, and a NUL. */
#define DH_FW_TEXT_ROOM(chars) ((chars) * 4u + 1u)

/* ---- The limits on trying ("Limits on trying", "The trial boot") ---- */

#define DH_FW_FULL_BUDGET 5u
/* Credit hours that earn one attempt. Credit hours run from 0 to one less than this. */
#define DH_FW_CREDIT_HOURS_PER_ATTEMPT 4u
#define DH_FW_CREDIT_HOUR_SECONDS 3600u
#define DH_FW_TRIAL_SECONDS 300u
/* Give up on a download that sends nothing for this long, or takes this long in all. */
#define DH_FW_DOWNLOAD_STALL_SECONDS 30u
#define DH_FW_DOWNLOAD_TIMEOUT_SECONDS 900u

/* A public key: 0x04, X, Y. Holding one says nothing about whether it is a valid point. */
typedef struct {
    uint8_t bytes[DH_FW_KEY_LEN];
} dh_fw_key;

/* Why an offer was refused (nothing was written) or an attempt failed: the report's `reason`. */
typedef enum {
    DH_FW_REASON_NONE = 0,
    /* Refusals. */
    DH_FW_REASON_NO_TRUSTED_KEYS,
    DH_FW_REASON_TOO_LARGE,
    DH_FW_REASON_BAD_FORMAT,
    DH_FW_REASON_UNTRUSTED_KEY,
    DH_FW_REASON_BAD_SIGNATURE,
    DH_FW_REASON_WRONG_DEVICE_TYPE,
    DH_FW_REASON_WRONG_HARDWARE,
    DH_FW_REASON_DOWNGRADE,
    /* Failures, after the checks. The old firmware is still running. */
    DH_FW_REASON_DOWNLOAD,
    DH_FW_REASON_IMAGE_HASH,
    DH_FW_REASON_FLASH,
} dh_fw_reason;

/* The report's `state`. */
typedef enum {
    DH_FW_REPORT_NONE = 0,
    DH_FW_REPORT_DOWNLOADING,
    DH_FW_REPORT_INSTALLED,
    DH_FW_REPORT_REFUSED,
    DH_FW_REPORT_FAILED,
    DH_FW_REPORT_ROLLED_BACK,
} dh_fw_report_state;

/* The wire names: "bad_format", "rolled_back" and so on. NULL for the NONE values. */
const char *dh_fw_reason_name(dh_fw_reason r);
const char *dh_fw_report_state_name(dh_fw_report_state s);
/* The state a reason is reported with: REFUSED or FAILED (NONE for DH_FW_REASON_NONE). */
dh_fw_report_state dh_fw_reason_state(dh_fw_reason r);

/* ---- What is kept in flash ("What it stores") ---- */

#define DH_FW_SAVE_RECORD 0x1u
#define DH_FW_SAVE_KEYS 0x2u

/* The trusted keys and the security floor. Keep them in an NVS namespace of their own: a factory
 * reset must leave them alone. Only dh_fw_marked_good(), dh_fw_start() (finishing what
 * dh_fw_marked_good() began) and dh_fw_bundle_apply() ever change them. */
typedef struct {
    uint8_t key_count; /* 0 to DH_FW_MAX_KEYS */
    dh_fw_key keys[DH_FW_MAX_KEYS];
    uint32_t security_floor;
} dh_fw_keys;

typedef enum {
    DH_FW_STAGE_IDLE = 0,
    /* An attempt was spent and its download may be running. Found at a start, it was cut short. */
    DH_FW_STAGE_ATTEMPT = 1,
    /* An image is written and set to boot, waiting for its trial boot to succeed. */
    DH_FW_STAGE_PENDING = 2,
} dh_fw_stage;

/* The update record. A display that has never tried has dh_fw_record_init()'s. */
typedef struct {
    /* The offer last acted on, which is also the one an attempt or a pending image belongs to. */
    bool has_last_offer;
    char last_offer_id[DH_OFFER_ID_HEX_LEN + 1];
    /* The attempt budget. */
    uint8_t attempts_left; /* 0 to DH_FW_FULL_BUDGET */
    uint8_t credit_hours;  /* 0 to 3; always 0 with a full budget */
    dh_fw_stage stage;
    /* For DH_FW_STAGE_PENDING: the release waiting to be marked good. */
    uint8_t pending_slot; /* the caller's number for the app partition it was written to */
    char pending_version[DH_FW_MAX_VERSION_BYTES + 1];
    uint32_t pending_security_counter;
    bool pending_has_key_set;
    uint8_t pending_key_count;
    dh_fw_key pending_key_set[DH_FW_MAX_KEYS];
    /* The last final result, and whether the hub has answered the report of it. */
    dh_fw_report_state result_state; /* DH_FW_REPORT_NONE: there is none */
    dh_fw_reason result_reason;
    char result_offer_id[DH_OFFER_ID_HEX_LEN + 1];
    bool result_answered;
} dh_fw_record;

void dh_fw_keys_init(dh_fw_keys *keys);     /* no keys, floor 0 */
void dh_fw_record_init(dh_fw_record *rec);  /* a full budget, nothing else */

/* The stored forms: fixed layouts that don't depend on the compiler, each starting with a format
 * number, so the firmware before and after an update read each other's. decode checks every
 * field and answers false for a blob of the wrong length, another format or with a value out of
 * range, leaving *out as init leaves it. Treat a blob that is missing as init does. For one that
 * won't decode, carry on with init's values too: with no keys a display refuses every update
 * until it is set up again over USB, which is the safe side. */
#define DH_FW_KEYS_BLOB_LEN 201u
#define DH_FW_RECORD_BLOB_LEN 321u
void dh_fw_keys_encode(const dh_fw_keys *keys, uint8_t out[DH_FW_KEYS_BLOB_LEN]);
bool dh_fw_keys_decode(dh_fw_keys *out, const uint8_t *blob, size_t len);
void dh_fw_record_encode(const dh_fw_record *rec, uint8_t out[DH_FW_RECORD_BLOB_LEN]);
bool dh_fw_record_decode(dh_fw_record *out, const uint8_t *blob, size_t len);

/* A key's id, the SHA-256 of its 65 bytes, as 64 lower-case hex digits and a NUL: what a display
 * reports in firmware.key_ids. False (and an empty string) if the hash couldn't be made. */
bool dh_fw_key_id_hex(const dh_fw_key *key, char out[DH_PACKAGE_ID_HEX_LEN + 1]);

/* ---- What is kept in memory for one start ---- */

/* Zeroed by dh_fw_start(). Losing it at a restart is part of the rules: running time not yet
 * saved as a credit hour is lost, and so is the memory of a refusal made before any request. */
typedef struct {
    /* New firmware on its trial boot: no offers, no heartbeat, no reports. */
    bool trial;
    uint8_t running_slot;
    /* The running time from which the current credit hour is counted. */
    uint64_t counted_from_s;
    /* The last offer refused from the offer alone, and whether the hub has answered its report. */
    bool early_refusal;
    bool early_answered;
    dh_fw_reason early_reason;
    char early_offer_id[DH_OFFER_ID_HEX_LEN + 1];
} dh_fw_session;

/* ---- A start ---- */

typedef enum {
    /* Nothing to settle. */
    DH_FW_START_NORMAL,
    /* New firmware on its trial boot: check in, and send nothing that names this version. */
    DH_FW_START_TRIAL,
    /* The previous firmware, back after an update that didn't prove itself. The pending key_set
     * is dropped, the floor is left alone, and `rolled_back` waits to be reported. */
    DH_FW_START_ROLLED_BACK,
    /* A restart or power cut ended an attempt: `failed` with `download` waits to be reported. */
    DH_FW_START_INTERRUPTED,
    /* The new firmware was marked good, and the start ended before dh_fw_marked_good()'s saves
     * were both made. They are made again now: `installed` waits to be reported. */
    DH_FW_START_INSTALLED,
} dh_fw_start_result;

/*
 * Call once at every start, with the keys and record as loaded from flash, before anything else
 * here. running_slot is the caller's number for the app partition that is running (the same
 * numbering it gives dh_fw_attempt_installed()), and on_trial is whether ESP-IDF says that
 * partition is ESP_OTA_IMG_PENDING_VERIFY.
 *
 * *save says what to write. Nothing else depends on the order, so a power cut before the write
 * just means the same answer at the next start.
 */
dh_fw_start_result dh_fw_start(dh_fw_session *s, dh_fw_record *rec, dh_fw_keys *keys,
                               uint8_t running_slot, bool on_trial, unsigned *save);

/* ---- The attempt budget ---- */

/* Counts running time towards the budget. While it is below 5, each full hour is a credit hour
 * and the fourth becomes an attempt; while it is 5, nothing is counted. Returns
 * DH_FW_SAVE_RECORD when the budget changed: save it then, so each credit hour is in flash before
 * the next is counted. Call it at least every few minutes; it is cheap. */
unsigned dh_fw_tick(dh_fw_session *s, dh_fw_record *rec, uint64_t uptime_s);

/* Seconds of running until the next attempt is earned; 0 with a full budget. For showing why an
 * update is waiting. */
uint64_t dh_fw_seconds_to_next_attempt(const dh_fw_session *s, const dh_fw_record *rec,
                                       uint64_t uptime_s);

/* ---- Offers ---- */

/* What a display knows about itself. The strings must outlive whatever they are passed to. */
typedef struct {
    const char *device_type;
    const char *hardware_revision;
    uint32_t slot_bytes; /* the size of the spare app partition */
} dh_fw_device;

typedef enum {
    /* The offer last acted on, or one refused from the offer alone since this start; or an
     * offer_id that isn't 32 lower-case hex digits. Do nothing. */
    DH_FW_OFFER_IGNORE,
    /* On a trial boot, or a final report is still unanswered: act on nothing new. Send the
     * report (dh_fw_report_next) and look at the offer again at the next poll. */
    DH_FW_OFFER_BUSY,
    /* Refused from the offer alone (`reason`). Nothing is spent or saved and nothing is
     * requested; dh_fw_report_next() now gives the `refused` report. */
    DH_FW_OFFER_REFUSE,
    /* No attempts left: request nothing, report nothing, and leave the offer open. */
    DH_FW_OFFER_WAIT,
    /* One attempt is spent, and the record names this offer. SAVE THE RECORD, in one write,
     * BEFORE requesting the download; if the write fails, don't request it. */
    DH_FW_OFFER_ATTEMPT,
} dh_fw_offer_action;

typedef struct {
    dh_fw_offer_action action;
    dh_fw_reason reason;   /* for DH_FW_OFFER_REFUSE */
    uint64_t wait_seconds; /* for DH_FW_OFFER_WAIT: running time until the next attempt */
    unsigned save;         /* DH_FW_SAVE_RECORD or 0, whatever the action */
} dh_fw_offer_decision;

/* What to do about the offer in a snapshot (dh_snapshot's firmware_update, when it is usable).
 * size is the offer's `size`. Nothing here reads a date or the hub's time, and an attempt's
 * result never changes the budget. */
dh_fw_offer_decision dh_fw_on_offer(dh_fw_session *s, dh_fw_record *rec, const dh_fw_keys *keys,
                                    const dh_fw_device *device, const char *offer_id,
                                    uint64_t size, uint64_t uptime_s);

/* Steps 1 and 2 as far as they go before the download is requested: no keys
 * (DH_FW_REASON_NO_TRUSTED_KEYS), or a size over a header, the largest manifest and the spare
 * partition (DH_FW_REASON_TOO_LARGE). DH_FW_REASON_NONE if neither. */
dh_fw_reason dh_fw_offer_check(const dh_fw_keys *keys, const dh_fw_device *device,
                               uint64_t offer_size);

/* ---- The manifest ("The manifest") ---- */

typedef struct {
    char device_type[DH_FW_TEXT_ROOM(DH_FW_MAX_DEVICE_FIELD_CHARS)];
    size_t hardware_revision_count; /* 1 to DH_FW_MAX_HARDWARE_REVISIONS */
    char hardware_revisions[DH_FW_MAX_HARDWARE_REVISIONS]
                           [DH_FW_TEXT_ROOM(DH_FW_MAX_DEVICE_FIELD_CHARS)];
    char firmware_version[DH_FW_MAX_VERSION_BYTES + 1];
    uint32_t security_counter;
    uint32_t image_size; /* 1 to DH_FW_MAX_IMAGE_SIZE */
    uint8_t image_sha256[DH_SHA256_LEN];
    bool has_key_set;
    uint8_t key_count; /* 1 to DH_FW_MAX_KEYS with a key_set, else 0 */
    dh_fw_key key_set[DH_FW_MAX_KEYS];
    /* The maker's note. A display has no need to show it; it is here because it is checked. */
    bool has_summary;
    char summary[DH_FW_TEXT_ROOM(DH_FW_MAX_SUMMARY_CHARS)];
} dh_fw_manifest;

/* A display's reading of a manifest: every rule except the contents of usb_install, which a
 * display ignores (Manifest::parse_for_display in crates/firmware-package). False for anything
 * the rules refuse. A display calls this only on bytes whose signature has verified;
 * dh_fw_download_push() does both, in that order. */
bool dh_fw_manifest_parse(const uint8_t *json, size_t len, dh_fw_manifest *out,
                          dh_text_scratch *scratch);

/* Step 7, for a manifest that parsed: is it for this display, not a downgrade, and does it fit?
 * In order: device_type (DH_FW_REASON_WRONG_DEVICE_TYPE), hardware_revisions
 * (DH_FW_REASON_WRONG_HARDWARE), a security counter below the floor, or a key_set that leaves
 * out a key the display trusts without a counter above the floor (DH_FW_REASON_DOWNGRADE), an
 * image larger than the spare partition (DH_FW_REASON_TOO_LARGE), and a header, manifest and
 * image that don't add up to the Content-Length (DH_FW_REASON_BAD_FORMAT). */
dh_fw_reason dh_fw_manifest_check(const dh_fw_manifest *m, const dh_fw_keys *keys,
                                  const dh_fw_device *device, uint16_t manifest_len,
                                  uint64_t content_length);

/* MAJOR.MINOR.PATCH, optionally "-" and a tag of ASCII letters, digits, "." and "-"; at most 48
 * bytes; each number plain decimal with no leading zero, fitting in 32 bits. */
bool dh_fw_version_valid(const char *s, size_t n);

/* The header's fixed fields (step 3): magic, format version, algorithm and manifest_len. */
bool dh_fw_header_check(const uint8_t header[DH_FW_HEADER_LEN], uint16_t *manifest_len);

/* Whether the header's signature over the signed bytes (the fixed text, header bytes 0 to 72 and
 * the manifest exactly as sent) verifies with stored_key. Pass the display's own copy of the
 * key, never the header's. */
bool dh_fw_signature_valid(const dh_fw_key *stored_key, const uint8_t header[DH_FW_HEADER_LEN],
                           const uint8_t *manifest, size_t manifest_len);

/* ---- One download ("Checks before installing", steps 2 to 9) ---- */

typedef enum {
    /* All of the piece was taken. Write `write` if there is any, then read on. */
    DH_FW_STEP_MORE,
    /* The manifest has passed every check. Open the spare partition (esp_ota_begin), then push
     * the rest of the piece. If it won't open, call dh_fw_download_fail(DH_FW_REASON_FLASH).
     * When `downloading` is reported (once) is the caller's choice: the reference display
     * reports it here, and this firmware reports it before the download is asked for, because
     * only one TLS session is ever open (firmware/README.md). */
    DH_FW_STEP_OPEN_SLOT,
    /* Write `write`; that is the whole image and its hash matches. Call esp_ota_end, then
     * dh_fw_attempt_installed(). Read nothing more from the connection. */
    DH_FW_STEP_FINISH,
    /* Over, for `reason`. If dh_fw_download_slot_open(), abandon it (esp_ota_abort). Then
     * dh_fw_attempt_ended(). */
    DH_FW_STEP_ENDED,
} dh_fw_step;

typedef struct {
    dh_fw_step step;
    dh_fw_reason reason; /* for DH_FW_STEP_ENDED */
    /* How much of the piece was taken. Less than all of it only with OPEN_SLOT (push the rest),
     * FINISH and ENDED (the rest is never read). */
    size_t used;
    /* Image bytes to write to the spare partition now, in order: a part of the piece that was
     * pushed. Only ever set after OPEN_SLOT, with MORE or FINISH. */
    const uint8_t *write;
    size_t write_len;
} dh_fw_push;

/* A download in progress. About 12 KB: keep it in static memory, not on a task's stack. It holds
 * the header and manifest and a running hash, never any of the image. */
typedef struct {
    dh_fw_device device;
    dh_fw_keys keys; /* the display's keys and floor when the download began */
    uint64_t content_length;
    uint64_t read; /* bytes of the body taken so far */
    uint8_t phase;
    bool slot_open;
    dh_fw_reason ended;
    size_t have; /* header or manifest bytes held */
    uint16_t manifest_len;
    uint8_t key_index; /* which stored key the header named */
    uint32_t image_left;
    dh_sha256 hash;
    dh_fw_manifest manifest;
    uint8_t header[DH_FW_HEADER_LEN];
    uint8_t manifest_bytes[DH_FW_MAX_MANIFEST_LEN];
    dh_text_scratch scratch;
} dh_fw_download;

/* Step 2 with the answer in hand, before any of the body is read: the Content-Length must equal
 * the offer's size (an answer without one, has_content_length false, can't). Returns
 * DH_FW_REASON_NONE and readies *d, or the refusal (the attempt is over: dh_fw_attempt_ended).
 * *keys and *device are copied, except the strings device points at. */
dh_fw_reason dh_fw_download_begin(dh_fw_download *d, const dh_fw_keys *keys,
                                  const dh_fw_device *device, uint64_t offer_size,
                                  bool has_content_length, uint64_t content_length);

/* Takes the next piece of the body, of any size. Never takes a byte past the update form. After
 * FINISH or ENDED, another push takes nothing and gives the same step again, with nothing to
 * write. */
dh_fw_push dh_fw_download_push(dh_fw_download *d, const uint8_t *piece, size_t len);

/* Ends the download for a reason only the caller can see: DH_FW_REASON_DOWNLOAD (the body ended,
 * sent nothing for 30 seconds, or took 15 minutes in all; or the hub answered with an error) or
 * DH_FW_REASON_FLASH (esp_ota_begin, a write or esp_ota_end failed). Returns the reason the
 * download ended with, which is an earlier one if it had ended already. */
dh_fw_reason dh_fw_download_fail(dh_fw_download *d, dh_fw_reason why);

/* Whether OPEN_SLOT was reached, so the spare partition may hold part of an image. */
bool dh_fw_download_slot_open(const dh_fw_download *d);
/* The manifest, once OPEN_SLOT was reached; NULL before. */
const dh_fw_manifest *dh_fw_download_manifest(const dh_fw_download *d);
uint64_t dh_fw_download_bytes_read(const dh_fw_download *d);

/* Whether to give up on a download that began at started_s and last sent a byte at last_byte_s. */
bool dh_fw_download_overdue(uint64_t started_s, uint64_t last_byte_s, uint64_t now_s);

/* ---- The end of an attempt ---- */

/* The attempt was refused or failed. Returns DH_FW_SAVE_RECORD: save, then report
 * (dh_fw_report_next). A power cut before the save reads as an attempt cut short at the next
 * start, and is reported as `failed` with `download`. The budget doesn't change. */
unsigned dh_fw_attempt_ended(dh_fw_record *rec, dh_fw_reason reason);

/* Step 9, after esp_ota_end() has passed: the release waits for its trial boot. slot is the
 * caller's number for the partition it was written to. Returns DH_FW_SAVE_RECORD: SAVE, THEN
 * esp_ota_set_boot_partition(), then restart. In that order a power cut can't leave new firmware
 * on trial with no record of its security counter and key_set. If the save fails, report
 * `failed` with `flash` (dh_fw_attempt_ended) and don't set the partition to boot. */
unsigned dh_fw_attempt_installed(dh_fw_record *rec, const dh_fw_manifest *manifest, uint8_t slot);

/* ---- The trial boot ---- */

typedef enum {
    DH_FW_CHECK_IN_FAILED,       /* no answer, or any other error */
    DH_FW_CHECK_IN_ANSWERED,     /* success */
    DH_FW_CHECK_IN_SET_UP_AGAIN, /* token_revoked or unknown_device: rolling back wouldn't help */
} dh_fw_check_in;

/* A check-in from what dh_read_response() made of a snapshot poll. A request that got no answer
 * at all is DH_FW_CHECK_IN_FAILED. */
dh_fw_check_in dh_fw_check_in_of(const dh_response *r);

typedef enum {
    DH_FW_TRIAL_NONE, /* not on a trial boot */
    DH_FW_TRIAL_WAIT,
    /* Call esp_ota_mark_app_valid_cancel_rollback(), then dh_fw_marked_good(). */
    DH_FW_TRIAL_MARK_GOOD,
    /* Five minutes are up: esp_ota_mark_app_invalid_rollback_and_reboot(). Save nothing; the
     * previous firmware settles the record when it starts. */
    DH_FW_TRIAL_ROLL_BACK,
} dh_fw_trial_step;

/* Time passing on a trial boot. Call it at least every second or so until it stops waiting. */
dh_fw_trial_step dh_fw_trial_tick(const dh_fw_session *s, uint64_t uptime_s);
/* The answer to an authenticated check-in. One that comes at 300 seconds or later is too late. */
dh_fw_trial_step dh_fw_trial_check_in(const dh_fw_session *s, uint64_t uptime_s,
                                      dh_fw_check_in answer);

/* Once the running image is marked good, and only then: raises the floor to the release's
 * security counter if that is higher, replaces the trusted keys with its key_set if it had one,
 * clears the pending parts of the record and leaves `installed` to report. Returns what to save:
 * the keys first, then the record. A power cut between the two is finished by dh_fw_start(). If
 * esp_ota_mark_app_valid_cancel_rollback() failed, don't call this: restart, and the bootloader
 * rolls back. */
unsigned dh_fw_marked_good(dh_fw_session *s, dh_fw_record *rec, dh_fw_keys *keys);

/* ---- Reports ---- */

typedef struct {
    char offer_id[DH_OFFER_ID_HEX_LEN + 1];
    dh_fw_report_state state;
    dh_fw_reason reason; /* DH_FW_REASON_NONE: leave `reason` out */
} dh_fw_report;

/* The final report that is waiting for an answer, if any: the record's, then a refusal made from
 * the offer alone. False on a trial boot, when nothing may be sent. Send it with the running
 * firmware's version, and again later (after the next poll, never in a loop) until an answer
 * settles it. */
bool dh_fw_report_next(const dh_fw_session *s, const dh_fw_record *rec, dh_fw_report *out);

/* The hub's answer settled *sent: it won't be sent again. Call this only for an answer that
 * settles a report, which dh_firmware_report_settled() in dh_hub.h decides: success, `not_found`
 * (the hub has no such offer) and the two "set up again" errors. No answer, and every other
 * error, leave the report waiting. (The reference display counts any answer at all.) Returns
 * DH_FW_SAVE_RECORD if the record changed. */
unsigned dh_fw_report_answered(dh_fw_session *s, dh_fw_record *rec, const dh_fw_report *sent);

/* ---- The provisioning bundle over USB ("What each action may change") ---- */

/* A bundle's `firmware` member. */
typedef struct {
    uint8_t key_count; /* 1 to DH_FW_MAX_KEYS */
    dh_fw_key keys[DH_FW_MAX_KEYS];
    uint32_t security_counter;
} dh_fw_bundle;

typedef enum {
    DH_FW_BUNDLE_OK,
    /* A key that isn't a valid P-256 point, a repeat, or a count outside 1 to 3: answer
     * `invalid_bundle` and store nothing. */
    DH_FW_BUNDLE_INVALID,
    /* The display already trusts a different set: answer `firmware_keys_locked`. */
    DH_FW_BUNDLE_KEYS_LOCKED,
} dh_fw_bundle_answer;

/* Reads a bundle's `firmware` member from a parsed document: trusted_keys, 1 to 3 strings of 130
 * lower-case hex digits starting 04 with no repeats, and security_counter. False if it breaks a
 * rule (`invalid_bundle`). Whether the keys are points is dh_fw_bundle_check()'s question. */
bool dh_fw_bundle_parse(const dh_json_doc *doc, dh_json_value firmware, dh_fw_bundle *out);

/* What a bundle's firmware member means for this display. bundle is NULL for a bundle without
 * the member, which is always DH_FW_BUNDLE_OK. Changes nothing. */
dh_fw_bundle_answer dh_fw_bundle_check(const dh_fw_keys *keys, const dh_fw_bundle *bundle);

/* Accepting a bundle (the whole of it, not only its firmware member): a display with no keys
 * takes the bundle's keys and its counter as the floor, a display with keys keeps its own, and
 * either way the attempt budget returns to 5. *save says what to write. Anything but
 * DH_FW_BUNDLE_OK changes nothing. */
dh_fw_bundle_answer dh_fw_bundle_apply(dh_fw_record *rec, dh_fw_keys *keys,
                                       const dh_fw_bundle *bundle, unsigned *save);

#ifdef __cplusplus
}
#endif

#endif
