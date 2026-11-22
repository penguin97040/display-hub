/*
 * What a display keeps for signed updates, and the rules on trying: "Limits on trying", "The
 * trial boot" and "What each action may change" in docs/firmware-updates.md, following the
 * reference display (tools/display-simulator, src/firmware: rules.rs, store.rs and mod.rs).
 *
 * Where the documents leave a detail open, this does what the reference does:
 *  - an offer that would be refused from the offer alone (no keys, or too large) is refused and
 *    reported even when no attempts are left, since that costs nothing;
 *  - nothing new is acted on while a final report is unanswered;
 *  - a record whose budget is out of range reads as the nearest value in range.
 */
#include "dh_update.h"

#include <string.h>

/* ---- Names ---- */

const char *dh_fw_reason_name(dh_fw_reason r)
{
    switch (r) {
    case DH_FW_REASON_NO_TRUSTED_KEYS: return "no_trusted_keys";
    case DH_FW_REASON_TOO_LARGE: return "too_large";
    case DH_FW_REASON_BAD_FORMAT: return "bad_format";
    case DH_FW_REASON_UNTRUSTED_KEY: return "untrusted_key";
    case DH_FW_REASON_BAD_SIGNATURE: return "bad_signature";
    case DH_FW_REASON_WRONG_DEVICE_TYPE: return "wrong_device_type";
    case DH_FW_REASON_WRONG_HARDWARE: return "wrong_hardware";
    case DH_FW_REASON_DOWNGRADE: return "downgrade";
    case DH_FW_REASON_DOWNLOAD: return "download";
    case DH_FW_REASON_IMAGE_HASH: return "image_hash";
    case DH_FW_REASON_FLASH: return "flash";
    case DH_FW_REASON_NONE: break;
    }
    return NULL;
}

const char *dh_fw_report_state_name(dh_fw_report_state s)
{
    switch (s) {
    case DH_FW_REPORT_DOWNLOADING: return "downloading";
    case DH_FW_REPORT_INSTALLED: return "installed";
    case DH_FW_REPORT_REFUSED: return "refused";
    case DH_FW_REPORT_FAILED: return "failed";
    case DH_FW_REPORT_ROLLED_BACK: return "rolled_back";
    case DH_FW_REPORT_NONE: break;
    }
    return NULL;
}

dh_fw_report_state dh_fw_reason_state(dh_fw_reason r)
{
    switch (r) {
    case DH_FW_REASON_NO_TRUSTED_KEYS:
    case DH_FW_REASON_TOO_LARGE:
    case DH_FW_REASON_BAD_FORMAT:
    case DH_FW_REASON_UNTRUSTED_KEY:
    case DH_FW_REASON_BAD_SIGNATURE:
    case DH_FW_REASON_WRONG_DEVICE_TYPE:
    case DH_FW_REASON_WRONG_HARDWARE:
    case DH_FW_REASON_DOWNGRADE:
        return DH_FW_REPORT_REFUSED;
    case DH_FW_REASON_DOWNLOAD:
    case DH_FW_REASON_IMAGE_HASH:
    case DH_FW_REASON_FLASH:
        return DH_FW_REPORT_FAILED;
    case DH_FW_REASON_NONE:
        break;
    }
    return DH_FW_REPORT_NONE;
}

/* ---- Small helpers ---- */

/* Whether s[0..n) is lower-case hex digits. */
static bool is_lower_hex(const char *s, size_t n)
{
    for (size_t i = 0; i < n; i++) {
        bool ok = (s[i] >= '0' && s[i] <= '9') || (s[i] >= 'a' && s[i] <= 'f');
        if (!ok) {
            return false;
        }
    }
    return true;
}

/* Whether s is an offer id: exactly 32 lower-case hex digits and a NUL. Reads no further than
 * the first byte that isn't a hex digit. */
static bool is_offer_id(const char *s)
{
    return s != NULL && is_lower_hex(s, DH_OFFER_ID_HEX_LEN) && s[DH_OFFER_ID_HEX_LEN] == '\0';
}

static bool same_offer(const char *a, const char *b)
{
    return memcmp(a, b, DH_OFFER_ID_HEX_LEN) == 0;
}

