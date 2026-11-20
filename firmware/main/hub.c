// The display's dealings with the hub. See hub.h.
//
// THE ORDER OF THE TWO WRITES AT ENROLMENT. The hub's answer to an enrolment holds the device
// token, and the one-time secret is used up from that moment. The token is written first
// (storage_save_device), and only once that write has been read back is the secret taken out of
// the stored setup (storage_clear_secret). A power cut between the two leaves a display with
// both, which the next start tidies up (it finds a token, so it drops the secret). The other
// order could leave it with neither: enrolled on the hub, but with no way to prove it. If the
// token can't be written at all, it is kept in memory and the write is tried again at each poll;
// the screen says so, because a restart before it succeeds means setting the display up again.
// When the hub refuses the secret (used or expired), the secret is erased and the display says
// it needs setting up again: there is nothing left to try.
//
// UPDATES (docs/firmware-updates.md; dh_core's README, "Signed updates"). After each poll the hub
// answered, this task sends any final report that is waiting, then looks at the offer in the
// snapshot. The order for one attempt is the update core's: the spent attempt and the offer id
// are saved before anything is requested (storage_fw_on_offer); then `downloading` is reported,
// once; then the update form is asked for with the same pinned TLS, token and walk of addresses
// as every other request, and checked as it arrives (update.c); then the record is saved, the
// spare partition set to boot, and the display restarts. Everything that touches the update core
// runs on this task or under storage's lock. On a trial boot the poll is the check-in: nothing
// that names the new version is sent (no enrolment, heartbeat or report) and no offer is acted
// on until the image is marked good.
//
// `downloading` is sent before the download is asked for, where the reference display sends it
// once the manifest has passed its checks. Sending it then would mean a second TLS session
// while the download's is open, and internal memory is the tight resource on this board; the
// hub only shows it, and the final report says what really happened.
//
// TIME. Every wait is measured with esp_timer_get_time(), which only ever counts up from this
// start. The display never asks anyone for the date: ages are worked out on the hub's clock
// (dh_page_freshness) from the snapshot's own times and how long it has been held.

#include "hub.h"

#include <inttypes.h>
#include <stdio.h>
#include <string.h>

#include "dh_hub.h"
#include "dh_setup.h"
#include "esp_heap_caps.h"
#include "esp_log.h"
#include "esp_random.h"
#include "esp_system.h"
#include "esp_timer.h"
#include "esp_wifi_types.h"
#include "freertos/FreeRTOS.h"
#include "freertos/semphr.h"
#include "freertos/task.h"
#include "hub_http.h"
#include "mbedtls/x509_crt.h"
#include "storage.h"
#include "update.h"
#include "usb_setup.h"
#include "wifi.h"

static const char *TAG = "hub";

// TLS handshakes, JSON reading and encrypted NVS writes all run on this task. It never calls
// LVGL. Its priority is below the USB, button and drawing tasks: nothing on the network is in
// a hurry, and a long handshake must not hold up the factory reset button.
#define TASK_STACK_BYTES 12288
#define TASK_PRIORITY    2

// How often the task's stack headroom is logged, and the pages' staleness looked at again.
#define MINUTE_MS 60000
// Wi-Fi is tried again after 15 s, 30 s, then every minute (each +-20%): a router that has been
// restarted should be found again quickly, and trying costs little.
#define WIFI_RETRY_MAX_SECONDS 60u
// A join that has neither succeeded nor failed by now (no address was given, say) is given up.
#define WIFI_JOIN_TIMEOUT_MS 30000
// On a trial boot a failed check-in is tried again at least this often, whatever the usual
// back-off says: the firmware has five minutes to reach the hub.
#define TRIAL_RETRY_MS 15000
// How long "restarting" stays on the screen, and the log has to drain, before the restart.
#define RESTART_DELAY_MS 1500

static const char *s_firmware_version;
static const char *s_hardware_revision;
static void (*s_on_change)(void);
static TaskHandle_t s_task;

static SemaphoreHandle_t s_status_lock;
static hub_status_t s_status;
// The copy of the pages that hub_pages() hands out, under s_status_lock. s_pages_seq counts every
// new snapshot held (s_current), and the copy catches up with it at the next publish();
// s_content_seq counts only those whose pages differ from the ones before.
static hub_pages_t *s_shown;
static uint32_t s_pages_seq;
static uint32_t s_content_seq;

// Too big for a stack: taken from PSRAM once, at start-up.
static dh_setup *s_setup;         // this session's copy of the setup; secrets wiped as they are used
static uint8_t *s_body;           // an answer's body: DH_MAX_SNAPSHOT_BYTES + 1
static uint8_t *s_saved;          // the snapshot body that is in flash: DH_MAX_SNAPSHOT_BYTES
static dh_snapshot *s_current;    // the last good snapshot
static dh_snapshot *s_spare;      // where the next one is read
static dh_text_scratch *s_scratch;

static char s_request[DH_HUB_REQUEST_MAX];
static char s_authorization[DH_AUTHORIZATION_MAX];
// Counts every change to what storage_fw_facts() says, so the screen is told (hub_status_t).
static uint32_t s_fw_seq;

// One session: from finding a setup to that setup being erased.
typedef struct {
    uint32_t generation;
    uint8_t anchor_count;
    const char *anchors[DH_SETUP_MAX_ANCHORS];

    hub_state_t state;
    hub_detail_t detail;
    // Nothing more can be done with this setup: only wait for it to be erased.
    bool stuck;

    bool enrolled;
    bool token_saved;
    dh_device device;
    dh_poll_state poll;
    dh_heartbeat_state heartbeat;
    int64_t next_try_ms; // the next enrolment attempt or poll
    // The hub address that answered last, which the next request tries first (dh_hub_walk). In
    // memory only: every start, and every new setup, begins with the bundle's order.
    uint8_t first_host;
    // The last poll got an answer from the hub (reports may go), and a snapshot (its offer may
    // be acted on).
    bool hub_answered;
    bool fresh_snapshot;
    hub_update_t update;

    bool wifi_started; // the radio is on with this setup's Wi-Fi details
    bool wifi_was_up;
    uint32_t wifi_failures; // failed joins, or failed starts, in a row
    uint32_t wifi_drops_seen;
    int64_t wifi_retry_ms;
    int64_t wifi_join_started_ms;
    // What the screen said about the hub before a Wi-Fi state took its place: shown again once
    // the display is back on Wi-Fi, if the hub had asked it to wait.
    hub_state_t hub_state;
    hub_detail_t hub_detail;

    bool has_snapshot;
    bool saved_pages;
    int64_t received_ms;
    size_t saved_len; // how much of s_saved is the snapshot in flash; 0 for none
    bool written;     // a snapshot has been written since this start
    uint64_t written_s;
    char offer_seen[DH_OFFER_ID_HEX_LEN + 1];

    int64_t minute_ms;
} session_t;

