// What the display keeps in flash. See storage.h.
//
// Every value is one blob, written with one nvs_set_blob() and one nvs_commit(). NVS writes a
// blob's new contents before it lets go of the old ones, so a power cut leaves either the old
// value or the new one. Each write is then read back and compared.
//
// A read that fails is never turned into "nothing there": a namespace or key that doesn't exist
// yet is one thing (a display that hasn't been set up), and any other error is a storage failure,
// which stops setup and shows on the screen.
//
// WHY THE SETUP HAS A PARTITION OF ITS OWN. Erasing a key or a namespace in NVS only marks its
// entries as erased (nvs_flash/src/nvs_page.cpp, Page::eraseEntryAndSpan: the entry's two state
// bits are cleared and its bytes are left as they are) until the page is one day reclaimed. So
// after an nvs_erase_all() the encrypted Wi-Fi password, hub addresses and token would still be
// in flash, and flash encryption is off: someone holding the board could run their own program
// on the chip and have its HMAC unit work out the NVS keys. A factory reset therefore erases
// flash itself: everything it must remove lives in the partition "nvs_setup", and the reset
// erases every sector of it and reads it back as blank (wipe_setup_partition). Everything it
// must keep lives in "nvs", which no code here ever erases.
//
// A power cut part-way through that erase leaves the first sectors blank and the rest as they
// were. NVS starts on that without complaint (tried: with the page naming the namespace among
// those erased, it comes up as not set up), and the rest of the old setup would stay in flash
// with nothing to say so. So a mark
// ("dh_reset"/"wiping") is written in "nvs" before the first sector is erased and taken out
// after NVS has started on the blank partition. A start that finds the mark finishes the erase
// before it reads anything. A setup partition that won't start at all is erased and
// started afresh in the same way, so the display comes up waiting for setup; the "nvs"
// partition is never erased, whatever state it is in.

#include "storage.h"

#include <inttypes.h>
#include <string.h>

#include "dh_crypto.h"
#include "dh_setup.h"
#include "esp_heap_caps.h"
#include "esp_log.h"
#include "esp_ota_ops.h"
#include "esp_partition.h"
#include "freertos/FreeRTOS.h"
#include "freertos/semphr.h"
#include "nvs.h"
#include "nvs_flash.h"

static const char *TAG = "storage";

// The two NVS partitions, by their names in partitions.csv.
#define MAIN_PARTITION  "nvs"       // kept by a factory reset
#define SETUP_PARTITION "nvs_setup" // erased, every sector of it, by a factory reset

// In SETUP_PARTITION.
#define SETUP_NAMESPACE "dh_setup"
#define SETUP_KEY       "setup"
#define DEVICE_KEY      "device"
#define SNAPSHOT_KEY    "snapshot"
// In MAIN_PARTITION.
#define FW_NAMESPACE    "dh_fw"
#define FW_KEY_KEYS     "keys"
#define FW_KEY_RECORD   "record"
// The mark that a factory reset's erase has begun and isn't known to have finished.
#define RESET_NAMESPACE "dh_reset"
#define RESET_KEY       "wiping"
// What the storage key check build (milestone D2) left behind: a boot count and a marker.
#define D2_NAMESPACE "dh_d2_check"

