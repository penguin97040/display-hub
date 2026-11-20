// What the display keeps in flash. See storage.h.
//
// Every value is one blob, written with one nvs_set_blob() and one nvs_commit(). NVS writes a
// blob's new contents before it lets go of the old ones, so a power cut leaves either the old
// value or the new one. Each write is then read back and compared.
//
// A read that fails is never turned into "nothing there": a namespace or key that doesn't exist
// yet is one thing (a display that hasn't been set up), and any other error is a storage failure,
// which stops setup and shows on the screen.

#include "storage.h"

#include <inttypes.h>
#include <string.h>

#include "dh_crypto.h"
#include "dh_setup.h"
#include "esp_heap_caps.h"
#include "esp_log.h"
#include "esp_ota_ops.h"
#include "esp_partition.h"
#include "esp_timer.h"
#include "freertos/FreeRTOS.h"
#include "freertos/semphr.h"
#include "nvs.h"
#include "nvs_flash.h"

static const char *TAG = "storage";

#define SETUP_NAMESPACE "dh_setup"
#define SETUP_KEY       "setup"
#define DEVICE_KEY      "device"
#define SNAPSHOT_KEY    "snapshot"
#define FW_NAMESPACE    "dh_fw"
#define FW_KEY_KEYS     "keys"
#define FW_KEY_RECORD   "record"
// What the storage key check build (milestone D2) left behind: a boot count and a marker.
#define D2_NAMESPACE "dh_d2_check"

// ROOM. The partition is 64 KiB (partitions.csv): 16 pages of 4096 bytes, one kept free by NVS,
// 126 entries of 32 bytes in each of the rest, so 1890 entries. A blob takes one entry for each
// 32 bytes, one more for each page it touches and one for its index.
//
//   the largest setup, 9464 bytes        about 300 entries
//   the keys, the record, the device     about 30
//   the largest snapshot, 32768 bytes    about 1035
//   ESP-IDF's radio calibration data     about 70 (its own namespace, "phy"; see below)
//
// So everything fits at once, with over 400 entries spare. What doesn't fit is two large
// snapshots: NVS writes a blob's new value before it lets go of the old one
// (nvs_storage.cpp, Storage::writeItem), so replacing a 32 KiB snapshot with another would need
// 2070 entries for the two, and fails with ESP_ERR_NVS_NOT_ENOUGH_SPACE (leaving the old one
// whole: writeMultiPageBlob erases what it had written of the new one). storage_save_snapshot()
// therefore erases the old snapshot first whenever the two together are over
// SNAPSHOT_REPLACE_BYTES, and again if NVS still says there is no room. Everything else is small
// and is always replaced in one step. This was tried against ESP-IDF v5.5.6's own NVS code built
// for a computer, with a 64 KiB partition holding the largest of everything: one step from 32768
// bytes to 32768 failed as above with the old snapshot left whole, and 1500 saves this way
// (300 of them 32768 over 32768) all succeeded with every other value unchanged.
//
// The radio's calibration data is the one thing here this firmware doesn't write: ESP-IDF's PHY
// code keeps it in the same partition (so it is encrypted too) and writes it at the first start
// of Wi-Fi. It holds nothing from a bundle, and a factory reset leaves it.
_Static_assert(DH_SETUP_BLOB_MAX < 3 * 126 * 32, "the largest setup must fit in three NVS pages");
// Old and new together under this are replaced in one step: about 1300 entries with their
// overhead, which fits beside everything else above.
#define SNAPSHOT_REPLACE_BYTES 40000u
_Static_assert(DH_MAX_SNAPSHOT_BYTES <= 15u * 125u * 32u, "a snapshot must be a size NVS takes");

static SemaphoreHandle_t s_lock;
static storage_facts_t s_facts = {.state = STORAGE_FAILED};
static storage_key_status_t s_key_status;

static dh_fw_keys s_keys;
static dh_fw_record s_record;
static dh_fw_session s_session;
static uint32_t s_spare_slot_bytes;
// See storage.h, "The setup's generation".
static uint32_t s_generation;