static session_t s;

static int64_t now_ms(void)
{
    return esp_timer_get_time() / 1000;
}

// PSRAM if there is any, so the internal memory is left for Wi-Fi and TLS.
static void *big_alloc(size_t bytes)
{
    void *p = heap_caps_calloc(1, bytes, MALLOC_CAP_SPIRAM | MALLOC_CAP_8BIT);
    return p != NULL ? p : heap_caps_calloc(1, bytes, MALLOC_CAP_DEFAULT);
}

static uint32_t random32(void *ctx)
{
    (void)ctx;
    return esp_random();
}

// Whether the setup this session works from has been erased or replaced.
static bool gone(void)
{
    return storage_setup_generation() != s.generation;
}

// Sleeps until woken, for at most `ms` and never past the next minute's log.
static void wait_ms(int64_t ms)
{
    const int64_t to_minute = s.minute_ms + MINUTE_MS - now_ms();
    if (ms > to_minute) {
        ms = to_minute;
    }
    if (ms < 10) {
        ms = 10;
    }
    ulTaskNotifyTake(pdTRUE, pdMS_TO_TICKS((uint32_t)ms));
}

// ---- The status the main task shows ----

static void publish(void)
{
    static hub_status_t next;
    memset(&next, 0, sizeof(next));
    next.state = s.state;
    // s.poll.problem is what the hub last said, and is kept while Wi-Fi is the trouble.
    if (s.detail == HUB_DETAIL_WIFI || s.detail == HUB_DETAIL_WIFI_START) {
        next.problem = DH_PROBLEM_UNREACHABLE;
    } else if (s.state == HUB_TROUBLE || s.state == HUB_NEEDS_SETUP) {
        next.problem = s.poll.problem;
    }
    next.detail = s.detail;
    next.token_unsaved = s.enrolled && !s.token_saved;
    next.generation = s.generation;
    next.has_snapshot = s.has_snapshot;
    next.saved_pages = s.has_snapshot && s.saved_pages;
    next.update = s.update;
    next.fw_seq = s_fw_seq;

    xSemaphoreTake(s_status_lock, portMAX_DELAY);
    bool changed = memcmp(&next, &s_status, sizeof(next)) != 0;
    if (changed) {
        memcpy(&s_status, &next, sizeof(next));
    }
    if (s_shown->seq != s_pages_seq) {
        // Only this task writes s_current, so it can be read here without a lock of its own.
        if (s.has_snapshot) {
            s_shown->has_snapshot = true;
            s_shown->generation = s.generation;
            s_shown->saved = s.saved_pages;
            s_shown->received_ms = s.received_ms;
            memcpy(&s_shown->snapshot, s_current, sizeof(s_shown->snapshot));
        } else {
            dh_wipe(s_shown, sizeof(*s_shown));
        }
        s_shown->seq = s_pages_seq;
        s_shown->content_seq = s_content_seq;
        changed = true;
    }
    xSemaphoreGive(s_status_lock);
    if (changed && s_on_change != NULL) {
        s_on_change();
    }
}

static void set_state(hub_state_t state, hub_detail_t detail)
{
    s.state = state;
    s.detail = detail;
    publish();
}

// A problem that only setting the display up again will change.
static void stick(dh_problem problem, hub_detail_t detail)
{
    s.stuck = true;
    s.poll.problem = problem;
    set_state(problem == DH_PROBLEM_NEEDS_SETUP ? HUB_NEEDS_SETUP : HUB_TROUBLE, detail);
}

// A new snapshot is held (or none): the screen's copy catches up at the next publish(). Every
// one is passed on, so the screen always works out ages from the latest generated_at and time of
// arrival. `content` says whether the pages themselves may differ from the ones before; only then
// does the screen draw them afresh. Passing on unchanged pages costs two copies of about 38 KB in
// PSRAM (here and in the main task), well under a millisecond, once a poll: at most every 30 s.
static void pages_changed(bool content)
{
    s_pages_seq++;
    if (content) {
        s_content_seq++;
    }
}

// Whether two snapshots hold the same pages, whatever their generated_at. dh_read_response()
// zeroes a snapshot before filling it, so equal pages are equal bytes. (Bytes that differ but
// mean the same could only make this say "changed", which costs no more than a redraw.)
static bool same_pages(const dh_snapshot *a, const dh_snapshot *b)
{
    return a->page_count == b->page_count && a->page_count <= DH_MAX_PAGES &&
           memcmp(a->pages, b->pages, a->page_count * sizeof(a->pages[0])) == 0;
}

void hub_status(hub_status_t *out)
{
    // No lock means hub_start() failed before the task could run: nothing else writes this.
    if (s_status_lock == NULL) {
        *out = s_status;
        return;
    }
    xSemaphoreTake(s_status_lock, portMAX_DELAY);
    *out = s_status;
    xSemaphoreGive(s_status_lock);
}

bool hub_pages(hub_pages_t *out)
{
    if (s_status_lock == NULL || s_shown == NULL) {
        return false;
    }
    xSemaphoreTake(s_status_lock, portMAX_DELAY);
    const bool changed = out->seq != s_shown->seq;
    if (changed) {
        memcpy(out, s_shown, sizeof(*out));
    }
    xSemaphoreGive(s_status_lock);
    return changed;
}

// ---- One request, at each address in turn ----

// One try of a walk: which address it was, by its place in the list, never the address itself.
static void log_try(uint8_t host, uint8_t anchor, dh_try_result result)
{
    if (result != DH_TRY_ANSWERED) {
        ESP_LOGW(TAG, "hub address %u of %u, certificate %u of %u: %s", (unsigned)host + 1,
                 (unsigned)s_setup->host_count, (unsigned)anchor + 1, (unsigned)s.anchor_count,
                 result == DH_TRY_WRONG_HUB ? "something else answered" : "no answer");
    }
}