// ROOM in the setup partition. It is 128 KiB (partitions.csv): 32 pages of 4096 bytes, one kept
// free by NVS, 126 entries of 32 bytes in each of the rest, so 3906 entries. A blob takes one
// entry for each 32 bytes, one more for each page it touches and one for its index.
//
//   the largest setup, 9464 bytes        about 300 entries
//   the device ID and token, 195 bytes   9
//   the largest snapshot, 32768 bytes    about 1035
//
// NVS writes a blob's new value before it lets go of the old one (nvs_storage.cpp,
// Storage::writeItem), so replacing a 32 KiB snapshot with another needs 2070 entries for the
// two, beside the setup and the token: about 2380 in all, which leaves 1500 spare for the pages
// NVS is part-way through reclaiming. So every value, the largest snapshot included, is replaced
// in one step, and a power cut leaves the old one or the new one. (In the 64 KiB partition
// everything once shared, the old snapshot had to be erased first.) This was tried against
// ESP-IDF v5.5.6's own NVS code built for a computer, with a 128 KiB partition holding the
// largest setup and token: 1500 snapshot saves in one step each, 900 of them 32768 bytes (and
// 300 of those straight over another of 32768), with the setup and the token rewritten every
// 50th, all succeeded and read back, and the setup and the token were unchanged at the end.
// storage_save_snapshot() still erases the old snapshot and tries once more if NVS ever says
// there is no room, so a full partition costs the saved pages and nothing else.
//
// The main partition (64 KiB) holds the firmware keys (201 bytes), the update record (321), the
// factory reset's mark and the one thing here this firmware doesn't write: ESP-IDF's PHY code
// keeps the radio's calibration data there, in a namespace of its own ("phy", about 70
// entries), and writes it at the first start of Wi-Fi. It holds nothing from a bundle, and a
// factory reset leaves it.
_Static_assert(DH_SETUP_BLOB_MAX < 3 * 126 * 32, "the largest setup must fit in three NVS pages");
_Static_assert(DH_MAX_SNAPSHOT_BYTES <= 15u * 125u * 32u, "a snapshot must be a size NVS takes");
// One sector of flash, which is also one NVS page.
#define SECTOR_BYTES 4096u
_Static_assert(DH_SETUP_BLOB_MAX >= SECTOR_BYTES, "the scratch must hold a sector for the wipe's check");

static SemaphoreHandle_t s_lock;
static storage_facts_t s_facts = {.state = STORAGE_FAILED};
static storage_key_status_t s_key_status;
// The eFuse reading main.c prints every minute: brought up to date when the key is made.
static efuse_report_t *s_efuse;
// Encrypted NVS is running on the main partition. False for a new board, whose storage key
// hasn't been made yet: nothing is stored, and nothing is written until store_prepare().
static bool s_nvs_started;
// And on the setup partition. False with s_nvs_started true only after a factory reset whose
// erase failed part-way: nothing is read or written there until a reset, or a restart, has
// finished the job.
static bool s_setup_started;

static uint32_t s_spare_slot_bytes;
// The app partition that is running, as the update core numbers them (0 for ota_0), and whether
// the bootloader started it on trial. Read before NVS is started, so a trial boot whose storage
// doesn't work is still known to be one, and goes back when its five minutes are up.
static bool s_running_known;
static uint8_t s_running_slot;
static bool s_image_on_trial;
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

// The partition a namespace is kept in.
static const char *partition_of(const char *namespace)
{
    return strcmp(namespace, SETUP_NAMESPACE) == 0 ? SETUP_PARTITION : MAIN_PARTITION;
}

// Opens a namespace in its partition. Nothing is opened in a partition that isn't running: the
// answer is then an error, never "not found".
static esp_err_t open_namespace(const char *namespace, nvs_open_mode_t mode, nvs_handle_t *nvs)
{
    const char *partition = partition_of(namespace);
    const bool started = strcmp(partition, SETUP_PARTITION) == 0 ? s_setup_started : s_nvs_started;
    if (!started) {
        return ESP_ERR_NVS_NOT_INITIALIZED;
    }
    return nvs_open_from_partition(partition, namespace, mode, nvs);
}

static blob_result_t read_blob(const char *namespace, const char *key, uint8_t *out, size_t cap,
                               size_t *len)
{
    *len = 0;
    nvs_handle_t nvs;
    esp_err_t err = open_namespace(namespace, NVS_READONLY, &nvs);
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
    esp_err_t err = open_namespace(namespace, NVS_READWRITE, &nvs);
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
    if (s_nvs_started && !s_setup_started) {
        // A factory reset's erase failed part-way: what the setup partition holds isn't known.
        return DH_HELD_UNKNOWN;
    }
    if (s_facts.set_up) {
        return DH_HELD_SETUP;
    }
    // After a failed write of the firmware keys or the update record, what flash holds of them
    // isn't known (storage_fw.h, "When a write fails"). A bundle would write them from memory
    // over it, so none is taken until the display has restarted: it is answered `storage_failed`,
    // with words that say to restart it, before anything is looked at or written.
    return storage_fw_stopped() ? DH_HELD_RESTART_NEEDED : DH_HELD_NOTHING;
}