static void set_offer(char dst[DH_OFFER_ID_HEX_LEN + 1], const char *src)
{
    memcpy(dst, src, DH_OFFER_ID_HEX_LEN);
    dst[DH_OFFER_ID_HEX_LEN] = '\0';
}

static bool budget_full(const dh_fw_record *rec)
{
    return rec->attempts_left >= DH_FW_FULL_BUDGET;
}

/* Brings a budget that is out of range into range. A full budget counts nothing. */
static void normalise_budget(dh_fw_record *rec)
{
    if (budget_full(rec)) {
        rec->attempts_left = DH_FW_FULL_BUDGET;
        rec->credit_hours = 0;
    } else if (rec->credit_hours >= DH_FW_CREDIT_HOURS_PER_ATTEMPT) {
        rec->credit_hours = DH_FW_CREDIT_HOURS_PER_ATTEMPT - 1u;
    }
}

static void clear_pending(dh_fw_record *rec)
{
    rec->pending_slot = 0;
    memset(rec->pending_version, 0, sizeof rec->pending_version);
    rec->pending_security_counter = 0;
    rec->pending_has_key_set = false;
    rec->pending_key_count = 0;
    memset(rec->pending_key_set, 0, sizeof rec->pending_key_set);
}

/* Leaves a final result for the offer last acted on, to be reported until the hub answers. It
 * never changes the attempt budget. */
static void settle(dh_fw_record *rec, dh_fw_report_state state, dh_fw_reason reason)
{
    rec->stage = DH_FW_STAGE_IDLE;
    clear_pending(rec);
    rec->result_state = state;
    rec->result_reason = reason;
    set_offer(rec->result_offer_id, rec->last_offer_id);
    rec->result_answered = false;
}

/* ---- What is kept in flash ---- */

void dh_fw_keys_init(dh_fw_keys *keys)
{
    memset(keys, 0, sizeof *keys);
}

void dh_fw_record_init(dh_fw_record *rec)
{
    memset(rec, 0, sizeof *rec);
    rec->attempts_left = DH_FW_FULL_BUDGET;
    rec->stage = DH_FW_STAGE_IDLE;
    rec->result_state = DH_FW_REPORT_NONE;
    rec->result_reason = DH_FW_REASON_NONE;
}

#define BLOB_FORMAT 1u

static void put_u32(uint8_t *out, uint32_t v)
{
    out[0] = (uint8_t)(v & 0xffu);
    out[1] = (uint8_t)((v >> 8) & 0xffu);
    out[2] = (uint8_t)((v >> 16) & 0xffu);
    out[3] = (uint8_t)((v >> 24) & 0xffu);
}

static uint32_t get_u32(const uint8_t *in)
{
    return (uint32_t)in[0] | ((uint32_t)in[1] << 8) | ((uint32_t)in[2] << 16) |
           ((uint32_t)in[3] << 24);
}

/* The keys blob: format, count, three keys, the floor (little-endian). */
enum {
    KEYS_AT_COUNT = 1,
    KEYS_AT_KEYS = 2,
    KEYS_AT_FLOOR = KEYS_AT_KEYS + DH_FW_MAX_KEYS * DH_FW_KEY_LEN,
};

void dh_fw_keys_encode(const dh_fw_keys *keys, uint8_t out[DH_FW_KEYS_BLOB_LEN])
{
    memset(out, 0, DH_FW_KEYS_BLOB_LEN);
    uint8_t count = keys->key_count <= DH_FW_MAX_KEYS ? keys->key_count : 0;
    out[0] = BLOB_FORMAT;
    out[KEYS_AT_COUNT] = count;
    for (uint8_t i = 0; i < count; i++) {
        memcpy(out + KEYS_AT_KEYS + (size_t)i * DH_FW_KEY_LEN, keys->keys[i].bytes,
               DH_FW_KEY_LEN);
    }
    put_u32(out + KEYS_AT_FLOOR, keys->security_floor);
}