// Ends a walk: the address that answered is the one the next request tries first.
static dh_problem walk_ended(const dh_hub_walk *walk)
{
    dh_wipe(s_authorization, sizeof(s_authorization));
    const dh_problem problem = dh_hub_walk_problem(walk);
    if (problem == DH_PROBLEM_NONE) {
        const uint8_t answered = dh_hub_walk_answered_host(walk);
        if (answered != s.first_host) {
            ESP_LOGI(TAG, "the hub answered at address %u of %u: trying that one first from now "
                          "on",
                     (unsigned)answered + 1, (unsigned)s_setup->host_count);
            s.first_host = answered;
        }
    }
    return problem;
}

// Tries the hub address that answered last, then the others in the setup's order, and at each
// one each trust anchor while the certificate doesn't match, and stops at the first answer from
// the real hub (dh_hub_walk). Returns DH_PROBLEM_NONE with the answer in *answer and its body in
// s_body, or why there was no answer. `stopped` in the answer means the setup has gone.
static dh_problem ask_hub(const char *path, const char *body, size_t body_len, bool with_token,
                          size_t ok_cap, hub_answer_t *answer)
{
    *answer = (hub_answer_t){.result = DH_TRY_NO_ANSWER};
    if (with_token && dh_authorization(&s.device, s_authorization, sizeof(s_authorization)) == 0) {
        return DH_PROBLEM_UNREACHABLE;
    }
    dh_hub_walk walk;
    uint8_t host = 0, anchor = 0;
    dh_hub_walk_begin_at(&walk, s_setup->host_count, s.anchor_count, s.first_host);
    while (dh_hub_walk_next(&walk, &host, &anchor)) {
        const hub_request_t request = {
            .host = s_setup->hosts[host].text,
            .port = s_setup->port,
            .anchor_pem = s.anchors[anchor],
            .path = path,
            .body = body,
            .body_len = body_len,
            .authorization = with_token ? s_authorization : NULL,
            .answer = s_body,
            .ok_cap = ok_cap,
            .error_cap = DH_MAX_HUB_ANSWER_BYTES,
            .stop = gone,
        };
        *answer = hub_http_request(&request);
        if (answer->stopped) {
            break;
        }
        log_try(host, anchor, answer->result);
        dh_hub_walk_tried(&walk, answer->result);
    }
    return walk_ended(&walk);
}

// The same walk for a firmware download, always with the token. With DH_PROBLEM_NONE and status
// 200, *download is open and none of its body has been read; with any other status the error
// answer is in s_body.
static dh_problem ask_hub_for_download(const char *path, hub_download_answer_t *answer,
                                       hub_download_t **download)
{
    *answer = (hub_download_answer_t){.result = DH_TRY_NO_ANSWER};
    *download = NULL;
    if (dh_authorization(&s.device, s_authorization, sizeof(s_authorization)) == 0) {
        return DH_PROBLEM_UNREACHABLE;
    }
    dh_hub_walk walk;
    uint8_t host = 0, anchor = 0;
    dh_hub_walk_begin_at(&walk, s_setup->host_count, s.anchor_count, s.first_host);
    while (dh_hub_walk_next(&walk, &host, &anchor)) {
        const hub_request_t request = {
            .host = s_setup->hosts[host].text,
            .port = s_setup->port,
            .anchor_pem = s.anchors[anchor],
            .path = path,
            .authorization = s_authorization,
            .answer = s_body,
            .error_cap = DH_MAX_HUB_ANSWER_BYTES,
            .stop = gone,
        };
        *answer = hub_http_download_open(&request, download);
        if (answer->stopped) {
            break;
        }
        log_try(host, anchor, answer->result);
        dh_hub_walk_tried(&walk, answer->result);
    }
    return walk_ended(&walk);
}

// Writes the body of an enrolment request or a heartbeat into s_request. What the display says
// about its firmware keys and attempt budget is read under storage's lock.
static size_t build_request(bool enrol)
{
    storage_lock();
    const bool working = storage_store() != NULL;
    const dh_device_info info = {
        .device_type = USB_SETUP_DEVICE_TYPE,
        .firmware_version = s_firmware_version,
        .hardware_revision = s_hardware_revision,
        .slot_bytes = storage_spare_slot_bytes(),
        .keys = working ? storage_fw_keys() : NULL,
        .record = working ? storage_fw_record() : NULL,
    };
    const size_t len =
        enrol ? dh_enrol_request(&info, s_setup->secret, s_setup->secret_len, s_request,
                                 sizeof(s_request))
              : dh_heartbeat_request(&info, s_request, sizeof(s_request));
    storage_unlock();
    return len;
}

static void forget_secret(void)
{
    dh_wipe(s_setup->secret, sizeof(s_setup->secret));
    s_setup->secret_len = 0;
}

// The token is in flash: now the secret can go. See the top of this file.
static void save_token(void)
{
    if (s.token_saved) {
        return;
    }
    s.token_saved = storage_save_device(s.generation, &s.device);
    if (!s.token_saved) {
        if (!gone()) {
            ESP_LOGE(TAG, "the device token couldn't be stored: it is kept in memory, and the "
                          "write will be tried again");
        }
        return;
    }
    ESP_LOGI(TAG, "device ID and token stored");
    if (!storage_clear_secret(s.generation) && !gone()) {
        // It is tried again at the next start, which finds a token and a secret.
        ESP_LOGW(TAG, "the used enrolment secret couldn't be erased from the stored setup yet");
    }
}

// ---- Enrolment ----

