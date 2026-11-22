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
//   The bootloader also falls back by itself: if the image the OTA data names doesn't pass its
//   check, it tries the other slots and starts any image that does
//   (bootloader_utility_load_boot_image, line 590 on), signed by this project or not, since
//   Secure Boot is off. And esp_ota_abort() only frees the handle (esp_ota_ops.c, line 456):
//   what was written stays. So an image that was
//   given up is spoilt here, by erasing its first sector (spoil_spare), and can never be the
//   one the bootloader falls back to. A partition that was opened and never written to is
//   left alone: it still holds the version before.
//
// HOW LONG ONE READ CAN TAKE. Two things in ESP-IDF stand between a download and its time limits,
// and neither can be interrupted from outside once it has been called:
// - Mbed TLS reads the socket until it has a whole TLS record, and on a blocking socket each
//   of those reads may wait 10 seconds for one byte. hub_http.c's connection is on a socket
//   that never blocks, so a record that isn't all there yet reads as "nothing yet" and the
//   call comes straight back (what was read in ESP-IDF for that is at the top of hub_http.c);
// - esp_http_client_read() (esp_http_client.c, line 1346) reads the connection again until it
//   has every byte it was asked for, and stops early only when one of those reads waits its
//   whole timeout for nothing. So a hub that sends tiny whole records, each just inside the
//   timeout, can hold one call open for (bytes asked for) x (timeout).
// So each call asks for at most READ_BYTES (256) with a timeout of 50 ms (hub_http.c,
// DOWNLOAD_STEP_TIMEOUT_MS): one call can take 12.8 seconds at the very worst. The two time
// limits and a factory reset are looked at between calls, so that is how far past the 30
// seconds, the 15 minutes or a factory reset a download can run, and only against a hub that
// trickles bytes on purpose; a hub that simply goes quiet is seen within 50 ms. An honest
// download isn't slowed: the hub's TLS records are up to 4 KiB, a read never waits while part
// of a decrypted record is still in hand (tcp_transport/transport_ssl.c, base_poll_read, line
// 173), and sixteen short calls a record cost microseconds beside the 45 ms a flash sector
// takes.
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

// How many bytes one read of the connection asks for: see "How long one read can take" at the
// top. Reads go on filling s_piece until it is full, so the flash is still written a sector's
// worth at a time.
#define READ_BYTES 256

// What is erased of an image that was given up: one flash sector, which holds its header.
#define SPOIL_BYTES 4096

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
        // carries on. It is the only firmware that works, so from now on the first answer from
        // the hub marks it good, however late (update_trial_check_in).
        ESP_LOGE(TAG, "there is no other firmware to go back to: carrying on with this one, "
                      "which is marked good as soon as it reaches the hub");
        storage_fw_trial_no_way_back();
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
            // Nothing to go back to: the clock has nothing more to do.
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
#if defined(DH_TEST_TRIAL_NO_CHECK_IN) || defined(DH_TEST_TRIAL_CRASH)
    // TEST ONLY (firmware/README.md, "Test builds"): never in an official build. The crash test
    // ignores the hub too, or a check-in would end the trial before the crash comes.
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

// esp_ota_set_boot_partition() failed after the record was saved. What happens next is decided
// by what the OTA data really says now, not by the error:
//  - it still names the running partition: the new image will not be started. The record goes
//    back to an attempt, which the caller ends as failed with `flash`, and there is no restart.
//    (If the record can't be put back, because it is no longer the one this attempt saved, the
//    display restarts instead and the start settles it from flash);
//  - it names the spare partition after all (the write landed and was then reported as failed):
//    this is an ordinary install, so restart into the trial boot;
//  - it can't be read, or names something else: the record is left as it is (a release waiting
//    for its trial boot) and the display restarts. Whichever image the bootloader starts then
//    settles it by the update core's rules: the new one on trial has its record, and the old one
//    reports that the update didn't start.
// Returns what the caller does next. UPDATE_NO_RESTART: the attempt has ended as failed.
static update_restart_t boot_not_set(const esp_partition_t *spare, esp_err_t err)
{
    const esp_partition_t *boots = esp_ota_get_boot_partition();
    const esp_partition_t *running = esp_ota_get_running_partition();
    const bool boots_spare = boots != NULL && boots->address == spare->address;
    const bool boots_running = boots != NULL && running != NULL && !boots_spare &&
                               boots->address == running->address;
    if (boots_running && storage_fw_attempt_not_set_to_boot()) {
        ESP_LOGE(TAG, "couldn't set the new firmware to start (%s). The display still starts "
                      "the firmware it is running, so the update ends here as failed",
                 esp_err_to_name(err));
        return UPDATE_NO_RESTART;
    }
    if (boots_running) {
        // The new image won't be started, but the record in memory isn't the one this attempt
        // saved a moment ago (something else changed it), so it can't be put back and ended
        // here: ending it would save nothing, and the record would go on saying a release is
        // waiting, with no new offer acted on, for the rest of this run. Flash holds the saved
        // record, and a start settles it from there by the usual rule: this same firmware
        // starts again, drops the release's keys and reports that the update didn't start.
        ESP_LOGE(TAG, "couldn't set the new firmware to start (%s). The display still starts "
                      "the firmware it is running, but the update record in memory isn't this "
                      "attempt's any more: restarting, so it is settled from what is in flash",
                 esp_err_to_name(err));
        return UPDATE_RESTART_TO_SETTLE;
    }
    if (boots_spare) {
        ESP_LOGW(TAG, "setting the new firmware to start reported a failure (%s), but it is set: "
                      "restarting into it on trial",
                 esp_err_to_name(err));
        return UPDATE_RESTART_INTO_NEW;
    }
    ESP_LOGE(TAG, "couldn't set the new firmware to start (%s), and can't tell which firmware "
                  "starts next: restarting, so that whichever starts settles it",
             esp_err_to_name(err));
    return UPDATE_RESTART_TO_SETTLE;
}

