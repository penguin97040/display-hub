// Installing a signed update, and the trial boot. See update.h.
//
// What was read in ESP-IDF v5.5.6 to write this (paths are under components/):
// - app_update/esp_ota_ops.c: esp_ota_begin() with OTA_SIZE_UNKNOWN erases the whole 6 MiB
//   partition in one call, and with a size erases that much in one call (line 197). With
//   OTA_WITH_SEQUENTIAL_WRITES it erases nothing, and esp_ota_write() then erases each 4 KiB
//   sector just before the first byte is written to it (line 330). That is what is used here: no
//   one call stalls for long, a download that fails early has worn only the sectors it reached,
//   and the writes are in order anyway. esp_ota_begin() refuses on a trial boot
//   (ESP_ERR_OTA_ROLLBACK_INVALID_STATE), which never gets that far here. esp_ota_end() checks
//   the whole image, its own SHA-256 included, and esp_ota_set_boot_partition() checks it again
//   before it writes the OTA data.
// - spi_flash/esp_flash_api.c, the write behind esp_flash_write() (line 1144): only a buffer in
//   internal memory is written as it is. One in PSRAM is copied through a 32-byte buffer, 32
//   bytes at a time, because PSRAM can't be read while the flash is busy. So the piece buffer
//   here is in internal memory (a static array; .bss isn't put in PSRAM), and esp_ota_write() is
//   only ever given parts of it.
// - bootloader_support/src/bootloader_utility.c: with no factory partition and blank OTA data
//   (a first install), the bootloader starts ota_0 and writes an OTA data entry for it, marked
//   valid (set_actual_ota_seq, line 488). So after a first install there is always a valid image
//   to go back to, and esp_ota_mark_app_invalid_rollback_and_reboot() finds it. A reset of any
//   kind on a trial boot makes the bootloader mark the image aborted and start the other.
//
// WHAT A FLASH WRITE DOES TO EVERYTHING ELSE. This firmware runs from flash (not from PSRAM:
// CONFIG_SPIRAM_XIP_FROM_PSRAM is off), so while a sector is erased or written the cache is off
// and both cores stop running anything that isn't in internal memory. Each erase of a sector
// takes about 45 ms (the chip's data sheet allows up to 400), so during a download the screen's
// task, touch and the 30-second refresh run in the gaps: the picture may stutter, and nothing is
// lost. The screen's own data isn't affected: its draw buffers are in internal memory
// (board.c), the panel keeps its picture in its own memory, a transfer under way is finished by
// the SPI hardware, and its interrupt simply waits (it isn't registered as one that runs with
// the cache off, so no code in flash is entered at the wrong moment). Wi-Fi keeps receiving in
// its own internal memory. This is ESP-IDF's ordinary way of updating, and nothing was changed
// to make room for it.

#include "update.h"

#include <inttypes.h>
#include <stdlib.h>
#include <string.h>

#include "esp_heap_caps.h"
#include "esp_log.h"
#include "esp_memory_utils.h"
#include "esp_ota_ops.h"
#include "esp_partition.h"
#include "esp_system.h"
#include "esp_timer.h"
#include "freertos/FreeRTOS.h"
#include "freertos/semphr.h"
#include "freertos/task.h"
#include "storage_fw.h"

static const char *TAG = "update";

// One piece of a download: a flash sector's worth, read from the connection and handed to the
// update core, which hands parts of it back to be written. In internal memory: see the top.
#define PIECE_BYTES 4096
static uint8_t s_piece[PIECE_BYTES];

// How often a download's progress, and the internal memory left, are logged.
#define PROGRESS_EVERY_BYTES (256u * 1024u)

// The trial task checks the clock this often. It needs room for esp_ota_mark_app_invalid_
// rollback_and_reboot(), which checks the image it is going back to.
#define TRIAL_TICK_MS      1000
#define TRIAL_STACK_BYTES  6144
#define TRIAL_PRIORITY     5

// Held around "is the trial still open, then mark good" and "is it still open, then go back",
// so a check-in that lands as the five minutes run out can't do both. Never held across the
// network or a wait.
static SemaphoreHandle_t s_trial_lock;
static bool s_trial_open;

static uint64_t uptime_seconds(void)
{
    return (uint64_t)(esp_timer_get_time() / 1000000);
}

// ---- The trial boot ----

