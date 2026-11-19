// A read-only look at the chip's eFuses. See efuse_report.h.

#include "efuse_report.h"

#include <inttypes.h>
#include <string.h>

#include "esp_efuse_table.h"
#include "esp_flash_encrypt.h"
#include "esp_log.h"
#include "esp_secure_boot.h"
#include "hal/efuse_hal.h"

static const char *TAG = "efuse";

static const char *yes_no(bool value)
{
    return value ? "yes" : "no";
}

// A field of up to 32 bits, as a number. 0 if it can't be read.
static uint32_t read_field(const esp_efuse_desc_t *field[], size_t bits)
{
    uint32_t value = 0;
    if (esp_efuse_read_field_blob(field, &value, bits) != ESP_OK) {
        return 0;
    }
    return value;
}

void efuse_report_read(efuse_report_t *report)
{
    *report = (efuse_report_t){0};

    for (int i = 0; i < EFUSE_REPORT_KEYS; i++) {
        const esp_efuse_block_t block = (esp_efuse_block_t)(EFUSE_BLK_KEY0 + i);
        efuse_key_report_t *key = &report->keys[i];
        key->purpose = esp_efuse_get_key_purpose(block);
        key->read_protected = esp_efuse_get_key_dis_read(block);
        key->write_protected = esp_efuse_get_key_dis_write(block);
        key->purpose_locked = esp_efuse_get_keypurpose_dis_write(block);
        key->unused = esp_efuse_key_block_unused(block);
        key->reads_as_zeros = esp_efuse_block_is_empty(block);
    }

    report->download_mode_disabled = esp_efuse_read_field_bit(ESP_EFUSE_DIS_DOWNLOAD_MODE);
    report->secure_download_mode = esp_efuse_read_field_bit(ESP_EFUSE_ENABLE_SECURITY_DOWNLOAD);
    report->usb_serial_jtag_disabled = esp_efuse_read_field_bit(ESP_EFUSE_DIS_USB_SERIAL_JTAG);
    report->usb_download_disabled =
        esp_efuse_read_field_bit(ESP_EFUSE_DIS_USB_SERIAL_JTAG_DOWNLOAD_MODE);
    report->usb_otg_download_disabled =
        esp_efuse_read_field_bit(ESP_EFUSE_DIS_USB_OTG_DOWNLOAD_MODE);
    report->force_download_disabled = esp_efuse_read_field_bit(ESP_EFUSE_DIS_FORCE_DOWNLOAD);
    report->direct_boot_disabled = esp_efuse_read_field_bit(ESP_EFUSE_DIS_DIRECT_BOOT);
    report->pad_jtag_disabled = esp_efuse_read_field_bit(ESP_EFUSE_DIS_PAD_JTAG);
    report->usb_jtag_disabled = esp_efuse_read_field_bit(ESP_EFUSE_DIS_USB_JTAG);
    report->soft_jtag_disable = (uint8_t)read_field(ESP_EFUSE_SOFT_DIS_JTAG, 3);
    report->flash_crypt_count = (uint8_t)read_field(ESP_EFUSE_SPI_BOOT_CRYPT_CNT, 3);
    report->secure_boot_on = esp_secure_boot_enabled();
    report->flash_encryption_on = esp_flash_encryption_enabled();

    report->write_disable = read_field(ESP_EFUSE_WR_DIS, 32);
    report->read_disable = (uint8_t)read_field(ESP_EFUSE_RD_DIS, 7);
    // All of block 0 as one field: esp_efuse_read_block() refuses EFUSE_BLK0 (esp_efuse_api.c:211),
    // but a field description may cover it. On the ESP32-S3 it is the six registers from
    // EFUSE_RD_WR_DIS_REG to EFUSE_RD_REPEAT_DATA4_REG (esp32s3/esp_efuse_utility.c:23).
    static const esp_efuse_desc_t block0_desc = {EFUSE_BLK0, 0, EFUSE_REPORT_BLOCK0_WORDS * 32};
    static const esp_efuse_desc_t *block0_field[] = {&block0_desc, NULL};
    report->block0_read =
        esp_efuse_read_field_blob(block0_field, report->block0, sizeof(report->block0) * 8) == ESP_OK;
    if (!report->block0_read) {
        memset(report->block0, 0, sizeof(report->block0));
    }
    // esp_efuse_check_errors() always says "fine" on an ESP32-S3
    // (efuse/esp32s3/esp_efuse_utility.c:71), so ask about each block.
    for (unsigned block = 0; block < EFUSE_BLK_MAX; block++) {
        if (efuse_hal_is_coding_error_in_block(block)) {
            report->damaged_blocks |= (uint16_t)(1u << block);
        }
    }
}