// Scratch for the setup, as a struct and as its stored form. Both hold secrets while in use and
// are wiped straight afterwards. They are too big for a task's stack, so they are taken from the
// heap once, at start-up.
static dh_setup *s_setup_scratch;
static uint8_t *s_blob;

typedef enum {
    BLOB_FOUND,
    BLOB_MISSING, // the namespace or the key doesn't exist
    BLOB_TOO_BIG, // it exists, and is longer than anything this firmware writes
    BLOB_ERROR,   // NVS couldn't read it
} blob_result_t;

static blob_result_t read_blob(const char *namespace, const char *key, uint8_t *out, size_t cap,
                               size_t *len)
{
    *len = 0;
    nvs_handle_t nvs;
    esp_err_t err = nvs_open(namespace, NVS_READONLY, &nvs);
    if (err == ESP_ERR_NVS_NOT_FOUND) {
        return BLOB_MISSING;
    }
    if (err != ESP_OK) {
        ESP_LOGE(TAG, "couldn't open %s: %s", namespace, esp_err_to_name(err));
        return BLOB_ERROR;
    }
    // Ask how long it is first, so a longer one is told apart from one that can't be read.
    size_t stored = 0;
    err = nvs_get_blob(nvs, key, NULL, &stored);
    blob_result_t result = BLOB_FOUND;
    if (err == ESP_ERR_NVS_NOT_FOUND) {
        result = BLOB_MISSING;
    } else if (err != ESP_OK) {
        result = BLOB_ERROR;
    } else if (stored > cap) {
        result = BLOB_TOO_BIG;
    } else {
        size_t got = cap;
        err = nvs_get_blob(nvs, key, out, &got);
        if (err != ESP_OK || got != stored) {
            result = BLOB_ERROR;
        } else {
            *len = got;
        }
    }
    if (result == BLOB_ERROR) {
        ESP_LOGE(TAG, "couldn't read %s/%s: %s", namespace, key, esp_err_to_name(err));
    }
    nvs_close(nvs);
    return result;
}

// Writes one blob and reads it back. `data` is used for the reading back, so it must be a buffer
// the caller owns; it holds the same bytes afterwards if this returns true. With `erase_if_bad`,
// a failed write that may have reached the flash is erased, so it can't be found at the next start;
// if even that fails, storage is marked failed for the rest of this run.
static bool write_blob(const char *namespace, const char *key, uint8_t *data, size_t len,
                       bool erase_if_bad)
{
    uint8_t before[DH_SHA256_LEN], after[DH_SHA256_LEN];
    if (len == 0 || !dh_sha256_of(data, len, before)) {
        dh_wipe(before, sizeof(before));
        return false;
    }
    nvs_handle_t nvs = 0;
    esp_err_t err = nvs_open(namespace, NVS_READWRITE, &nvs);
    if (err != ESP_OK) {
        ESP_LOGE(TAG, "couldn't open %s for writing: %s", namespace, esp_err_to_name(err));
        dh_wipe(before, sizeof(before));
        return false;
    }
    err = nvs_set_blob(nvs, key, data, len);
    // From here the new value may be in the flash, whatever happens next.
    const bool set = err == ESP_OK;
    if (set) {
        err = nvs_commit(nvs);
    }
    bool ok = err == ESP_OK;
    bool differs = false;
    if (ok) {
        size_t got = len;
        memset(data, 0, len);
        err = nvs_get_blob(nvs, key, data, &got);
        ok = err == ESP_OK && got == len && dh_sha256_of(data, len, after) &&
             memcmp(before, after, sizeof(before)) == 0;
        differs = !ok && err == ESP_OK;
    }
    if (differs) {
        ESP_LOGE(TAG, "couldn't write %s/%s: it read back differently", namespace, key);
    } else if (!ok) {
        ESP_LOGE(TAG, "couldn't write %s/%s: %s", namespace, key, esp_err_to_name(err));
    }
    if (!ok && set && erase_if_bad) {
        esp_err_t gone = nvs_erase_key(nvs, key);
        if (gone == ESP_OK || gone == ESP_ERR_NVS_NOT_FOUND) {
            gone = nvs_commit(nvs);
        }
        if (gone != ESP_OK) {
            // What is in the flash can't be known now, so take nothing further this run.
            ESP_LOGE(TAG, "couldn't erase %s/%s after the failed write: %s; storage is now "
                          "treated as failed until a restart",
                     namespace, key, esp_err_to_name(gone));
            s_facts.state = STORAGE_FAILED;
        }
    }
    nvs_close(nvs);
    dh_wipe(before, sizeof(before));
    dh_wipe(after, sizeof(after));
    return ok;
}