static void enrol(void)
{
    if (s.poll.failures == 0) {
        set_state(HUB_ENROLLING, HUB_DETAIL_NONE);
    }
    const size_t len = build_request(true);
    hub_answer_t answer = {.result = DH_TRY_NO_ANSWER};
    const dh_problem problem =
        len > 0 ? ask_hub(DH_PATH_ENROL, s_request, len, false, DH_MAX_HUB_ANSWER_BYTES, &answer)
                : DH_PROBLEM_SETTINGS;
    dh_wipe(s_request, sizeof(s_request));
    if (answer.stopped || gone()) {
        dh_wipe(s_body, DH_MAX_HUB_ANSWER_BYTES + 1);
        return;
    }
    if (problem != DH_PROBLEM_NONE) {
        const uint32_t wait = dh_poll_failed(&s.poll, problem, random32, NULL);
        ESP_LOGW(TAG, "enrolment: %s; trying again in %" PRIu32 " s",
                 dh_problem_headline(problem), wait / 1000);
        s.next_try_ms = now_ms() + wait;
        set_state(HUB_TROUBLE, HUB_DETAIL_NONE);
        return;
    }

    const dh_enrol_answer read =
        dh_read_enrol_response(answer.status, s_body, answer.len, &s.device);
    // The body held the token.
    dh_wipe(s_body, DH_MAX_HUB_ANSWER_BYTES + 1);
    if (read.enrolled) {
        ESP_LOGI(TAG, "enrolled with the hub as %s", s.device.id);
        s.enrolled = true;
        s.token_saved = false;
        save_token();
        forget_secret();
        s.poll = (dh_poll_state){0};
        s.next_try_ms = now_ms();
        set_state(HUB_WAITING, HUB_DETAIL_NONE);
        return;
    }

    const uint32_t wait = dh_poll_answered(&s.poll, &read.response, 0, random32, NULL);
    const char *code = dh_error_code_name(read.response.error_code);
    if (read.response.outcome == DH_OUTCOME_REPROVISION) {
        // Used or expired: the secret is of no further use. Erase it, and stop.
        ESP_LOGE(TAG, "the hub refused the enrolment (status %d, %s): this display must be set "
                      "up again",
                 answer.status, code != NULL ? code : "no code");
        if (!storage_clear_secret(s.generation) && !gone()) {
            ESP_LOGW(TAG, "the refused enrolment secret couldn't be erased from the stored setup");
        }
        forget_secret();
        stick(DH_PROBLEM_NEEDS_SETUP, HUB_DETAIL_ENROL_REFUSED);
        return;
    }
    ESP_LOGW(TAG, "enrolment: %s (status %d, %s, %s); trying again in %" PRIu32 " s",
             dh_problem_headline(s.poll.problem), answer.status,
             dh_outcome_name(read.response.outcome), code != NULL ? code : "no code",
             wait / 1000);
    s.next_try_ms = now_ms() + wait;
    set_state(HUB_TROUBLE, HUB_DETAIL_NONE);
}

// ---- Polling ----

// Saves the snapshot in s_body as the last good one, if it is worth a write (dh_hub.h).
static void save_snapshot_if_due(size_t len)
{
    const uint64_t now_s = (uint64_t)(now_ms() / 1000);
    const bool has_saved = s.saved_len > 0;
    const bool matches = has_saved && dh_snapshot_bodies_match(s_body, len, s_saved, s.saved_len);
    if (!dh_snapshot_save_due(has_saved, matches, s.written, s.written_s, now_s)) {
        return;
    }
    memcpy(s_saved, s_body, len);
    const bool ok = storage_save_snapshot(s.generation, s_saved, len);
    s.saved_len = ok ? len : 0;
    s.written = true;
    s.written_s = now_s;
    if (ok) {
        ESP_LOGI(TAG, "saved the snapshot (%u bytes) as the last good one", (unsigned)len);
    }
}

// On a trial boot, a check-in that failed is tried again soon, whatever the back-off says.
static uint32_t trial_wait(uint32_t wait)
{
    return storage_fw_on_trial() && wait > TRIAL_RETRY_MS ? TRIAL_RETRY_MS : wait;
}

static void poll(void)
{
    // A token that couldn't be stored earlier: try again before anything else.
    save_token();

    s.hub_answered = false;
    s.fresh_snapshot = false;
    hub_answer_t answer;
    const dh_problem problem =
        ask_hub(DH_PATH_SNAPSHOT, NULL, 0, true, DH_MAX_SNAPSHOT_BYTES, &answer);
    if (answer.stopped || gone()) {
        return;
    }
    if (problem != DH_PROBLEM_NONE) {
        const uint32_t wait = trial_wait(dh_poll_failed(&s.poll, problem, random32, NULL));
        ESP_LOGW(TAG, "poll: %s; trying again in %" PRIu32 " s", dh_problem_headline(problem),
                 wait / 1000);
        s.next_try_ms = now_ms() + wait;
        set_state(HUB_TROUBLE, HUB_DETAIL_NONE);
        return;
    }
    s.hub_answered = true;

    // A 200 body over 32 KiB arrives here as 32 KiB and one byte, and is refused by its length
    // before anything is parsed. Any other status is an error answer, with its own smaller limit.
    const dh_response response =
        answer.status == 200
            ? dh_read_response(200, s_body, answer.len, NULL, s_spare, s_scratch)
            : dh_read_error_response(answer.status, s_body, answer.len);
    // On a trial boot this poll is the check-in: a request that doesn't name the new version.
    // An answer, or one that only setting the display up again would change, marks it good.
    if (storage_fw_on_trial() && update_trial_check_in(dh_fw_check_in_of(&response))) {
        s_fw_seq++;
    }
    uint32_t refresh = 0;
    if (response.outcome == DH_OUTCOME_SHOW) {
        // The same pages as before need no redraw (which would also drop a swipe in progress),
        // only their ages brought up to date. Pages restored from flash count as changed: their
        // age becomes known.
        const bool same = s.has_snapshot && !s.saved_pages && same_pages(s_current, s_spare);
        dh_snapshot *const shown = s_spare;
        s_spare = s_current;
        s_current = shown;
        s.has_snapshot = true;
        s.saved_pages = false;
        s.received_ms = now_ms();
        pages_changed(!same);
        refresh = s_current->refresh_after_seconds;
        save_snapshot_if_due(answer.len);
        s.fresh_snapshot = true;
    }
    uint32_t wait = dh_poll_answered(&s.poll, &response, refresh, random32, NULL);
    if (response.outcome != DH_OUTCOME_SHOW) {
        wait = trial_wait(wait);
    }
    s.next_try_ms = now_ms() + wait;
    if (response.outcome == DH_OUTCOME_SHOW) {
        ESP_LOGI(TAG, "poll: %u %s (%u bytes); next in %" PRIu32 " s",
                 (unsigned)s_current->page_count, s_current->page_count == 1 ? "page" : "pages",
                 (unsigned)answer.len, wait / 1000);
        set_state(HUB_ONLINE, HUB_DETAIL_NONE);
        return;
    }
    const char *code = dh_error_code_name(response.error_code);
    ESP_LOGW(TAG, "poll: %s (status %d, %s, %s, %u bytes); trying again in %" PRIu32 " s",
             dh_problem_headline(s.poll.problem), answer.status,
             dh_outcome_name(response.outcome), code != NULL ? code : "no code",
             (unsigned)answer.len, wait / 1000);
    set_state(HUB_TROUBLE, response.outcome == DH_OUTCOME_REPROVISION ? HUB_DETAIL_REVOKED
                                                                       : HUB_DETAIL_NONE);
}

