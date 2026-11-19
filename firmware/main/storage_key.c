// The storage key check. See storage_key.h.
//
// What was read in ESP-IDF v5.5.6 to write this (paths are under components/):
//
// - What can burn the key. Only generate_keys_hmac() (nvs_sec_provider/nvs_sec_provider.c:156),
//   which calls esp_efuse_write_key() at line 181. It is reached only through
//   nvs_flash_generate_keys_v2() (nvs_flash/src/nvs_api.cpp:747), and ESP-IDF calls that from one
//   place: nvs_flash_init(), when reading the keys fails (nvs_api.cpp:145-149). So in this build
//   nvs_flash_init() is NEVER called. Start-up uses nvs_flash_read_security_cfg_v2() and
//   nvs_flash_secure_init(), which can't burn anything: with no key, read_security_cfg_hmac()
//   just returns an error (nvs_sec_provider.c:218-221). The burn calls
//   nvs_flash_generate_keys_v2() directly, from storage_key_burn() below and nowhere else.
// - The start-up hook that these settings add (nvs_sec_provider.c:267) only records which scheme
//   and key block to use. It reads and writes no eFuse.
// - How the key is made. generate_keys_hmac() turns the chip's noise source on
//   (bootloader_random_enable), takes 32 bytes from esp_fill_random(), and turns it off again
//   (lines 175-177). ESP-IDF's "Random Number Generation" page says the generator gives true
//   random numbers while that source is on, and only pseudo-random ones in an app with no Wi-Fi
//   or Bluetooth running otherwise. Turning it off must happen before the ADC is used; nothing in
//   this firmware uses the ADC.
// - What gets burnt. esp_efuse_write_key()
//   (efuse/src/efuse_controller/keys/with_key_purposes/esp_efuse_api_key.c:270) writes the key,
//   write-protects it (285), read-protects it because the purpose is HMAC_UP (332-335), sets the
//   purpose (345) and locks the purpose (346), all in one go. The key block is burnt and checked
//   first, then block 0 with the protection bits (efuse/esp32s3/esp_efuse_utility.c:102-104).

#include "storage_key.h"

#include <inttypes.h>
#include <string.h>

#include "bootloader_random.h"
#include "esp_efuse.h"
#include "esp_efuse_table.h"
#include "esp_heap_caps.h"
#include "esp_log.h"
#include "esp_partition.h"
#include "esp_random.h"
#include "nvs.h"
#include "nvs_flash.h"
#include "nvs_sec_provider.h"
#include "sdkconfig.h"

static const char *TAG = "storage-key";

_Static_assert(CONFIG_NVS_SEC_HMAC_EFUSE_KEY_ID == STORAGE_KEY_BLOCK,
               "the storage key must be in eFuse key block KEY5");

// What the checks keep in NVS. The names and the marker are searched for in the raw partition, so
// they are chosen to be unlikely to turn up there by chance.
#define CHECK_NAMESPACE "dh_d2_check"
#define CHECK_KEY_COUNT "dh_boot_count"
#define CHECK_KEY_MARK  "dh_marker"
static const char CHECK_MARKER[] = "Display Hub D2 marker: this text must not be readable in flash.";

// The bits of eFuse block 0 that burning an HMAC_UP key into KEY5 sets, from
// efuse/esp32s3/esp_efuse_table.csv: WR_DIS.KEY_PURPOSE_5 is bit 13 and WR_DIS.BLOCK_KEY5 bit 28
// (word 0); RD_DIS.BLOCK_KEY5 is bit 37 (word 1, bit 5); KEY_PURPOSE_5 is the 4 bits from bit 108
// (word 3, bits 12 to 15), and HMAC_UP is purpose 8.
static const uint32_t BURN_SETS[EFUSE_REPORT_BLOCK0_WORDS] = {
    (1u << 13) | (1u << 28), 1u << 5, 0, 8u << 12, 0, 0,
};

static const char *yes_no(bool value)
{
    return value ? "yes" : "no";
}

static const char *result_name(check_result_t result)
{
    switch (result) {
    case CHECK_PASS:
        return "pass";
    case CHECK_FAIL:
        return "FAIL";
    default:
        return "not run";
    }
}

