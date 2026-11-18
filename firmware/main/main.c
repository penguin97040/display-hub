// The official Display Hub firmware. For now it is a board test (milestone D1): it starts the
// screen, touch and power reading, shows what it finds on three screens, and prints the same to
// the console every 10 seconds. It doesn't speak the hub protocol, join Wi-Fi or take setup over
// USB yet, and it writes nothing to flash or eFuses.

#include "board/board.h"
#include "efuse_report.h"
#include "esp_log.h"
#include "esp_lvgl_port.h"
#include "freertos/FreeRTOS.h"
#include "freertos/task.h"
#include "report.h"
#include "ui_board_test.h"

static const char *TAG = "display-hub";

// Comfortable on an AMOLED with white text on black, and well short of full brightness.
#define SCREEN_BRIGHTNESS_PERCENT 50

#define LOG_EVERY_MS 10000

static board_t s_board;
static system_report_t s_system;
static efuse_report_t s_efuse;

void app_main(void)
{
    system_report_read(&s_system);
    ESP_LOGI(TAG, "display-hub firmware %s (board test)", s_system.version);

    efuse_report_read(&s_efuse);

    const esp_err_t err = board_start(&s_board);
    if (err != ESP_OK) {
        ESP_LOGE(TAG, "the screen didn't start (%s); reporting over the console only",
                 esp_err_to_name(err));
    } else if (lvgl_port_lock(0)) {
        ui_board_test_start(&s_board, &s_system, &s_efuse);
        // Draw the first screen before turning the brightness up, so nothing left over in the
        // screen's memory is shown brightly.
        lv_refr_now(s_board.display);
        board_set_brightness(SCREEN_BRIGHTNESS_PERCENT);
        lvgl_port_unlock();
    }

    while (true) {
        report_log(&s_board, &s_system, &s_efuse, ui_board_test_last_swipe());
        vTaskDelay(pdMS_TO_TICKS(LOG_EVERY_MS));
    }
}
