// The official Display Hub firmware. It takes its setup over USB from the desktop app, keeps it in
// encrypted storage, joins Wi-Fi, enrols with the hub, polls its pages and sends heartbeats, and
// can be factory-reset with the BOOT button (milestone D5). It draws the pages, one at a time with
// a swipe between them, and says how things stand (D6). It installs the signed updates the hub
// offers, and goes back to the version before if a new one can't reach the hub (D7).
//
// What runs:
// - the USB task (usb_setup.c) reads the port and answers `hello` and `provision`;
// - the button task (reset_button.c) watches BOOT;
// - the hub task (hub.c) does everything on the network, installing an update included;
// - on a trial boot only, a small task (update.c) counts the five minutes new firmware has to
//   reach the hub;
// - LVGL's own task draws, and also turns the pages when the screen is swiped and keeps their
//   ages up to date (ui.c);
// - this task, the main one, decides what the screen shows and does the factory reset. The other
//   tasks only send it events. Everything that touches the screen holds the LVGL lock.

#include <stdbool.h>
#include <stdio.h>
#include <string.h>

#include "board/board.h"
#include "efuse_report.h"
#include "esp_heap_caps.h"
#include "esp_log.h"
#include "esp_lvgl_port.h"
#include "freertos/FreeRTOS.h"
#include "freertos/queue.h"
#include "esp_timer.h"
#include "freertos/task.h"
#include "hub.h"
#include "report.h"
#include "reset_button.h"
#include "storage.h"
#include "ui.h"
#include "update.h"
#include "usb_setup.h"

static const char *TAG = "display-hub";

// Comfortable on an AMOLED with white text on black, and well short of full brightness.
#define SCREEN_BRIGHTNESS_PERCENT 50

// How often the read-only facts are printed to the console.
#define LOG_EVERY_MS 60000

// How long "Erasing" is given to reach the screen before a factory reset's erase begins: a few
// of LVGL's 33 ms refreshes. (The erase stops everything for about 45 ms a sector, so without
// this the old words could stay up for most of it.)
#define ERASING_SHOWN_MS 150

typedef enum {
    EVENT_SETUP_STORED,    // the USB task stored a bundle
    EVENT_RESET_COUNTING,  // BOOT is held: `value` seconds to go
    EVENT_RESET_CANCELLED, // BOOT was let go in time
    EVENT_RESET_HELD,      // BOOT was held to the end: do the factory reset
    EVENT_HUB_CHANGED,     // the hub task has something new to show
    EVENT_UPDATES_STOPPED, // a write to storage failed: no updates or setup until a restart
} event_type_t;

typedef struct {
    event_type_t type;
    int value;
} event_t;

static board_t s_board;
static system_report_t s_system;
static efuse_report_t s_efuse;
static QueueHandle_t s_events;
// What the hub task last had to show.
static hub_status_t s_hub;
// The screen's copy of the pages (about 38 KB, in PSRAM). The screen draws from it whenever it
// likes, so it is only ever changed while holding the LVGL lock. NULL if there was no memory.
static hub_pages_t *s_pages;
// The factory reset countdown is on the screen: nothing else may draw over it.
static bool s_counting;
// A note that is kept on the screen, whatever else is drawn, until it no longer matters: a
// factory reset that was asked for and didn't happen. `s_lasting_line` is the same in a few
// words for the foot of each page (ui.h), and `s_lasting_generation` the setup it was said
// about: once that setup has gone or been replaced, so has the note.
static const char *s_lasting_note;
static const char *s_lasting_line;
static uint32_t s_lasting_generation;
// Whether the last hold of BOOT was turned away at once, with nothing a reset could erase.
static bool s_reset_refused;
// What didn't start at start-up, if anything, said under whatever else the status screen says.
static char s_start_note[200];
// Update work has stopped after a failed write (storage_fw.h), and the screen hasn't said so
// yet. Set from whichever task the write was on; read and cleared by the main task only.
static volatile bool s_updates_stopped;
// LVGL's own task (esp_lvgl_port names it), for its stack headroom in the log. Tiny TTF draws
// each new glyph on that stack.
static TaskHandle_t s_lvgl_task;

// `must_arrive` is for the two events that end a factory reset's countdown. Any other may be
// dropped if the queue is ever full (it is far longer than anything these tasks send at once):
// that only leaves the screen a moment out of date. A countdown that never heard BOOT had been
// let go, or held to the end, would stay on the screen for good, so those two wait for room.
static void send_event(event_type_t type, int value, bool must_arrive)
{
    const event_t event = {type, value};
    xQueueSend(s_events, &event, must_arrive ? portMAX_DELAY : 0);
}

