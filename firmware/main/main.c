// The official Display Hub firmware. It takes its setup over USB from the desktop app, keeps it in
// encrypted storage, joins Wi-Fi, enrols with the hub, polls its pages and sends heartbeats, and
// can be factory-reset with the BOOT button (milestone D5). It draws the pages, one at a time with
// a swipe between them, and says how things stand (D6). Installing updates comes later.
//
// What runs:
// - the USB task (usb_setup.c) reads the port and answers `hello` and `provision`;
// - the button task (reset_button.c) watches BOOT;
// - the hub task (hub.c) does everything on the network;
// - LVGL's own task draws, and also turns the pages when the screen is swiped and keeps their
//   ages up to date (ui.c);
// - this task, the main one, decides what the screen shows and does the factory reset. The other
//   tasks only send it events. Everything that touches the screen holds the LVGL lock.

#include <stdbool.h>
#include <string.h>

#include "board/board.h"
#include "dh_usb.h"
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
#include "usb_setup.h"

static const char *TAG = "display-hub";

// Comfortable on an AMOLED with white text on black, and well short of full brightness.
#define SCREEN_BRIGHTNESS_PERCENT 50

// How often the read-only facts are printed to the console.
#define LOG_EVERY_MS 60000

typedef enum {
    EVENT_SETUP_STORED,    // the USB task stored a bundle
    EVENT_RESET_COUNTING,  // BOOT is held: `value` seconds to go
    EVENT_RESET_CANCELLED, // BOOT was let go in time
    EVENT_RESET_HELD,      // BOOT was held to the end: do the factory reset
    EVENT_HUB_CHANGED,     // the hub task has something new to show
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

static void send_event(event_type_t type, int value)
{
    const event_t event = {type, value};
    // The queue is far longer than anything these tasks can send at once. If it were ever full,
    // dropping an event only leaves the screen a moment out of date.
    xQueueSend(s_events, &event, 0);
}

// Runs in the USB task.
static void on_setup_stored(void)
{
    send_event(EVENT_SETUP_STORED, 0);
}

// Runs in the hub task.
static void on_hub_changed(void)
{
    send_event(EVENT_HUB_CHANGED, 0);
}

// Runs in the button's task.
static void on_reset_button(reset_button_event_t event, int seconds_left)
{
    switch (event) {
    case RESET_BUTTON_COUNTING:
        send_event(EVENT_RESET_COUNTING, seconds_left);
        break;
    case RESET_BUTTON_CANCELLED:
        send_event(EVENT_RESET_CANCELLED, 0);
        break;
    case RESET_BUTTON_HELD:
        send_event(EVENT_RESET_HELD, 0);
        break;
    }
}

// Forgets the screen's copy of the pages, but not its seq, so that hub_pages() doesn't copy the
// same pages straight back: it copies again only once the hub task's pages change. Call it while
// holding the LVGL lock.
static void forget_pages(void)
{
    if (s_pages != NULL) {
        const uint32_t seq = s_pages->seq;
        memset(s_pages, 0, sizeof(*s_pages));
        s_pages->seq = seq;
    }
}

static void show_state(const char *note)
{
    if (s_board.display == NULL) {
        return;
    }
    const storage_facts_t facts = storage_facts();
    hub_status(&s_hub);
    if (lvgl_port_lock(0)) {
        if (s_pages != NULL) {
            hub_pages(s_pages);
            // Pages that came with a setup that has since been erased or replaced are never
            // drawn. The hub task can take a few seconds to notice, and until it does it still
            // holds them.
            if (s_pages->has_snapshot && s_pages->generation != storage_setup_generation()) {
                forget_pages();
            }
        }
        ui_show_state(&facts, storage_key_status(), &s_hub, s_pages, note);
        lvgl_port_unlock();
    }
}

// The factory reset: erases the setup namespace and nothing else. The firmware keys, the security
// floor and the update record stay (storage.c, store_erase_setup). No restart is needed: the USB
// task reads the same facts and goes straight back to accepting a bundle, and the hub task finds
// its setup gone, stops Wi-Fi and forgets the token (hub.h). This never waits for the network.
static void factory_reset(void)
{
    storage_lock();
    const dh_usb_display display = {.store = storage_store()};
    const bool working = display.store != NULL;
    const bool erased = dh_usb_factory_reset(&display);
    storage_unlock();
    hub_wake();
    // The pages went with the setup (and an erase that failed part-way still ends that setup's
    // session). show_state() would find them out of date too; this doesn't depend on it.
    if (s_board.display != NULL && lvgl_port_lock(0)) {
        forget_pages();
        lvgl_port_unlock();
    }

    if (erased) {
        ESP_LOGW(TAG, "factory reset: the setup was erased; firmware keys and update record kept");
        show_state("The setup was erased.");
    } else if (!working) {
        ESP_LOGE(TAG, "factory reset: nothing to do, storage isn't working");
        show_state(NULL);
    } else {
        ESP_LOGE(TAG, "factory reset: the setup couldn't be erased");
        show_state("The setup couldn't be erased. Try again.");
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
    case EVENT_RESET_COUNTING:
        s_counting = true;
        if (s_board.display != NULL && lvgl_port_lock(0)) {
            ui_show_reset_countdown(event->value);
            lvgl_port_unlock();
        }
        break;
    case EVENT_RESET_CANCELLED:
        s_counting = false;
        show_state("Factory reset cancelled.");
        break;
    case EVENT_RESET_HELD:
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

    // The eFuses are read, never written, except that a new board's first start makes its storage
    // key (storage_key.h). Then encrypted NVS, and what it holds.
    efuse_report_read(&s_efuse);
    efuse_report_log(&s_efuse, "at start-up");
    storage_start(&s_efuse);
    storage_log();

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
        const storage_facts_t facts = storage_facts();
        s_pages = heap_caps_calloc(1, sizeof(*s_pages), MALLOC_CAP_SPIRAM | MALLOC_CAP_8BIT);
        if (s_pages == NULL) {
            ESP_LOGE(TAG, "out of memory for the screen's copy of the pages; showing the status "
                          "only");
        }
        ui_start(s_system.version, board_note);
        // Nothing from the hub yet: its task starts below.
        ui_show_state(&facts, storage_key_status(), &s_hub, s_pages, NULL);
        // Draw the first screen before turning the brightness up, so nothing left over in the
        // screen's memory is shown brightly.
        lv_refr_now(s_board.display);
        board_set_brightness(SCREEN_BRIGHTNESS_PERCENT);
        lvgl_port_unlock();
    }

    if (usb_err == ESP_OK) {
        usb_setup_start(s_system.version, board_hardware_revision(s_board.revision),
                        on_setup_stored);
    }
    reset_button_start(on_reset_button);
    const esp_err_t hub_err =
        hub_start(s_system.version, board_hardware_revision(s_board.revision), on_hub_changed);
    if (hub_err != ESP_OK) {
        // The screen says so (HUB_FAILED) once the display is set up.
        ESP_LOGE(TAG, "the hub task didn't start (%s): this display can't reach the hub until "
                      "it restarts",
                 esp_err_to_name(hub_err));
        show_state(NULL);
    }

    report_log(&s_board, &s_system, &s_efuse);
    int64_t logged_ms = esp_timer_get_time() / 1000;
    while (true) {
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
        report_log(&s_board, &s_system, &s_efuse);
        storage_log();
        // How close each task has come to the end of its stack, so the headroom is seen, not
        // guessed.
        if (hub_err == ESP_OK) {
            ESP_LOGI(TAG,
                     "stack never used: main task %u bytes, USB task %u bytes, hub task %u bytes",
                     (unsigned)uxTaskGetStackHighWaterMark(NULL), usb_setup_stack_unused(),
                     hub_stack_unused());
        } else {
            ESP_LOGI(TAG, "stack never used: main task %u bytes, USB task %u bytes; the hub task "
                          "isn't running",
                     (unsigned)uxTaskGetStackHighWaterMark(NULL), usb_setup_stack_unused());
        }
    }
}