bool dh_fw_keys_decode(dh_fw_keys *out, const uint8_t *blob, size_t len)
{
    dh_fw_keys_init(out);
    if (blob == NULL || len != DH_FW_KEYS_BLOB_LEN || blob[0] != BLOB_FORMAT ||
        blob[KEYS_AT_COUNT] > DH_FW_MAX_KEYS) {
        return false;
    }
    out->key_count = blob[KEYS_AT_COUNT];
    for (uint8_t i = 0; i < out->key_count; i++) {
        memcpy(out->keys[i].bytes, blob + KEYS_AT_KEYS + (size_t)i * DH_FW_KEY_LEN,
               DH_FW_KEY_LEN);
    }
    out->security_floor = get_u32(blob + KEYS_AT_FLOOR);
    return true;
}

/* The record blob. */
enum {
    REC_AT_FLAGS = 1,
    REC_AT_LAST_OFFER = 2,
    REC_AT_ATTEMPTS = REC_AT_LAST_OFFER + DH_OFFER_ID_HEX_LEN,
    REC_AT_CREDIT,
    REC_AT_STAGE,
    REC_AT_SLOT,
    REC_AT_VERSION_LEN,
    REC_AT_VERSION,
    REC_AT_COUNTER = REC_AT_VERSION + DH_FW_MAX_VERSION_BYTES,
    REC_AT_KEY_COUNT = REC_AT_COUNTER + 4,
    REC_AT_KEYS,
    REC_AT_RESULT_STATE = REC_AT_KEYS + DH_FW_MAX_KEYS * DH_FW_KEY_LEN,
    REC_AT_RESULT_REASON,
    REC_AT_RESULT_OFFER,
    REC_END = REC_AT_RESULT_OFFER + DH_OFFER_ID_HEX_LEN,
};

#define REC_FLAG_LAST_OFFER 0x1u
#define REC_FLAG_KEY_SET 0x2u
#define REC_FLAG_ANSWERED 0x4u

/* The lengths in the header are these layouts'. */
typedef char keys_blob_len_is_right[KEYS_AT_FLOOR + 4 == DH_FW_KEYS_BLOB_LEN ? 1 : -1];
typedef char record_blob_len_is_right[REC_END == DH_FW_RECORD_BLOB_LEN ? 1 : -1];

static size_t bounded_length(const char *s, size_t room)
{
    size_t n = 0;
    while (n < room && s[n] != '\0') {
        n++;
    }
    return n;
}

void dh_fw_record_encode(const dh_fw_record *rec, uint8_t out[DH_FW_RECORD_BLOB_LEN])
{
    memset(out, 0, DH_FW_RECORD_BLOB_LEN);
    out[0] = BLOB_FORMAT;
    unsigned flags = 0;
    if (rec->has_last_offer) {
        flags |= REC_FLAG_LAST_OFFER;
        memcpy(out + REC_AT_LAST_OFFER, rec->last_offer_id, DH_OFFER_ID_HEX_LEN);
    }
    out[REC_AT_ATTEMPTS] = rec->attempts_left;
    out[REC_AT_CREDIT] = rec->credit_hours;
    out[REC_AT_STAGE] = (uint8_t)rec->stage;
    if (rec->stage == DH_FW_STAGE_PENDING) {
        size_t n = bounded_length(rec->pending_version, DH_FW_MAX_VERSION_BYTES);
        out[REC_AT_SLOT] = rec->pending_slot;
        out[REC_AT_VERSION_LEN] = (uint8_t)n;
        memcpy(out + REC_AT_VERSION, rec->pending_version, n);
        put_u32(out + REC_AT_COUNTER, rec->pending_security_counter);
        if (rec->pending_has_key_set && rec->pending_key_count <= DH_FW_MAX_KEYS) {
            flags |= REC_FLAG_KEY_SET;
            out[REC_AT_KEY_COUNT] = rec->pending_key_count;
            for (uint8_t i = 0; i < rec->pending_key_count; i++) {
                memcpy(out + REC_AT_KEYS + (size_t)i * DH_FW_KEY_LEN,
                       rec->pending_key_set[i].bytes, DH_FW_KEY_LEN);
            }
        }
    }
    if (rec->result_state != DH_FW_REPORT_NONE) {
        out[REC_AT_RESULT_STATE] = (uint8_t)rec->result_state;
        out[REC_AT_RESULT_REASON] = (uint8_t)rec->result_reason;
        memcpy(out + REC_AT_RESULT_OFFER, rec->result_offer_id, DH_OFFER_ID_HEX_LEN);
        if (rec->result_answered) {
            flags |= REC_FLAG_ANSWERED;
        }
    }
    out[REC_AT_FLAGS] = (uint8_t)flags;
}