// Runs in the USB task.
static void on_setup_stored(void)
{
    send_event(EVENT_SETUP_STORED, 0, false);
}

// Runs in whichever task made the write that failed (the hub task, the USB task or this one),
// with storage's lock held: it only queues the event.
static void on_updates_stopped(void)
{
    // The flag is what counts: the event only wakes the main task, and may be dropped if the
    // queue is full (in which case the task is awake anyway).
    s_updates_stopped = true;
    send_event(EVENT_UPDATES_STOPPED, 0, false);
}

// Runs in the hub task.
static void on_hub_changed(void)
{
    send_event(EVENT_HUB_CHANGED, 0, false);
}

// Runs in the button's task, which may wait here: it does nothing else meanwhile.
static void on_reset_button(reset_button_event_t event, int seconds_left)
{
    switch (event) {
    case RESET_BUTTON_COUNTING:
        // One of these lost only leaves the number a second behind.
        send_event(EVENT_RESET_COUNTING, seconds_left, false);
        break;
    case RESET_BUTTON_CANCELLED:
        send_event(EVENT_RESET_CANCELLED, 0, true);
        break;
    case RESET_BUTTON_HELD:
        send_event(EVENT_RESET_HELD, 0, true);
        break;
    }
}

// Forgets the screen's copy of the pages, but not its seq, so that hub_pages() doesn't copy the
// same pages straight back. It copies again at the hub task's next change: every snapshot it
// takes, or the end of its session. If that is still the old setup's (a poll that finished just
// before the task noticed the setup had gone), show_state() finds it out of date and forgets it
// again; a new setup's pages come with the right generation. Call it while holding the LVGL
// lock.
static void forget_pages(void)
{
    if (s_pages != NULL) {
        const uint32_t seq = s_pages->seq;
        memset(s_pages, 0, sizeof(*s_pages));
        s_pages->seq = seq;
    }
}

// Keeps `note` on the screen from now on (see s_lasting_note). Both strings must stay valid.
static void keep_note(const char *note, const char *line)
{
    s_lasting_note = note;
    s_lasting_line = line;
    s_lasting_generation = storage_setup_generation();
}

static void drop_note(void)
{
    s_lasting_note = NULL;
    s_lasting_line = NULL;
}

// `note` is NULL, or a line about what just happened, shown until the screen is next drawn.
// Without one, a note that is being kept is shown, and failing that whatever didn't start.
static void show_state(const char *note)
{
    if (s_board.display == NULL) {
        return;
    }
    // Copies, each taken under storage's lock and before LVGL's: the USB task changes the key's
    // status when it makes a new board's storage key. Everything wanted from storage is read
    // here, so storage's lock is never taken while LVGL's is held (nor LVGL's waited for while
    // storage's is held), here or anywhere.
    const storage_facts_t facts = storage_facts();
    const storage_key_status_t key = storage_key_status();
    const storage_fw_facts_t fw = storage_fw_facts();
    const uint32_t generation = storage_setup_generation();
    hub_status(&s_hub);
    // A status about a setup that has since been erased or replaced says nothing about the one
    // there is now. Until the hub task takes the new one up, it is joining, as for any new setup.
    // (HUB_FAILED is about the task, not the setup, and stays.)
    if (s_hub.state != HUB_FAILED && s_hub.generation != generation) {
        const hub_status_t joining = {.state = HUB_JOINING_WIFI, .generation = generation};
        s_hub = joining;
    }
    // A kept note is about the setup there was when it was said.
    if (s_lasting_note != NULL && s_lasting_generation != generation) {
        drop_note();
    }
    const char *line = NULL;
    if (note == NULL && s_lasting_note != NULL) {
        note = s_lasting_note;
        line = s_lasting_line;
    } else if (note == NULL && s_start_note[0] != '\0') {
        note = s_start_note;
    }
    if (lvgl_port_lock(0)) {
        if (s_pages != NULL) {
            hub_pages(s_pages);
            // Pages that came with a setup that has since been erased or replaced are never
            // drawn. The hub task can take a few seconds to notice, and until it does it still
            // holds them. (If the setup changes between the reading above and here, the event
            // that change sends draws the screen again.)
            if (s_pages->has_snapshot && s_pages->generation != generation) {
                forget_pages();
            }
        }
        ui_show_state(&facts, &key, &s_hub, &fw, s_pages, note, line);
        lvgl_port_unlock();
    }
}