// An image that was given up must never be started. It isn't set to boot, but the bootloader
// would fall back to it if the running image were ever damaged, and a download that failed its
// hash can be a complete, valid app that nobody signed. So the first sector of the spare
// partition, which holds the image's header, is erased: the bootloader then finds no image
// there at all. Only ever the spare partition, never the one that is running or the one set to
// boot next; and only once a write to it has been tried for this download (update_receive,
// spare_written). An offer that was refused, or a download that broke off, before the first
// write leaves the version before where it is: opening the partition erases nothing.
static void spoil_spare(const esp_partition_t *spare)
{
    const esp_partition_t *running = esp_ota_get_running_partition();
    const esp_partition_t *boots = esp_ota_get_boot_partition();
    if (spare == NULL || running == NULL || spare->address == running->address ||
        (boots != NULL && boots->address == spare->address)) {
        ESP_LOGE(TAG, "not erasing the start of the abandoned image: its partition can't be "
                      "told from the one in use");
        return;
    }
    uint8_t start[16];
    esp_err_t err = esp_partition_erase_range(spare, 0, SPOIL_BYTES);
    if (err == ESP_OK) {
        err = esp_partition_read(spare, 0, start, sizeof(start));
    }
    bool blank = err == ESP_OK;
    for (size_t i = 0; blank && i < sizeof(start); i++) {
        blank = start[i] == 0xff;
    }
    if (blank) {
        ESP_LOGI(TAG, "the abandoned image in %s can't be started: its first sector is erased",
                 spare->label);
    } else {
        // Nothing more can be done here. It still isn't set to boot, and the next download
        // that opens the partition erases the same sector before it writes.
        ESP_LOGE(TAG, "couldn't erase the start of the abandoned image in %s: %s", spare->label,
                 err == ESP_OK ? "it didn't read back as blank" : esp_err_to_name(err));
    }
}

