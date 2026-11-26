// Restarts that reset the radio. See radio.h.
//
// What was read in ESP-IDF v5.5.6 to write this (paths are under components/):
// - esp_phy/src/phy_init.c: the first esp_phy_enable() of a start, which esp_wifi_start() makes,
//   calibrates the radio (esp_phy_load_cal_and_init, line 926); later ones in the same start only
//   wake it (344-349), so stopping and starting Wi-Fi again changes nothing. When the stored
//   calibration doesn't load (esp_phy_load_cal_data_from_nvs, 773: none stored, or one made by
//   another radio library or for another MAC address, 829-877) it makes a full one, and it makes
//   a new one when the radio finds the stored one doesn't check out (ESP_CAL_DATA_CHECK_FAIL);
//   either way it stores what it made (983-998). So "a new calibration was made" is seen from
//   outside as what is stored having changed, or having appeared. A load that succeeds is one
//   the next start will use.
// - esp_system/port/soc/esp32s3/system_internal.c: esp_restart() keeps the PLL that the USB
//   port runs on (line 136), so the computer doesn't see the port go and come back.
// - esp_driver_usb_serial_jtag/src/usb_serial_jtag.c: usb_serial_jtag_wait_tx_done() (291)
//   holds new writes back and waits until the computer has taken everything already queued.

#include "radio.h"

#include <stdlib.h>
#include <string.h>

#include "driver/usb_serial_jtag.h"
#include "esp_attr.h"
#include "esp_err.h"
#include "esp_heap_caps.h"
#include "esp_log.h"
#include "esp_phy_init.h"
#include "esp_system.h"
#include "freertos/FreeRTOS.h"
#include "freertos/task.h"
#include "nvs.h"
#include "storage.h"

static const char *TAG = "radio";

// How long a restart waits for the computer to take what is queued for the USB port.
#define USB_DRAIN_MS 1000
// How long, after a start with no usable calibration stored, the new one is waited for.
#define STORED_WAIT_MS 1000
#define STORED_POLL_MS 100

#define KEPT_MAGIC 0x52414431u // "RAD1"
#define KEPT_SEED  0x9e3779b9u

// In RTC memory, which esp_restart() leaves alone and a power cut leaves as noise. `check` ties
// the rest together, so noise is never taken for a count.
typedef struct {
    uint32_t magic;
    uint32_t stuck_restarts;
    uint32_t calibrated; // 1 once a restart has followed a new calibration
    uint32_t check;
} kept_t;

static RTC_NOINIT_ATTR kept_t s_kept;
static uint32_t s_start_generation;

// Wi-Fi has started once in this start, so the radio has made whatever calibration it makes.
static bool s_wifi_started;
// What the first radio_calibration_before() found: whether it has been asked, whether it is
// watching, whether a calibration loaded, and if so a copy of it.
static bool s_before_taken;
static bool s_watching;
static bool s_before_loaded;
static esp_phy_calibration_data_t *s_before;
static esp_phy_calibration_data_t *s_after;

static uint32_t check_of(const kept_t *k)
{
    return k->magic ^ k->stuck_restarts ^ (k->calibrated * 0x01000193u) ^ KEPT_SEED;
}

static void keep(void)
{
    s_kept.magic = KEPT_MAGIC;
    s_kept.check = check_of(&s_kept);
}

void radio_start(void)
{
    s_start_generation = storage_setup_generation();
    const esp_reset_reason_t reason = esp_reset_reason();
    const bool believed = reason == ESP_RST_SW && s_kept.magic == KEPT_MAGIC &&
                          s_kept.check == check_of(&s_kept) && s_kept.calibrated <= 1u;
    if (!believed) {
        s_kept = (kept_t){0};
        keep();
        return;
    }
    if (s_kept.stuck_restarts > 0 || s_kept.calibrated) {
        ESP_LOGI(TAG, "restarted by this firmware: %u restarts for a stuck radio since anything "
                      "last came back from the hub%s",
                 (unsigned)s_kept.stuck_restarts,
                 s_kept.calibrated ? "; a new calibration has been made" : "");
    }
}

