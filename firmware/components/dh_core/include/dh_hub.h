/*
 * Talking to the hub once the display is set up (docs/firmware-handoff.md, "Joining and
 * enrolling", "Heartbeat", "Errors and back-off"; docs/protocol-v1.md), following the reference
 * display (tools/display-simulator, src/paired.rs and src/main.rs) and crates/hub-client:
 *
 *  - the bodies a display sends: the enrolment request and the heartbeat;
 *  - what it makes of the answers: the enrolment response, and any error answer;
 *  - the device ID and token it is given, and their stored form;
 *  - which hub address and trust anchor to try next;
 *  - when a heartbeat is due, and when the last good snapshot is worth saving.
 *
 * The answer to a snapshot request is dh_snapshot.h's, and when to poll next is dh_poll.h's.
 *
 * Nothing here touches the network, flash or a clock, and nothing keeps state between calls. Time
 * is the display's own running time in whole seconds, from a monotonic timer.
 *
 * An enrolment request holds the one-time secret, and a dh_device holds the token: never log or
 * show either, and wipe them (dh_wipe, dh_device_wipe) once they are no longer needed.
 */
#ifndef DH_HUB_H
#define DH_HUB_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#include "dh_poll.h"
#include "dh_setup.h"
#include "dh_snapshot.h"
#include "dh_update.h"