// One write of the firmware keys, for an update (storage_fw.c) or for setup over USB.
static bool write_fw_keys(const dh_fw_keys *keys)
{
    // Never while update work is stopped, whoever asks: see store_held().
    if (storage_fw_stopped()) {
        ESP_LOGE(TAG, "not writing the firmware keys: updates are off until a restart");
        return false;
    }
    // (On a new board nothing asks for this before store_prepare() has started NVS: its record
    // is new and its budget full, so the update core has nothing to save. If something did,
    // write_blob() fails for want of NVS and update work stops, as for any failed write.)
    uint8_t blob[DH_FW_KEYS_BLOB_LEN];
    dh_fw_keys_encode(keys, blob);
    // Never erased if the write goes wrong: a missing set of keys would read as none.
    const bool ok = write_blob(FW_NAMESPACE, FW_KEY_KEYS, blob, sizeof(blob), false);
    if (!ok) {
        // What flash holds now is unknown: no more update work in this run (storage_fw.h).
        storage_fw_write_failed();
    }
    return ok;
}

static bool write_fw_record(const dh_fw_record *record)
{
    if (storage_fw_stopped()) {
        ESP_LOGE(TAG, "not writing the update record: updates are off until a restart");
        return false;
    }
    uint8_t blob[DH_FW_RECORD_BLOB_LEN];
    dh_fw_record_encode(record, blob);
    // Never erased if the write goes wrong: a missing record would read as a new one, with a
    // full attempt budget.
    const bool ok = write_blob(FW_NAMESPACE, FW_KEY_RECORD, blob, sizeof(blob), false);
    if (!ok) {
        storage_fw_write_failed();
    }
    return ok;
}

// For setup over USB (dh_usb.h).
static bool store_save_keys(void *ctx, const dh_fw_keys *keys)
{
    (void)ctx;
    return write_fw_keys(keys);
}

static bool store_save_record(void *ctx, const dh_fw_record *record)
{
    (void)ctx;
    return write_fw_record(record);
}

