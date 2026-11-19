// The Waveshare ESP32-S3-Touch-AMOLED-1.8 board: power, the I/O expander, the screen and touch.
//
// This is the only part of the firmware that knows about the board's wiring. Everything else asks
// it for an LVGL display and keeps to plain LVGL calls, so the hub protocol code never depends on
// which board it runs on.

#pragma once

#include <stdbool.h>
#include <stdint.h>

#include "esp_err.h"
#include "lvgl.h"

#ifdef __cplusplus
extern "C" {
#endif

// The screen's size in pixels, upright with the USB socket at the bottom.
#define BOARD_SCREEN_WIDTH  368
#define BOARD_SCREEN_HEIGHT 448

// Which revision of the board this is, told apart by its touch chip.
typedef enum {
    BOARD_REVISION_UNKNOWN = 0, // neither touch chip answered
    BOARD_REVISION_V1,          // FT3168 touch at I2C 0x38 (SH8601 screen)
    BOARD_REVISION_V2,          // CST820 touch at I2C 0x15 (CO5300 screen)
} board_revision_t;

// What the AXP2101 power chip reports. Only read, never written.
typedef struct {
    bool found;           // it answered on I2C and its chip ID is the AXP2101's
    uint8_t chip_id;      // register 0x03 (0x4A for an AXP2101)
    bool usb_power_good;  // status 1, bit 5
    bool battery_present; // status 1, bit 3
    bool charging;        // status 2, bits 7..5 = 001
    uint8_t dcdc_on;      // register 0x80: which DC-DC outputs are on (bits 0..4 = DC1..DC5)
    uint8_t ldo_on;       // register 0x90: which LDO outputs are on (ALDO1..4, BLDO1..2, CPUSLDO, DLDO1)
} board_power_t;

typedef struct {
    board_revision_t revision;
    bool expander_found;  // the TCA9554 at 0x20 answered and was set up
    board_power_t power;
    lv_display_t *display; // NULL if the screen couldn't be started
    bool touch_ready;      // touch is feeding LVGL
} board_t;

// Starts the board: I2C, the expander (screen power and resets), the screen, touch and LVGL.
// Fills in `board` as far as it got. Returns ESP_OK if the screen is running, even if touch
// isn't. It never writes to the power chip.
esp_err_t board_start(board_t *board);

// Reads the AXP2101's status again. Read-only.
void board_read_power(board_power_t *power);

// Sets the screen's brightness, 0 to 100. Call it while holding the LVGL lock.
esp_err_t board_set_brightness(int percent);

// Short names for log lines and the screen.
const char *board_revision_name(board_revision_t revision);

// The `hardware_revision` the display reports to the app and the hub. Only V2 is supported; a V1
// board says so truthfully, and a board that couldn't be told apart doesn't claim to be either.
const char *board_hardware_revision(board_revision_t revision);

// The BOOT button's pin. It is a strapping pin read by the chip at reset; once the firmware is
// running it is an ordinary button, low while pressed.
#define BOARD_BOOT_BUTTON_GPIO 0

#ifdef __cplusplus
}
#endif