// The five minutes are up. Returns only if there is nothing to go back to.
static void roll_back(void)
{
    ESP_LOGE(TAG, "this firmware didn't reach the hub within %u minutes of starting: going back "
                  "to the version before",
             (unsigned)(DH_FW_TRIAL_SECONDS / 60u));
    // Nothing is saved: the previous firmware settles the record when it starts.
    const esp_err_t err = esp_ota_mark_app_invalid_rollback_and_reboot();
    if (err == ESP_ERR_OTA_ROLLBACK_FAILED) {
        // No other image passes its check. Restarting would only start this one again, so it
        // stays as it is: on trial, sending no heartbeat and no report.
        ESP_LOGE(TAG, "there is no other firmware to go back to: carrying on, still on trial");
        return;
    }
    // The OTA data couldn't be written. A restart does the same job: the bootloader takes an
    // image that was on trial and restarted as one that failed.
    ESP_LOGE(TAG, "couldn't mark this firmware as bad (%s): restarting, so the bootloader goes "
                  "back",
             esp_err_to_name(err));
    esp_restart();
}

static void trial_task(void *arg)
{
    (void)arg;
    while (true) {
        vTaskDelay(pdMS_TO_TICKS(TRIAL_TICK_MS));
#ifdef DH_TEST_TRIAL_CRASH
        // TEST ONLY (firmware/README.md, "Test builds"): never in an official build.
        if (uptime_seconds() >= 20) {
            ESP_LOGE(TAG, "dh-test-trial: TEST BUILD, crashing on purpose during the trial boot");
            abort();
        }
#endif
        xSemaphoreTake(s_trial_lock, portMAX_DELAY);
        if (!s_trial_open) {
            xSemaphoreGive(s_trial_lock);
            break;
        }
        // The update core's rule, asked of a session of this task's own: the answer depends
        // only on the running time, and this way it never waits for storage's lock.
        const dh_fw_session on_trial = {.trial = true};
        if (dh_fw_trial_tick(&on_trial, uptime_seconds()) == DH_FW_TRIAL_ROLL_BACK) {
            roll_back();
            // Nothing to go back to: stop asking.
            xSemaphoreGive(s_trial_lock);
            break;
        }
        xSemaphoreGive(s_trial_lock);
    }
    vTaskDelete(NULL);
}

void update_trial_start(void)
{
    if (!storage_image_on_trial()) {
        return;
    }
#if defined(DH_TEST_TRIAL_NO_CHECK_IN) || defined(DH_TEST_TRIAL_CRASH)
    ESP_LOGE(TAG, "dh-test-trial: TEST BUILD. This firmware fails its trial boot on purpose");
#endif
    s_trial_lock = xSemaphoreCreateMutex();
    s_trial_open = true;
    if (s_trial_lock == NULL ||
        xTaskCreate(trial_task, "trial", TRIAL_STACK_BYTES, NULL, TRIAL_PRIORITY, NULL) !=
            pdPASS) {
        // Without the task nothing would ever send this firmware back. A restart does: the
        // bootloader goes back from an image that was on trial.
        ESP_LOGE(TAG, "out of memory for the trial boot's clock: restarting, so the bootloader "
                      "goes back to the version before");
        esp_restart();
    }
}

bool update_trial_check_in(dh_fw_check_in answer)
{
    if (s_trial_lock == NULL) {
        return false;
    }
#ifdef DH_TEST_TRIAL_NO_CHECK_IN
    // TEST ONLY (firmware/README.md, "Test builds"): never in an official build.
    ESP_LOGE(TAG, "dh-test-trial: TEST BUILD, ignoring the hub's answer during the trial boot");
    answer = DH_FW_CHECK_IN_FAILED;
#endif
    bool marked = false;
    xSemaphoreTake(s_trial_lock, portMAX_DELAY);
    if (s_trial_open && storage_fw_trial_check_in(answer) == DH_FW_TRIAL_MARK_GOOD) {
        const esp_err_t err = esp_ota_mark_app_valid_cancel_rollback();
        if (err != ESP_OK) {
            // dh_update.h: don't apply anything. Restart, and the bootloader goes back.
            ESP_LOGE(TAG, "couldn't mark this firmware good (%s): restarting, so the bootloader "
                          "goes back to the version before",
                     esp_err_to_name(err));
            esp_restart();
        }
        s_trial_open = false;
        marked = true;
    }
    xSemaphoreGive(s_trial_lock);
    if (marked) {
        ESP_LOGI(TAG, "reached the hub after %" PRIu64 " s: this firmware is marked good",
                 uptime_seconds());
        // Only now: the floor and the keys, then the record. The report comes after both.
        storage_fw_marked_good();
    }
    return marked;
}