uint32_t radio_start_generation(void)
{
    return s_start_generation;
}

uint32_t radio_stuck_restarts(void)
{
    return s_kept.stuck_restarts;
}

void radio_heard(void)
{
    if (s_kept.stuck_restarts != 0) {
        s_kept.stuck_restarts = 0;
        keep();
    }
}

void radio_setup_changed(void)
{
    radio_heard();
}

// PSRAM if there is any: the internal memory is wanted for Wi-Fi and TLS.
static esp_phy_calibration_data_t *calibration_alloc(void)
{
    void *p = heap_caps_calloc(1, sizeof(esp_phy_calibration_data_t),
                               MALLOC_CAP_SPIRAM | MALLOC_CAP_8BIT);
    return p != NULL ? p : calloc(1, sizeof(esp_phy_calibration_data_t));
}

static void watch_end(void)
{
    s_watching = false;
    s_before_loaded = false;
    free(s_before);
    free(s_after);
    s_before = NULL;
    s_after = NULL;
}

// NVS isn't there to be read at all: nothing can be told, and nothing would be stored.
static bool nvs_unusable(esp_err_t err)
{
    return err == ESP_ERR_NVS_NOT_INITIALIZED || err == ESP_ERR_NVS_PART_NOT_FOUND ||
           err == ESP_ERR_NVS_INVALID_HANDLE || err == ESP_ERR_NVS_KEYS_NOT_INITIALIZED ||
           err == ESP_ERR_NVS_CORRUPT_KEY_PART || err == ESP_ERR_NO_MEM;
}

void radio_calibration_before(void)
{
    if (s_before_taken || s_wifi_started || s_kept.calibrated) {
        return;
    }
    s_before_taken = true;
    s_before = calibration_alloc();
    s_after = calibration_alloc();
    if (s_before == NULL || s_after == NULL) {
        watch_end();
        return;
    }
    const esp_err_t err = esp_phy_load_cal_data_from_nvs(s_before);
    if (nvs_unusable(err)) {
        watch_end();
        return;
    }
    s_watching = true;
    s_before_loaded = err == ESP_OK;
}

bool radio_wifi_started(void)
{
    s_wifi_started = true;
    if (!s_watching) {
        watch_end();
        return false;
    }
    bool made = false;
    if (s_before_loaded) {
        // The start loaded what was stored. It may have replaced it all the same, if the radio
        // found it didn't check out: then what is stored now differs.
        made = esp_phy_load_cal_data_from_nvs(s_after) == ESP_OK &&
               memcmp(s_before, s_after, sizeof(*s_after)) != 0;
    } else {
        // None that loaded: the start made a full one, and is storing it.
        for (int waited = 0; waited <= STORED_WAIT_MS; waited += STORED_POLL_MS) {
            if (esp_phy_load_cal_data_from_nvs(s_after) == ESP_OK) {
                made = true;
                break;
            }
            vTaskDelay(pdMS_TO_TICKS(STORED_POLL_MS));
        }
        if (!made) {
            ESP_LOGW(TAG, "the radio's new calibration couldn't be read back from storage");
        }
    }
    watch_end();
    return made;
}

void radio_restart(radio_restart_t why, uint32_t generation, void (*wipe)(void *ctx), void *ctx)
{
    // Whoever holds the lock finishes first, and nothing else starts: no storage write is cut
    // short, and no new setup can be stored behind this check.
    storage_lock();
    if (storage_setup_generation() != generation) {
        storage_unlock();
        ESP_LOGW(TAG, "a new setup was stored just now: not restarting");
        return;
    }
    if (why == RADIO_RESTART_CALIBRATED) {
        s_kept.calibrated = 1;
    } else if (s_kept.stuck_restarts < UINT32_MAX) {
        s_kept.stuck_restarts++;
    }
    keep();
    if (wipe != NULL) {
        wipe(ctx);
    }
    if (usb_serial_jtag_is_driver_installed()) {
        (void)usb_serial_jtag_wait_tx_done(pdMS_TO_TICKS(USB_DRAIN_MS));
    }
    esp_restart();
}