// ---- The store dh_usb.h writes through ----

static dh_held store_held(void *ctx)
{
    (void)ctx;
    if (s_facts.state != STORAGE_WORKING) {
        return DH_HELD_UNKNOWN;
    }
    return s_facts.set_up ? DH_HELD_SETUP : DH_HELD_NOTHING;
}

static bool store_save_keys(void *ctx, const dh_fw_keys *keys)
{
    (void)ctx;
    uint8_t blob[DH_FW_KEYS_BLOB_LEN];
    dh_fw_keys_encode(keys, blob);
    return write_blob(FW_NAMESPACE, FW_KEY_KEYS, blob, sizeof(blob), false);
}

static bool store_save_record(void *ctx, const dh_fw_record *record)
{
    (void)ctx;
    uint8_t blob[DH_FW_RECORD_BLOB_LEN];
    dh_fw_record_encode(record, blob);
    return write_blob(FW_NAMESPACE, FW_KEY_RECORD, blob, sizeof(blob), false);
}

static bool store_save_setup(void *ctx, const dh_setup *setup)
{
    (void)ctx;
    const size_t len = dh_setup_encode(setup, s_blob);
    // A setup that didn't read back whole must not be left to read as "set up".
    const bool ok = len > 0 && write_blob(SETUP_NAMESPACE, SETUP_KEY, s_blob, len, true);
    dh_wipe(s_blob, DH_SETUP_BLOB_MAX);
    if (ok) {
        s_generation++;
        s_facts.set_up = true;
        s_facts.setup_unreadable = false;
        s_facts.hub_hosts = setup->host_count;
        s_facts.wifi_set = setup->has_wifi;
        s_facts.enrolled = false;
        s_facts.snapshot_bytes = 0;
    }
    return ok;
}

// A factory reset: everything in the setup namespace, and nothing in the firmware one.
static bool store_erase_setup(void *ctx)
{
    (void)ctx;
    // Whatever happens next, nothing more is written for the setup that was held.
    s_generation++;
    nvs_handle_t nvs = 0;
    esp_err_t err = nvs_open(SETUP_NAMESPACE, NVS_READWRITE, &nvs);
    if (err == ESP_OK) {
        err = nvs_erase_all(nvs);
        if (err == ESP_OK) {
            err = nvs_commit(nvs);
        }
        // They must be gone, not merely reported so: the setup, the token and the saved pages.
        static const char *const keys[] = {SETUP_KEY, DEVICE_KEY, SNAPSHOT_KEY};
        for (size_t i = 0; err == ESP_OK && i < sizeof(keys) / sizeof(keys[0]); i++) {
            size_t stored = 0;
            const esp_err_t left = nvs_get_blob(nvs, keys[i], NULL, &stored);
            if (left != ESP_ERR_NVS_NOT_FOUND) {
                err = left == ESP_OK ? ESP_FAIL : left;
            }
        }
        nvs_close(nvs);
    }
    if (err != ESP_OK) {
        ESP_LOGE(TAG, "couldn't erase the setup: %s", esp_err_to_name(err));
        return false;
    }
    s_facts.set_up = false;
    s_facts.setup_unreadable = false;
    s_facts.hub_hosts = 0;
    s_facts.wifi_set = false;
    s_facts.enrolled = false;
    s_facts.snapshot_bytes = 0;
    return true;
}