static bool decode_record(dh_fw_record *out, const uint8_t *blob)
{
    unsigned flags = blob[REC_AT_FLAGS];
    if ((flags & ~(REC_FLAG_LAST_OFFER | REC_FLAG_KEY_SET | REC_FLAG_ANSWERED)) != 0) {
        return false;
    }
    if (flags & REC_FLAG_LAST_OFFER) {
        const char *id = (const char *)blob + REC_AT_LAST_OFFER;
        if (!is_lower_hex(id, DH_OFFER_ID_HEX_LEN)) {
            return false;
        }
        out->has_last_offer = true;
        set_offer(out->last_offer_id, id);
    }
    out->attempts_left = blob[REC_AT_ATTEMPTS];
    out->credit_hours = blob[REC_AT_CREDIT];
    normalise_budget(out);

    uint8_t stage = blob[REC_AT_STAGE];
    if (stage > DH_FW_STAGE_PENDING || (stage != DH_FW_STAGE_IDLE && !out->has_last_offer)) {
        return false;
    }
    out->stage = (dh_fw_stage)stage;
    if (stage == DH_FW_STAGE_PENDING) {
        size_t n = blob[REC_AT_VERSION_LEN];
        const char *version = (const char *)blob + REC_AT_VERSION;
        if (!dh_fw_version_valid(version, n)) {
            return false;
        }
        memcpy(out->pending_version, version, n);
        out->pending_slot = blob[REC_AT_SLOT];
        out->pending_security_counter = get_u32(blob + REC_AT_COUNTER);
        uint8_t count = blob[REC_AT_KEY_COUNT];
        if (flags & REC_FLAG_KEY_SET) {
            if (count < 1 || count > DH_FW_MAX_KEYS) {
                return false;
            }
            out->pending_has_key_set = true;
            out->pending_key_count = count;
            for (uint8_t i = 0; i < count; i++) {
                memcpy(out->pending_key_set[i].bytes,
                       blob + REC_AT_KEYS + (size_t)i * DH_FW_KEY_LEN, DH_FW_KEY_LEN);
            }
        } else if (count != 0) {
            return false;
        }
    }

    uint8_t state = blob[REC_AT_RESULT_STATE];
    uint8_t reason = blob[REC_AT_RESULT_REASON];
    if (state == DH_FW_REPORT_NONE) {
        return reason == DH_FW_REASON_NONE && (flags & REC_FLAG_ANSWERED) == 0;
    }
    /* A final result: its reason must be one that goes with its state. */
    if (state == DH_FW_REPORT_DOWNLOADING || state > DH_FW_REPORT_ROLLED_BACK ||
        reason > DH_FW_REASON_FLASH) {
        return false;
    }
    dh_fw_report_state with = dh_fw_reason_state((dh_fw_reason)reason);
    bool needs_reason = state == DH_FW_REPORT_REFUSED || state == DH_FW_REPORT_FAILED;
    if (needs_reason ? with != (dh_fw_report_state)state : reason != DH_FW_REASON_NONE) {
        return false;
    }
    const char *id = (const char *)blob + REC_AT_RESULT_OFFER;
    if (!is_lower_hex(id, DH_OFFER_ID_HEX_LEN)) {
        return false;
    }
    out->result_state = (dh_fw_report_state)state;
    out->result_reason = (dh_fw_reason)reason;
    set_offer(out->result_offer_id, id);
    out->result_answered = (flags & REC_FLAG_ANSWERED) != 0;
    return true;
}

/* A RULE FOR EVERY LATER FORMAT. A record this function can't decode reads as a new one, with a
 * full attempt budget and no release waiting. So a later firmware must never leave behind a
 * record that this one can't read: after going back to this version (a failed trial boot), an
 * undecodable record would hand back every attempt already spent, and lose the release that
 * was waiting, so the going back would never be reported. A later format must therefore keep
 * this length and layout and format number for as long as a display may go back to firmware
 * that reads only this one, and add to it in a way this decoder still accepts (or keep writing
 * this form until the trial boot is over). The same holds for the keys blob above, where an
 * undecodable one reads as no keys. */