// What the screen says when a factory reset was asked for and didn't happen. Each is kept up
// (keep_note) and gives the next step, with Install firmware as the way out of a fault that
// lasts.
#define RESET_NOT_STARTED \
    "The reset couldn't start, and nothing was erased. Hold BOOT for 10 seconds to try again. " \
    "If it keeps happening, use Install firmware in Display Hub on your computer, which erases " \
    "everything."
#define RESET_NO_STORAGE \
    "The reset wasn't done: storage isn't working, so nothing could be erased. Install " \
    "firmware in Display Hub erases everything."
// The same in a few words, for the foot of each page.
#define RESET_LINE "Factory reset didn't happen"

// The factory reset: erases the setup partition, every sector of it, and nothing else. The
// firmware keys, the security floor and the update record are in the other partition and stay
// (storage.c, erase_setup). No restart is needed: the USB task reads the same facts and goes
// straight back to accepting a bundle, and the hub task finds its setup gone, stops Wi-Fi and
// forgets the token (hub.h). This never waits for the network. The erase itself takes a second
// or two, with storage's lock held.
//
// WHAT IT CLEARS. Flash: every sector of the setup partition, read back as blank. Memory: the
// hub task wipes its copy of the setup, the token and the pages (hub.c), the screen's copy of
// the pages goes here, and the USB task drops a line it had part-read (usb_setup_forget). It
// does not promise more of memory than that: the USB driver's receive buffer and freed heap
// may hold bytes of the old setup until they are overwritten or the display loses power
// (usb_setup.c, "What stays in memory", and hub.c on the HTTP client's buffers).
static void factory_reset(void)
{
    // It works whenever NVS started, also with storage out of use for this run (storage.h).
    const storage_reset_t result = storage_factory_reset();
    // A bundle that never finished arriving is forgotten too, whatever became of the erase.
    usb_setup_forget();
    if (result == STORAGE_RESET_DONE || result == STORAGE_RESET_UNFINISHED) {
        hub_wake();
        // The pages went with the setup (and an erase that failed part-way still ends that
        // setup's session). show_state() would find them out of date too; this doesn't depend
        // on it.
        if (s_board.display != NULL && lvgl_port_lock(0)) {
            forget_pages();
            lvgl_port_unlock();
        }
    }

    switch (result) {
    case STORAGE_RESET_DONE:
        ESP_LOGW(TAG, "factory reset: the setup partition was erased; firmware keys and update "
                      "record kept");
        drop_note();
        // (With storage out of use, the screen's own words say the setup was erased.)
        show_state(storage_facts().state == STORAGE_WORKING ? "The setup was erased." : NULL);
        break;
    case STORAGE_RESET_UNFINISHED:
        // The screen says so from storage's own facts, and goes on saying so whatever is
        // drawn next, until a reset or a restart has finished the erase.
        ESP_LOGE(TAG, "factory reset: the setup couldn't be erased");
        drop_note();
        show_state(NULL);
        break;
    case STORAGE_RESET_NO_STORAGE:
        // Nothing could be erased: said on the screen, under what it says about storage.
        ESP_LOGE(TAG, "factory reset: not done, because storage isn't working. Nothing was "
                      "erased");
        keep_note(RESET_NO_STORAGE, RESET_LINE);
        show_state(NULL);
        break;
    default:
        // Nothing was erased and nothing is waiting to be: the display carries on as it was.
        ESP_LOGE(TAG, "factory reset: it couldn't start, and the setup is as it was. Hold BOOT "
                      "for 10 seconds to try again; if it keeps happening, Install firmware "
                      "over USB erases everything");
        keep_note(RESET_NOT_STARTED, RESET_LINE);
        show_state(NULL);
        break;
    }
    storage_log();
}