static const dh_setup_store s_store = {
    .ctx = NULL,
    .held = store_held,
    .save_keys = store_save_keys,
    .save_record = store_save_record,
    .save_setup = store_save_setup,
    .erase_setup = store_erase_setup,
};

// ---- Start-up ----

// Erases what the storage key check build kept, if it is still there. A first install erases
// the whole flash, so this only matters for a board that went from that build to this one some
// other way.
static void erase_check_build_leftovers(void)
{
    nvs_handle_t nvs;
    if (nvs_open(D2_NAMESPACE, NVS_READONLY, &nvs) != ESP_OK) {
        return;
    }
    nvs_close(nvs);
    if (nvs_open(D2_NAMESPACE, NVS_READWRITE, &nvs) != ESP_OK) {
        return;
    }
    const bool erased = nvs_erase_all(nvs) == ESP_OK && nvs_commit(nvs) == ESP_OK;
    nvs_close(nvs);
    ESP_LOGI(TAG, "the storage key check build's test data: %s",
             erased ? "erased" : "couldn't be erased");
}

// Loads the firmware keys and the update record, and settles them with dh_fw_start(). False if
// they couldn't be read, or the running partition isn't known.
static bool load_firmware_state(void)
{
    uint8_t blob[DH_FW_RECORD_BLOB_LEN];
    _Static_assert(DH_FW_RECORD_BLOB_LEN >= DH_FW_KEYS_BLOB_LEN, "one buffer for both blobs");
    size_t len = 0;

    dh_fw_keys_init(&s_keys);
    blob_result_t found = read_blob(FW_NAMESPACE, FW_KEY_KEYS, blob, sizeof(blob), &len);
    if (found == BLOB_ERROR) {
        return false;
    }
    if (found != BLOB_MISSING && !dh_fw_keys_decode(&s_keys, blob, len)) {
        // dh_update.h: carry on with no keys. The display then refuses every update until it is
        // set up again over USB, which is the safe side.
        ESP_LOGE(TAG, "the stored firmware keys can't be read: carrying on with none");
    }

    dh_fw_record_init(&s_record);
    found = read_blob(FW_NAMESPACE, FW_KEY_RECORD, blob, sizeof(blob), &len);
    if (found == BLOB_ERROR) {
        return false;
    }
    if (found != BLOB_MISSING && !dh_fw_record_decode(&s_record, blob, len)) {
        ESP_LOGE(TAG, "the stored update record can't be read: starting a new one");
    }

    // Which app partition is running, as the update core numbers them (0 for ota_0, 1 for
    // ota_1), and whether the bootloader started it on trial.
    const esp_partition_t *running = esp_ota_get_running_partition();
    if (running == NULL || running->type != ESP_PARTITION_TYPE_APP ||
        running->subtype < ESP_PARTITION_SUBTYPE_APP_OTA_MIN ||
        running->subtype >= ESP_PARTITION_SUBTYPE_APP_OTA_MAX) {
        ESP_LOGE(TAG, "can't tell which app partition is running");
        return false;
    }
    const uint8_t slot = (uint8_t)(running->subtype - ESP_PARTITION_SUBTYPE_APP_OTA_MIN);
    esp_ota_img_states_t image_state = ESP_OTA_IMG_UNDEFINED;
    const bool on_trial = esp_ota_get_state_partition(running, &image_state) == ESP_OK &&
                          image_state == ESP_OTA_IMG_PENDING_VERIFY;

    unsigned save = 0;
    const dh_fw_start_result started =
        dh_fw_start(&s_session, &s_record, &s_keys, slot, on_trial, &save);
    // The keys first, then the record. If a write fails, the same is settled at the next start.
    if ((save & DH_FW_SAVE_KEYS) != 0 && !store_save_keys(NULL, &s_keys)) {
        ESP_LOGE(TAG, "couldn't save the firmware keys after start-up");
    }
    if ((save & DH_FW_SAVE_RECORD) != 0 && !store_save_record(NULL, &s_record)) {
        ESP_LOGE(TAG, "couldn't save the update record after start-up");
    }
    if (started != DH_FW_START_NORMAL) {
        // Nothing in this build installs updates, so this isn't expected yet.
        ESP_LOGW(TAG, "the update record had something to settle at start-up (%d)", (int)started);
    }

    const esp_partition_t *spare = esp_ota_get_next_update_partition(NULL);
    s_spare_slot_bytes = spare != NULL ? spare->size : 0;
    return true;
}