bool dh_fw_record_decode(dh_fw_record *out, const uint8_t *blob, size_t len)
{
    dh_fw_record_init(out);
    if (blob == NULL || len != DH_FW_RECORD_BLOB_LEN || blob[0] != BLOB_FORMAT ||
        !decode_record(out, blob)) {
        dh_fw_record_init(out);
        return false;
    }
    return true;
}

bool dh_fw_key_id_hex(const dh_fw_key *key, char out[DH_PACKAGE_ID_HEX_LEN + 1])
{
    static const char DIGITS[] = "0123456789abcdef";
    uint8_t id[DH_SHA256_LEN];
    out[0] = '\0';
    if (!dh_sha256_of(key->bytes, DH_FW_KEY_LEN, id)) {
        return false;
    }
    for (size_t i = 0; i < DH_SHA256_LEN; i++) {
        out[2u * i] = DIGITS[id[i] >> 4];
        out[2u * i + 1u] = DIGITS[id[i] & 0x0fu];
    }
    out[2u * DH_SHA256_LEN] = '\0';
    return true;
}

/* ---- A start, and the trial boot ---- */

/* Once marked good, and only then: the floor raised to the release's counter if that is higher,
 * its key_set, if it had one, in place of the trusted keys, the pending parts cleared, and
 * `installed` to report. Safe to repeat, which is what makes a power cut between the two saves
 * harmless. */
static unsigned apply_good(dh_fw_record *rec, dh_fw_keys *keys)
{
    if (rec->pending_security_counter > keys->security_floor) {
        keys->security_floor = rec->pending_security_counter;
    }
    if (rec->pending_has_key_set && rec->pending_key_count >= 1 &&
        rec->pending_key_count <= DH_FW_MAX_KEYS) {
        memset(keys->keys, 0, sizeof keys->keys);
        keys->key_count = rec->pending_key_count;
        for (uint8_t i = 0; i < rec->pending_key_count; i++) {
            keys->keys[i] = rec->pending_key_set[i];
        }
    }
    settle(rec, DH_FW_REPORT_INSTALLED, DH_FW_REASON_NONE);
    return DH_FW_SAVE_KEYS | DH_FW_SAVE_RECORD;
}

dh_fw_start_result dh_fw_start(dh_fw_session *s, dh_fw_record *rec, dh_fw_keys *keys,
                               uint8_t running_slot, bool on_trial, unsigned *save)
{
    memset(s, 0, sizeof *s);
    s->running_slot = running_slot;
    *save = 0;
    normalise_budget(rec);
    if (on_trial) {
        /* Nothing is written until the image is marked good. */
        s->trial = true;
        return DH_FW_START_TRIAL;
    }
    if (rec->stage != DH_FW_STAGE_IDLE && !rec->has_last_offer) {
        /* A record no function here leaves behind. There is no offer to report against. */
        rec->stage = DH_FW_STAGE_IDLE;
        clear_pending(rec);
        *save = DH_FW_SAVE_RECORD;
        return DH_FW_START_NORMAL;
    }
    if (rec->stage == DH_FW_STAGE_PENDING) {
        if (rec->pending_slot != running_slot) {
            /* The previous firmware, with an update it didn't finish: the new one didn't prove
             * itself. Its keys are dropped and the floor is left alone. */
            settle(rec, DH_FW_REPORT_ROLLED_BACK, DH_FW_REASON_NONE);
            *save = DH_FW_SAVE_RECORD;
            return DH_FW_START_ROLLED_BACK;
        }
        /* The new firmware, already marked good: the last start ended before both saves. */
        *save = apply_good(rec, keys);
        return DH_FW_START_INSTALLED;
    }
    if (rec->stage == DH_FW_STAGE_ATTEMPT) {
        /* A restart or power cut during the attempt: the old firmware is still the one running. */
        settle(rec, DH_FW_REPORT_FAILED, DH_FW_REASON_DOWNLOAD);
        *save = DH_FW_SAVE_RECORD;
        return DH_FW_START_INTERRUPTED;
    }
    return DH_FW_START_NORMAL;
}

