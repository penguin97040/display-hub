// The storage key. See storage_key.h.
//
// What was read in ESP-IDF v5.5.6 to write this (paths are under components/):
//
// - What can burn the key. Only generate_keys_hmac() (nvs_sec_provider/nvs_sec_provider.c:156),
//   which calls esp_efuse_write_key() at line 181. It is reached only through
//   nvs_flash_generate_keys_v2() (nvs_flash/src/nvs_api.cpp:747), and ESP-IDF calls that from one
//   place: nvs_flash_init(), when reading the keys fails (nvs_api.cpp:145-149). So this firmware
//   NEVER calls nvs_flash_init(): it would burn a key into KEY5 whatever state the chip is in.
//   Start-up uses nvs_flash_read_security_cfg_v2() and nvs_flash_secure_init(), which can't burn
//   anything: with no key, read_security_cfg_hmac() just returns an error
//   (nvs_sec_provider.c:218-221). A new board's key is made by calling
//   nvs_flash_generate_keys_v2() directly, from make_key() below and nowhere else, after the
//   checks in storage_key_assess().
// - The start-up hook that these settings add (nvs_sec_provider.c:267) only records which scheme
//   and key block to use. It reads and writes no eFuse.
// - How the key is made. generate_keys_hmac() turns the chip's noise source on
//   (bootloader_random_enable), takes 32 bytes from esp_fill_random(), and turns it off again
//   (lines 175-177). ESP-IDF's "Random Number Generation" page says the generator gives true
//   random numbers while that source is on, and only pseudo-random ones in an app with no Wi-Fi
//   or Bluetooth running otherwise. Turning it off must happen before the ADC is used; nothing in
//   this firmware uses the ADC. The key is made before Wi-Fi is ever started.
// - What gets burnt. esp_efuse_write_key()
//   (efuse/src/efuse_controller/keys/with_key_purposes/esp_efuse_api_key.c:270) writes the key,
//   write-protects it (285), read-protects it because the purpose is HMAC_UP (332-335), sets the
//   purpose (345) and locks the purpose (346), all in one go. The key block is burnt and checked
//   first, then block 0 with the protection bits (efuse/esp32s3/esp_efuse_utility.c:102-104).
// - On a real V2 board (milestone D2) the burnt key read back as all zeros, encrypted NVS worked
//   across restarts, and the board could still be erased and reflashed over USB.

#include "storage_key.h"

#include <inttypes.h>
#include <string.h>

#include "bootloader_random.h"
#include "esp_efuse.h"
#include "esp_log.h"
#include "esp_random.h"
#include "nvs_flash.h"
#include "nvs_sec_provider.h"
#include "sdkconfig.h"

static const char *TAG = "storage-key";

// Set if this start burnt the key and the burn changed more of the eFuses than KEY5's own bits.
// It is permanent by then, so it doesn't stop the display; it is reported every minute instead.
static bool s_burn_changed_more;

_Static_assert(CONFIG_NVS_SEC_HMAC_EFUSE_KEY_ID == STORAGE_KEY_BLOCK,
               "the storage key must be in eFuse key block KEY5");

// The bits of eFuse block 0 that burning an HMAC_UP key into KEY5 sets, from
// efuse/esp32s3/esp_efuse_table.csv: WR_DIS.KEY_PURPOSE_5 is bit 13 and WR_DIS.BLOCK_KEY5 bit 28
// (word 0); RD_DIS.BLOCK_KEY5 is bit 37 (word 1, bit 5); KEY_PURPOSE_5 is the 4 bits from bit 108
// (word 3, bits 12 to 15), and HMAC_UP is purpose 8.
static const uint32_t BURN_SETS[EFUSE_REPORT_BLOCK0_WORDS] = {
    (1u << 13) | (1u << 28), 1u << 5, 0, 8u << 12, 0, 0,
};

// KEY0 to KEY5 are eFuse blocks 4 to 9 (efuse_report.h, damaged_blocks).
#define STORAGE_KEY_EFUSE_BLOCK (4 + STORAGE_KEY_BLOCK)

static void add_blocker(storage_key_status_t *status, const char *why)
{
    if (status->blocker_count < STORAGE_KEY_MAX_BLOCKERS) {
        status->blockers[status->blocker_count++] = why;
    }
}

// The scheme ESP-IDF's start-up hook registered, if it is the HMAC one on KEY5. NULL otherwise.
static nvs_sec_scheme_t *hmac_scheme(void)
{
    nvs_sec_scheme_t *scheme = nvs_flash_get_default_security_scheme();
    if (scheme == NULL || scheme->scheme_id != NVS_SEC_SCHEME_HMAC || scheme->scheme_data == NULL ||
        scheme->nvs_flash_key_gen == NULL || scheme->nvs_flash_read_cfg == NULL) {
        return NULL;
    }
    const nvs_sec_config_hmac_t *cfg = scheme->scheme_data;
    if ((int)cfg->hmac_key_id != STORAGE_KEY_BLOCK) {
        return NULL;
    }
    return scheme;
}