static bool store_save_setup(void *ctx, const dh_setup *setup)
{
    (void)ctx;
    // See store_held(): no bundle is taken while update work is stopped.
    if (storage_fw_stopped()) {
        return false;
    }
    const size_t len = dh_setup_encode(setup, s_blob);
    // A setup that didn't read back whole must not be left to read as "set up".
    const bool ok = len > 0 && write_blob(SETUP_NAMESPACE, SETUP_KEY, s_blob, len, true);
    dh_wipe(s_blob, DH_SETUP_BLOB_MAX);
    if (ok) {
        // The setup is the last of a bundle's writes, so the bundle is accepted: the display
        // may have taken its keys, and its attempt budget is full again. An offer refused
        // earlier in this run is looked at afresh (storage_fw.h).
        storage_fw_changed_by_setup();
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

// ---- The factory reset's erase ----

// Whether the mark of an unfinished erase is there. False if it couldn't be read.
static bool read_reset_mark(bool *marked)
{
    *marked = false;
    nvs_handle_t nvs;
    esp_err_t err = open_namespace(RESET_NAMESPACE, NVS_READONLY, &nvs);
    if (err == ESP_ERR_NVS_NOT_FOUND) {
        return true;
    }
    if (err != ESP_OK) {
        return false;
    }
    uint8_t value = 0;
    err = nvs_get_u8(nvs, RESET_KEY, &value);
    nvs_close(nvs);
    if (err == ESP_ERR_NVS_NOT_FOUND) {
        return true;
    }
    // Any value at all is the mark.
    *marked = err == ESP_OK;
    return err == ESP_OK;
}

static bool write_reset_mark(bool marked)
{
    nvs_handle_t nvs;
    esp_err_t err = open_namespace(RESET_NAMESPACE, NVS_READWRITE, &nvs);
    if (err == ESP_OK) {
        err = marked ? nvs_set_u8(nvs, RESET_KEY, 1) : nvs_erase_key(nvs, RESET_KEY);
        if (err == ESP_OK || (!marked && err == ESP_ERR_NVS_NOT_FOUND)) {
            err = nvs_commit(nvs);
        }
        nvs_close(nvs);
    }
    if (err != ESP_OK) {
        ESP_LOGE(TAG, "couldn't %s the mark of a factory reset under way: %s",
                 marked ? "write" : "take out", esp_err_to_name(err));
    }
    return err == ESP_OK;
}

static const esp_partition_t *setup_partition(void)
{
    return esp_partition_find_first(ESP_PARTITION_TYPE_DATA, ESP_PARTITION_SUBTYPE_DATA_NVS,
                                    SETUP_PARTITION);
}

// Erases every sector of the setup partition and reads each back as blank (0xFF), straight
// from flash. NVS must not be running on it. A sector at a time, so no one call holds the chip
// for long. True only if the whole partition then reads as blank.
static bool wipe_setup_partition(const esp_partition_t *partition)
{
    if (partition->size % SECTOR_BYTES != 0) {
        ESP_LOGE(TAG, "the setup partition isn't a whole number of sectors");
        return false;
    }
    for (size_t at = 0; at < partition->size; at += SECTOR_BYTES) {
        esp_err_t err = esp_partition_erase_range(partition, at, SECTOR_BYTES);
        if (err == ESP_OK) {
            err = esp_partition_read_raw(partition, at, s_blob, SECTOR_BYTES);
        }
        bool blank = err == ESP_OK;
        for (size_t i = 0; blank && i < SECTOR_BYTES; i++) {
            blank = s_blob[i] == 0xff;
        }
        if (!blank) {
            ESP_LOGE(TAG, "couldn't erase the setup partition at 0x%x: %s", (unsigned)at,
                     err == ESP_OK ? "it didn't read back as blank" : esp_err_to_name(err));
            // What was read may be part of the old setup.
            dh_wipe(s_blob, DH_SETUP_BLOB_MAX);
            return false;
        }
    }
    return true;
}

// Stops NVS on the setup partition, erases all of it, checks it, and (with a storage key)
// starts NVS on it again, blank. The mark is written first and taken out last: see the top of
// this file. True only if every step was seen to work.
static bool erase_setup_partition(void)
{
    const esp_partition_t *partition = setup_partition();
    if (partition == NULL) {
        ESP_LOGE(TAG, "this flash layout has no \"%s\" partition", SETUP_PARTITION);
        return false;
    }
    // If the mark can't be written the erase still goes ahead: it is what was asked for. Only
    // a power cut in the next second or two could then leave part of the setup behind.
    const bool marked = s_nvs_started && write_reset_mark(true);
    if (s_setup_started) {
        // Closes every handle on it. It is given back to NVS only once it is blank.
        s_setup_started = false;
        const esp_err_t err = nvs_flash_deinit_partition(SETUP_PARTITION);
        if (err != ESP_OK && err != ESP_ERR_NVS_NOT_INITIALIZED) {
            ESP_LOGE(TAG, "couldn't stop NVS on the setup partition: %s", esp_err_to_name(err));
            return false;
        }
    }
    if (!wipe_setup_partition(partition)) {
        return false;
    }
    ESP_LOGW(TAG, "the setup partition was erased: all %u bytes at 0x%x read back as blank",
             (unsigned)partition->size, (unsigned)partition->address);
    if (!s_nvs_started) {
        // A new board with no storage key yet: there is no NVS to start, and no mark.
        return true;
    }
    if (storage_key_start_partition(&s_key_status, SETUP_PARTITION) != ESP_OK) {
        return false;
    }
    // While the mark is there the next start erases the partition again, so nothing may be
    // stored in it: if the mark can't be taken out, the partition stays out of use and the
    // reset is reported as failed, to be tried again or finished by a restart.
    if (marked && !write_reset_mark(false)) {
        (void)nvs_flash_deinit_partition(SETUP_PARTITION);
        return false;
    }
    s_setup_started = true;
    return true;
}

// A factory reset: every sector of the setup partition, and nothing in the main one but the
// mark while it is under way.
static bool store_erase_setup(void *ctx)
{
    (void)ctx;
    // Whatever happens next, nothing more is written for the setup that was held.
    s_generation++;
    // And nothing is held: part of it may be gone already if the erase fails.
    s_facts.set_up = false;
    s_facts.setup_unreadable = false;
    s_facts.hub_hosts = 0;
    s_facts.wifi_set = false;
    s_facts.enrolled = false;
    s_facts.snapshot_bytes = 0;
    if (!erase_setup_partition()) {
        ESP_LOGE(TAG, "couldn't erase the setup: hold BOOT again, or restart the display, to "
                      "finish the job");
        return false;
    }
    return true;
}

// Before a bundle's first write (dh_usb.h): on a new board, makes the storage key, PERMANENTLY
// (storage_key.h says why it is left until now), and starts encrypted NVS for the first time.
// On every other board there is nothing to do.
static bool start_nvs(void);
static bool nothing_stored(void);

static bool store_prepare(void *ctx)
{
    (void)ctx;
    if (s_facts.state != STORAGE_WORKING) {
        return false;
    }
    if (s_nvs_started) {
        return s_setup_started;
    }
    ESP_LOGW(TAG, "a setup has arrived for a board with no storage key: making the key first");
    const esp_err_t err = storage_key_make(s_efuse, &s_key_status);
    storage_key_log(&s_key_status);
    if (err != ESP_OK) {
        s_facts = (storage_facts_t){.state = s_key_status.state == STORAGE_KEY_BLOCKED
                                                 ? STORAGE_NO_KEY
                                                 : STORAGE_FAILED};
        ESP_LOGE(TAG, "the storage key couldn't be made: this display can't be set up");
        return false;
    }
    if (!start_nvs()) {
        s_facts = (storage_facts_t){.state = STORAGE_FAILED};
        ESP_LOGE(TAG, "storage isn't working: this display can't be set up");
        return false;
    }
    ESP_LOGI(TAG, "encrypted NVS started, with a storage key made just now");
    // A new board's flash holds nothing of this firmware's, and what is in memory says so. If
    // something is there after all, memory is wrong: take nothing until a restart has read it.
    if (!nothing_stored()) {
        s_facts = (storage_facts_t){.state = STORAGE_FAILED};
        ESP_LOGE(TAG, "this board had no storage key, yet something is stored: restart the "
                      "display before setting it up");
        return false;
    }
    return true;
}

static const dh_setup_store s_store = {
    .ctx = NULL,
    .held = store_held,
    .save_keys = store_save_keys,
    .save_record = store_save_record,
    .save_setup = store_save_setup,
    .erase_setup = store_erase_setup,
    .prepare = store_prepare,
};

// ---- Start-up ----

// Erases what the storage key check build kept, if it is still there. A first install erases
// the whole flash, so this only matters for a board that went from that build to this one some
// other way.
static void erase_check_build_leftovers(void)
{
    nvs_handle_t nvs;
    if (open_namespace(D2_NAMESPACE, NVS_READONLY, &nvs) != ESP_OK) {
        return;
    }
    nvs_close(nvs);
    if (open_namespace(D2_NAMESPACE, NVS_READWRITE, &nvs) != ESP_OK) {
        return;
    }
    const bool erased = nvs_erase_all(nvs) == ESP_OK && nvs_commit(nvs) == ESP_OK;
    nvs_close(nvs);
    ESP_LOGI(TAG, "the storage key check build's test data: %s",
             erased ? "erased" : "couldn't be erased");
}

// Which app partition is running, and whether the bootloader started it on trial. Needs no NVS.
static void read_running_partition(void)
{
    const esp_partition_t *running = esp_ota_get_running_partition();
    if (running == NULL || running->type != ESP_PARTITION_TYPE_APP ||
        running->subtype < ESP_PARTITION_SUBTYPE_APP_OTA_MIN ||
        running->subtype >= ESP_PARTITION_SUBTYPE_APP_OTA_MAX) {
        ESP_LOGE(TAG, "can't tell which app partition is running");
        return;
    }
    s_running_known = true;
    s_running_slot = (uint8_t)(running->subtype - ESP_PARTITION_SUBTYPE_APP_OTA_MIN);
    esp_ota_img_states_t image_state = ESP_OTA_IMG_UNDEFINED;
    s_image_on_trial = esp_ota_get_state_partition(running, &image_state) == ESP_OK &&
                       image_state == ESP_OTA_IMG_PENDING_VERIFY;
    const esp_partition_t *spare = esp_ota_get_next_update_partition(NULL);
    s_spare_slot_bytes = spare != NULL ? spare->size : 0;
}

// Loads the firmware keys and the update record, and settles them with dh_fw_start()
// (storage_fw.c). False if they couldn't be read, or the running partition isn't known.
static bool load_firmware_state(void)
{
    uint8_t blob[DH_FW_RECORD_BLOB_LEN];
    _Static_assert(DH_FW_RECORD_BLOB_LEN >= DH_FW_KEYS_BLOB_LEN, "one buffer for both blobs");
    size_t len = 0;

    dh_fw_keys *keys = storage_fw_keys();
    dh_fw_keys_init(keys);
    blob_result_t found = read_blob(FW_NAMESPACE, FW_KEY_KEYS, blob, sizeof(blob), &len);
    if (found == BLOB_ERROR) {
        return false;
    }
    if (found != BLOB_MISSING && !dh_fw_keys_decode(keys, blob, len)) {
        // dh_update.h: carry on with no keys. The display then refuses every update until it is
        // set up again over USB, which is the safe side.
        ESP_LOGE(TAG, "the stored firmware keys can't be read: carrying on with none");
    }

    dh_fw_record *record = storage_fw_record();
    dh_fw_record_init(record);
    found = read_blob(FW_NAMESPACE, FW_KEY_RECORD, blob, sizeof(blob), &len);
    if (found == BLOB_ERROR) {
        return false;
    }
    if (found != BLOB_MISSING && !dh_fw_record_decode(record, blob, len)) {
        ESP_LOGE(TAG, "the stored update record can't be read: starting a new one");
    }

    if (!s_running_known) {
        return false;
    }
    storage_fw_settle_start(s_running_slot, s_image_on_trial);
    return true;
}

// How long a stored value is, without reading it. 0 if there is none; false if NVS couldn't say.
static bool stored_length(const char *namespace, const char *key, size_t *len)
{
    *len = 0;
    nvs_handle_t nvs;
    esp_err_t err = open_namespace(namespace, NVS_READONLY, &nvs);
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

// Starts encrypted NVS on the setup partition. If the mark of an unfinished factory reset is
// there, or NVS won't start on it, the partition is erased first, so it comes up blank and the
// display waits for setup. The main partition is only read, and its mark taken out.
static bool start_setup_partition(void)
{
    if (setup_partition() == NULL) {
        ESP_LOGE(TAG, "this flash layout has no \"%s\" partition: it is from an older build. "
                      "Install firmware over USB sets the layout",
                 SETUP_PARTITION);
        return false;
    }
    bool marked = false;
    if (!read_reset_mark(&marked)) {
        ESP_LOGE(TAG, "couldn't tell whether a factory reset was under way");
        return false;
    }
    if (marked) {
        ESP_LOGW(TAG, "a factory reset was cut short: finishing its erase now");
    } else {
        const esp_err_t err = storage_key_start_partition(&s_key_status, SETUP_PARTITION);
        if (err == ESP_OK) {
            s_setup_started = true;
            return true;
        }
        if (err == ESP_ERR_NO_MEM) {
            // Nothing wrong with what is stored.
            return false;
        }
        ESP_LOGE(TAG, "the setup partition can't be used as it is: erasing it, so this display "
                      "will wait for setup");
    }
    return erase_setup_partition();
}

// Starts encrypted NVS on both partitions. Needs a storage key (STORAGE_KEY_PRESENT).
static bool start_nvs(void)
{
    if (storage_key_start_partition(&s_key_status, MAIN_PARTITION) != ESP_OK) {
        return false;
    }
    s_nvs_started = true;
    erase_check_build_leftovers();
    return start_setup_partition();
}

// Whether neither partition holds anything this firmware keeps. False if any is there, or
// that couldn't be told.
static bool nothing_stored(void)
{
    static const struct {
        const char *namespace, *key;
    } all[] = {
        {FW_NAMESPACE, FW_KEY_KEYS},  {FW_NAMESPACE, FW_KEY_RECORD},  {SETUP_NAMESPACE, SETUP_KEY},
        {SETUP_NAMESPACE, DEVICE_KEY}, {SETUP_NAMESPACE, SNAPSHOT_KEY},
    };
    for (size_t i = 0; i < sizeof(all) / sizeof(all[0]); i++) {
        size_t len = 0;
        if (!stored_length(all[i].namespace, all[i].key, &len) || len != 0) {
            return false;
        }
    }
    return true;
}

void storage_start(efuse_report_t *efuse)
{
    s_lock = xSemaphoreCreateRecursiveMutex();
    s_setup_scratch = scratch_alloc(sizeof(*s_setup_scratch));
    s_blob = scratch_alloc(DH_SETUP_BLOB_MAX);
    s_facts = (storage_facts_t){.state = STORAGE_FAILED};
    s_efuse = efuse;
    dh_fw_keys_init(storage_fw_keys());
    dh_fw_record_init(storage_fw_record());
    if (s_lock == NULL || s_setup_scratch == NULL || s_blob == NULL) {
        ESP_LOGE(TAG, "out of memory at start-up");
        abort();
    }

    read_running_partition();

    storage_key_assess(efuse, &s_key_status);
    storage_key_log(&s_key_status);
    if (s_key_status.state == STORAGE_KEY_BLOCKED) {
        s_facts.state = STORAGE_NO_KEY;
        ESP_LOGE(TAG, "storage isn't working: this display can't be set up");
        return;
    }
    if (s_key_status.state == STORAGE_KEY_EMPTY) {
        // A new board. Nothing is burnt and NVS isn't started: without a key nothing can have
        // been stored, so the display holds no setup, no firmware keys and a new update record,
        // which is what memory already says. The key is made by store_prepare().
        if (!s_running_known) {
            ESP_LOGE(TAG, "storage isn't working: this display can't be set up");
            return;
        }
        storage_fw_settle_start(s_running_slot, s_image_on_trial);
        s_facts.state = STORAGE_WORKING;
        return;
    }

    if (!start_nvs()) {
        ESP_LOGE(TAG, "storage isn't working: this display can't be set up");
        return;
    }
    ESP_LOGI(TAG, "encrypted NVS started");
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

const char *storage_usb_words(void)
{
    // Fixed words, at most 200 bytes (dh_usb.h, storage_why).
    return s_facts.state == STORAGE_NO_KEY && s_key_status.unfinished
               ? "This display's storage key was never finished, so it can't keep a setup "
                 "safely and can't be set up. Its screen says more."
               : NULL;
}

const dh_setup_store *storage_store(void)
{
    return s_facts.state == STORAGE_WORKING ? &s_store : NULL;
}

// For storage_fw.c: see storage_fw.h, "What storage.c provides".
bool storage_fw_usable(void)
{
    return s_facts.state == STORAGE_WORKING;
}

bool storage_fw_write_keys(const dh_fw_keys *keys)
{
    return write_fw_keys(keys);
}

bool storage_fw_write_record(const dh_fw_record *record)
{
    return write_fw_record(record);
}

bool storage_image_on_trial(void)
{
    return s_image_on_trial;
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
            // (Rewriting only marks the old copy's entries as erased, so that copy, with the
            // used secret, stays in the setup partition's flash until NVS reclaims the page or
            // a factory reset erases the partition. That is accepted: the secret is spent, and
            // everything else in the old copy is in the new one too.)
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
    if (setup_is(generation) && open_namespace(SETUP_NAMESPACE, NVS_READWRITE, &nvs) == ESP_OK) {
        // One step: the old snapshot stays until the new one is whole (see "Room" at the top of
        // this file). If NVS ever says there is no room all the same, the old one is erased and
        // the write tried once more; a power cut between those two leaves no saved snapshot,
        // which only costs the pages shown before the first poll.
        esp_err_t err = nvs_set_blob(nvs, SNAPSHOT_KEY, body, len);
        if (err == ESP_ERR_NVS_NOT_ENOUGH_SPACE) {
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

void storage_log(void)
{
    storage_lock();
    if (storage_key_burn_changed_more()) {
        ESP_LOGE(TAG, "making the storage key in this run changed more of the eFuses than "
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
        } else if (!s_nvs_started) {
            ESP_LOGI(TAG, "set up: no, waiting for setup over USB (a new board: its storage key "
                          "will be made when the setup arrives)");
        } else if (!s_setup_started) {
            ESP_LOGE(TAG, "set up: no, and a factory reset didn't finish: hold BOOT again or "
                          "restart the display");
        } else {
            ESP_LOGI(TAG, "set up: no, waiting for setup over USB");
        }
        ESP_LOGI(TAG, "spare slot: %" PRIu32 " bytes", s_spare_slot_bytes);
        storage_fw_log();
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