// After start-up, hourly, and when the attempts left change (dh_heartbeat_due): but only while
// polls are succeeding, and never on a trial boot.
static void heartbeat_if_due(void)
{
    if (s.state != HUB_ONLINE || storage_fw_on_trial()) {
        return;
    }
    const uint64_t now_s = (uint64_t)(now_ms() / 1000);
    const uint8_t attempts = storage_fw_attempts_left();
    if (!dh_heartbeat_due(&s.heartbeat, now_s, attempts)) {
        return;
    }
    const size_t len = build_request(false);
    if (len == 0) {
        return;
    }
    hub_answer_t answer;
    const dh_problem problem =
        ask_hub(DH_PATH_HEARTBEAT, s_request, len, true, DH_MAX_HUB_ANSWER_BYTES, &answer);
    if (answer.stopped || gone()) {
        return;
    }
    const bool taken = problem == DH_PROBLEM_NONE && dh_heartbeat_taken(answer.status);
    dh_heartbeat_sent(&s.heartbeat, now_s, attempts, taken);
    if (taken) {
        ESP_LOGI(TAG, "heartbeat taken (update attempts left: %u)", (unsigned)attempts);
    } else if (problem != DH_PROBLEM_NONE) {
        ESP_LOGW(TAG, "heartbeat not sent: %s", dh_problem_headline(problem));
    } else {
        const dh_response response =
            dh_read_error_response(answer.status, s_body, answer.len);
        const char *code = dh_error_code_name(response.error_code);
        ESP_LOGW(TAG, "heartbeat not taken (status %d, %s)", answer.status,
                 code != NULL ? code : "no code");
    }
}

// ---- Signed updates ----

// Sends one firmware report. True if the hub answered it with anything at all: a final report
// is only sent again when there was no answer (docs/protocol-v1.md: `not_found` means the hub
// has no such offer any more, and a report for an offer already settled is answered 204).
static bool send_report(const dh_fw_report *report)
{
    const size_t len =
        dh_firmware_report_request(report, s_firmware_version, s_request, sizeof(s_request));
    if (len == 0) {
        ESP_LOGE(TAG, "a firmware report couldn't be written");
        return false;
    }
    hub_answer_t answer;
    const dh_problem problem =
        ask_hub(DH_PATH_FIRMWARE_REPORT, s_request, len, true, DH_MAX_HUB_ANSWER_BYTES, &answer);
    if (answer.stopped || gone()) {
        return false;
    }
    const char *state = dh_fw_report_state_name(report->state);
    const char *reason = dh_fw_reason_name(report->reason);
    if (problem != DH_PROBLEM_NONE) {
        ESP_LOGW(TAG, "update report (%s) not sent: %s", state, dh_problem_headline(problem));
        return false;
    }
    if (answer.status >= 200 && answer.status < 300) {
        ESP_LOGI(TAG, "update report taken: %s%s%s", state, reason != NULL ? ", " : "",
                 reason != NULL ? reason : "");
    } else {
        const dh_response response = dh_read_error_response(answer.status, s_body, answer.len);
        const char *code = dh_error_code_name(response.error_code);
        ESP_LOGW(TAG, "update report (%s) answered with status %d (%s): not sent again", state,
                 answer.status, code != NULL ? code : "no code");
    }
    return true;
}

// Sends the final reports that are waiting: the record's, then a refusal made from an offer
// alone. One that gets no answer is left for the next poll. Nothing is given out on a trial boot.
static void send_reports(void)
{
    dh_fw_report report;
    for (int i = 0; i < 2 && !gone() && storage_fw_report_next(&report); i++) {
        if (!send_report(&report)) {
            return;
        }
        storage_fw_report_answered(&report);
        s_fw_seq++;
    }
}

static void restart_into_update(void)
{
    s.update = HUB_UPDATE_RESTARTING;
    publish();
    ESP_LOGW(TAG, "restarting into the new firmware");
    vTaskDelay(pdMS_TO_TICKS(RESTART_DELAY_MS));
    esp_restart();
}

// One attempt, whose spent budget and offer id are already in flash.
static void install(const dh_firmware_offer *offer, const dh_fw_device *device)
{
    ESP_LOGI(TAG, "the hub offers a firmware update (%" PRIu64 " bytes): downloading it. "
                  "Update attempts left: %u",
             offer->size, (unsigned)storage_fw_attempts_left());
    s.update = HUB_UPDATE_DOWNLOADING;
    s_fw_seq++;
    publish();

    // Once, and whatever becomes of it: only a final report is sent again.
    dh_fw_report downloading = {.state = DH_FW_REPORT_DOWNLOADING};
    strlcpy(downloading.offer_id, offer->offer_id, sizeof(downloading.offer_id));
    (void)send_report(&downloading);

    dh_fw_reason reason = DH_FW_REASON_DOWNLOAD;
    char path[DH_FIRMWARE_PATH_MAX];
    if (!gone() && dh_firmware_path(offer->package_id, path, sizeof(path)) > 0) {
        // The 15 minutes run from here: finding the hub is part of the download.
        const int64_t started_ms = now_ms();
        hub_download_answer_t answer;
        hub_download_t *download = NULL;
        const dh_problem problem = ask_hub_for_download(path, &answer, &download);
        if (answer.stopped || gone()) {
            ESP_LOGW(TAG, "the setup has gone: the download isn't made");
        } else if (problem != DH_PROBLEM_NONE) {
            ESP_LOGW(TAG, "the download wasn't answered: %s", dh_problem_headline(problem));
        } else if (answer.status != 200 || download == NULL) {
            const dh_response response =
                dh_read_error_response(answer.status, s_body, answer.len);
            const char *code = dh_error_code_name(response.error_code);
            ESP_LOGW(TAG, "the hub didn't send the download (status %d, %s)", answer.status,
                     code != NULL ? code : "no code");
        } else {
            reason = update_receive(download, device, offer->size, answer.has_length,
                                    answer.length, started_ms, gone);
        }
        hub_http_download_close(download);
    }
    ESP_LOGI(TAG, "hub task stack never used: %u bytes", hub_stack_unused());
    if (reason == DH_FW_REASON_NONE) {
        restart_into_update();
    }

    // Refused or failed: the record first, then the report.
    storage_fw_attempt_ended(reason);
    s.update = HUB_UPDATE_NONE;
    s_fw_seq++;
    publish();
    send_reports();
}

