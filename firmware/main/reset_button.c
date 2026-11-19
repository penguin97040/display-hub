// The factory reset button. See reset_button.h.

#include "reset_button.h"

#include <stdbool.h>

#include "board/board.h"
#include "driver/gpio.h"
#include "esp_log.h"
#include "esp_timer.h"
#include "freertos/FreeRTOS.h"
#include "freertos/task.h"

static const char *TAG = "reset-button";

#define SAMPLE_EVERY_MS 20
// The button's state changes only after this many samples in a row agree (100 ms).
#define DEBOUNCE_SAMPLES 5
// A press shorter than this shows nothing, so a knock doesn't flash the countdown up.
#define SHOW_AFTER_MS 1000
#define HOLD_MS       (RESET_BUTTON_HOLD_SECONDS * 1000)

static reset_button_handler_t s_handler;

static void button_task(void *arg)
{
    (void)arg;
    // A button that is already down when the display starts counts for nothing until it has
    // been let go.
    bool pressed = gpio_get_level(BOARD_BOOT_BUTTON_GPIO) == 0; // the debounced state
    bool armed = !pressed; // the button has been up since start-up, or since the last reset
    bool counting = false;
    int same = 0;
    int shown_seconds = -1;
    int64_t pressed_at_ms = 0;

    while (true) {
        vTaskDelay(pdMS_TO_TICKS(SAMPLE_EVERY_MS));
        const bool down = gpio_get_level(BOARD_BOOT_BUTTON_GPIO) == 0;
        const int64_t now_ms = esp_timer_get_time() / 1000;

        // Debounce: follow the pin only once it has held still.
        if (down != pressed) {
            if (++same >= DEBOUNCE_SAMPLES) {
                pressed = down;
                same = 0;
                if (pressed) {
                    pressed_at_ms = now_ms;
                } else {
                    if (counting) {
                        counting = false;
                        ESP_LOGI(TAG, "BOOT let go: factory reset cancelled");
                        s_handler(RESET_BUTTON_CANCELLED, 0);
                    }
                    armed = true;
                }
            }
        } else {
            same = 0;
        }

        if (!pressed || !armed) {
            continue;
        }
        const int64_t held_ms = now_ms - pressed_at_ms;
        if (held_ms >= HOLD_MS) {
            // Once per hold: nothing more happens until the button has been let go.
            armed = false;
            counting = false;
            ESP_LOGW(TAG, "BOOT held for %d seconds: factory reset", RESET_BUTTON_HOLD_SECONDS);
            s_handler(RESET_BUTTON_HELD, 0);
        } else if (held_ms >= SHOW_AFTER_MS) {
            const int seconds_left = (int)((HOLD_MS - held_ms + 999) / 1000);
            if (!counting || seconds_left != shown_seconds) {
                if (!counting) {
                    ESP_LOGI(TAG, "BOOT is held: counting down to a factory reset");
                }
                counting = true;
                shown_seconds = seconds_left;
                s_handler(RESET_BUTTON_COUNTING, seconds_left);
            }
        }
    }
}

esp_err_t reset_button_start(reset_button_handler_t handler)
{
    s_handler = handler;
    // An input with the chip's own pull-up, so the pin reads high unless the button pulls it low.
    const gpio_config_t config = {
        .pin_bit_mask = 1ULL << BOARD_BOOT_BUTTON_GPIO,
        .mode = GPIO_MODE_INPUT,
        .pull_up_en = GPIO_PULLUP_ENABLE,
        .pull_down_en = GPIO_PULLDOWN_DISABLE,
        .intr_type = GPIO_INTR_DISABLE,
    };
    const esp_err_t err = gpio_config(&config);
    if (err != ESP_OK) {
        ESP_LOGE(TAG, "couldn't set up the BOOT button: %s", esp_err_to_name(err));
        return err;
    }
    if (xTaskCreate(button_task, "reset-button", 3072, NULL, 3, NULL) != pdPASS) {
        ESP_LOGE(TAG, "the button task didn't start");
        return ESP_ERR_NO_MEM;
    }
    return ESP_OK;
}
