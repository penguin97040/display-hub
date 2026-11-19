// The official Display Hub firmware. For now it is the storage key check build (milestone D2):
// the board test's three screens (screen, touch, power and a read-only eFuse report), plus a
// fourth that burns the NVS storage key into eFuse key block KEY5 once it is confirmed on the
// screen, and then checks encrypted NVS on the real board. See storage_key.h.
//
// It doesn't speak the hub protocol, join Wi-Fi or take setup over USB yet. The only eFuse it can
// write is KEY5 (with its protection bits), and only from confirmed_burn() below.

#include <stdbool.h>

#include "board/board.h"
#include "efuse_report.h"
#include "esp_log.h"
#include "esp_lvgl_port.h"
#include "freertos/FreeRTOS.h"
#include "freertos/task.h"
#include "report.h"
#include "storage_key.h"
#include "ui_board_test.h"
#include "ui_storage_key.h"

static const char *TAG = "display-hub";

// Comfortable on an AMOLED with white text on black, and well short of full brightness.
#define SCREEN_BRIGHTNESS_PERCENT 50

#define LOG_EVERY_MS 10000

static board_t s_board;
static system_report_t s_system;
static efuse_report_t s_efuse;
static storage_key_status_t s_key;
static storage_key_checks_t s_checks;

static TaskHandle_t s_main_task;
// Set only by the second confirming tap on the screen, and used up by the burn.
static volatile bool s_burn_confirmed;

// Runs in LVGL's task when "Yes, burn it now" has been tapped. The burn itself is done by the
// main task, so the screen keeps drawing.
static void on_burn_confirmed(void)
{
    s_burn_confirmed = true;
    xTaskNotifyGive(s_main_task);
}

static void show_storage_key(const char *burn_failure)
{
    if (s_board.display != NULL && lvgl_port_lock(0)) {
        ui_board_test_refresh_efuse(&s_efuse);
        ui_storage_key_show(&s_key, &s_checks, burn_failure, on_burn_confirmed);
        lvgl_port_unlock();
    }
}

// The one place a burn starts from. Reached only when the main task is woken by the tap.
static void confirmed_burn(void)
{
    if (!s_burn_confirmed) {
        return;
    }
    s_burn_confirmed = false;

    efuse_report_t before, after;
    const esp_err_t err = storage_key_burn(&before, &after);
    s_efuse = after;
    storage_key_assess(&s_efuse, &s_key);

    const char *failure = NULL;
    if (err != ESP_OK) {
        failure = s_key.state == STORAGE_KEY_PRESENT
                      ? "The burn reported an error. See the console."
                      : "The burn didn't happen. See the console.";
    }
    if (s_key.state == STORAGE_KEY_PRESENT) {
        storage_key_check_burn(&before, &after, &s_checks);
        storage_key_run_checks(&s_efuse, &s_checks);
    }
    storage_key_log(&s_key, &s_checks);
    show_storage_key(failure);
}

void app_main(void)
{
    s_main_task = xTaskGetCurrentTaskHandle();

    system_report_read(&s_system);
    ESP_LOGI(TAG, "display-hub firmware %s (storage key check)", s_system.version);

    // Read-only. If KEY5 already holds the storage key, run the checks: on every start, so they
    // run again after a reflash. With no key, nothing here can make one.
    efuse_report_read(&s_efuse);
    efuse_report_log(&s_efuse, "at start-up");
    storage_key_assess(&s_efuse, &s_key);
    if (s_key.state == STORAGE_KEY_PRESENT) {
        storage_key_run_checks(&s_efuse, &s_checks);
    }
    storage_key_log(&s_key, &s_checks);

    const esp_err_t err = board_start(&s_board);
    if (err != ESP_OK) {
        ESP_LOGE(TAG, "the screen didn't start (%s); reporting over the console only",
                 esp_err_to_name(err));
    } else if (lvgl_port_lock(0)) {
        ui_board_test_start(&s_board, &s_system, &s_efuse);
        ui_storage_key_show(&s_key, &s_checks, NULL, on_burn_confirmed);
        // Draw the first screen before turning the brightness up, so nothing left over in the
        // screen's memory is shown brightly.
        lv_refr_now(s_board.display);
        board_set_brightness(SCREEN_BRIGHTNESS_PERCENT);
        lvgl_port_unlock();
    }

    while (true) {
        report_log(&s_board, &s_system, &s_efuse, ui_board_test_last_swipe());
        storage_key_log(&s_key, &s_checks);
        // How close app_main has come to the end of its stack, so the headroom is seen, not guessed.
        ESP_LOGI(TAG, "main task stack never used: %u bytes",
                 (unsigned)uxTaskGetStackHighWaterMark(NULL));

        // Wait for the next report, or for the confirming tap.
        if (ulTaskNotifyTake(pdTRUE, pdMS_TO_TICKS(LOG_EVERY_MS)) > 0) {
            confirmed_burn();
        }
    }
}