// What to do about the offer in the snapshot just taken, if it has one.
static void act_on_offer(void)
{
    const dh_firmware_offer *offer = &s_current->firmware_update;
    if (!offer->present || !offer->usable) {
        return;
    }
    const dh_fw_device device = {
        .device_type = USB_SETUP_DEVICE_TYPE,
        .hardware_revision = s_hardware_revision,
        .slot_bytes = storage_spare_slot_bytes(),
    };
    dh_fw_offer_decision decision;
    if (!storage_fw_on_offer(&device, offer->offer_id, offer->size, &decision)) {
        return;
    }
    // An offer is in every snapshot until it is settled: say what became of it once.
    const bool first_sight = strcmp(offer->offer_id, s.offer_seen) != 0;
    switch (decision.action) {
    case DH_FW_OFFER_ATTEMPT:
        strlcpy(s.offer_seen, offer->offer_id, sizeof(s.offer_seen));
        install(offer, &device);
        return;
    case DH_FW_OFFER_REFUSE:
        strlcpy(s.offer_seen, offer->offer_id, sizeof(s.offer_seen));
        ESP_LOGW(TAG, "refused the firmware update the hub offers (%s): nothing was requested "
                      "or written",
                 dh_fw_reason_name(decision.reason));
        s_fw_seq++;
        send_reports();
        return;
    case DH_FW_OFFER_WAIT:
        if (first_sight) {
            strlcpy(s.offer_seen, offer->offer_id, sizeof(s.offer_seen));
            ESP_LOGW(TAG, "the hub offers a firmware update, but no update attempts are left: "
                          "the next is earned after %" PRIu64 " more minutes of running",
                     (decision.wait_seconds + 59u) / 60u);
        }
        return;
    default:
        // The offer last acted on, or nothing new is acted on yet (a report is still waiting
        // for the hub's answer, or this is a trial boot).
        return;
    }
}

// After a poll: any report that is waiting, then the offer in a snapshot just taken.
static void updates(void)
{
    if (!s.hub_answered || storage_fw_on_trial()) {
        return;
    }
    send_reports();
    if (s.fresh_snapshot && !gone()) {
        act_on_offer();
    }
}

// ---- Wi-Fi ----

static bool showing_wifi(void)
{
    return s.state == HUB_JOINING_WIFI || s.detail == HUB_DETAIL_WIFI ||
           s.detail == HUB_DETAIL_WIFI_START;
}

// Shows how Wi-Fi stands in place of what the hub last said, which is kept to go back to.
static void show_wifi(hub_state_t state, hub_detail_t detail)
{
    if (!showing_wifi()) {
        s.hub_state = s.state;
        s.hub_detail = s.detail;
    }
    set_state(state, detail);
}

// How long to wait after another failure in a row to join Wi-Fi, or to start it.
static uint32_t wifi_failed(void)
{
    s.wifi_failures++;
    uint32_t seconds = dh_backoff_seconds(s.wifi_failures);
    if (seconds > WIFI_RETRY_MAX_SECONDS) {
        seconds = WIFI_RETRY_MAX_SECONDS;
    }
    const uint32_t wait = dh_jitter_ms(seconds * 1000u, 20, random32, NULL);
    s.wifi_retry_ms = now_ms() + wait;
    return wait;
}

// Turns the radio on with `setup`'s Wi-Fi details. If that fails (out of memory, say), it is
// tried again on the same back-off as joining (start_wifi_again).
static void start_wifi(const dh_setup *setup)
{
    esp_err_t err = wifi_prepare(s_task);
    if (err == ESP_OK) {
        err = wifi_start(setup);
    }
    if (err != ESP_OK) {
        const uint32_t wait = wifi_failed();
        ESP_LOGE(TAG, "Wi-Fi didn't start (%s, %" PRIu32 " in a row); trying again in %" PRIu32
                      " s",
                 esp_err_to_name(err), s.wifi_failures, wait / 1000);
        show_wifi(HUB_TROUBLE, HUB_DETAIL_WIFI_START);
        return;
    }
    s.wifi_started = true;
    s.wifi_failures = 0;
    s.wifi_drops_seen = wifi_drops();
    // Join straight away.
    s.wifi_retry_ms = now_ms();
    show_wifi(HUB_JOINING_WIFI, HUB_DETAIL_NONE);
}

// The session's copy of the setup has had its Wi-Fi details wiped, so another try at starting
// Wi-Fi reads them from storage again, and wipes them straight after.
static void start_wifi_again(void)
{
    dh_setup *fresh = big_alloc(sizeof(*fresh));
    if (fresh == NULL) {
        const uint32_t wait = wifi_failed();
        ESP_LOGE(TAG, "Wi-Fi didn't start (out of memory to read its details, %" PRIu32
                      " in a row); trying again in %" PRIu32 " s",
                 s.wifi_failures, wait / 1000);
        return;
    }
    // Under storage's lock, which it takes itself.
    uint32_t generation = 0;
    const bool loaded = storage_load_setup(fresh, &generation);
    if (generation != s.generation) {
        // Erased or replaced: the session ends.
    } else if (!loaded) {
        const uint32_t wait = wifi_failed();
        ESP_LOGE(TAG, "Wi-Fi didn't start (its details couldn't be read, %" PRIu32
                      " in a row); trying again in %" PRIu32 " s",
                 s.wifi_failures, wait / 1000);
    } else {
        start_wifi(fresh);
    }
    dh_setup_wipe(fresh);
    heap_caps_free(fresh);
}

// The signal that came with a failed join or a drop, for the log. Only when an access point was
// actually reached: with reason 0 (this firmware gave up, or the address lapsed) the driver gave
// none, and with "not found" reasons it describes no access point. 0 dBm is a real reading.
static void signal_words(unsigned reason, int rssi, char *out, size_t size)
{
    switch (reason) {
    case 0:
    case WIFI_REASON_NO_AP_FOUND:
    case WIFI_REASON_NO_AP_FOUND_W_COMPATIBLE_SECURITY:
    case WIFI_REASON_NO_AP_FOUND_IN_AUTHMODE_THRESHOLD:
        snprintf(out, size, "none");
        return;
    default:
        snprintf(out, size, "%d dBm", rssi);
        return;
    }
}