dh_fw_check_in dh_fw_check_in_of(const dh_response *r)
{
    if (r->outcome == DH_OUTCOME_SHOW) {
        return DH_FW_CHECK_IN_ANSWERED;
    }
    if (r->error_code == DH_ERROR_TOKEN_REVOKED || r->error_code == DH_ERROR_UNKNOWN_DEVICE) {
        return DH_FW_CHECK_IN_SET_UP_AGAIN;
    }
    return DH_FW_CHECK_IN_FAILED;
}

dh_fw_trial_step dh_fw_trial_tick(const dh_fw_session *s, uint64_t uptime_s)
{
    if (!s->trial) {
        return DH_FW_TRIAL_NONE;
    }
    return uptime_s >= DH_FW_TRIAL_SECONDS ? DH_FW_TRIAL_ROLL_BACK : DH_FW_TRIAL_WAIT;
}

dh_fw_trial_step dh_fw_trial_check_in(const dh_fw_session *s, uint64_t uptime_s,
                                      dh_fw_check_in answer)
{
    dh_fw_trial_step step = dh_fw_trial_tick(s, uptime_s);
    if (step != DH_FW_TRIAL_WAIT) {
        return step;
    }
    return answer == DH_FW_CHECK_IN_ANSWERED || answer == DH_FW_CHECK_IN_SET_UP_AGAIN
               ? DH_FW_TRIAL_MARK_GOOD
               : DH_FW_TRIAL_WAIT;
}

unsigned dh_fw_marked_good(dh_fw_session *s, dh_fw_record *rec, dh_fw_keys *keys)
{
    if (!s->trial) {
        return 0;
    }
    s->trial = false;
    if (rec->stage != DH_FW_STAGE_PENDING || !rec->has_last_offer) {
        /* An image on trial that the record knows nothing of: there is nothing to apply. */
        return 0;
    }
    if (rec->pending_slot != s->running_slot) {
        /* The record is about some other image. Applying its floor and keys would be wrong. */
        rec->stage = DH_FW_STAGE_IDLE;
        clear_pending(rec);
        return DH_FW_SAVE_RECORD;
    }
    return apply_good(rec, keys);
}

/* ---- The attempt budget ---- */

unsigned dh_fw_tick(dh_fw_session *s, dh_fw_record *rec, uint64_t uptime_s)
{
    unsigned save = 0;
    normalise_budget(rec);
    /* At most 20 hours can count before the budget is full, so this loop is short however long
     * it has been since the last call. */
    while (!budget_full(rec) && uptime_s >= s->counted_from_s &&
           uptime_s - s->counted_from_s >= DH_FW_CREDIT_HOUR_SECONDS) {
        s->counted_from_s += DH_FW_CREDIT_HOUR_SECONDS;
        rec->credit_hours++;
        if (rec->credit_hours >= DH_FW_CREDIT_HOURS_PER_ATTEMPT) {
            rec->credit_hours = 0;
            rec->attempts_left++;
        }
        save = DH_FW_SAVE_RECORD;
    }
    if (budget_full(rec)) {
        /* While it is full nothing is counted: the next hour starts when an attempt is spent. */
        s->counted_from_s = uptime_s;
    }
    return save;
}

uint64_t dh_fw_seconds_to_next_attempt(const dh_fw_session *s, const dh_fw_record *rec,
                                       uint64_t uptime_s)
{
    if (budget_full(rec)) {
        return 0;
    }
    uint64_t hours = rec->credit_hours < DH_FW_CREDIT_HOURS_PER_ATTEMPT
                         ? DH_FW_CREDIT_HOURS_PER_ATTEMPT - rec->credit_hours
                         : 0;
    uint64_t needed = hours * DH_FW_CREDIT_HOUR_SECONDS;
    uint64_t counted = uptime_s > s->counted_from_s ? uptime_s - s->counted_from_s : 0;
    return counted < needed ? needed - counted : 0;
}

/* ---- Offers ---- */