// How long a stored value is, without reading it. 0 if there is none; false if NVS couldn't say.
static bool stored_length(const char *namespace, const char *key, size_t *len)
{
    *len = 0;
    nvs_handle_t nvs;
    esp_err_t err = nvs_open(namespace, NVS_READONLY, &nvs);
    if (err == ESP_ERR_NVS_NOT_FOUND) {
        return true;
    }
    if (err != ESP_OK) {
        return false;
    }
    err = nvs_get_blob(nvs, key, NULL, len);
    nvs_close(nvs);
    if (err == ESP_ERR_NVS_NOT_FOUND) {
        *len = 0;
        return true;
    }
    return err == ESP_OK;
}

// Whether a device ID and token and a saved snapshot are held. A token is "held" even if it
// turns out not to read: `hello` must go on saying the display is set up.
static bool load_presence(void)
{
    size_t device = 0, snapshot = 0;
    if (!stored_length(SETUP_NAMESPACE, DEVICE_KEY, &device) ||
        !stored_length(SETUP_NAMESPACE, SNAPSHOT_KEY, &snapshot)) {
        ESP_LOGE(TAG, "couldn't tell whether a device token and a saved snapshot are stored");
        return false;
    }
    s_facts.enrolled = device > 0;
    s_facts.snapshot_bytes = (uint32_t)snapshot;
    if (s_facts.enrolled && !s_facts.set_up) {
        // Never written this way round, but a display holding a token is set up all the same:
        // it refuses another bundle until a factory reset.
        s_facts.set_up = true;
        s_facts.setup_unreadable = true;
    }
    return true;
}

// Loads the setup, keeping only the facts about it. False if it couldn't be read.
static bool load_setup(void)
{
    size_t len = 0;
    const blob_result_t found =
        read_blob(SETUP_NAMESPACE, SETUP_KEY, s_blob, DH_SETUP_BLOB_MAX, &len);
    bool ok = true;
    switch (found) {
    case BLOB_MISSING:
        break;
    case BLOB_FOUND:
        s_facts.set_up = true;
        if (dh_setup_decode(s_setup_scratch, s_blob, len)) {
            s_facts.hub_hosts = s_setup_scratch->host_count;
            s_facts.wifi_set = s_setup_scratch->has_wifi;
        } else {
            s_facts.setup_unreadable = true;
        }
        break;
    case BLOB_TOO_BIG:
        s_facts.set_up = true;
        s_facts.setup_unreadable = true;
        break;
    default:
        ok = false;
        break;
    }
    dh_setup_wipe(s_setup_scratch);
    dh_wipe(s_blob, DH_SETUP_BLOB_MAX);
    if (s_facts.setup_unreadable) {
        // Still "set up": a setup is held, so another is refused until a factory reset.
        ESP_LOGE(TAG, "a setup is stored, but not in a form this firmware reads; "
                      "a factory reset (hold BOOT for 10 seconds) clears it");
    }
    return ok && load_presence();
}

// From PSRAM if there is any: the internal memory is wanted for Wi-Fi and TLS.
static void *scratch_alloc(size_t bytes)
{
    void *p = heap_caps_calloc(1, bytes, MALLOC_CAP_SPIRAM | MALLOC_CAP_8BIT);
    return p != NULL ? p : heap_caps_calloc(1, bytes, MALLOC_CAP_DEFAULT);
}