static void handle_event(const event_t *event)
{
    switch (event->type) {
    case EVENT_SETUP_STORED:
        hub_wake();
        show_state(NULL);
        break;
    case EVENT_HUB_CHANGED:
        // Only a display that is set up shows the hub's status. After a factory reset the
        // screen already says so, and the hub task winding down must not wipe that note.
        if (!s_counting && storage_facts().set_up) {
            show_state(NULL);
        }
        break;
    case EVENT_UPDATES_STOPPED:
        // Only a wake-up: the main loop looks at s_updates_stopped on every pass.
        break;
    case EVENT_RESET_COUNTING:
        if (!storage_reset_possible()) {
            // Storage isn't working and never started, so a reset could erase nothing: say so
            // now, not after ten seconds of holding the button.
            if (!s_reset_refused) {
                s_reset_refused = true;
                ESP_LOGE(TAG, "factory reset: BOOT is held, but storage isn't working, so "
                              "there is nothing a reset could erase");
                keep_note(RESET_NO_STORAGE, RESET_LINE);
                show_state(NULL);
            }
            break;
        }
        // A new try: what the last one left to say goes.
        drop_note();
        s_reset_refused = false;
        s_counting = true;
        if (s_board.display != NULL && lvgl_port_lock(0)) {
            ui_show_reset_countdown(event->value);
            lvgl_port_unlock();
        }
        break;
    case EVENT_RESET_CANCELLED: {
        // (If the countdown was never shown, there is nothing to say was cancelled.)
        const bool counted = s_counting;
        s_counting = false;
        s_reset_refused = false;
        show_state(counted ? "Factory reset cancelled." : NULL);
        break;
    }
    case EVENT_RESET_HELD:
        s_reset_refused = false;
        // "Let go now to cancel" is no longer true: say what is happening before the erase,
        // which holds this task for a second or two. LVGL's task draws it during the wait.
        if (storage_reset_possible() && s_board.display != NULL && lvgl_port_lock(0)) {
            s_counting = true;
            ui_show_reset_erasing();
            lvgl_port_unlock();
            vTaskDelay(pdMS_TO_TICKS(ERASING_SHOWN_MS));
        }
        s_counting = false;
        factory_reset();
        break;
    }
}