void storage_key_assess(const efuse_report_t *efuse, storage_key_status_t *status)
{
    *status = (storage_key_status_t){0};
    const efuse_key_report_t *key = &efuse->keys[STORAGE_KEY_BLOCK];

    if (hmac_scheme() == NULL) {
        add_blocker(status, "This build isn't set to use the HMAC scheme on KEY5.");
    }

    if (key->purpose == ESP_EFUSE_KEY_PURPOSE_HMAC_UP) {
        // A key block the chip's own error check flags can't be relied on to give the same key
        // at every start.
        if (efuse->damaged_blocks & (1u << STORAGE_KEY_EFUSE_BLOCK)) {
            add_blocker(status, "KEY5 holds the storage key, but reads back as damaged.");
        }
        status->state = status->blocker_count == 0 ? STORAGE_KEY_PRESENT : STORAGE_KEY_BLOCKED;
        return;
    }

    if (key->purpose != ESP_EFUSE_KEY_PURPOSE_USER) {
        add_blocker(status, "KEY5 already has another purpose.");
    }
    if (!key->reads_as_zeros) {
        add_blocker(status, "KEY5 isn't empty.");
    }
    if (key->read_protected || key->write_protected || key->purpose_locked) {
        add_blocker(status, "KEY5 is already protected or locked.");
    }
    for (int i = 0; i < EFUSE_REPORT_KEYS; i++) {
        if (i != STORAGE_KEY_BLOCK && efuse->keys[i].purpose == ESP_EFUSE_KEY_PURPOSE_HMAC_UP) {
            add_blocker(status, "Another key block already holds an HMAC_UP key.");
            break;
        }
    }
    // Bit 0 of the write-disable bits locks the read-disable bits. If it were set, the new key
    // couldn't be read-protected.
    if (efuse->write_disable & 1u) {
        add_blocker(status, "The read-protection bits are locked.");
    }
    if (efuse->download_mode_disabled || efuse->usb_download_disabled ||
        efuse->usb_serial_jtag_disabled) {
        add_blocker(status, "Download mode or USB is turned off by eFuse.");
    }
    if (efuse->secure_download_mode) {
        add_blocker(status, "Secure download mode is on.");
    }
    if (efuse->secure_boot_on) {
        add_blocker(status, "Secure Boot is on.");
    }
    if (efuse->flash_encryption_on || efuse->flash_crypt_count != 0) {
        add_blocker(status, "Flash encryption is on.");
    }
    if (efuse->damaged_blocks != 0) {
        add_blocker(status, "An eFuse block reads back as damaged.");
    }
    // Block 0's raw words are compared before and after the key is made. If they can't be read,
    // the burn couldn't be checked, so none is made.
    if (!efuse->block0_read) {
        add_blocker(status, "eFuse block 0 couldn't be read.");
    }

    status->state = status->blocker_count == 0 ? STORAGE_KEY_EMPTY : STORAGE_KEY_BLOCKED;
}

// A last look at the random number generator before something permanent is made from it. This
// can't prove the numbers are random; it catches a generator that is stuck. The bytes are never
// printed.
static bool random_looks_alive(void)
{
    uint8_t a[32], b[32];
    bootloader_random_enable();
    esp_fill_random(a, sizeof(a));
    esp_fill_random(b, sizeof(b));
    bootloader_random_disable();

    bool all_same = true;
    for (size_t i = 1; i < sizeof(a); i++) {
        if (a[i] != a[0]) {
            all_same = false;
        }
    }
    const bool repeated = memcmp(a, b, sizeof(a)) == 0;
    memset(a, 0, sizeof(a));
    memset(b, 0, sizeof(b));
    return !all_same && !repeated;
}

// Whether the burn changed nothing but KEY5's own eFuse bits.
static bool burn_changed_only_key(const efuse_report_t *before, const efuse_report_t *after)
{
    bool ok = efuse_report_same_but_key(before, after, STORAGE_KEY_BLOCK);
    if (!ok) {
        ESP_LOGE(TAG, "something other than KEY%d changed when the key was made",
                 STORAGE_KEY_BLOCK);
    }
    const bool words_read = before->block0_read && after->block0_read;
    if (!words_read) {
        ESP_LOGE(TAG, "eFuse block 0 couldn't be read, so the change can't be compared");
        ok = false;
    }
    for (int i = 0; words_read && i < EFUSE_REPORT_BLOCK0_WORDS; i++) {
        const uint32_t expected = before->block0[i] | BURN_SETS[i];
        if (after->block0[i] != expected) {
            ESP_LOGE(TAG,
                     "eFuse block 0, word %d: expected %08" PRIx32
                     " after the key was made, read %08" PRIx32,
                     i, expected, after->block0[i]);
            ok = false;
        }
    }
    return ok;
}