void storage_start(efuse_report_t *efuse)
{
    s_lock = xSemaphoreCreateRecursiveMutex();
    s_setup_scratch = scratch_alloc(sizeof(*s_setup_scratch));
    s_blob = scratch_alloc(DH_SETUP_BLOB_MAX);
    s_facts = (storage_facts_t){.state = STORAGE_FAILED};
    dh_fw_keys_init(&s_keys);
    dh_fw_record_init(&s_record);
    if (s_lock == NULL || s_setup_scratch == NULL || s_blob == NULL) {
        ESP_LOGE(TAG, "out of memory at start-up");
        abort();
    }

    bool key_made = false;
    const esp_err_t err = storage_key_start_nvs(efuse, &s_key_status, &key_made);
    storage_key_log(&s_key_status);
    if (err != ESP_OK) {
        s_facts.state =
            s_key_status.state == STORAGE_KEY_BLOCKED ? STORAGE_NO_KEY : STORAGE_FAILED;
        ESP_LOGE(TAG, "storage isn't working: this display can't be set up");
        return;
    }
    ESP_LOGI(TAG, "encrypted NVS started%s", key_made ? " (with a storage key made just now)" : "");

    erase_check_build_leftovers();
    if (!load_firmware_state() || !load_setup()) {
        s_facts = (storage_facts_t){.state = STORAGE_FAILED};
        ESP_LOGE(TAG, "storage couldn't be read: this display can't be set up");
        return;
    }
    s_facts.state = STORAGE_WORKING;
}

void storage_lock(void)
{
    xSemaphoreTakeRecursive(s_lock, portMAX_DELAY);
}

void storage_unlock(void)
{
    xSemaphoreGiveRecursive(s_lock);
}

storage_facts_t storage_facts(void)
{
    storage_lock();
    const storage_facts_t facts = s_facts;
    storage_unlock();
    return facts;
}

const storage_key_status_t *storage_key_status(void)
{
    return &s_key_status;
}

const dh_setup_store *storage_store(void)
{
    return s_facts.state == STORAGE_WORKING ? &s_store : NULL;
}

dh_fw_keys *storage_fw_keys(void)
{
    return &s_keys;
}

dh_fw_record *storage_fw_record(void)
{
    return &s_record;
}

uint32_t storage_spare_slot_bytes(void)
{
    return s_spare_slot_bytes;
}

// ---- For the task that talks to the hub ----

uint32_t storage_setup_generation(void)
{
    storage_lock();
    const uint32_t generation = s_generation;
    storage_unlock();
    return generation;
}

// Whether a readable setup with this number is held. Call with the lock held.
static bool setup_is(uint32_t generation)
{
    return s_facts.state == STORAGE_WORKING && s_facts.set_up && !s_facts.setup_unreadable &&
           generation == s_generation;
}

bool storage_load_setup(dh_setup *out, uint32_t *generation)
{
    storage_lock();
    bool ok = setup_is(s_generation);
    if (ok) {
        size_t len = 0;
        ok = read_blob(SETUP_NAMESPACE, SETUP_KEY, s_blob, DH_SETUP_BLOB_MAX, &len) ==
                 BLOB_FOUND &&
             dh_setup_decode(out, s_blob, len);
        dh_wipe(s_blob, DH_SETUP_BLOB_MAX);
    }
    if (!ok) {
        dh_setup_wipe(out);
    }
    *generation = s_generation;
    storage_unlock();
    return ok;
}

storage_held_t storage_load_device(uint32_t generation, dh_device *out)
{
    uint8_t blob[DH_DEVICE_BLOB_MAX];
    size_t len = 0;
    storage_held_t held = STORAGE_UNREADABLE;
    dh_device_wipe(out);
    storage_lock();
    if (setup_is(generation)) {
        switch (read_blob(SETUP_NAMESPACE, DEVICE_KEY, blob, sizeof(blob), &len)) {
        case BLOB_MISSING:
            held = STORAGE_NOT_HELD;
            break;
        case BLOB_FOUND:
            if (dh_device_decode(out, blob, len)) {
                held = STORAGE_HELD;
            }
            break;
        default:
            break;
        }
    }
    storage_unlock();
    dh_wipe(blob, sizeof(blob));
    return held;
}

