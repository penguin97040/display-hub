/* A stand-in for the board, for building main/update.c on a computer: see README.md. */
#include "glue.h"

#include <setjmp.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "esp_err.h"
#include "esp_heap_caps.h"
#include "esp_memory_utils.h"
#include "esp_ota_ops.h"
#include "esp_system.h"
#include "esp_timer.h"
#include "freertos/FreeRTOS.h"
#include "freertos/semphr.h"
#include "freertos/task.h"
#include "hub_http.h"
#include "storage_fw.h"

glue_state glue;

static jmp_buf back_to_test;
static bool can_jump;
static bool restarted;
static TaskFunction_t task_function;
static void *task_arg;

struct glue_mutex {
    bool held;
};

void glue_reset(void)
{
    free(glue.written);
    free(glue.download);
    memset(&glue, 0, sizeof glue);
    glue.piece = 4096;
    glue.write_fails_after = SIZE_MAX;
    glue.may_read = UINT64_MAX;
    task_function = NULL;
    task_arg = NULL;
    dh_fw_keys keys;
    dh_fw_record record;
    dh_fw_keys_init(&keys);
    dh_fw_record_init(&record);
    glue_start(&keys, &record, 0, false);
}

bool glue_run_until_restart(void (*f)(void *), void *arg)
{
    restarted = false;
    can_jump = true;
    if (setjmp(back_to_test) == 0) {
        f(arg);
    }
    can_jump = false;
    return restarted;
}

bool glue_run_task(void)
{
    if (task_function == NULL) {
        return false;
    }
    glue_run_until_restart(task_function, task_arg);
    return true;
}

static _Noreturn void jump_back(void)
{
    if (!can_jump) {
        fprintf(stderr, "the code under test restarted or ended a task outside a run\n");
        exit(1);
    }
    longjmp(back_to_test, 1);
}

/* ---- esp_err.h, esp_system.h, esp_timer.h, esp_memory_utils.h ---- */

const char *esp_err_to_name(esp_err_t err)
{
    return err == ESP_OK ? "ESP_OK" : "an error";
}

void esp_restart(void)
{
    glue.restarts++;
    restarted = true;
    jump_back();
}

int64_t esp_timer_get_time(void)
{
    return glue.now_us;
}

bool esp_ptr_internal(const void *p)
{
    (void)p;
    return !glue.piece_is_external;
}

/* ---- esp_heap_caps.h ---- */

void *heap_caps_calloc(size_t n, size_t size, uint32_t caps)
{
    (void)caps;
    return calloc(n, size);
}

void heap_caps_free(void *p)
{
    /* A download is kept for the test to look at, and freed by glue_reset(). */
    if (p != NULL && glue.download == NULL && glue.keep_next_free) {
        glue.download = p;
        glue.keep_next_free = false;
        return;
    }
    free(p);
}

size_t heap_caps_get_free_size(uint32_t caps)
{
    (void)caps;
    return 100u * 1024u;
}

size_t heap_caps_get_minimum_free_size(uint32_t caps)
{
    (void)caps;
    return 50u * 1024u;
}

/* ---- FreeRTOS ---- */

SemaphoreHandle_t xSemaphoreCreateMutex(void)
{
    static struct glue_mutex mutex;
    mutex.held = false;
    return &mutex;
}

BaseType_t xSemaphoreTake(SemaphoreHandle_t mutex, TickType_t wait)
{
    (void)wait;
    if (mutex->held) {
        glue.mutex_misused = true;
    }
    mutex->held = true;
    return pdTRUE;
}

BaseType_t xSemaphoreGive(SemaphoreHandle_t mutex)
{
    if (!mutex->held) {
        glue.mutex_misused = true;
    }
    mutex->held = false;
    return pdTRUE;
}

BaseType_t xTaskCreate(TaskFunction_t task, const char *name, uint32_t stack_bytes, void *arg,
                       unsigned priority, TaskHandle_t *out)
{
    (void)name;
    (void)stack_bytes;
    (void)priority;
    (void)out;
    task_function = task;
    task_arg = arg;
    glue.task_made = true;
    return pdPASS;
}

void vTaskDelay(TickType_t ticks)
{
    glue.now_us += (int64_t)ticks * 1000;
}

void vTaskDelete(TaskHandle_t task)
{
    (void)task;
    glue.task_ended = true;
    jump_back();
}

/* ---- esp_ota_ops.h: the spare partition is an array ---- */

static const esp_partition_t RUNNING = {ESP_PARTITION_SUBTYPE_APP_OTA_0, 0x20000,
                                        6u * 1024u * 1024u, "ota_0"};