static check_result_t pass_if(bool ok)
{
    return ok ? CHECK_PASS : CHECK_FAIL;
}

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

    if (key->purpose == ESP_EFUSE_KEY_PURPOSE_HMAC_UP) {
        status->state = STORAGE_KEY_PRESENT;
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
    if (hmac_scheme() == NULL) {
        add_blocker(status, "This build isn't set to use the HMAC scheme on KEY5.");
    }

    status->state = status->blocker_count == 0 ? STORAGE_KEY_CAN_BURN : STORAGE_KEY_BLOCKED;
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

esp_err_t storage_key_burn(efuse_report_t *before, efuse_report_t *after)
{
    storage_key_status_t status;

    efuse_report_read(before);
    *after = *before;
    efuse_report_log(before, "before the burn");
    storage_key_assess(before, &status);
    if (status.state != STORAGE_KEY_CAN_BURN) {
        ESP_LOGE(TAG, "not burning: the eFuses aren't as expected");
        return ESP_ERR_INVALID_STATE;
    }
    nvs_sec_scheme_t *scheme = hmac_scheme();
    if (scheme == NULL) {
        ESP_LOGE(TAG, "not burning: the HMAC scheme on KEY5 isn't registered");
        return ESP_ERR_INVALID_STATE;
    }
    if (!random_looks_alive()) {
        ESP_LOGE(TAG, "not burning: the random number generator looks stuck");
        return ESP_ERR_INVALID_STATE;
    }

    // The one call that burns. It is ESP-IDF's own: the same function nvs_flash_init() uses in
    // production firmware the first time it finds no key, so the key is made in exactly the same
    // way. It also works out the NVS encryption keys, which aren't wanted here, so wipe them.
    ESP_LOGW(TAG, "burning the storage key into eFuse key block KEY%d now", STORAGE_KEY_BLOCK);
    nvs_sec_cfg_t cfg = {0};
    const esp_err_t err = nvs_flash_generate_keys_v2(scheme, &cfg);
    memset(&cfg, 0, sizeof(cfg));

    efuse_report_read(after);
    efuse_report_log(after, "after the burn");
    storage_key_assess(after, &status);

    if (err != ESP_OK) {
        ESP_LOGE(TAG, "ESP-IDF's key generation returned 0x%x (%s)", (unsigned)err,
                 esp_err_to_name(err));
    }
    if (status.state != STORAGE_KEY_PRESENT) {
        ESP_LOGE(TAG, "KEY%d doesn't hold an HMAC_UP key after the burn", STORAGE_KEY_BLOCK);
        return err != ESP_OK ? err : ESP_FAIL;
    }
    // The key is there. If ESP-IDF still reported an error, it was in working out the NVS keys
    // straight afterwards; the checks will show whether that works.
    ESP_LOGW(TAG, "KEY%d now holds the storage key", STORAGE_KEY_BLOCK);
    return ESP_OK;
}

void storage_key_check_burn(const efuse_report_t *before, const efuse_report_t *after,
                            storage_key_checks_t *checks)
{
    bool ok = efuse_report_same_but_key(before, after, STORAGE_KEY_BLOCK);
    if (!ok) {
        ESP_LOGE(TAG, "something other than KEY%d changed in the burn", STORAGE_KEY_BLOCK);
    }
    for (int i = 0; i < EFUSE_REPORT_BLOCK0_WORDS; i++) {
        const uint32_t expected = before->block0[i] | BURN_SETS[i];
        if (after->block0[i] != expected) {
            ESP_LOGE(TAG,
                     "eFuse block 0, word %d: expected %08" PRIx32 " after the burn, read %08" PRIx32,
                     i, expected, after->block0[i]);
            ok = false;
        }
    }
    if (ok) {
        ESP_LOGI(TAG, "the burn changed only KEY%d's own eFuse bits", STORAGE_KEY_BLOCK);
    }
    checks->burn_changed_only_key = pass_if(ok);
}

// True if `needle` is somewhere in `hay`.
static bool contains(const uint8_t *hay, size_t hay_len, const void *needle, size_t needle_len)
{
    if (needle_len == 0 || hay_len < needle_len) {
        return false;
    }
    for (size_t i = 0; i + needle_len <= hay_len; i++) {
        if (hay[i] == *(const uint8_t *)needle && memcmp(hay + i, needle, needle_len) == 0) {
            return true;
        }
    }
    return false;
}

// Reads the NVS partition exactly as it is in flash and looks for the plain bytes of what the
// checks stored: the marker, and the names it is stored under. Encrypted NVS scrambles all of
// those (only each page's small header is left plain), so finding any of them is a failure.
static void scan_raw_partition(storage_key_checks_t *checks)
{
    static const struct {
        const char *text;
        const char *what;
    } needles[] = {
        {CHECK_MARKER, "the marker"},
        {CHECK_NAMESPACE, "the namespace's name"},
        {CHECK_KEY_COUNT, "the count's name"},
        {CHECK_KEY_MARK, "the marker's name"},
    };
    enum { NEEDLES = sizeof(needles) / sizeof(needles[0]), CHUNK = 4096, OVERLAP = 96 };
    _Static_assert(sizeof(CHECK_MARKER) <= OVERLAP, "the overlap must cover the longest needle");

    checks->raw_flash = CHECK_FAIL;
    checks->raw_bytes_used = 0;

    const esp_partition_t *part = esp_partition_find_first(
        ESP_PARTITION_TYPE_DATA, ESP_PARTITION_SUBTYPE_DATA_NVS, NVS_DEFAULT_PART_NAME);
    uint8_t *buf = heap_caps_malloc(OVERLAP + CHUNK, MALLOC_CAP_DEFAULT);
    if (part == NULL || buf == NULL) {
        ESP_LOGE(TAG, "raw check: no NVS partition, or out of memory");
        heap_caps_free(buf);
        return;
    }

    // First show that the search can find the marker at all, in a buffer that does hold it.
    memset(buf, 0xA5, OVERLAP + CHUNK);
    memcpy(buf + 1000, CHECK_MARKER, strlen(CHECK_MARKER));
    checks->scanner_works = contains(buf, OVERLAP + CHUNK, CHECK_MARKER, strlen(CHECK_MARKER));

    bool found[NEEDLES] = {false};
    bool read_ok = true;
    size_t kept = 0; // bytes carried over from the end of the last chunk
    for (size_t offset = 0; offset < part->size; offset += CHUNK) {
        const size_t len = part->size - offset < CHUNK ? part->size - offset : CHUNK;
        if (esp_partition_read_raw(part, offset, buf + kept, len) != ESP_OK) {
            read_ok = false;
            break;
        }
        for (size_t i = 0; i < len; i++) {
            if (buf[kept + i] != 0xFF) {
                checks->raw_bytes_used++;
            }
        }
        for (int n = 0; n < NEEDLES; n++) {
            if (contains(buf, kept + len, needles[n].text, strlen(needles[n].text))) {
                found[n] = true;
            }
        }
        // Keep the tail, so a needle that straddles two chunks is still found.
        const size_t total = kept + len;
        kept = total < OVERLAP ? total : OVERLAP;
        memmove(buf, buf + total - kept, kept);
    }
    heap_caps_free(buf);

    bool any = false;
    for (int n = 0; n < NEEDLES; n++) {
        if (found[n]) {
            ESP_LOGE(TAG, "raw check: found %s as plain text in the NVS partition", needles[n].what);
            any = true;
        }
    }
    if (!read_ok) {
        ESP_LOGE(TAG, "raw check: couldn't read the NVS partition");
    }
    checks->plain_text_found = any;
    // The partition must hold something, or not finding the marker would mean nothing.
    checks->raw_flash =
        pass_if(read_ok && checks->scanner_works && !any && checks->raw_bytes_used > 0);
}

// Starts encrypted NVS with the keys from the HMAC unit. Never makes a key.
static esp_err_t start_encrypted_nvs(void)
{
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
    return err;
}

static void check_nvs(storage_key_checks_t *checks)
{
    checks->nvs_error = start_encrypted_nvs();
    checks->nvs_started = pass_if(checks->nvs_error == ESP_OK);
    if (checks->nvs_error != ESP_OK) {
        ESP_LOGE(TAG, "encrypted NVS didn't start: 0x%x (%s)", (unsigned)checks->nvs_error,
                 esp_err_to_name(checks->nvs_error));
        return;
    }

    nvs_handle_t nvs;
    esp_err_t err = nvs_open(CHECK_NAMESPACE, NVS_READWRITE, &nvs);
    if (err != ESP_OK) {
        ESP_LOGE(TAG, "couldn't open the check's namespace: %s", esp_err_to_name(err));
        checks->stored_data = CHECK_FAIL;
        checks->written_data = CHECK_FAIL;
        return;
    }

    // What the last start left.
    uint32_t count = 0;
    char text[sizeof(CHECK_MARKER) + 8] = {0};
    size_t text_len = sizeof(text);
    const esp_err_t count_err = nvs_get_u32(nvs, CHECK_KEY_COUNT, &count);
    const esp_err_t mark_err = nvs_get_str(nvs, CHECK_KEY_MARK, text, &text_len);
    if (count_err == ESP_ERR_NVS_NOT_FOUND) {
        // No count yet: the first start since the flash was erased (or since the burn). The
        // marker is written before the count, so a start that lost power part-way leaves no count.
        checks->first_start = true;
        checks->stored_data = CHECK_PASS;
        count = 0;
    } else {
        const bool ok = count_err == ESP_OK && count >= 1 && mark_err == ESP_OK &&
                        strcmp(text, CHECK_MARKER) == 0;
        checks->stored_data = pass_if(ok);
        if (!ok) {
            ESP_LOGE(TAG, "the last start's data didn't read back (count: %s; marker: %s)",
                     esp_err_to_name(count_err), esp_err_to_name(mark_err));
        }
        if (count_err != ESP_OK) {
            count = 0;
        }
    }

    // This start's.
    checks->boot_count = count + 1;
    err = nvs_set_str(nvs, CHECK_KEY_MARK, CHECK_MARKER);
    if (err == ESP_OK) {
        err = nvs_set_u32(nvs, CHECK_KEY_COUNT, checks->boot_count);
    }
    if (err == ESP_OK) {
        err = nvs_commit(nvs);
    }
    nvs_close(nvs);

    // Read it back through a new handle.
    bool written = false;
    if (err == ESP_OK && nvs_open(CHECK_NAMESPACE, NVS_READONLY, &nvs) == ESP_OK) {
        uint32_t again = 0;
        memset(text, 0, sizeof(text));
        text_len = sizeof(text);
        written = nvs_get_u32(nvs, CHECK_KEY_COUNT, &again) == ESP_OK &&
                  again == checks->boot_count &&
                  nvs_get_str(nvs, CHECK_KEY_MARK, text, &text_len) == ESP_OK &&
                  strcmp(text, CHECK_MARKER) == 0;
        nvs_close(nvs);
    }
    if (!written) {
        ESP_LOGE(TAG, "this start's count and marker didn't write and read back (%s)",
                 esp_err_to_name(err));
    }
    checks->written_data = pass_if(written);

    scan_raw_partition(checks);
}

void storage_key_run_checks(const efuse_report_t *efuse, storage_key_checks_t *checks)
{
    const check_result_t burn_check = checks->burn_changed_only_key;
    *checks = (storage_key_checks_t){0};
    checks->burn_changed_only_key = burn_check;
    checks->ran = true;

    const efuse_key_report_t *key = &efuse->keys[STORAGE_KEY_BLOCK];
    checks->key_purpose = pass_if(key->purpose == ESP_EFUSE_KEY_PURPOSE_HMAC_UP);
    checks->key_protected =
        pass_if(key->read_protected && key->write_protected && key->purpose_locked);

    // Try to read the key, as any firmware could. A read-protected block reads back as all zeros.
    // Only whether it did is kept: the bytes are wiped and never printed.
    uint8_t block[32];
    memset(block, 0xFF, sizeof(block));
    const esp_err_t err =
        esp_efuse_read_block(EFUSE_BLK_KEY0 + STORAGE_KEY_BLOCK, block, 0, sizeof(block) * 8);
    bool zeros = err == ESP_OK;
    for (size_t i = 0; i < sizeof(block); i++) {
        if (block[i] != 0) {
            zeros = false;
        }
    }
    memset(block, 0, sizeof(block));
    checks->key_readable = !zeros;
    checks->key_unreadable = pass_if(zeros && key->reads_as_zeros);

    if (checks->key_purpose == CHECK_PASS) {
        check_nvs(checks);
    }

    checks->all_passed = checks->key_purpose == CHECK_PASS && checks->key_protected == CHECK_PASS &&
                         checks->key_unreadable == CHECK_PASS &&
                         checks->nvs_started == CHECK_PASS && checks->stored_data == CHECK_PASS &&
                         checks->written_data == CHECK_PASS && checks->raw_flash == CHECK_PASS &&
                         checks->burn_changed_only_key != CHECK_FAIL;
}

void storage_key_log(const storage_key_status_t *status, const storage_key_checks_t *checks)
{
    switch (status->state) {
    case STORAGE_KEY_CAN_BURN:
        ESP_LOGI(TAG, "storage key: none yet. KEY%d is empty; the burn is offered on screen 4",
                 STORAGE_KEY_BLOCK);
        return;
    case STORAGE_KEY_BLOCKED:
        ESP_LOGW(TAG, "storage key: none, and the burn is NOT offered, because:");
        for (int i = 0; i < status->blocker_count; i++) {
            ESP_LOGW(TAG, "  %s", status->blockers[i]);
        }
        return;
    default:
        break;
    }
    if (!checks->ran) {
        ESP_LOGW(TAG, "storage key: present, but the checks haven't run");
        return;
    }

    ESP_LOGI(TAG, "--- storage key checks: %s ---",
             checks->all_passed ? "PASS" : "FAIL");
    ESP_LOGI(TAG, "  KEY%d purpose is HMAC_UP: %s", STORAGE_KEY_BLOCK,
             result_name(checks->key_purpose));
    ESP_LOGI(TAG, "  KEY%d read-protected, write-protected and purpose locked: %s",
             STORAGE_KEY_BLOCK, result_name(checks->key_protected));
    ESP_LOGI(TAG, "  key readable: %s: %s", yes_no(checks->key_readable),
             result_name(checks->key_unreadable));
    if (checks->nvs_started == CHECK_PASS) {
        ESP_LOGI(TAG, "  encrypted NVS started (HMAC scheme): pass");
    } else {
        ESP_LOGI(TAG, "  encrypted NVS started (HMAC scheme): %s (0x%x, %s)",
                 result_name(checks->nvs_started), (unsigned)checks->nvs_error,
                 esp_err_to_name(checks->nvs_error));
    }
    if (checks->first_start) {
        ESP_LOGI(TAG, "  data from the last start: none yet (first start since an erase): %s",
                 result_name(checks->stored_data));
    } else {
        ESP_LOGI(TAG, "  data from the last start read back: %s", result_name(checks->stored_data));
    }
    ESP_LOGI(TAG, "  boot count: %" PRIu32 " (was %" PRIu32 "); written and read back: %s",
             checks->boot_count, checks->boot_count > 0 ? checks->boot_count - 1 : 0,
             result_name(checks->written_data));
    ESP_LOGI(TAG,
             "  marker not in the raw NVS partition: %s (found as plain text: %s; search "
             "self-test %s; %" PRIu32 " bytes in use)",
             result_name(checks->raw_flash), yes_no(checks->plain_text_found),
             checks->scanner_works ? "ok" : "FAILED", checks->raw_bytes_used);
    if (checks->burn_changed_only_key != CHECK_NOT_RUN) {
        ESP_LOGI(TAG, "  the burn changed only KEY%d's own eFuse bits: %s", STORAGE_KEY_BLOCK,
                 result_name(checks->burn_changed_only_key));
    }
}