bool storage_save_device(uint32_t generation, const dh_device *device)
{
    uint8_t blob[DH_DEVICE_BLOB_MAX];
    const size_t len = dh_device_encode(device, blob);
    bool ok = false;
    storage_lock();
    if (len > 0 && setup_is(generation)) {
        // A token that didn't read back whole must not be found at the next start.
        ok = write_blob(SETUP_NAMESPACE, DEVICE_KEY, blob, len, true);
        if (ok) {
            s_facts.enrolled = true;
        }
    }
    storage_unlock();
    dh_wipe(blob, sizeof(blob));
    return ok;
}

bool storage_clear_secret(uint32_t generation)
{
    bool ok = false;
    storage_lock();
    if (setup_is(generation)) {
        size_t len = 0;
        ok = read_blob(SETUP_NAMESPACE, SETUP_KEY, s_blob, DH_SETUP_BLOB_MAX, &len) ==
                 BLOB_FOUND &&
             dh_setup_decode(s_setup_scratch, s_blob, len);
        if (ok && s_setup_scratch->secret_len > 0) {
            dh_wipe(s_setup_scratch->secret, sizeof(s_setup_scratch->secret));
            s_setup_scratch->secret_len = 0;
            len = dh_setup_encode(s_setup_scratch, s_blob);
            // Never erased if the write goes wrong: the old setup, secret and all, is better
            // than none, and the secret is of no use to anyone once the hub has seen it.
            ok = len > 0 && write_blob(SETUP_NAMESPACE, SETUP_KEY, s_blob, len, false);
        }
        dh_setup_wipe(s_setup_scratch);
        dh_wipe(s_blob, DH_SETUP_BLOB_MAX);
    }
    storage_unlock();
    return ok;
}

size_t storage_load_snapshot(uint32_t generation, uint8_t *out, size_t cap)
{
    size_t len = 0;
    storage_lock();
    if (setup_is(generation) &&
        read_blob(SETUP_NAMESPACE, SNAPSHOT_KEY, out, cap, &len) != BLOB_FOUND) {
        len = 0;
    }
    storage_unlock();
    return len;
}

bool storage_save_snapshot(uint32_t generation, uint8_t *body, size_t len)
{
    if (len == 0 || len > DH_MAX_SNAPSHOT_BYTES) {
        return false;
    }
    uint8_t before[DH_SHA256_LEN], after[DH_SHA256_LEN];
    if (!dh_sha256_of(body, len, before)) {
        return false;
    }
    bool ok = false;
    storage_lock();
    nvs_handle_t nvs = 0;
    if (setup_is(generation) && nvs_open(SETUP_NAMESPACE, NVS_READWRITE, &nvs) == ESP_OK) {
        // See "Room" at the top of this file. Erasing first means a power cut between the two
        // steps leaves no saved snapshot, which only costs the pages shown before the first poll.
        bool erased = false;
        esp_err_t err = ESP_OK;
        if (s_facts.snapshot_bytes + len > SNAPSHOT_REPLACE_BYTES) {
            err = nvs_erase_key(nvs, SNAPSHOT_KEY);
            if (err == ESP_OK || err == ESP_ERR_NVS_NOT_FOUND) {
                err = nvs_commit(nvs);
            }
            erased = true;
        }
        if (err == ESP_OK) {
            err = nvs_set_blob(nvs, SNAPSHOT_KEY, body, len);
        }
        if (err == ESP_ERR_NVS_NOT_ENOUGH_SPACE && !erased) {
            err = nvs_erase_key(nvs, SNAPSHOT_KEY);
            if (err == ESP_OK || err == ESP_ERR_NVS_NOT_FOUND) {
                err = nvs_commit(nvs);
            }
            if (err == ESP_OK) {
                err = nvs_set_blob(nvs, SNAPSHOT_KEY, body, len);
            }
        }
        if (err == ESP_OK) {
            err = nvs_commit(nvs);
        }
        if (err == ESP_OK) {
            size_t got = len;
            memset(body, 0, len);
            err = nvs_get_blob(nvs, SNAPSHOT_KEY, body, &got);
            ok = err == ESP_OK && got == len && dh_sha256_of(body, len, after) &&
                 memcmp(before, after, sizeof(before)) == 0;
        }
        if (!ok) {
            ESP_LOGE(TAG, "couldn't save the snapshot (%u bytes): %s", (unsigned)len,
                     err == ESP_OK ? "it read back differently" : esp_err_to_name(err));
            // Leave none, never part of one or an old one that reads as current.
            if (nvs_erase_key(nvs, SNAPSHOT_KEY) == ESP_OK) {
                nvs_commit(nvs);
            }
        }
        s_facts.snapshot_bytes = ok ? (uint32_t)len : 0;
        nvs_close(nvs);
    }
    storage_unlock();
    return ok;
}

