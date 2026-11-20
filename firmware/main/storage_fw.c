// What storage keeps for signed updates, and every call that changes it. See storage_fw.h.
//
// THE ORDER OF THE WRITES is the safety argument (dh_core's README, "Save points"): whatever
// the update core says to save is written before the step it guards, the keys before the record,
// and a write that failed is tried again before any later one. Nothing here touches NVS itself:
// storage.c writes the two blobs.

#include "storage_fw.h"

#include <inttypes.h>
#include <string.h>

#include "esp_log.h"
#include "esp_timer.h"

static const char *TAG = "storage";

static dh_fw_keys s_keys;
static dh_fw_record s_record;
static dh_fw_session s_session;
// What the update core said to save that isn't in flash yet (DH_FW_SAVE_KEYS, DH_FW_SAVE_RECORD).
static unsigned s_unsaved;

dh_fw_keys *storage_fw_keys(void)
{
    return &s_keys;
}

dh_fw_record *storage_fw_record(void)
{
    return &s_record;
}

// Writes what the update core named, and whatever an earlier write left unwritten: the keys
// first, and the record only once the keys are in flash. A record that says "installed" must
// never be in flash ahead of the floor and the keys that came with it. True when nothing is
// left to write.
static bool fw_save(unsigned what)
{
    s_unsaved |= what & (DH_FW_SAVE_KEYS | DH_FW_SAVE_RECORD);
    if ((s_unsaved & DH_FW_SAVE_KEYS) != 0 && storage_fw_write_keys(&s_keys)) {
        s_unsaved &= ~DH_FW_SAVE_KEYS;
    }
    if ((s_unsaved & DH_FW_SAVE_KEYS) == 0 && (s_unsaved & DH_FW_SAVE_RECORD) != 0 &&
        storage_fw_write_record(&s_record)) {
        s_unsaved &= ~DH_FW_SAVE_RECORD;
    }
    return s_unsaved == 0;
}

void storage_fw_settle_start(uint8_t running_slot, bool on_trial)
{
    unsigned save = 0;
    s_unsaved = 0;
    const dh_fw_start_result started =
        dh_fw_start(&s_session, &s_record, &s_keys, running_slot, on_trial, &save);
    // The keys first, then the record. If a write fails it is tried again before the next one,
    // and the same is settled at the next start.
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
             s_session.trial ? "on trial, not yet marked good" : "not on trial",
             result != NULL ? result : "none", reason != NULL ? " (" : "",
             reason != NULL ? reason : "", reason != NULL ? ")" : "",
             result != NULL && !s_record.result_answered ? ", not yet answered by the hub" : "");
    if (s_unsaved != 0) {
        ESP_LOGE(TAG, "updates: a write to storage failed and is still owed");
    }
}

static uint64_t uptime_seconds(void)
{
    return (uint64_t)(esp_timer_get_time() / 1000000);
}

void storage_fw_tick(void)
{
    storage_lock();
    if (storage_fw_usable()) {
        const uint8_t attempts = s_record.attempts_left;
        const unsigned save = dh_fw_tick(&s_session, &s_record, uptime_seconds());
        // If the write fails, it is tried again here every minute; a restart before it succeeds
        // counts the same hour again.
        if ((save != 0 || s_unsaved != 0) && !fw_save(save)) {
            ESP_LOGE(TAG, "couldn't save the update record or the firmware keys");
        }
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
    // Nothing new while an earlier save is still owed: an attempt must start from what is in
    // flash.
    if (storage_fw_usable() && fw_save(0)) {
        const dh_fw_record record_before = s_record;
        const dh_fw_session session_before = s_session;
        *decision = dh_fw_on_offer(&s_session, &s_record, &s_keys, device, offer_id, size,
                                   uptime_seconds());
        ok = fw_save(decision->save);
        if (!ok && decision->action == DH_FW_OFFER_ATTEMPT) {
            // The spent attempt isn't in flash, so there is no attempt: memory goes back to what
            // flash holds, and nothing is requested.
            s_record = record_before;
            s_session = session_before;
            s_unsaved = 0;
            ESP_LOGE(TAG, "couldn't save the update record before a download: not requesting it");
        } else {
            // Anything else saved here was only a credit hour, which is tried again later.
            ok = true;
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
    if (!fw_save(dh_fw_attempt_ended(&s_record, reason))) {
        ESP_LOGE(TAG, "couldn't save the update's result: if the display restarts first, it "
                      "will be reported as a download cut short");
    }
    storage_unlock();
}

bool storage_fw_attempt_installed(const dh_fw_manifest *manifest, uint8_t slot)
{
    storage_lock();
    const dh_fw_record record_before = s_record;
    const unsigned unsaved_before = s_unsaved;
    const unsigned save = dh_fw_attempt_installed(&s_record, manifest, slot);
    // No save named means the record wasn't one for an attempt: nothing may be set to boot.
    const bool ok = save != 0 && fw_save(save);
    if (!ok) {
        // The record goes back to what it was: an attempt still running, which the caller now
        // ends as failed (storage_fw_attempt_ended). Left as it was, it would say a release is
        // waiting for a trial boot that will never be started.
        s_record = record_before;
        s_unsaved = unsaved_before;
    }
    storage_unlock();
    return ok;
}

dh_fw_trial_step storage_fw_trial_check_in(dh_fw_check_in answer)
{
    storage_lock();
    const dh_fw_trial_step step = dh_fw_trial_check_in(&s_session, uptime_seconds(), answer);
    storage_unlock();
    return step;
}

bool storage_fw_marked_good(void)
{
    storage_lock();
    const bool ok = fw_save(dh_fw_marked_good(&s_session, &s_record, &s_keys));
    if (ok) {
        ESP_LOGI(TAG, "the new firmware's security floor (%" PRIu32 ") and keys (%u) are stored",
                 s_keys.security_floor, (unsigned)s_keys.key_count);
    } else {
        // The next start finds the release waiting in the running partition, no longer on
        // trial, and finishes the job (dh_fw_start); the writes are also tried again here.
        ESP_LOGE(TAG, "couldn't save the new firmware's security floor, keys or record yet");
    }
    storage_unlock();
    return ok;
}

bool storage_fw_report_next(dh_fw_report *out)
{
    storage_lock();
    const bool waiting = storage_fw_usable() && fw_save(0) &&
                         dh_fw_report_next(&s_session, &s_record, out);
    storage_unlock();
    if (!waiting) {
        memset(out, 0, sizeof(*out));
    }
    return waiting;
}

void storage_fw_report_answered(const dh_fw_report *sent)
{
    storage_lock();
    // If the write fails, the report is sent again after the next start, and the hub answers a
    // final report it has already had with 204.
    if (!fw_save(dh_fw_report_answered(&s_session, &s_record, sent))) {
        ESP_LOGE(TAG, "couldn't save that the hub has the update's result");
    }
    storage_unlock();
}

storage_fw_facts_t storage_fw_facts(void)
{
    storage_lock();
    const storage_fw_facts_t facts = {
        .on_trial = s_session.trial,
        .last_result = s_record.result_state,
        .last_reason = s_record.result_reason,
        .attempts_left = s_record.attempts_left,
    };
    storage_unlock();
    return facts;
}