dh_fw_offer_decision dh_fw_on_offer(dh_fw_session *s, dh_fw_record *rec, const dh_fw_keys *keys,
                                    const dh_fw_device *device, const char *offer_id,
                                    uint64_t size, uint64_t uptime_s)
{
    dh_fw_offer_decision d = {DH_FW_OFFER_IGNORE, DH_FW_REASON_NONE, 0, 0};
    if (!is_offer_id(offer_id)) {
        return d;
    }
    dh_fw_report waiting;
    if (s->trial || rec->stage != DH_FW_STAGE_IDLE || dh_fw_report_next(s, rec, &waiting)) {
        d.action = DH_FW_OFFER_BUSY;
        return d;
    }
    if (s->early_refusal && same_offer(s->early_offer_id, offer_id)) {
        return d;
    }
    d.save = dh_fw_tick(s, rec, uptime_s);
    if (rec->has_last_offer && same_offer(rec->last_offer_id, offer_id)) {
        return d;
    }
    /* Steps 1 and 2 need nothing from the hub. A refusal here spends nothing and writes nothing:
     * it is remembered, and reported, only until the next start. */
    dh_fw_reason reason = dh_fw_offer_check(keys, device, size);
    if (reason != DH_FW_REASON_NONE) {
        s->early_refusal = true;
        s->early_answered = false;
        s->early_reason = reason;
        set_offer(s->early_offer_id, offer_id);
        d.action = DH_FW_OFFER_REFUSE;
        d.reason = reason;
        return d;
    }
    if (rec->attempts_left == 0) {
        d.action = DH_FW_OFFER_WAIT;
        d.wait_seconds = dh_fw_seconds_to_next_attempt(s, rec, uptime_s);
        return d;
    }
    /* One attempt spent, with the offer it was spent on, to be saved in a single write before
     * anything is requested, so a restart still knows about it. */
    rec->attempts_left--;
    rec->has_last_offer = true;
    set_offer(rec->last_offer_id, offer_id);
    rec->stage = DH_FW_STAGE_ATTEMPT;
    d.action = DH_FW_OFFER_ATTEMPT;
    d.save = DH_FW_SAVE_RECORD;
    return d;
}

/* ---- The end of an attempt ---- */

unsigned dh_fw_attempt_ended(dh_fw_record *rec, dh_fw_reason reason)
{
    if (rec->stage != DH_FW_STAGE_ATTEMPT || !rec->has_last_offer) {
        return 0;
    }
    if (dh_fw_reason_state(reason) == DH_FW_REPORT_NONE) {
        reason = DH_FW_REASON_DOWNLOAD;
    }
    settle(rec, dh_fw_reason_state(reason), reason);
    return DH_FW_SAVE_RECORD;
}

unsigned dh_fw_attempt_installed(dh_fw_record *rec, const dh_fw_manifest *manifest, uint8_t slot)
{
    if (rec->stage != DH_FW_STAGE_ATTEMPT || !rec->has_last_offer || manifest == NULL) {
        return 0;
    }
    clear_pending(rec);
    rec->stage = DH_FW_STAGE_PENDING;
    rec->pending_slot = slot;
    size_t n = bounded_length(manifest->firmware_version, DH_FW_MAX_VERSION_BYTES);
    memcpy(rec->pending_version, manifest->firmware_version, n);
    rec->pending_security_counter = manifest->security_counter;
    if (manifest->has_key_set && manifest->key_count >= 1 &&
        manifest->key_count <= DH_FW_MAX_KEYS) {
        rec->pending_has_key_set = true;
        rec->pending_key_count = manifest->key_count;
        for (uint8_t i = 0; i < manifest->key_count; i++) {
            rec->pending_key_set[i] = manifest->key_set[i];
        }
    }
    return DH_FW_SAVE_RECORD;
}

/* ---- Reports ---- */

bool dh_fw_report_next(const dh_fw_session *s, const dh_fw_record *rec, dh_fw_report *out)
{
    memset(out, 0, sizeof *out);
    /* A report names the running version, and on a trial boot that version isn't good yet. */
    if (s->trial) {
        return false;
    }
    if (rec->result_state != DH_FW_REPORT_NONE && !rec->result_answered) {
        set_offer(out->offer_id, rec->result_offer_id);
        out->state = rec->result_state;
        out->reason = rec->result_reason;
        return true;
    }
    if (s->early_refusal && !s->early_answered) {
        set_offer(out->offer_id, s->early_offer_id);
        out->state = DH_FW_REPORT_REFUSED;
        out->reason = s->early_reason;
        return true;
    }
    return false;
}