static const esp_partition_t SPARE = {ESP_PARTITION_SUBTYPE_APP_OTA_1, 0x620000,
                                      6u * 1024u * 1024u, "ota_1"};
#define OTA_HANDLE 7u

const esp_partition_t *esp_ota_get_next_update_partition(const esp_partition_t *start_from)
{
    (void)start_from;
    return glue.spare_is_running ? &RUNNING : &SPARE;
}

const esp_partition_t *esp_ota_get_running_partition(void)
{
    return &RUNNING;
}

const esp_partition_t *esp_ota_get_boot_partition(void)
{
    /* Copies, as ESP-IDF may hand out: only what they say can be compared. */
    static esp_partition_t boots;
    if (glue.boot_unreadable) {
        return NULL;
    }
    boots = glue.set_to_boot || (glue.set_boot_fails && glue.set_boot_lands) ? SPARE : RUNNING;
    return &boots;
}

esp_err_t esp_ota_begin(const esp_partition_t *partition, size_t image_size,
                        esp_ota_handle_t *out_handle)
{
    if ((partition != &SPARE && !(glue.spare_is_running && partition == &RUNNING)) ||
        glue.begin_fails || glue.begun) {
        return ESP_FAIL;
    }
    glue.begun = true;
    glue.open = true;
    glue.sequential = image_size == OTA_WITH_SEQUENTIAL_WRITES;
    *out_handle = OTA_HANDLE;
    return ESP_OK;
}

esp_err_t esp_ota_write(esp_ota_handle_t handle, const void *data, size_t size)
{
    if (handle != OTA_HANDLE || !glue.open) {
        glue.wrote_while_closed = true;
        return ESP_FAIL;
    }
    if (glue.written_len + size > glue.write_fails_after) {
        return ESP_FAIL;
    }
    if (glue.written_len + size > glue.written_cap) {
        size_t cap = (glue.written_len + size) * 2u;
        uint8_t *bigger = realloc(glue.written, cap);
        if (bigger == NULL) {
            fprintf(stderr, "out of memory in the stand-in\n");
            exit(1);
        }
        glue.written = bigger;
        glue.written_cap = cap;
    }
    memcpy(glue.written + glue.written_len, data, size);
    glue.written_len += size;
    return ESP_OK;
}

esp_err_t esp_ota_end(esp_ota_handle_t handle)
{
    if (handle != OTA_HANDLE || !glue.open) {
        glue.wrote_while_closed = true;
        return ESP_FAIL;
    }
    /* As in ESP-IDF, the handle is gone whether or not the image passes. */
    glue.open = false;
    glue.ended = true;
    return glue.end_fails ? ESP_FAIL : ESP_OK;
}

esp_err_t esp_ota_abort(esp_ota_handle_t handle)
{
    if (handle != OTA_HANDLE || !glue.open) {
        glue.wrote_while_closed = true;
        return ESP_FAIL;
    }
    glue.open = false;
    glue.aborted = true;
    return ESP_OK;
}

/* ---- esp_partition.h: only the spare partition's first sector may ever be erased ---- */

esp_err_t esp_partition_erase_range(const esp_partition_t *partition, size_t offset, size_t size)
{
    if (partition == NULL || partition->address != SPARE.address || offset != 0 ||
        size != 4096 || glue.open) {
        glue.erased_elsewhere = true;
        return ESP_FAIL;
    }
    if (glue.erase_fails) {
        return ESP_FAIL;
    }
    glue.spare_spoilt++;
    glue.written_when_spoilt = glue.written_len;
    if (glue.written_len > 0) {
        memset(glue.written, 0xff, glue.written_len < size ? glue.written_len : size);
    }
    return ESP_OK;
}

esp_err_t esp_partition_read(const esp_partition_t *partition, size_t offset, void *out,
                             size_t size)
{
    if (partition == NULL || partition->address != SPARE.address) {
        return ESP_FAIL;
    }
    /* Flash that was never written reads as blank. */
    memset(out, 0xff, size);
    if (offset < glue.written_len) {
        size_t n = glue.written_len - offset;
        memcpy(out, glue.written + offset, n < size ? n : size);
    }
    return ESP_OK;
}

esp_err_t esp_ota_set_boot_partition(const esp_partition_t *partition)
{
    if (glue.on_set_boot != NULL) {
        glue.on_set_boot();
    }
    if (partition != &SPARE || glue.set_boot_fails) {
        return ESP_FAIL;
    }
    glue.set_to_boot = true;
    glue.record_writes_at_set_boot = glue.record_writes;
    return ESP_OK;
}