// Keeps the display on Wi-Fi. True when it is; otherwise it has done the next thing towards
// joining, or waited, and the caller should come round again.
static bool wifi_ready(void)
{
    int64_t now = now_ms();
    bool join_failed = false;
    const uint32_t drops = wifi_drops();
    if (drops != s.wifi_drops_seen) {
        s.wifi_drops_seen = drops;
        if (s.wifi_was_up) {
            // It was joined and has dropped: try once straight away.
            s.wifi_was_up = false;
            s.wifi_failures = 0;
            s.wifi_retry_ms = now + 1000;
            const unsigned reason = wifi_last_reason();
            char signal[16];
            signal_words(reason, wifi_last_rssi(), signal, sizeof(signal));
            // A drop with reason 0 is the address lapsing (wifi.c records the lost address so),
            // not the "no answer in time" that reason 0 means for a join.
            ESP_LOGW(TAG, "Wi-Fi dropped (reason %u: %s; signal %s); joining again", reason,
                     reason == 0 ? "the address lapsed" : wifi_reason_words(reason), signal);
        } else {
            const uint32_t wait = wifi_failed();
            const unsigned reason = wifi_last_reason();
            char signal[16];
            signal_words(reason, wifi_last_rssi(), signal, sizeof(signal));
            ESP_LOGW(TAG, "couldn't join Wi-Fi (reason %u: %s; signal %s; %" PRIu32 " in a row); "
                          "trying again in %" PRIu32 " s",
                     reason, wifi_reason_words(reason), signal, s.wifi_failures, wait / 1000);
            join_failed = true;
        }
    }

    switch (wifi_link()) {
    case WIFI_LINK_UP:
        if (!s.wifi_was_up) {
            s.wifi_was_up = true;
            s.wifi_failures = 0;
            ESP_LOGI(TAG, "joined Wi-Fi and was given an address");
            wifi_log_joined();
            // Straight to the hub, unless the last answer said to wait (the hub is busy, the
            // display needs setting up again): Wi-Fi coming back doesn't change those, so what
            // the screen said about them is shown again.
            const bool hub_waits =
                s.poll.problem != DH_PROBLEM_NONE && s.poll.problem != DH_PROBLEM_UNREACHABLE;
            if (!hub_waits) {
                s.next_try_ms = now;
            }
            if (showing_wifi()) {
                if (hub_waits) {
                    set_state(s.hub_state, s.hub_detail);
                } else {
                    set_state(s.enrolled ? HUB_WAITING : HUB_ENROLLING, HUB_DETAIL_NONE);
                }
            }
        }
        return true;
    case WIFI_LINK_JOINING:
        if (now - s.wifi_join_started_ms > WIFI_JOIN_TIMEOUT_MS) {
            ESP_LOGW(TAG, "joining Wi-Fi took too long; giving this try up");
            wifi_give_up();
        } else {
            wait_ms(WIFI_JOIN_TIMEOUT_MS - (now - s.wifi_join_started_ms) + 10);
        }
        return false;
    default:
        break;
    }

    // Not joined. Say so once it has failed twice: one miss is often just the router being slow.
    if (s.wifi_failures >= 2) {
        if (s.detail != HUB_DETAIL_WIFI) {
            show_wifi(HUB_TROUBLE, HUB_DETAIL_WIFI);
        }
    } else if (s.state != HUB_JOINING_WIFI) {
        show_wifi(HUB_JOINING_WIFI, HUB_DETAIL_NONE);
    }
    // Once for each failed join, after the screen has been told, and inside the wait before the
    // next try (at least 12 s): the link is down here and only this task starts a join, so the
    // scan can't overlap one.
    if (join_failed) {
        wifi_log_survey();
    }
    now = now_ms();
    if (now >= s.wifi_retry_ms) {
        s.wifi_join_started_ms = now;
        if (wifi_join() != ESP_OK) {
            s.wifi_retry_ms = now + 15000;
        }
    } else {
        wait_ms(s.wifi_retry_ms - now);
    }
    return false;
}

// ---- A session ----

// Keeps the anchors that read as a certificate. One that doesn't could never match a hub, and
// esp-tls would refuse to connect at all with it.
static void find_anchors(void)
{
    s.anchor_count = 0;
    for (uint8_t i = 0; i < s_setup->anchor_count && i < DH_SETUP_MAX_ANCHORS; i++) {
        const dh_setup_anchor *anchor = &s_setup->anchors[i];
        mbedtls_x509_crt certificate;
        mbedtls_x509_crt_init(&certificate);
        // A NUL inside the text would cut it short wherever it is used as a string.
        const bool reads =
            strlen(anchor->pem) == anchor->pem_len &&
            mbedtls_x509_crt_parse(&certificate, (const unsigned char *)anchor->pem,
                                   (size_t)anchor->pem_len + 1) == 0;
        mbedtls_x509_crt_free(&certificate);
        if (reads) {
            s.anchors[s.anchor_count++] = anchor->pem;
        } else {
            ESP_LOGE(TAG, "the hub certificate %u in the setup doesn't read as a certificate",
                     (unsigned)i + 1);
        }
    }
}

// Shows the snapshot saved before this start, until a poll succeeds.
static void load_saved_snapshot(void)
{
    s.saved_len = storage_load_snapshot(s.generation, s_saved, DH_MAX_SNAPSHOT_BYTES);
    if (s.saved_len == 0) {
        return;
    }
    const dh_response response =
        dh_read_response(200, s_saved, s.saved_len, NULL, s_current, s_scratch);
    if (response.outcome == DH_OUTCOME_SHOW) {
        s.has_snapshot = true;
        s.saved_pages = true;
        s.received_ms = now_ms();
        pages_changed(true);
        ESP_LOGI(TAG, "showing the saved snapshot (%u bytes, %u pages) until the hub answers",
                 (unsigned)s.saved_len, (unsigned)s_current->page_count);
    } else {
        // Written by other firmware, perhaps. The next good snapshot replaces it.
        ESP_LOGW(TAG, "the saved snapshot can't be read by this firmware; ignoring it");
    }
}

static void minute_log(void)
{
    const int64_t now = now_ms();
    if (now - s.minute_ms < MINUTE_MS) {
        return;
    }
    s.minute_ms = now;
    // The screen works out the pages' ages itself; this only makes sure nothing is missed.
    publish();
}