#ifdef __cplusplus
extern "C" {
#endif

/* ---- The requests ---- */

#define DH_PATH_ENROL "/v1/device/enrol"
#define DH_PATH_SNAPSHOT "/v1/device/snapshot"
#define DH_PATH_HEARTBEAT "/v1/device/heartbeat"
/* Every request carries this header. */
#define DH_PROTOCOL_HEADER "X-Display-Protocol"
#define DH_PROTOCOL_HEADER_VALUE "1"

/* Room for the longest request body, with a NUL: a 64-byte secret with every byte escaped, three
 * 48-character fields and three key ids come to under 900 bytes. */
#define DH_HUB_REQUEST_MAX 1024u

/* The most an enrolment response or an error answer may be. The reference stops reading an error
 * at 8 KiB; a hub's are a few hundred bytes. */
#define DH_MAX_HUB_ANSWER_BYTES 8192u

/* What a display says about itself. The strings must be plain ASCII that outlives the call. */
typedef struct {
    const char *device_type;
    const char *firmware_version;
    const char *hardware_revision;
    /* The `firmware` member, for a display that takes signed updates. With keys or record NULL
     * (storage isn't working) it is left out. */
    uint32_t slot_bytes;
    const dh_fw_keys *keys;
    const dh_fw_record *record;
} dh_device_info;

/* The body of POST /v1/device/enrol: {"enrolment_secret": ..., "device": {...}}. Returns its
 * length, with a NUL after it, or 0 if it doesn't fit in cap bytes (DH_HUB_REQUEST_MAX always
 * does) or the secret is empty. The secret is secret_len bytes of UTF-8. */
size_t dh_enrol_request(const dh_device_info *info, const char *secret, size_t secret_len,
                        char *out, size_t cap);

/* The body of POST /v1/device/heartbeat: the same `device` object, on its own. */
size_t dh_heartbeat_request(const dh_device_info *info, char *out, size_t cap);

/* Signed updates (dh_update.h): where a download is asked for and where reports go. */
#define DH_PATH_FIRMWARE_REPORT "/v1/device/firmware/report"
#define DH_PATH_FIRMWARE_PREFIX "/v1/device/firmware/"
/* Room for the download's path, with a NUL. */
#define DH_FIRMWARE_PATH_MAX (sizeof DH_PATH_FIRMWARE_PREFIX + DH_PACKAGE_ID_HEX_LEN)

/* The path of GET /v1/device/firmware/{package_id}, for an offer's package_id. Returns its
 * length, or 0 (and an empty string) if package_id isn't 64 lower-case hex digits or the path
 * doesn't fit in cap bytes (DH_FIRMWARE_PATH_MAX always does). */
size_t dh_firmware_path(const char *package_id, char *out, size_t cap);

/* The body of POST /v1/device/firmware/report: {"offer_id", "state", "reason" (only when the
 * report has one), "firmware_version"}. firmware_version is the version running as the report
 * is sent. Returns its length, with a NUL after it, or 0 if the report has no state, its
 * offer_id isn't 32 lower-case hex digits, or it doesn't fit. */
size_t dh_firmware_report_request(const dh_fw_report *report, const char *firmware_version,
                                  char *out, size_t cap);

/* Whether the hub's answer to a final report settles it, so it isn't sent again
 * (docs/protocol-v1.md, "Signed firmware updates" and the table of error codes):
 *  - any 2xx (a hub answers 204, also for an offer that is already settled);
 *  - 404 with the code `not_found`: the hub has no offer with that id, and never will;
 *  - `token_revoked` or `unknown_device`, with any status: the hub no longer knows this
 *    display, and nothing it sends will be taken until it is set up again.
 * Everything else is a hub that didn't take the report this time, and it is sent again after a
 * later poll: a 5xx, `rate_limited`, `unavailable`, the faults the protocol says to retry
 * (`unauthorised`, `forbidden`, `bad_request`, `payload_too_large`, `conflict`, and `not_found`
 * with any status but 404), a code this firmware doesn't know, and an answer with no error body.
 * body is the answer's body, len bytes (it may be NULL when len is 0). */
bool dh_firmware_report_settled(int http_status, const uint8_t *body, size_t len);

/* ---- The device ID and token ---- */

#define DH_DEVICE_MAX_ID_BYTES 64u
#define DH_DEVICE_MAX_TOKEN_BYTES 128u

/* What the hub gives a display when it enrols. A hub's ID looks like "d-3fa91c07b2e4" and its
 * token like "dhd_" and 43 more characters; anything of 1 to 64 (ID) or 1 to 128 (token)
 * printable ASCII characters with no space is taken, so a hub may change how it makes them. That
 * also makes a token safe to put in a header as it is. */
typedef struct {
    uint8_t id_len;
    char id[DH_DEVICE_MAX_ID_BYTES + 1u];
    uint8_t token_len;
    char token[DH_DEVICE_MAX_TOKEN_BYTES + 1u];
} dh_device;

bool dh_device_valid(const dh_device *device);
void dh_device_wipe(dh_device *device);

/* The stored form, for encrypted NVS: a format number, then each text with its length. encode
 * returns how many bytes it wrote, or 0 for a device that isn't valid; decode checks everything
 * and answers false, with *out wiped, for anything else. */
#define DH_DEVICE_BLOB_MAX (3u + DH_DEVICE_MAX_ID_BYTES + DH_DEVICE_MAX_TOKEN_BYTES)
size_t dh_device_encode(const dh_device *device, uint8_t out[DH_DEVICE_BLOB_MAX]);
bool dh_device_decode(dh_device *out, const uint8_t *blob, size_t len);

/* "Bearer <token>" for the Authorization header, with a NUL. Returns its length, or 0 (and an
 * empty string) if the device isn't valid or it doesn't fit. Wipe it after the request. */
#define DH_AUTHORIZATION_MAX (7u + DH_DEVICE_MAX_TOKEN_BYTES + 1u)
size_t dh_authorization(const dh_device *device, char *out, size_t cap);

/* ---- The answers ---- */

/* An error answer from any route: the same grouping as dh_read_response() gives a snapshot
 * request's (reprovision, incompatible, retry, fault). A body over DH_MAX_HUB_ANSWER_BYTES, or
 * one that isn't the error form, counts as no body: a 5xx status is then "the hub is busy" and
 * anything else a fault. Never returns DH_OUTCOME_SHOW. */
dh_response dh_read_error_response(int http_status, const uint8_t *body, size_t len);

typedef struct {
    /* The hub took the secret: *out holds the device ID and token. */
    bool enrolled;
    /* Otherwise, what to do, as for a poll. DH_OUTCOME_UNREADABLE for a success status whose body
     * isn't an enrolment response (the hub may have used the secret up all the same). */
    dh_response response;
} dh_enrol_answer;

/* Reads the answer to an enrolment request. Any 2xx status with a readable body is success, as
 * in the reference (a hub answers 201). Unless `enrolled`, *out is wiped. The body holds the
 * token: wipe the buffer afterwards. */
dh_enrol_answer dh_read_enrol_response(int http_status, const uint8_t *body, size_t len,
                                       dh_device *out);

/* Whether the hub took a heartbeat: any 2xx status (a hub answers 204). */
bool dh_heartbeat_taken(int http_status);

/* ---- Which address to try ---- */

typedef enum {
    /* No answer: the name didn't resolve, nothing connected, or the connection broke. */
    DH_TRY_NO_ANSWER,
    /* Something answered, but not with a certificate this anchor vouches for. */
    DH_TRY_WRONG_HUB,
    /* The real hub answered, with success or an error. Stop here. */
    DH_TRY_ANSWERED,
} dh_try_result;

/* One request, tried at each hub address: the one that answered last time first, then the others
 * in the bundle's order. At an address, each trust anchor is tried in turn while the certificate
 * doesn't match; an address that doesn't answer isn't tried again with the other anchor. The
 * first answer from the real hub ends it.
 *
 *   dh_hub_walk walk;
 *   dh_hub_walk_begin_at(&walk, host_count, anchor_count, first_host);
 *   while (dh_hub_walk_next(&walk, &host, &anchor)) {
 *       dh_hub_walk_tried(&walk, <what happened at hosts[host] with anchors[anchor]>);
 *   }
 *   problem = dh_hub_walk_problem(&walk);
 *   if (problem == DH_PROBLEM_NONE) {
 *       first_host = dh_hub_walk_answered_host(&walk);
 *   }
 *
 * Starting where the hub last answered matters when an earlier address never does: a name that
 * can't be looked up costs several seconds at every request. Keep first_host in memory only (0
 * at every start, and for every new setup): it is a short cut, never a setting.
 */
typedef struct {
    uint8_t host_count;
    uint8_t anchor_count;
    uint8_t first; /* the address tried first */
    uint8_t tried; /* how many addresses have been finished with */
    uint8_t host;
    uint8_t anchor;
    bool waiting; /* a try was handed out and its result hasn't come back */
    bool done;
    bool answered;
    dh_problem last;
} dh_hub_walk;

/* A walk in the bundle's order. */
void dh_hub_walk_begin(dh_hub_walk *w, uint8_t host_count, uint8_t anchor_count);
/* A walk that tries hosts[first_host] first, then the others in the bundle's order. A first_host
 * that isn't one of the addresses is read as 0. */
void dh_hub_walk_begin_at(dh_hub_walk *w, uint8_t host_count, uint8_t anchor_count,
                          uint8_t first_host);
/* The next address and anchor to try. False when there is nothing left to try, or the hub has
 * answered. */
bool dh_hub_walk_next(dh_hub_walk *w, uint8_t *host, uint8_t *anchor);
void dh_hub_walk_tried(dh_hub_walk *w, dh_try_result result);
/* DH_PROBLEM_NONE if the hub answered. Otherwise what the last try found, as the reference
 * reports it: DH_PROBLEM_UNREACHABLE or DH_PROBLEM_WRONG_HUB (and DH_PROBLEM_UNREACHABLE when
 * there was nothing to try). */
dh_problem dh_hub_walk_problem(const dh_hub_walk *w);
/* The address the hub answered at: the one to begin the next walk at. If the hub didn't answer,
 * the address this walk began at, so a round of failures changes nothing. */
uint8_t dh_hub_walk_answered_host(const dh_hub_walk *w);

/* ---- When a heartbeat is due ---- */

#define DH_HEARTBEAT_EVERY_SECONDS 3600u
/* How soon a change the hub didn't take is sent again with the same value. */
#define DH_HEARTBEAT_RETRY_SECONDS 300u

/* Send a heartbeat after start-up, then hourly, and once as soon as the attempts left change. If
 * the hub doesn't take that one, the same value is tried again every five minutes until it does,
 * never at every poll. Zero it at every start. The caller holds heartbeats back while the last
 * poll didn't succeed, and on a trial boot (dh_update.h). */
typedef struct {
    bool sent_any;
    uint64_t last_s; /* when the last one was sent, taken or not */
    bool has_taken;
    uint8_t taken_attempts; /* the attempts left that the hub last took */
    bool failed; /* the last one wasn't taken */
    uint64_t failed_s;
    uint8_t failed_attempts;
} dh_heartbeat_state;

bool dh_heartbeat_due(const dh_heartbeat_state *st, uint64_t now_s, uint8_t attempts_left);
/* Records a heartbeat that carried attempts_left, and whether the hub took it. */
void dh_heartbeat_sent(dh_heartbeat_state *st, uint64_t now_s, uint8_t attempts_left, bool taken);

/* ---- Saving the last good snapshot ---- */

/* A snapshot's `generated_at` moves with every poll, and writing 32 KiB to flash at every poll
 * would wear it out in months. So the saved copy is only replaced when something other than
 * `generated_at` differs, and then at most this often. A saved copy is shown after a restart as
 * "saved; age unknown" until the first poll, so an hour behind costs nothing. */
#define DH_SNAPSHOT_SAVE_EVERY_SECONDS 3600u

/* Whether two snapshot bodies are the same apart from the value of the top-level `generated_at`.
 * Both must be bodies dh_read_response() accepted; anything it wouldn't read is "different". */
bool dh_snapshot_bodies_match(const uint8_t *a, size_t a_len, const uint8_t *b, size_t b_len);

/* Whether to save a new last good snapshot. has_saved: one is in flash already. matches_saved:
 * dh_snapshot_bodies_match() against it. saved_s: when one was last written since this start
 * (ignored unless written_since_start). */
bool dh_snapshot_save_due(bool has_saved, bool matches_saved, bool written_since_start,
                          uint64_t saved_s, uint64_t now_s);

#ifdef __cplusplus
}
#endif

#endif