void storage_fw_tick(void)
{
    storage_lock();
    if (s_facts.state == STORAGE_WORKING) {
        const uint8_t attempts = s_record.attempts_left;
        const uint64_t uptime_s = (uint64_t)(esp_timer_get_time() / 1000000);
        if ((dh_fw_tick(&s_session, &s_record, uptime_s) & DH_FW_SAVE_RECORD) != 0) {
            // If the write fails, the same hour is counted again after the next start.
            if (!store_save_record(NULL, &s_record)) {
                ESP_LOGE(TAG, "couldn't save the update record after counting running time");
            }
            if (s_record.attempts_left != attempts) {
                ESP_LOGI(TAG, "update attempts left: %u", (unsigned)s_record.attempts_left);
            }
        }
    }
    storage_unlock();
}

uint8_t storage_fw_attempts_left(void)
{
    storage_lock();
    const uint8_t attempts = s_record.attempts_left;
    storage_unlock();
    return attempts;
}

bool storage_fw_on_trial(void)
{
    storage_lock();
    const bool trial = s_session.trial;
    storage_unlock();
    return trial;
}

void storage_log(void)
{
    storage_lock();
    if (storage_key_burn_changed_more()) {
        ESP_LOGE(TAG, "making the storage key at this start changed more of the eFuses than "
                      "KEY%d: compare the eFuse reports printed at start-up",
                 STORAGE_KEY_BLOCK);
    }
    switch (s_facts.state) {
    case STORAGE_WORKING:
        if (s_facts.setup_unreadable) {
            ESP_LOGW(TAG, "set up: yes, but the stored setup can't be read");
        } else if (s_facts.set_up) {
            ESP_LOGI(TAG,
                     "set up: yes (%u hub %s, Wi-Fi %s, device token %s, saved snapshot %" PRIu32
                     " bytes)",
                     (unsigned)s_facts.hub_hosts, s_facts.hub_hosts == 1 ? "address" : "addresses",
                     s_facts.wifi_set ? "set" : "not set", s_facts.enrolled ? "held" : "not held",
                     s_facts.snapshot_bytes);
        } else {
            ESP_LOGI(TAG, "set up: no, waiting for setup over USB");
        }
        ESP_LOGI(TAG,
                 "firmware keys: %u; security floor: %" PRIu32 "; update attempts left: %u; "
                 "spare slot: %" PRIu32 " bytes",
                 (unsigned)s_keys.key_count, s_keys.security_floor,
                 (unsigned)s_record.attempts_left, s_spare_slot_bytes);
        break;
    case STORAGE_NO_KEY:
        ESP_LOGE(TAG, "storage isn't working: the storage key can't be used");
        storage_key_log(&s_key_status);
        break;
    default:
        ESP_LOGE(TAG, "storage isn't working: encrypted NVS didn't start or couldn't be read");
        break;
    }
    storage_unlock();
}
