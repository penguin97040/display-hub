// The Waveshare ESP32-S3-Touch-AMOLED-1.8 board. See board.h.
//
// Sources followed (read 18/11/2026):
// - Waveshare's board package, waveshare/esp32_s3_touch_amoled_1_8 2.0.3: the I2C bus, the screen's
//   set-up (bsp_display_new), touch (bsp_touch_new) and brightness. Its all-in-one
//   bsp_display_start() is NOT used: it registers this QSPI screen with LVGL as if it were an RGB
//   panel, which makes esp_lvgl_port write RGB-panel callbacks into memory past the end of the
//   CO5300 driver's own data, and finish each frame before the screen has received it. The display
//   is added to LVGL here with lvgl_port_add_disp(), the call meant for SPI screens.
// - Waveshare's examples (github.com/waveshareteam/ESP32-S3-Touch-AMOLED-1.8, commit 01fd320):
//   90_axp2101_pmu's board_variant.c and the V2 Arduino sketches pulse the TCA9554's screen reset,
//   screen power and touch reset low for 20 ms, then high, before talking to touch. The board
//   package doesn't, so it's done here first. 13_display_colorbar and the board package agree on a
//   16-pixel column offset for V2 screens and none for V1.
// - The AXP2101 is left exactly as the board starts it. Waveshare's own AXP2101 example changes no
//   output and warns against it, and the board package never touches it. It is only read here.

#include "board.h"

#include "bsp/esp-bsp.h"
#include "bsp/display.h"
#include "bsp/touch.h"
#include "driver/i2c_master.h"
#include "esp_io_expander_tca9554.h"
#include "esp_lcd_panel_ops.h"
#include "esp_lcd_touch.h"
#include "esp_log.h"
#include "esp_lvgl_port.h"
#include "freertos/FreeRTOS.h"
#include "freertos/task.h"

static const char *TAG = "board";

// I2C addresses on the board's one bus (SDA GPIO 15, SCL GPIO 14).
#define ADDR_TCA9554 0x20
#define ADDR_CST820  0x15
#define ADDR_FT3168  0x38
#define ADDR_AXP2101 0x34

// The TCA9554's outputs that matter here.
#define EXP_LCD_RESET   IO_EXPANDER_PIN_NUM_0
#define EXP_LCD_POWER   IO_EXPANDER_PIN_NUM_1
#define EXP_TOUCH_RESET IO_EXPANDER_PIN_NUM_2
#define EXP_SD_CS       IO_EXPANDER_PIN_NUM_7

// AXP2101 registers (from XPowersLib's AXP2101Constants.h, as bundled in Waveshare's example).
#define AXP2101_REG_STATUS1   0x00
#define AXP2101_REG_STATUS2   0x01
#define AXP2101_REG_CHIP_ID   0x03
#define AXP2101_REG_DCDC_ON   0x80
#define AXP2101_REG_LDO_ON    0x90
#define AXP2101_CHIP_ID_VALUE 0x4A

// The V2 screen's visible area starts 16 columns into the CO5300's memory.
#define V2_COLUMN_OFFSET 16

// LVGL draws into two strips of this many lines, in internal DMA-capable memory, and sends one
// while drawing the other.
#define DRAW_LINES 40

static i2c_master_bus_handle_t s_bus;
static i2c_master_dev_handle_t s_axp2101;
static esp_lcd_panel_handle_t s_panel;

static bool probe(uint8_t addr)
{
    return i2c_master_probe(s_bus, addr, 50) == ESP_OK;
}

// i2c_master_probe() logs an error for every address that doesn't answer, which is expected while
// looking for the right touch chip. Quieten that log while probing.
static void quiet_i2c_log(bool quiet)
{
    static esp_log_level_t saved = ESP_LOG_INFO;
    if (quiet) {
        saved = esp_log_level_get("i2c.master");
        esp_log_level_set("i2c.master", ESP_LOG_NONE);
    } else {
        esp_log_level_set("i2c.master", saved);
    }
}