void efuse_report_log(const efuse_report_t *report, const char *when)
{
    ESP_LOGI(TAG, "eFuses %s:", when);
    for (int i = 0; i < EFUSE_REPORT_KEYS; i++) {
        const efuse_key_report_t *key = &report->keys[i];
        ESP_LOGI(TAG,
                 "  KEY%d: purpose %s (%d); read-protected %s; write-protected %s; "
                 "purpose locked %s; reads as zeros %s; empty %s",
                 i, efuse_purpose_name(key->purpose), (int)key->purpose, yes_no(key->read_protected),
                 yes_no(key->write_protected), yes_no(key->purpose_locked),
                 yes_no(key->reads_as_zeros), yes_no(key->unused));
    }
    ESP_LOGI(TAG,
             "  download mode disabled %s; secure download mode %s; USB-Serial-JTAG disabled %s; "
             "USB download disabled %s; USB-OTG download disabled %s",
             yes_no(report->download_mode_disabled), yes_no(report->secure_download_mode),
             yes_no(report->usb_serial_jtag_disabled), yes_no(report->usb_download_disabled),
             yes_no(report->usb_otg_download_disabled));
    ESP_LOGI(TAG,
             "  forced download disabled %s; direct boot disabled %s; JTAG on pins disabled %s; "
             "JTAG over USB disabled %s; soft JTAG disable 0x%x",
             yes_no(report->force_download_disabled), yes_no(report->direct_boot_disabled),
             yes_no(report->pad_jtag_disabled), yes_no(report->usb_jtag_disabled),
             (unsigned)report->soft_jtag_disable);
    ESP_LOGI(TAG, "  Secure Boot %s; flash encryption %s (count 0x%x); damaged eFuse blocks 0x%03x",
             report->secure_boot_on ? "on" : "off", report->flash_encryption_on ? "on" : "off",
             (unsigned)report->flash_crypt_count, (unsigned)report->damaged_blocks);
    ESP_LOGI(TAG, "  write-disable bits 0x%08" PRIx32 "; read-disable bits 0x%02x",
             report->write_disable, (unsigned)report->read_disable);
    if (!report->block0_read) {
        ESP_LOGE(TAG, "  block 0: couldn't be read");
        return;
    }
    ESP_LOGI(TAG,
             "  block 0: %08" PRIx32 " %08" PRIx32 " %08" PRIx32 " %08" PRIx32 " %08" PRIx32
             " %08" PRIx32,
             report->block0[0], report->block0[1], report->block0[2], report->block0[3],
             report->block0[4], report->block0[5]);
}

bool efuse_report_same_but_key(const efuse_report_t *before, const efuse_report_t *after, int key)
{
    const uint16_t key_block_bit = (uint16_t)(1u << (EFUSE_BLK_KEY0 + key));
    for (int i = 0; i < EFUSE_REPORT_KEYS; i++) {
        if (i == key) {
            continue;
        }
        const efuse_key_report_t *a = &before->keys[i];
        const efuse_key_report_t *b = &after->keys[i];
        if (a->purpose != b->purpose || a->read_protected != b->read_protected ||
            a->write_protected != b->write_protected || a->purpose_locked != b->purpose_locked ||
            a->unused != b->unused || a->reads_as_zeros != b->reads_as_zeros) {
            return false;
        }
    }
    return before->download_mode_disabled == after->download_mode_disabled &&
           before->secure_download_mode == after->secure_download_mode &&
           before->usb_serial_jtag_disabled == after->usb_serial_jtag_disabled &&
           before->usb_download_disabled == after->usb_download_disabled &&
           before->usb_otg_download_disabled == after->usb_otg_download_disabled &&
           before->force_download_disabled == after->force_download_disabled &&
           before->direct_boot_disabled == after->direct_boot_disabled &&
           before->pad_jtag_disabled == after->pad_jtag_disabled &&
           before->usb_jtag_disabled == after->usb_jtag_disabled &&
           before->soft_jtag_disable == after->soft_jtag_disable &&
           before->flash_crypt_count == after->flash_crypt_count &&
           before->secure_boot_on == after->secure_boot_on &&
           before->flash_encryption_on == after->flash_encryption_on &&
           (before->damaged_blocks & ~key_block_bit) == (after->damaged_blocks & ~key_block_bit);
}

const char *efuse_purpose_name(esp_efuse_purpose_t purpose)
{
    switch (purpose) {
    case ESP_EFUSE_KEY_PURPOSE_USER:
        return "USER";
    case ESP_EFUSE_KEY_PURPOSE_RESERVED:
        return "RESERVED";
    case ESP_EFUSE_KEY_PURPOSE_XTS_AES_256_KEY_1:
        return "XTS_AES_256_KEY_1";
    case ESP_EFUSE_KEY_PURPOSE_XTS_AES_256_KEY_2:
        return "XTS_AES_256_KEY_2";
    case ESP_EFUSE_KEY_PURPOSE_XTS_AES_128_KEY:
        return "XTS_AES_128_KEY";
    case ESP_EFUSE_KEY_PURPOSE_HMAC_DOWN_ALL:
        return "HMAC_DOWN_ALL";
    case ESP_EFUSE_KEY_PURPOSE_HMAC_DOWN_JTAG:
        return "HMAC_DOWN_JTAG";
    case ESP_EFUSE_KEY_PURPOSE_HMAC_DOWN_DIGITAL_SIGNATURE:
        return "HMAC_DOWN_DIGITAL_SIGNATURE";
    case ESP_EFUSE_KEY_PURPOSE_HMAC_UP:
        return "HMAC_UP";
    case ESP_EFUSE_KEY_PURPOSE_SECURE_BOOT_DIGEST0:
        return "SECURE_BOOT_DIGEST0";
    case ESP_EFUSE_KEY_PURPOSE_SECURE_BOOT_DIGEST1:
        return "SECURE_BOOT_DIGEST1";
    case ESP_EFUSE_KEY_PURPOSE_SECURE_BOOT_DIGEST2:
        return "SECURE_BOOT_DIGEST2";
    default:
        return "unknown";
    }
}