esp_err_t esp_ota_mark_app_valid_cancel_rollback(void)
{
    if (glue.mark_valid_error != ESP_OK) {
        return glue.mark_valid_error;
    }
    glue.marked_valid++;
    glue.writes_at_marked_valid = glue.record_writes + glue.keys_writes;
    return ESP_OK;
}

esp_err_t esp_ota_mark_app_invalid_rollback_and_reboot(void)
{
    if (glue.roll_back_error != ESP_OK) {
        return glue.roll_back_error;
    }
    glue.rolled_back++;
    glue.rolled_back_at_us = glue.now_us;
    esp_restart();
}

/* ---- hub_http.h: the connection is an array ---- */

int hub_http_download_read(hub_download_t *download, uint8_t *out, size_t cap)
{
    (void)download;
    if (cap > glue.most_asked) {
        glue.most_asked = cap;
    }
    if ((uint64_t)glue.handed + cap > glue.may_read) {
        glue.read_past_the_form = true;
    }
    size_t left = glue.body_len - glue.handed;
    if (glue.pauses && glue.paused && left > 0) {
        glue.paused = false;
        glue.now_us += 50000;
        return 0;
    }
    if (left == 0) {
        if (!glue.stalls) {
            return -1;
        }
        /* One wait that ran out with nothing read. */
        glue.now_us += 50000;
        return 0;
    }
    size_t n = left < cap ? left : cap;
    if (n > glue.piece) {
        n = glue.piece;
    }
    memcpy(out, glue.body + glue.handed, n);
    glue.handed += n;
    glue.paused = true;
    glue.now_us += glue.read_takes_us + (int64_t)n * glue.read_takes_us_per_byte;
    return (int)n;
}

/* ---- storage_fw.h: what storage.c provides. storage_fw.c itself is the real one. ---- */

void storage_lock(void)
{
    glue.lock_depth++;
}

void storage_unlock(void)
{
    if (--glue.lock_depth < 0) {
        glue.lock_misused = true;
    }
}

bool storage_fw_usable(void)
{
    return !glue.storage_unusable;
}

static void wrote(char what)
{
    size_t n = strlen(glue.writes);
    if (n + 1 < sizeof glue.writes) {
        glue.writes[n] = what;
    }
}

bool storage_fw_write_keys(const dh_fw_keys *keys)
{
    if (glue.lock_depth <= 0) {
        glue.lock_misused = true;
    }
    if (glue.keys_write_fails) {
        if (glue.failed_write_lands) {
            dh_fw_keys_encode(keys, glue.flash_keys);
        }
        return false;
    }
    dh_fw_keys_encode(keys, glue.flash_keys);
    glue.keys_writes++;
    wrote('k');
    return true;
}

bool storage_fw_write_record(const dh_fw_record *record)
{
    if (glue.lock_depth <= 0) {
        glue.lock_misused = true;
    }
    if (glue.record_write_fails) {
        if (glue.failed_write_lands) {
            dh_fw_record_encode(record, glue.flash_record);
        }
        return false;
    }
    dh_fw_record_encode(record, glue.flash_record);
    glue.record_writes++;
    wrote('r');
    return true;
}

bool storage_image_on_trial(void)
{
    return glue.image_on_trial;
}

bool glue_restart(uint8_t running_slot, bool on_trial)
{
    /* Whatever was in memory is gone. */
    memset(storage_fw_keys(), 0xee, sizeof(dh_fw_keys));
    memset(storage_fw_record(), 0xee, sizeof(dh_fw_record));
    if (!dh_fw_keys_decode(storage_fw_keys(), glue.flash_keys, sizeof glue.flash_keys) ||
        !dh_fw_record_decode(storage_fw_record(), glue.flash_record, sizeof glue.flash_record)) {
        return false;
    }
    glue.now_us = 0;
    storage_lock();
    glue.image_on_trial = storage_fw_settle_start(
        running_slot, on_trial               ? STORAGE_IMAGE_ON_TRIAL
                      : glue.image_unknown   ? STORAGE_IMAGE_UNKNOWN
                                             : STORAGE_IMAGE_GOOD);
    storage_unlock();
    return true;
}

void glue_start(const dh_fw_keys *keys, const dh_fw_record *record, uint8_t running_slot,
                bool on_trial)
{
    dh_fw_keys_encode(keys, glue.flash_keys);
    dh_fw_record_encode(record, glue.flash_record);
    if (!glue_restart(running_slot, on_trial)) {
        fprintf(stderr, "the stand-in was given keys or a record that don't read back\n");
        exit(1);
    }
}
