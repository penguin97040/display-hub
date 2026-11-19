// What the board test reports. See report.h.

#include "report.h"

#include <inttypes.h>
#include <string.h>

#include "esp_app_desc.h"
#include "esp_heap_caps.h"
#include "esp_log.h"
#include "esp_ota_ops.h"
#include "esp_partition.h"

static const char *TAG = "board-test";

static const char *yes_no(bool value)
{
    return value ? "yes" : "no";
}

void system_report_read(system_report_t *report)
{
    *report = (system_report_t){0};
    report->version = esp_app_get_description()->version;
    report->boot_record = "unknown";

    const esp_partition_t *running = esp_ota_get_running_partition();
    if (running == NULL) {
        return;
    }
    report->running_known = true;
    strlcpy(report->running_label, running->label, sizeof(report->running_label));
    report->running_address = running->address;
    report->running_size = running->size;

    // After a first install the OTA data is blank, so it has no entry for the running slot.
    esp_ota_img_states_t state;
    const esp_err_t err = esp_ota_get_state_partition(running, &state);
    if (err == ESP_ERR_NOT_FOUND) {
        report->boot_record = "blank (first install)";
    } else if (err != ESP_OK) {
        report->boot_record = "unreadable";
    } else {
        switch (state) {
        case ESP_OTA_IMG_NEW:
            report->boot_record = "new";
            break;
        case ESP_OTA_IMG_PENDING_VERIFY:
            report->boot_record = "on trial";
            break;
        case ESP_OTA_IMG_VALID:
            report->boot_record = "marked good";
            break;
        case ESP_OTA_IMG_INVALID:
            report->boot_record = "marked bad";
            break;
        case ESP_OTA_IMG_ABORTED:
            report->boot_record = "aborted";
            break;
        default:
            report->boot_record = "not set";
            break;
        }
    }
}

void memory_report_read(memory_report_t *report)
{
    report->psram_free = heap_caps_get_free_size(MALLOC_CAP_SPIRAM);
    report->psram_total = heap_caps_get_total_size(MALLOC_CAP_SPIRAM);
    report->internal_free = heap_caps_get_free_size(MALLOC_CAP_INTERNAL);
    report->internal_smallest = heap_caps_get_minimum_free_size(MALLOC_CAP_INTERNAL);
}

void report_log(const board_t *board, const system_report_t *system, const efuse_report_t *efuse,
                const char *last_swipe)
{
    ESP_LOGI(TAG, "--- Display Hub check build, firmware %s ---", system->version);
    if (system->running_known) {
        ESP_LOGI(TAG, "running from %s at 0x%08" PRIx32 " (%" PRIu32 " bytes); boot record: %s",
                 system->running_label, system->running_address, system->running_size,
                 system->boot_record);
    } else {
        ESP_LOGW(TAG, "couldn't tell which partition is running");
    }

    ESP_LOGI(TAG, "board: %s; I/O expander: %s; screen: %s; touch: %s",
             board_revision_name(board->revision), board->expander_found ? "found" : "not found",
             board->display != NULL ? "running" : "failed", board->touch_ready ? "running" : "off");
    if (board->revision == BOARD_REVISION_V1) {
        ESP_LOGW(TAG, "V1 board: not supported yet");
    }

    board_power_t power;
    board_read_power(&power);
    if (power.found) {
        ESP_LOGI(TAG,
                 "power chip AXP2101: USB power good %s; battery %s; charging %s; "
                 "DC-DC on 0x%02x; LDO on 0x%02x",
                 yes_no(power.usb_power_good), power.battery_present ? "present" : "none",
                 yes_no(power.charging), power.dcdc_on, power.ldo_on);
    } else {
        ESP_LOGW(TAG, "power chip AXP2101: not found (chip ID read 0x%02x)", power.chip_id);
    }

    memory_report_t mem;
    memory_report_read(&mem);
    ESP_LOGI(TAG,
             "memory: PSRAM free %" PRIu32 " of %" PRIu32 " KB; internal free %" PRIu32
             " KB (lowest %" PRIu32 " KB)",
             mem.psram_free / 1024, mem.psram_total / 1024, mem.internal_free / 1024,
             mem.internal_smallest / 1024);

    efuse_report_log(efuse, "as last read");
    ESP_LOGI(TAG, "touch: last swipe %s", last_swipe);
}