void app_main(void)
{
    // First, so that whatever the app sends from now on is kept until the USB task reads it.
    const esp_err_t usb_err = usb_setup_install();

    system_report_read(&s_system);
    ESP_LOGI(TAG, "display-hub firmware %s", s_system.version);

    s_events = xQueueCreate(16, sizeof(event_t));
    if (s_events == NULL) {
        ESP_LOGE(TAG, "out of memory at start-up");
        abort();
    }

    // The eFuses are read, never written, here. (A new board's storage key is made later, when
    // its first setup arrives over USB: storage_key.h.) Then encrypted NVS, and what it holds.
    efuse_report_read(&s_efuse);
    efuse_report_log(&s_efuse, "at start-up");
    storage_fw_on_stopped(on_updates_stopped);
    storage_start(&s_efuse);
    storage_log();
    // New firmware on its trial boot has five minutes from here to reach the hub.
    update_trial_start();

    // What the first screen says, read before LVGL's lock is taken (show_state says why).
    const storage_facts_t facts = storage_facts();
    const storage_key_status_t key = storage_key_status();
    const storage_fw_facts_t fw = storage_fw_facts();

    // The board: its revision is part of what `hello` reports, so it comes before the USB task.
    const esp_err_t board_err = board_start(&s_board);
    const char *board_note = NULL;
    if (s_board.revision == BOARD_REVISION_V1) {
        board_note = "This is a V1 board, which this firmware doesn't support.";
        ESP_LOGW(TAG, "V1 board: this firmware doesn't support it");
    } else if (s_board.revision == BOARD_REVISION_UNKNOWN) {
        board_note = "The board's revision couldn't be told.";
    }
    if (board_err != ESP_OK) {
        ESP_LOGE(TAG, "the screen didn't start (%s); carrying on without it",
                 esp_err_to_name(board_err));
    } else if (lvgl_port_lock(0)) {
        s_lvgl_task = xTaskGetHandle("taskLVGL");
        s_pages = heap_caps_calloc(1, sizeof(*s_pages), MALLOC_CAP_SPIRAM | MALLOC_CAP_8BIT);
        if (s_pages == NULL) {
            ESP_LOGE(TAG, "out of memory for the screen's copy of the pages; showing the status "
                          "only");
        }
        ui_start(s_system.version, board_note);
        // Nothing from the hub yet: its task starts below.
        ui_show_state(&facts, &key, &s_hub, &fw, s_pages, NULL, NULL);
        // Draw the first screen before turning the brightness up, so nothing left over in the
        // screen's memory is shown brightly.
        lv_refr_now(s_board.display);
        board_set_brightness(SCREEN_BRIGHTNESS_PERCENT);
        lvgl_port_unlock();
    }

    // Each of these says in the log why it failed. The screen says what it means for the
    // owner, and goes on saying it (s_start_note).
    const esp_err_t usb_started =
        usb_err != ESP_OK ? usb_err
                          : usb_setup_start(s_system.version,
                                            board_hardware_revision(s_board.revision),
                                            on_setup_stored);
    const esp_err_t button_started = reset_button_start(on_reset_button);
    if (usb_started != ESP_OK) {
        ESP_LOGE(TAG, "setup over USB didn't start (%s): this display can't be set up, or "
                      "answer the desktop app, until it restarts",
                 esp_err_to_name(usb_started));
    }
    if (button_started != ESP_OK) {
        ESP_LOGE(TAG, "the BOOT button didn't start (%s): this display can't be factory-reset "
                      "until it restarts",
                 esp_err_to_name(button_started));
    }
    if (usb_started != ESP_OK || button_started != ESP_OK) {
        snprintf(s_start_note, sizeof(s_start_note), "%s didn't start, so this display can't be "
                                                     "%s. Unplug it and plug it back in.",
                 usb_started != ESP_OK && button_started != ESP_OK
                     ? "Its USB setup and its BOOT button"
                     : usb_started != ESP_OK ? "Its USB setup" : "Its BOOT button",
                 usb_started != ESP_OK && button_started != ESP_OK
                     ? "set up from a computer or factory-reset"
                     : usb_started != ESP_OK ? "set up from a computer" : "factory-reset");
    }
    const esp_err_t hub_err =
        hub_start(s_system.version, board_hardware_revision(s_board.revision), on_hub_changed);
    if (hub_err != ESP_OK) {
        // The screen says so (HUB_FAILED) once the display is set up.
        ESP_LOGE(TAG, "the hub task didn't start (%s): this display can't reach the hub until "
                      "it restarts",
                 esp_err_to_name(hub_err));
    }
    if (hub_err != ESP_OK || s_start_note[0] != '\0') {
        show_state(NULL);
    }

    storage_efuse_report(&s_efuse);
    report_log(&s_board, &s_system, &s_efuse);
    int64_t logged_ms = esp_timer_get_time() / 1000;
    while (true) {
        // Whatever the display is doing, set up or not: the screen says a restart is needed.
        // Looked at on every pass, so it doesn't depend on its event reaching the queue. (While
        // the factory reset countdown is up it waits; whatever is drawn after that says it
        // too, since every screen is drawn from the same facts.)
        if (s_updates_stopped && !s_counting) {
            s_updates_stopped = false;
            show_state(NULL);
        }
        // Events come often once the display is polling, so the minute is counted by the clock
        // and not by the queue staying quiet.
        const int64_t since_ms = esp_timer_get_time() / 1000 - logged_ms;
        event_t event;
        if (since_ms < LOG_EVERY_MS &&
            xQueueReceive(s_events, &event, pdMS_TO_TICKS(LOG_EVERY_MS - since_ms)) == pdTRUE) {
            handle_event(&event);
            continue;
        }
        logged_ms = esp_timer_get_time() / 1000;
        // Running time earns update attempts back (dh_update.h), set up or not.
        storage_fw_tick();
        // The eFuses as storage last read them: the USB task reads them again if it makes a
        // new board's storage key, so this task prints a copy taken under storage's lock.
        storage_efuse_report(&s_efuse);
        report_log(&s_board, &s_system, &s_efuse);
        storage_log();
        // How close each task has come to the end of its stack, so the headroom is seen, not
        // guessed.
        char hub_words[40] = "isn't running";
        if (hub_err == ESP_OK) {
            snprintf(hub_words, sizeof(hub_words), "%u bytes", hub_stack_unused());
        }
        char lvgl_words[40] = "isn't running";
        if (s_lvgl_task != NULL) {
            snprintf(lvgl_words, sizeof(lvgl_words), "%u bytes",
                     (unsigned)uxTaskGetStackHighWaterMark(s_lvgl_task));
        }
        ESP_LOGI(TAG,
                 "stack never used: main task %u bytes, USB task %u bytes, hub task %s, screen "
                 "task %s",
                 (unsigned)uxTaskGetStackHighWaterMark(NULL), usb_setup_stack_unused(), hub_words,
                 lvgl_words);
    }
}