// ---- A download ----

static void log_memory(const char *when, uint64_t read, uint64_t length)
{
    ESP_LOGI(TAG,
             "%s: %" PRIu64 " of %" PRIu64 " bytes; internal memory free %u KB (lowest %u KB)",
             when, read, length,
             (unsigned)(heap_caps_get_free_size(MALLOC_CAP_INTERNAL) / 1024),
             (unsigned)(heap_caps_get_minimum_free_size(MALLOC_CAP_INTERNAL) / 1024));
}

dh_fw_reason update_receive(hub_download_t *download, const dh_fw_device *device,
                            uint64_t offer_size, bool has_length, uint64_t length,
                            int64_t started_ms, bool (*stop)(void))
{
    // About 12 KB, which only this task reads: PSRAM is fine for it.
    dh_fw_download *d = heap_caps_calloc(1, sizeof(*d), MALLOC_CAP_SPIRAM | MALLOC_CAP_8BIT);
    if (d == NULL) {
        d = heap_caps_calloc(1, sizeof(*d), MALLOC_CAP_DEFAULT);
    }
    if (d == NULL) {
        ESP_LOGE(TAG, "out of memory for a download");
        return DH_FW_REASON_DOWNLOAD;
    }

    // Step 2 with the answer in hand: the Content-Length must be the offer's size, exactly.
    dh_fw_reason reason =
        storage_fw_download_begin(d, device, offer_size, has_length, length);
    if (reason != DH_FW_REASON_NONE) {
        ESP_LOGW(TAG, "the download's length isn't the offer's (%s): none of it was read",
                 dh_fw_reason_name(reason));
        heap_caps_free(d);
        return reason;
    }

    const esp_partition_t *spare = esp_ota_get_next_update_partition(NULL);
    esp_ota_handle_t ota = 0;
    bool ota_open = false;
    bool finished = false;
    const uint64_t started_s = (uint64_t)(started_ms / 1000);
    uint64_t last_byte_s = uptime_seconds();
    uint64_t logged_at = 0;

    while (!finished && reason == DH_FW_REASON_NONE) {
        if (stop != NULL && stop()) {
            ESP_LOGW(TAG, "the setup has gone: giving the download up");
            reason = dh_fw_download_fail(d, DH_FW_REASON_DOWNLOAD);
            break;
        }
        if (dh_fw_download_overdue(started_s, last_byte_s, uptime_seconds())) {
            ESP_LOGW(TAG, "the download sent nothing for %u s, or has taken %u minutes: giving "
                          "it up",
                     (unsigned)DH_FW_DOWNLOAD_STALL_SECONDS,
                     (unsigned)(DH_FW_DOWNLOAD_TIMEOUT_SECONDS / 60u));
            reason = dh_fw_download_fail(d, DH_FW_REASON_DOWNLOAD);
            break;
        }
        // Never ask the connection for a byte past the update form.
        const uint64_t left = length - dh_fw_download_bytes_read(d);
        if (left == 0) {
            // Everything the answer said it had was read, and the update form isn't complete.
            reason = dh_fw_download_fail(d, DH_FW_REASON_DOWNLOAD);
            break;
        }
        const size_t want = left < sizeof(s_piece) ? (size_t)left : sizeof(s_piece);
        const int got = hub_http_download_read(download, s_piece, want);
        if (got < 0) {
            ESP_LOGW(TAG, "the download broke off");
            reason = dh_fw_download_fail(d, DH_FW_REASON_DOWNLOAD);
            break;
        }
        if (got == 0) {
            continue;
        }
        last_byte_s = uptime_seconds();

        const uint8_t *piece = s_piece;
        size_t n = (size_t)got;
        while (n > 0 && !finished && reason == DH_FW_REASON_NONE) {
            const dh_fw_push push = dh_fw_download_push(d, piece, n);
            piece += push.used;
            n -= push.used;
            // The update core only ever names bytes to write after it has said to open the
            // partition, which it says only once the manifest has passed every check.
            if (push.write_len > 0) {
                const esp_err_t err = ota_open && esp_ptr_internal(push.write)
                                          ? esp_ota_write(ota, push.write, push.write_len)
                                          : ESP_ERR_INVALID_STATE;
                if (err != ESP_OK) {
                    ESP_LOGE(TAG, "couldn't write to the spare partition: %s",
                             esp_err_to_name(err));
                    reason = dh_fw_download_fail(d, DH_FW_REASON_FLASH);
                    break;
                }
            }
            switch (push.step) {
            case DH_FW_STEP_OPEN_SLOT: {
                const dh_fw_manifest *manifest = dh_fw_download_manifest(d);
                ESP_LOGI(TAG,
                         "the package is genuine and for this display: firmware %s, security "
                         "counter %" PRIu32 ", %" PRIu32 " bytes. Writing it to %s",
                         manifest != NULL ? manifest->firmware_version : "?",
                         manifest != NULL ? manifest->security_counter : 0,
                         manifest != NULL ? manifest->image_size : 0,
                         spare != NULL ? spare->label : "no partition");
                const esp_err_t err =
                    spare != NULL ? esp_ota_begin(spare, OTA_WITH_SEQUENTIAL_WRITES, &ota)
                                  : ESP_ERR_NOT_FOUND;
                if (err != ESP_OK) {
                    ESP_LOGE(TAG, "couldn't open the spare partition: %s", esp_err_to_name(err));
                    reason = dh_fw_download_fail(d, DH_FW_REASON_FLASH);
                } else {
                    ota_open = true;
                }
                break;
            }
            case DH_FW_STEP_FINISH:
                finished = true;
                break;
            case DH_FW_STEP_ENDED:
                reason = push.reason;
                break;
            default:
                break;
            }
        }
        const uint64_t read = dh_fw_download_bytes_read(d);
        if (read - logged_at >= PROGRESS_EVERY_BYTES) {
            logged_at = read;
            log_memory("downloading", read, length);
        }
    }

    if (finished && reason == DH_FW_REASON_NONE) {
        log_memory("downloaded", dh_fw_download_bytes_read(d), length);
        // Step 9. esp_ota_end() checks that what was written is a whole, valid app image.
        esp_err_t err = esp_ota_end(ota);
        ota_open = false;
        if (err != ESP_OK) {
            ESP_LOGE(TAG, "the image written isn't a valid app: %s", esp_err_to_name(err));
            reason = dh_fw_download_fail(d, DH_FW_REASON_FLASH);
        } else if (spare == NULL || spare->subtype < ESP_PARTITION_SUBTYPE_APP_OTA_MIN ||
                   spare->subtype >= ESP_PARTITION_SUBTYPE_APP_OTA_MAX ||
                   !storage_fw_attempt_installed(
                       dh_fw_download_manifest(d),
                       (uint8_t)(spare->subtype - ESP_PARTITION_SUBTYPE_APP_OTA_MIN))) {
            // The record isn't in flash, so the partition isn't set to boot: a trial boot must
            // never start without a record of its security counter and keys.
            ESP_LOGE(TAG, "couldn't save the update record: not starting the new firmware");
            reason = dh_fw_download_fail(d, DH_FW_REASON_FLASH);
        } else {
            // The record is saved. Only now is the new image set to boot.
            err = esp_ota_set_boot_partition(spare);
            if (err != ESP_OK) {
                // The record says a release is waiting in a partition that won't be started.
                // The restart settles that as the update core's rules say: the same firmware
                // starts again, drops the release's keys and reports that it went back.
                ESP_LOGE(TAG, "couldn't set the new firmware to start (%s): it will be reported "
                              "as not having started",
                         esp_err_to_name(err));
            } else {
                ESP_LOGI(TAG, "the new firmware is written, checked and set to start on trial");
            }
        }
    }
    if (ota_open) {
        // Nothing of an abandoned image is ever started: the partition isn't set to boot.
        esp_ota_abort(ota);
    }
    if (reason != DH_FW_REASON_NONE) {
        ESP_LOGW(TAG, "the update ended: %s (%s), after %" PRIu64 " bytes. Nothing was installed",
                 dh_fw_reason_name(reason),
                 dh_fw_report_state_name(dh_fw_reason_state(reason)),
                 dh_fw_download_bytes_read(d));
    }
    heap_caps_free(d);
    return reason;
}