unsigned dh_fw_report_answered(dh_fw_session *s, dh_fw_record *rec, const dh_fw_report *sent)
{
    if (rec->result_state != DH_FW_REPORT_NONE && !rec->result_answered &&
        rec->result_state == sent->state && rec->result_reason == sent->reason &&
        same_offer(rec->result_offer_id, sent->offer_id)) {
        rec->result_answered = true;
        return DH_FW_SAVE_RECORD;
    }
    if (s->early_refusal && !s->early_answered && sent->state == DH_FW_REPORT_REFUSED &&
        s->early_reason == sent->reason && same_offer(s->early_offer_id, sent->offer_id)) {
        s->early_answered = true;
    }
    return 0;
}

/* ---- The provisioning bundle ---- */

dh_fw_bundle_answer dh_fw_bundle_check(const dh_fw_keys *keys, const dh_fw_bundle *bundle)
{
    if (bundle == NULL) {
        return DH_FW_BUNDLE_OK;
    }
    if (bundle->key_count < 1 || bundle->key_count > DH_FW_MAX_KEYS) {
        return DH_FW_BUNDLE_INVALID;
    }
    /* Every key must be a valid point before anything else is decided, or stored. */
    for (uint8_t i = 0; i < bundle->key_count; i++) {
        if (!dh_p256_key_valid(bundle->keys[i].bytes)) {
            return DH_FW_BUNDLE_INVALID;
        }
        for (uint8_t k = 0; k < i; k++) {
            if (memcmp(bundle->keys[k].bytes, bundle->keys[i].bytes, DH_FW_KEY_LEN) == 0) {
                return DH_FW_BUNDLE_INVALID;
            }
        }
    }
    uint8_t mine = keys->key_count <= DH_FW_MAX_KEYS ? keys->key_count : 0;
    if (mine == 0) {
        return DH_FW_BUNDLE_OK;
    }
    /* A display with keys accepts only the same set, in any order. Neither side repeats a key,
     * so the same count and every bundle key held means the sets are equal. */
    if (bundle->key_count != mine) {
        return DH_FW_BUNDLE_KEYS_LOCKED;
    }
    for (uint8_t i = 0; i < bundle->key_count; i++) {
        bool held = false;
        for (uint8_t k = 0; k < mine; k++) {
            if (memcmp(keys->keys[k].bytes, bundle->keys[i].bytes, DH_FW_KEY_LEN) == 0) {
                held = true;
            }
        }
        if (!held) {
            return DH_FW_BUNDLE_KEYS_LOCKED;
        }
    }
    return DH_FW_BUNDLE_OK;
}

dh_fw_bundle_answer dh_fw_bundle_apply(dh_fw_record *rec, dh_fw_keys *keys,
                                       const dh_fw_bundle *bundle, unsigned *save)
{
    *save = 0;
    dh_fw_bundle_answer answer = dh_fw_bundle_check(keys, bundle);
    if (answer != DH_FW_BUNDLE_OK) {
        return answer;
    }
    uint8_t mine = keys->key_count <= DH_FW_MAX_KEYS ? keys->key_count : 0;
    if (bundle != NULL && mine == 0) {
        memset(keys->keys, 0, sizeof keys->keys);
        keys->key_count = bundle->key_count;
        for (uint8_t i = 0; i < bundle->key_count; i++) {
            keys->keys[i] = bundle->keys[i];
        }
        keys->security_floor = bundle->security_counter;
        *save |= DH_FW_SAVE_KEYS;
    }
    /* Accepting any bundle refills the budget. Nothing is written for one that is full already. */
    if (rec->attempts_left != DH_FW_FULL_BUDGET || rec->credit_hours != 0) {
        rec->attempts_left = DH_FW_FULL_BUDGET;
        rec->credit_hours = 0;
        *save |= DH_FW_SAVE_RECORD;
    }
    return DH_FW_BUNDLE_OK;
}
