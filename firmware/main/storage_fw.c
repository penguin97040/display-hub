// What storage keeps for signed updates, and every call that changes it. See storage_fw.h.
//
// THE ORDER OF THE WRITES is the safety argument (dh_core's README, "Save points"): whatever
// the update core says to save is written before the step it guards, the keys before the record.
// A write that fails stops all update work for the rest of the run (storage_fw.h, "When a write
// fails"). Nothing here touches NVS itself: storage.c writes the two blobs.

#include "storage_fw.h"

#include <inttypes.h>
#include <string.h>

#include "esp_log.h"
#include "esp_timer.h"

static const char *TAG = "storage";

static dh_fw_keys s_keys;
static dh_fw_record s_record;
static dh_fw_session s_session;
// A write of the keys or the record failed: what flash holds is unknown, and all update work
// has stopped until the next start (storage_fw.h, "When a write fails").
static bool s_stopped;
// On trial, past the five minutes, with no other firmware to go back to.
static bool s_no_way_back;
// The latest result is a refusal made from an offer alone, which only the session holds.
static bool s_refusal_is_latest;
// The record as it stood just before the release was recorded as waiting for its trial boot,
// in case the partition can't be set to boot after all.
static bool s_has_attempt;
static dh_fw_record s_attempt;
// The final report the hub last answered without settling it, and how often it has.
static dh_fw_report s_not_taken;
static unsigned s_not_taken_count;
static void (*s_on_stopped)(void);

dh_fw_keys *storage_fw_keys(void)
{
    return &s_keys;
}

dh_fw_record *storage_fw_record(void)
{
    return &s_record;
}

void storage_fw_on_stopped(void (*notify)(void))
{
    s_on_stopped = notify;
}

void storage_fw_write_failed(void)
{
    if (!s_stopped) {
        s_stopped = true;
        if (s_on_stopped != NULL) {
            s_on_stopped();
        }
        ESP_LOGE(TAG, "a write of the firmware keys or the update record failed, so what is in "
                      "flash is no longer known: no updates, saves or update reports until "
                      "this display restarts. Its pages carry on");
    }
}

bool storage_fw_stopped(void)
{
    return s_stopped;
}

void storage_fw_changed_by_setup(void)
{
    s_refusal_is_latest = false;
    s_session.early_refusal = false;
    s_session.early_answered = false;
    s_session.early_reason = DH_FW_REASON_NONE;
    memset(s_session.early_offer_id, 0, sizeof(s_session.early_offer_id));
}

bool storage_fw_reportable(const dh_fw_keys **keys, const dh_fw_record **record)
{
    const bool ok = storage_fw_usable() && !s_stopped;
    *keys = ok ? &s_keys : NULL;
    *record = ok ? &s_record : NULL;
    return ok;
}

// Whether update work may go on: storage works, and no write has failed.
static bool working(void)
{
    return storage_fw_usable() && !s_stopped;
}

// Writes what the update core named: the keys first, and the record only once the keys are in
// flash. A record that says "installed" must never be in flash ahead of the floor and the keys
// that came with it. True if everything named was written (or nothing was). After a write has
// failed, nothing is written again in this run.
static bool fw_save(unsigned what)
{
    what &= DH_FW_SAVE_KEYS | DH_FW_SAVE_RECORD;
    if (what == 0) {
        return true;
    }
    if (s_stopped) {
        return false;
    }
    if ((what & DH_FW_SAVE_KEYS) != 0 && !storage_fw_write_keys(&s_keys)) {
        storage_fw_write_failed();
        return false;
    }
    if ((what & DH_FW_SAVE_RECORD) != 0 && !storage_fw_write_record(&s_record)) {
        storage_fw_write_failed();
        return false;
    }
    return true;
}

void storage_fw_settle_start(uint8_t running_slot, bool on_trial)
{
    unsigned save = 0;
    s_stopped = false;
    s_no_way_back = false;
    s_refusal_is_latest = false;
    s_has_attempt = false;
    s_not_taken_count = 0;
    memset(&s_not_taken, 0, sizeof(s_not_taken));
    const dh_fw_start_result started =
        dh_fw_start(&s_session, &s_record, &s_keys, running_slot, on_trial, &save);
    // The keys first, then the record. If a write fails, the same is settled at the next start.
    if (!fw_save(save)) {
        ESP_LOGE(TAG, "couldn't save what the update record needed settling at start-up");
    }
    switch (started) {
    case DH_FW_START_TRIAL:
        ESP_LOGW(TAG, "new firmware on its trial boot: it must reach the hub within %u minutes, "
                      "or the display goes back to the version before",
                 (unsigned)(DH_FW_TRIAL_SECONDS / 60u));
        break;
    case DH_FW_START_ROLLED_BACK:
        ESP_LOGW(TAG, "an update didn't prove itself: this is the version before it, running "
                      "again. Its keys were dropped and the security floor left alone");
        break;
    case DH_FW_START_INTERRUPTED:
        ESP_LOGW(TAG, "a restart cut an update's download short: it will be reported as failed");
        break;
    case DH_FW_START_INSTALLED:
        ESP_LOGW(TAG, "an update was marked good just before the last restart: its security "
                      "floor and keys are applied now");
        break;
    default:
        break;
    }
}