// Works from the setup in s_setup until it is erased or replaced.
static void run_session(void)
{
    find_anchors();

    switch (storage_load_device(s.generation, &s.device)) {
    case STORAGE_HELD:
        s.enrolled = true;
        s.token_saved = true;
        ESP_LOGI(TAG, "enrolled already, as %s", s.device.id);
        if (s_setup->secret_len > 0) {
            // A power cut came between the two writes at enrolment. Finish the job.
            if (storage_clear_secret(s.generation)) {
                ESP_LOGI(TAG, "erased the used enrolment secret from the stored setup");
            }
            forget_secret();
        }
        break;
    case STORAGE_NOT_HELD:
        if (s_setup->secret_len == 0) {
            ESP_LOGE(TAG, "set up, but with no enrolment secret and no device token");
            stick(DH_PROBLEM_NEEDS_SETUP, HUB_DETAIL_NO_TOKEN);
        }
        break;
    default:
        if (!gone()) {
            ESP_LOGE(TAG, "the stored device token can't be read");
            stick(DH_PROBLEM_NEEDS_SETUP, HUB_DETAIL_NO_TOKEN);
        }
        break;
    }
    load_saved_snapshot();

    if (!s.stuck && !s_setup->has_wifi) {
        // "If its wifi is null, keep the settings you have": this display has no others.
        ESP_LOGE(TAG, "the setup has no Wi-Fi details, and this display has none of its own");
        stick(DH_PROBLEM_SETTINGS, HUB_DETAIL_NO_WIFI_DETAILS);
    }
    if (!s.stuck && s.anchor_count == 0) {
        stick(DH_PROBLEM_SETTINGS, HUB_DETAIL_NO_ANCHOR);
    }
    if (!s.stuck) {
        start_wifi(s_setup);
    }
    // The driver has its own copy now. If it didn't start, start_wifi_again reads them again.
    dh_wipe(s_setup->ssid, sizeof(s_setup->ssid));
    dh_wipe(s_setup->password, sizeof(s_setup->password));
    s_setup->ssid_len = 0;
    s_setup->password_len = 0;
    publish();

    while (!gone()) {
        minute_log();
        if (s.stuck) {
            wait_ms(MINUTE_MS);
            continue;
        }
        if (!s.wifi_started) {
            const int64_t now = now_ms();
            if (now < s.wifi_retry_ms) {
                wait_ms(s.wifi_retry_ms - now);
            } else {
                start_wifi_again();
            }
            continue;
        }
        if (!wifi_ready()) {
            continue;
        }
        const int64_t now = now_ms();
        if (now < s.next_try_ms) {
            wait_ms(s.next_try_ms - now);
            continue;
        }
        if (!s.enrolled) {
            if (storage_fw_on_trial()) {
                // An enrolment names the running version, which a trial boot must not do. A
                // display with no token can't check in, so this firmware goes back when its
                // five minutes are up (update.c).
                wait_ms(TRIAL_RETRY_MS);
            } else {
                enrol();
            }
        } else {
            poll();
            if (!gone()) {
                updates();
            }
            if (!gone()) {
                heartbeat_if_due();
            }
        }
    }
}

// Stops the network and wipes everything the session held.
static void end_session(void)
{
    wifi_stop();
    dh_setup_wipe(s_setup);
    dh_device_wipe(&s.device);
    dh_wipe(s_request, sizeof(s_request));
    dh_wipe(s_authorization, sizeof(s_authorization));
    dh_wipe(s_body, DH_MAX_SNAPSHOT_BYTES + 1);
    dh_wipe(s_saved, DH_MAX_SNAPSHOT_BYTES);
    dh_wipe(s_current, sizeof(*s_current));
    dh_wipe(s_spare, sizeof(*s_spare));
    const int64_t minute = s.minute_ms;
    memset(&s, 0, sizeof(s));
    s.minute_ms = minute;
    s.state = HUB_IDLE;
    pages_changed(true);
    publish();
}

static void hub_task(void *arg)
{
    (void)arg;
    s.minute_ms = now_ms();
    while (true) {
        uint32_t generation = 0;
        if (!storage_load_setup(s_setup, &generation)) {
            // Not set up, or storage isn't working: nothing to do until that changes.
            wait_ms(MINUTE_MS);
            if (now_ms() - s.minute_ms >= MINUTE_MS) {
                s.minute_ms = now_ms();
            }
            continue;
        }
        s.generation = generation;
        ESP_LOGI(TAG, "set up: starting to talk to the hub (%u %s)", (unsigned)s_setup->host_count,
                 s_setup->host_count == 1 ? "address" : "addresses");
        run_session();
        ESP_LOGW(TAG, "the setup has gone: stopping Wi-Fi and forgetting what was held");
        end_session();
    }
}

esp_err_t hub_start(const char *firmware_version, const char *hardware_revision,
                    void (*on_change)(void))
{
    s_firmware_version = firmware_version;
    s_hardware_revision = hardware_revision;
    s_on_change = on_change;
    // esp-tls logs the name it couldn't look up, which is one of the hub's addresses. Keep those
    // out of the console: hub_http.c logs why a connection failed, without the address.
    esp_log_level_set("esp-tls", ESP_LOG_NONE);
    s_status_lock = xSemaphoreCreateMutex();
    s_setup = big_alloc(sizeof(*s_setup));
    s_body = big_alloc(DH_MAX_SNAPSHOT_BYTES + 1);
    s_saved = big_alloc(DH_MAX_SNAPSHOT_BYTES);
    s_current = big_alloc(sizeof(*s_current));
    s_spare = big_alloc(sizeof(*s_spare));
    s_scratch = big_alloc(sizeof(*s_scratch));
    s_shown = big_alloc(sizeof(*s_shown));
    if (s_status_lock == NULL || s_setup == NULL || s_body == NULL || s_saved == NULL ||
        s_current == NULL || s_spare == NULL || s_scratch == NULL || s_shown == NULL) {
        ESP_LOGE(TAG, "out of memory for the hub task");
        s_status.state = HUB_FAILED;
        return ESP_ERR_NO_MEM;
    }
    _Static_assert(DH_MAX_SNAPSHOT_BYTES >= DH_MAX_HUB_ANSWER_BYTES, "one buffer for every answer");
    if (xTaskCreate(hub_task, "hub", TASK_STACK_BYTES, NULL, TASK_PRIORITY, &s_task) != pdPASS) {
        ESP_LOGE(TAG, "the hub task didn't start");
        s_status.state = HUB_FAILED;
        return ESP_ERR_NO_MEM;
    }
    return ESP_OK;
}

void hub_wake(void)
{
    if (s_task != NULL) {
        xTaskNotifyGive(s_task);
    }
}

unsigned hub_stack_unused(void)
{
    return s_task != NULL ? (unsigned)uxTaskGetStackHighWaterMark(s_task) : 0;
}