// Screen power on, and both resets pulsed, as Waveshare's examples do.
static bool start_expander(void)
{
    if (!probe(ADDR_TCA9554)) {
        ESP_LOGW(TAG, "I/O expander (TCA9554) didn't answer at 0x%02x", ADDR_TCA9554);
        return false;
    }

    esp_io_expander_handle_t expander = NULL;
    // Creating it sets every pin to an input with its output latch high: the chip's own power-on
    // state.
    esp_err_t err = esp_io_expander_new_i2c_tca9554(s_bus, ESP_IO_EXPANDER_I2C_TCA9554_ADDRESS_000,
                                                     &expander);
    if (err != ESP_OK) {
        ESP_LOGW(TAG, "I/O expander set-up failed: %s", esp_err_to_name(err));
        return false;
    }

    const uint32_t outputs = EXP_LCD_RESET | EXP_LCD_POWER | EXP_TOUCH_RESET | EXP_SD_CS;
    const uint32_t pulsed = EXP_LCD_RESET | EXP_LCD_POWER | EXP_TOUCH_RESET;
    err = esp_io_expander_set_dir(expander, outputs, IO_EXPANDER_OUTPUT);
    if (err == ESP_OK) {
        // The SD card's chip select stays high (not selected) throughout.
        err = esp_io_expander_set_level(expander, pulsed, 0);
    }
    vTaskDelay(pdMS_TO_TICKS(20));
    if (err == ESP_OK) {
        err = esp_io_expander_set_level(expander, outputs, 1);
    }
    // Give the screen and touch chip time to wake from reset.
    vTaskDelay(pdMS_TO_TICKS(150));

    if (err != ESP_OK) {
        ESP_LOGW(TAG, "I/O expander write failed: %s", esp_err_to_name(err));
        return false;
    }
    ESP_LOGI(TAG, "I/O expander: screen power on, screen and touch out of reset");
    return true;
}

static bool axp2101_read(uint8_t reg, uint8_t *value)
{
    return s_axp2101 != NULL &&
           i2c_master_transmit_receive(s_axp2101, &reg, 1, value, 1, 50) == ESP_OK;
}

void board_read_power(board_power_t *power)
{
    *power = (board_power_t){0};
    if (s_axp2101 == NULL) {
        return;
    }
    uint8_t id = 0, status1 = 0, status2 = 0, dcdc = 0, ldo = 0;
    if (!axp2101_read(AXP2101_REG_CHIP_ID, &id)) {
        return;
    }
    power->chip_id = id;
    if (id != AXP2101_CHIP_ID_VALUE) {
        return;
    }
    power->found = true;
    if (axp2101_read(AXP2101_REG_STATUS1, &status1)) {
        power->usb_power_good = (status1 & (1u << 5)) != 0;
        power->battery_present = (status1 & (1u << 3)) != 0;
    }
    if (axp2101_read(AXP2101_REG_STATUS2, &status2)) {
        power->charging = (status2 >> 5) == 0x01;
    }
    if (axp2101_read(AXP2101_REG_DCDC_ON, &dcdc)) {
        power->dcdc_on = dcdc;
    }
    if (axp2101_read(AXP2101_REG_LDO_ON, &ldo)) {
        power->ldo_on = ldo;
    }
}

static void start_power_chip(void)
{
    quiet_i2c_log(true);
    const bool present = probe(ADDR_AXP2101);
    quiet_i2c_log(false);
    if (!present) {
        ESP_LOGW(TAG, "power chip (AXP2101) didn't answer at 0x%02x", ADDR_AXP2101);
        return;
    }
    const i2c_device_config_t cfg = {
        .dev_addr_length = I2C_ADDR_BIT_LEN_7,
        .device_address = ADDR_AXP2101,
        // Waveshare's AXP2101 example talks to it at 100 kHz.
        .scl_speed_hz = 100000,
    };
    if (i2c_master_bus_add_device(s_bus, &cfg, &s_axp2101) != ESP_OK) {
        s_axp2101 = NULL;
    }
}

// The CO5300 only accepts areas that start on an even pixel and end on an odd one.
static void round_to_even(lv_area_t *area)
{
    area->x1 &= ~1;
    area->y1 &= ~1;
    area->x2 |= 1;
    area->y2 |= 1;
}