void storage_fw_log(void)
{
    const char *result = dh_fw_report_state_name(s_record.result_state);
    const char *reason = dh_fw_reason_name(s_record.result_reason);
    ESP_LOGI(TAG, "firmware keys: %u; security floor: %" PRIu32 "; update attempts left: %u",
             (unsigned)s_keys.key_count, s_keys.security_floor,
             (unsigned)s_record.attempts_left);
    ESP_LOGI(TAG, "updates: %s; last result: %s%s%s%s%s",
             !s_session.trial ? "not on trial"
             : s_no_way_back  ? "on trial, with no other firmware to go back to"
                              : "on trial, not yet marked good",
             result != NULL ? result : "none", reason != NULL ? " (" : "",
             reason != NULL ? reason : "", reason != NULL ? ")" : "",
             result != NULL && !s_record.result_answered ? ", not yet answered by the hub" : "");
    if (s_stopped) {
        ESP_LOGE(TAG, "updates: off until this display restarts, because a write to storage "
                      "failed");
    }
}

static uint64_t uptime_seconds(void)
{
    return (uint64_t)(esp_timer_get_time() / 1000000);
}

void storage_fw_tick(void)
{
    storage_lock();
    if (working()) {
        const uint8_t attempts = s_record.attempts_left;
        // If the write fails, the same hour is counted again after the next start.
        (void)fw_save(dh_fw_tick(&s_session, &s_record, uptime_seconds()));
        if (s_record.attempts_left != attempts) {
            ESP_LOGI(TAG, "update attempts left: %u", (unsigned)s_record.attempts_left);
        }
    }
    storage_unlock();
}

uint8_t storage_fw_attempts_left(void)
{
    storage_lock();
    const uint8_t attempts = s_record.attempts_left;
    storage_unlock();
    return attempts;
}

bool storage_fw_on_trial(void)
{
    storage_lock();
    const bool trial = s_session.trial;
    storage_unlock();
    return trial;
}

bool storage_fw_on_offer(const dh_fw_device *device, const char *offer_id, uint64_t size,
                         dh_fw_offer_decision *decision)
{
    *decision = (dh_fw_offer_decision){.action = DH_FW_OFFER_IGNORE};
    bool ok = false;
    storage_lock();
    if (working()) {
        const dh_fw_record record_before = s_record;
        const dh_fw_session session_before = s_session;
        *decision = dh_fw_on_offer(&s_session, &s_record, &s_keys, device, offer_id, size,
                                   uptime_seconds());
        ok = fw_save(decision->save);
        if (!ok) {
            // Nothing is requested. Memory goes back to what it held, so the display doesn't
            // tell the hub of an attempt it never made; what flash holds is for the next start.
            s_record = record_before;
            s_session = session_before;
            *decision = (dh_fw_offer_decision){.action = DH_FW_OFFER_IGNORE};
        } else if (decision->action == DH_FW_OFFER_REFUSE) {
            s_refusal_is_latest = true;
        } else if (decision->action == DH_FW_OFFER_ATTEMPT) {
            s_refusal_is_latest = false;
        }
    }
    storage_unlock();
    return ok;
}

dh_fw_reason storage_fw_download_begin(dh_fw_download *download, const dh_fw_device *device,
                                       uint64_t offer_size, bool has_content_length,
                                       uint64_t content_length)
{
    storage_lock();
    const dh_fw_reason reason = dh_fw_download_begin(download, &s_keys, device, offer_size,
                                                     has_content_length, content_length);
    storage_unlock();
    return reason;
}

void storage_fw_attempt_ended(dh_fw_reason reason)
{
    storage_lock();
    s_has_attempt = false;
    if (!fw_save(dh_fw_attempt_ended(&s_record, reason))) {
        ESP_LOGE(TAG, "the update's result isn't saved: the next start will settle the attempt "
                      "from what is in flash");
    }
    storage_unlock();
}

bool storage_fw_attempt_installed(const dh_fw_manifest *manifest, uint8_t slot)
{
    storage_lock();
    const dh_fw_record record_before = s_record;
    const unsigned save = working() ? dh_fw_attempt_installed(&s_record, manifest, slot) : 0;
    // No save named means the record wasn't one for an attempt: nothing may be set to boot.
    const bool ok = save != 0 && fw_save(save);
    if (ok) {
        s_attempt = record_before;
        s_has_attempt = true;
    } else {
        // In memory the record goes back to an attempt still running, which the caller now ends
        // as failed (storage_fw_attempt_ended).
        s_record = record_before;
    }
    storage_unlock();
    return ok;
}