// Makes the storage key and burns it into KEY5. PERMANENT. Called only from
// storage_key_start_nvs(), for a new board. It reads the eFuses itself and refuses unless the
// state is STORAGE_KEY_EMPTY. `efuse` is left as the reading taken afterwards.
static esp_err_t make_key(efuse_report_t *efuse)
{
    efuse_report_t before;
    storage_key_status_t status;

    efuse_report_read(&before);
    *efuse = before;
    efuse_report_log(&before, "before the storage key is made");
    storage_key_assess(&before, &status);
    if (status.state != STORAGE_KEY_EMPTY) {
        ESP_LOGE(TAG, "not making a key: the eFuses aren't as expected");
        return ESP_ERR_INVALID_STATE;
    }
    nvs_sec_scheme_t *scheme = hmac_scheme();
    if (scheme == NULL) {
        ESP_LOGE(TAG, "not making a key: the HMAC scheme on KEY5 isn't registered");
        return ESP_ERR_INVALID_STATE;
    }
    if (!random_looks_alive()) {
        ESP_LOGE(TAG, "not making a key: the random number generator looks stuck");
        return ESP_ERR_INVALID_STATE;
    }

    // The one call that burns. It is ESP-IDF's own, the function nvs_flash_init() would use. It
    // also works out the NVS encryption keys, which are read again properly afterwards, so wipe
    // them here.
    ESP_LOGW(TAG,
             "FIRST START: making this board's storage key and burning it into eFuse key block "
             "KEY%d. This is permanent and happens once.",
             STORAGE_KEY_BLOCK);
    nvs_sec_cfg_t cfg = {0};
    const esp_err_t err = nvs_flash_generate_keys_v2(scheme, &cfg);
    memset(&cfg, 0, sizeof(cfg));

    efuse_report_read(efuse);
    efuse_report_log(efuse, "after the storage key was made");
    storage_key_assess(efuse, &status);

    if (err != ESP_OK) {
        ESP_LOGE(TAG, "ESP-IDF's key generation returned 0x%x (%s)", (unsigned)err,
                 esp_err_to_name(err));
    }
    if (status.state != STORAGE_KEY_PRESENT) {
        ESP_LOGE(TAG, "KEY%d doesn't hold a usable HMAC_UP key after the burn", STORAGE_KEY_BLOCK);
        return err != ESP_OK ? err : ESP_FAIL;
    }
    // The key is there. If ESP-IDF still reported an error, it was in working out the NVS keys
    // straight afterwards; starting NVS will show whether that works.
    if (burn_changed_only_key(&before, efuse)) {
        ESP_LOGW(TAG, "KEY%d now holds the storage key; nothing else in the eFuses changed",
                 STORAGE_KEY_BLOCK);
    } else {
        s_burn_changed_more = true;
        ESP_LOGE(TAG, "KEY%d now holds the storage key, but compare the two eFuse reports above",
                 STORAGE_KEY_BLOCK);
    }
    return ESP_OK;
}

esp_err_t storage_key_start_nvs(efuse_report_t *efuse, storage_key_status_t *status,
                                bool *key_made)
{
    *key_made = false;
    storage_key_assess(efuse, status);
    if (status->state == STORAGE_KEY_EMPTY) {
        const esp_err_t err = make_key(efuse);
        storage_key_assess(efuse, status);
        if (err != ESP_OK) {
            return err;
        }
        *key_made = true;
    }
    if (status->state != STORAGE_KEY_PRESENT) {
        return ESP_ERR_INVALID_STATE;
    }

    // Read the keys through the HMAC unit and start NVS with them. Neither call can make a key.
    nvs_sec_scheme_t *scheme = hmac_scheme();
    if (scheme == NULL) {
        return ESP_ERR_INVALID_STATE;
    }
    nvs_sec_cfg_t cfg = {0};
    esp_err_t err = nvs_flash_read_security_cfg_v2(scheme, &cfg);
    if (err == ESP_OK) {
        err = nvs_flash_secure_init(&cfg);
    }
    memset(&cfg, 0, sizeof(cfg));
    if (err != ESP_OK) {
        ESP_LOGE(TAG, "encrypted NVS didn't start: 0x%x (%s)", (unsigned)err,
                 esp_err_to_name(err));
    }
    return err;
}

bool storage_key_burn_changed_more(void)
{
    return s_burn_changed_more;
}

void storage_key_log(const storage_key_status_t *status)
{
    switch (status->state) {
    case STORAGE_KEY_PRESENT:
        ESP_LOGI(TAG, "storage key: KEY%d holds an HMAC_UP key", STORAGE_KEY_BLOCK);
        break;
    case STORAGE_KEY_EMPTY:
        ESP_LOGW(TAG, "storage key: none yet; KEY%d is empty and a key can be made",
                 STORAGE_KEY_BLOCK);
        break;
    default:
        ESP_LOGE(TAG, "storage key: storage can't be used, because:");
        for (int i = 0; i < status->blocker_count; i++) {
            ESP_LOGE(TAG, "  %s", status->blockers[i]);
        }
        break;
    }
}