static lv_display_t *start_screen(board_revision_t revision)
{
    // The panel itself, with Waveshare's start-up commands. bsp_display_new() also turns the
    // screen on at full brightness, so turn it straight down until LVGL has drawn something.
    esp_lcd_panel_io_handle_t io = NULL;
    const bsp_display_config_t cfg = {0};
    if (bsp_display_new(&cfg, &s_panel, &io) != ESP_OK || s_panel == NULL) {
        ESP_LOGE(TAG, "screen set-up failed");
        return NULL;
    }
    bsp_display_brightness_set(0);
    esp_lcd_panel_set_gap(s_panel, revision == BOARD_REVISION_V2 ? V2_COLUMN_OFFSET : 0, 0);

    const lvgl_port_cfg_t port_cfg = {
        .task_priority = 4,
        .task_stack = 8192,
        .task_affinity = -1,
        .task_max_sleep_ms = 500,
        .task_stack_caps = MALLOC_CAP_INTERNAL | MALLOC_CAP_DEFAULT,
        .timer_period_ms = 5,
    };
    esp_err_t err = lvgl_port_init(&port_cfg);
    if (err != ESP_OK) {
        ESP_LOGE(TAG, "LVGL start failed: %s", esp_err_to_name(err));
        return NULL;
    }

    const lvgl_port_display_cfg_t disp_cfg = {
        .io_handle = io,
        .panel_handle = s_panel,
        .buffer_size = BOARD_SCREEN_WIDTH * DRAW_LINES,
        .double_buffer = true,
        .hres = BOARD_SCREEN_WIDTH,
        .vres = BOARD_SCREEN_HEIGHT,
        .monochrome = false,
        .rounder_cb = round_to_even,
        .color_format = LV_COLOR_FORMAT_RGB565,
        .flags = {
            .buff_dma = true,
            .buff_spiram = false,
            .sw_rotate = false,
            // The screen takes each pixel's high byte first.
            .swap_bytes = true,
        },
    };
    lv_display_t *display = lvgl_port_add_disp(&disp_cfg);
    if (display == NULL) {
        ESP_LOGE(TAG, "couldn't add the screen to LVGL");
    }
    return display;
}

static bool start_touch(lv_display_t *display)
{
    // The board package picks the driver by probing 0x15 (CST820, via the CST816S driver) and
    // then 0x38 (FT3168, via the FT5x06 driver). It also sets the same column offset as above.
    esp_lcd_touch_handle_t touch = NULL;
    quiet_i2c_log(true);
    esp_err_t err = bsp_touch_new(NULL, &touch);
    quiet_i2c_log(false);
    if (err != ESP_OK || touch == NULL) {
        ESP_LOGW(TAG, "touch set-up failed: %s", esp_err_to_name(err));
        return false;
    }
    const lvgl_port_touch_cfg_t touch_cfg = {
        .disp = display,
        .handle = touch,
    };
    if (lvgl_port_add_touch(&touch_cfg) == NULL) {
        ESP_LOGW(TAG, "couldn't add touch to LVGL");
        return false;
    }
    return true;
}

esp_err_t board_start(board_t *board)
{
    *board = (board_t){0};

    esp_err_t err = bsp_i2c_init();
    if (err != ESP_OK) {
        ESP_LOGE(TAG, "I2C bus start failed: %s", esp_err_to_name(err));
        return err;
    }
    s_bus = bsp_i2c_get_handle();

    board->expander_found = start_expander();

    start_power_chip();
    board_read_power(&board->power);

    quiet_i2c_log(true);
    if (probe(ADDR_CST820)) {
        board->revision = BOARD_REVISION_V2;
    } else if (probe(ADDR_FT3168)) {
        board->revision = BOARD_REVISION_V1;
    } else {
        board->revision = BOARD_REVISION_UNKNOWN;
    }
    quiet_i2c_log(false);
    ESP_LOGI(TAG, "board revision: %s", board_revision_name(board->revision));

    board->display = start_screen(board->revision);
    if (board->display == NULL) {
        return ESP_FAIL;
    }

    if (board->revision != BOARD_REVISION_UNKNOWN) {
        board->touch_ready = start_touch(board->display);
    }
    return ESP_OK;
}

esp_err_t board_set_brightness(int percent)
{
    if (s_panel == NULL) {
        return ESP_ERR_INVALID_STATE;
    }
    if (percent < 0) {
        percent = 0;
    } else if (percent > 100) {
        percent = 100;
    }
    return bsp_display_brightness_set(percent);
}

const char *board_revision_name(board_revision_t revision)
{
    switch (revision) {
    case BOARD_REVISION_V1:
        return "V1 (FT3168 touch)";
    case BOARD_REVISION_V2:
        return "V2 (CST820 touch)";
    default:
        return "not recognised (no touch chip answered)";
    }
}

const char *board_hardware_revision(board_revision_t revision)
{
    switch (revision) {
    case BOARD_REVISION_V1:
        return "waveshare-amoled-1.8-v1";
    case BOARD_REVISION_V2:
        return "waveshare-amoled-1.8-v2";
    default:
        return "waveshare-amoled-1.8-unknown";
    }
}