bool storage_fw_attempt_not_set_to_boot(void)
{
    storage_lock();
    const bool ok = s_has_attempt && s_record.stage == DH_FW_STAGE_PENDING &&
                    s_attempt.stage == DH_FW_STAGE_ATTEMPT;
    if (ok) {
        // Only what dh_fw_attempt_installed() changed goes back: the budget may have earned a
        // credit hour since.
        s_record.stage = s_attempt.stage;
        s_record.pending_slot = s_attempt.pending_slot;
        memcpy(s_record.pending_version, s_attempt.pending_version,
               sizeof(s_record.pending_version));
        s_record.pending_security_counter = s_attempt.pending_security_counter;
        s_record.pending_has_key_set = s_attempt.pending_has_key_set;
        s_record.pending_key_count = s_attempt.pending_key_count;
        memcpy(s_record.pending_key_set, s_attempt.pending_key_set,
               sizeof(s_record.pending_key_set));
    }
    s_has_attempt = false;
    storage_unlock();
    return ok;
}

dh_fw_trial_step storage_fw_trial_check_in(dh_fw_check_in answer)
{
    storage_lock();
    // With nothing to go back to, the five minutes no longer count: the update core is asked
    // as at the start of the trial, so the same answers mark the image good.
    const dh_fw_trial_step step =
        dh_fw_trial_check_in(&s_session, s_no_way_back ? 0 : uptime_seconds(), answer);
    storage_unlock();
    return step;
}

void storage_fw_trial_no_way_back(void)
{
    storage_lock();
    s_no_way_back = s_session.trial;
    storage_unlock();
}

bool storage_fw_marked_good(void)
{
    storage_lock();
    s_no_way_back = false;
    const bool ok = fw_save(dh_fw_marked_good(&s_session, &s_record, &s_keys));
    if (ok) {
        ESP_LOGI(TAG, "the new firmware's security floor (%" PRIu32 ") and keys (%u) are stored",
                 s_keys.security_floor, (unsigned)s_keys.key_count);
    } else {
        // The next start finds the release waiting in the running partition, no longer on
        // trial, and finishes the job (dh_fw_start).
        ESP_LOGE(TAG, "the new firmware's security floor, keys and record aren't all saved: "
                      "the next start finishes that, and reports the update then");
    }
    storage_unlock();
    return ok;
}

bool storage_fw_report_next(dh_fw_report *out)
{
    storage_lock();
    const bool waiting = working() && dh_fw_report_next(&s_session, &s_record, out);
    storage_unlock();
    if (!waiting) {
        memset(out, 0, sizeof(*out));
    }
    return waiting;
}

static bool same_report(const dh_fw_report *a, const dh_fw_report *b)
{
    return a->state == b->state && a->reason == b->reason &&
           strncmp(a->offer_id, b->offer_id, sizeof(a->offer_id)) == 0;
}

bool storage_fw_report_not_taken(const dh_fw_report *sent)
{
    bool given_up = false;
    storage_lock();
    dh_fw_report waiting;
    // Only the report that is waiting now is counted.
    if (working() && dh_fw_report_next(&s_session, &s_record, &waiting) &&
        same_report(&waiting, sent)) {
        if (s_not_taken_count == 0 || !same_report(&s_not_taken, sent)) {
            s_not_taken = *sent;
            s_not_taken_count = 0;
        }
        s_not_taken_count++;
        if (s_not_taken_count >= DH_FW_REPORT_MAX_REFUSALS) {
            ESP_LOGE(TAG, "the hub has answered the update report (%s) %u times without taking "
                          "it: giving it up, so that later updates aren't held back",
                     dh_fw_report_state_name(sent->state), s_not_taken_count);
            s_not_taken_count = 0;
            (void)fw_save(dh_fw_report_answered(&s_session, &s_record, sent));
            given_up = true;
        }
    }
    storage_unlock();
    return given_up;
}

void storage_fw_report_answered(const dh_fw_report *sent)
{
    storage_lock();
    s_not_taken_count = 0;
    // If the write fails, the report is sent again after the next start, and the hub answers a
    // final report it has already had with 204.
    (void)fw_save(dh_fw_report_answered(&s_session, &s_record, sent));
    storage_unlock();
}

storage_fw_facts_t storage_fw_facts(void)
{
    storage_lock();
    storage_fw_facts_t facts = {
        .on_trial = s_session.trial,
        .no_way_back = s_session.trial && s_no_way_back,
        .stopped = s_stopped,
        .last_result = s_record.result_state,
        .last_reason = s_record.result_reason,
        .attempts_left = s_record.attempts_left,
    };
    if (s_refusal_is_latest && s_session.early_refusal) {
        facts.last_result = DH_FW_REPORT_REFUSED;
        facts.last_reason = s_session.early_reason;
    }
    storage_unlock();
    return facts;
}