dh_fw_reason update_receive(hub_download_t *download, const dh_fw_device *device,
                            uint64_t offer_size, bool has_length, uint64_t length,
                            int64_t started_ms, bool (*stop)(void), update_restart_t *restart)
{
    *restart = UPDATE_NO_RESTART;
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
    // A write to the spare partition was tried. Opening it changes nothing in flash
    // (OTA_WITH_SEQUENTIAL_WRITES: see the top), so until then the partition still holds
    // whatever it held, which may be the last good firmware. A write that fails counts,
    // whatever error it gives: it may have erased the first sector, or written part of it,
    // before it failed. That goes for a first byte that isn't an image's, too. With writes in
    // order, esp_ota_write() erases before it looks at that byte (app_update/esp_ota_ops.c:
    // "ret = esp_partition_erase_range(it->partition.staging, it->wrote_size, ...)" at 336,
    // then "if (data_bytes[0] != ESP_IMAGE_HEADER_MAGIC)" at 347), so by the time it answers
    // ESP_ERR_OTA_VALIDATE_FAILED the first sector of what was there has gone. The only
    // answers given before any flash call are ESP_ERR_INVALID_ARG for no data (316-318) and
    // for a handle it doesn't know (403-405); the erase and the write give the same error for
    // a range they refuse, the write after the erase, so that one can't be told apart and
    // counts as well.
    bool spare_written = false;
    bool finished = false;
    const uint64_t started_s = (uint64_t)(started_ms / 1000);
    uint64_t last_byte_s = uptime_seconds();
    uint64_t logged_at = 0;
    // How much of s_piece is filled and not yet handed to the update core.
    size_t filled = 0;

    while (!finished && reason == DH_FW_REASON_NONE) {
        // The setup, and both time limits, are looked at before every read (see "How long one
        // read can take" at the top).
        if (stop != NULL && stop()) {
            ESP_LOGW(TAG, "the setup has gone: giving the download up");
            reason = dh_fw_download_fail(d, DH_FW_REASON_DOWNLOAD);
            break;
        }
        const bool overdue = dh_fw_download_overdue(started_s, last_byte_s, uptime_seconds());
        // Never ask the connection for a byte past the update form.
        const uint64_t left = length - dh_fw_download_bytes_read(d) - filled;
        size_t got = 0;
        bool ended = false;
        if (!overdue && left > 0 && filled < sizeof(s_piece)) {
            size_t want = sizeof(s_piece) - filled;
            if (want > READ_BYTES) {
                want = READ_BYTES;
            }
            if ((uint64_t)want > left) {
                want = (size_t)left;
            }
            const int read = hub_http_download_read(download, s_piece + filled, want);
            if (read < 0) {
                ended = true;
            } else if (read > 0) {
                got = (size_t)read;
                filled += got;
                last_byte_s = uptime_seconds();
            }
        }
        // A piece goes to the update core when it is full or holds the last of the form, and
        // also when nothing more is coming just now, so what has arrived is checked (and
        // written) before any wait.
        const bool whole = filled == sizeof(s_piece) || (uint64_t)got == left;
        if (filled > 0 && (whole || got == 0)) {
            const uint8_t *piece = s_piece;
            size_t n = filled;
            filled = 0;
            while (n > 0 && !finished && reason == DH_FW_REASON_NONE) {
                const dh_fw_push push = dh_fw_download_push(d, piece, n);
                piece += push.used;
                n -= push.used;
                // The update core only ever names bytes to write after it has said to open the
                // partition, which it says only once the manifest has passed every check.
                if (push.write_len > 0) {
                    const bool can_write = ota_open && esp_ptr_internal(push.write);
                    spare_written = spare_written || can_write;
                    const esp_err_t err = can_write
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
                             "the package is genuine and for this display: firmware %s, "
                             "security counter %" PRIu32 ", %" PRIu32 " bytes. Writing it to %s",
                             manifest != NULL ? manifest->firmware_version : "?",
                             manifest != NULL ? manifest->security_counter : 0,
                             manifest != NULL ? manifest->image_size : 0,
                             spare != NULL ? spare->label : "no partition");
                    const esp_err_t err =
                        spare != NULL ? esp_ota_begin(spare, OTA_WITH_SEQUENTIAL_WRITES, &ota)
                                      : ESP_ERR_NOT_FOUND;
                    if (err != ESP_OK) {
                        ESP_LOGE(TAG, "couldn't open the spare partition: %s",
                                 esp_err_to_name(err));
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
        if (finished || reason != DH_FW_REASON_NONE) {
            break;
        }
        // Only now, with everything that arrived handed over, is the download given up.
        if (ended) {
            ESP_LOGW(TAG, "the download broke off");
            reason = dh_fw_download_fail(d, DH_FW_REASON_DOWNLOAD);
        } else if (overdue) {
            ESP_LOGW(TAG, "the download sent nothing for %u s, or has taken %u minutes: giving "
                          "it up",
                     (unsigned)DH_FW_DOWNLOAD_STALL_SECONDS,
                     (unsigned)(DH_FW_DOWNLOAD_TIMEOUT_SECONDS / 60u));
            reason = dh_fw_download_fail(d, DH_FW_REASON_DOWNLOAD);
        } else if (left == 0) {
            // Everything the answer said it had was read, and the update form isn't complete.
            reason = dh_fw_download_fail(d, DH_FW_REASON_DOWNLOAD);
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
            if (err == ESP_OK) {
                ESP_LOGI(TAG, "the new firmware is written, checked and set to start on trial");
                *restart = UPDATE_RESTART_INTO_NEW;
            } else {
                *restart = boot_not_set(spare, err);
                if (*restart == UPDATE_NO_RESTART) {
                    reason = dh_fw_download_fail(d, DH_FW_REASON_FLASH);
                }
            }
        }
    }
    if (ota_open) {
        // Nothing of an abandoned image is ever started: the partition isn't set to boot.
        esp_ota_abort(ota);
    }
    if (spare_written && reason != DH_FW_REASON_NONE && *restart == UPDATE_NO_RESTART) {
        // And it can't be fallen back to either. (Not when a restart is asked for: then the
        // image was written, checked and recorded, and may be the one that starts next. And
        // not when the partition was opened and no write was tried: nothing of it has
        // changed, and what it holds may be the last good firmware, to go back to.)
        spoil_spare(spare);
    }
    if (reason != DH_FW_REASON_NONE) {
        ESP_LOGW(TAG, "the update ended: %s (%s), after %" PRIu64 " bytes. Nothing was installed",
                 dh_fw_reason_name(reason),
                 dh_fw_report_state_name(dh_fw_reason_state(reason)),
                 dh_fw_download_bytes_read(d));
    }
    heap_caps_free(d);
    // DH_FW_REASON_NONE is only ever returned for "restart now".
    if (*restart != UPDATE_NO_RESTART) {
        return DH_FW_REASON_NONE;
    }
    return reason != DH_FW_REASON_NONE ? reason : DH_FW_REASON_FLASH;
}
