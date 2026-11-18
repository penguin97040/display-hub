// A read-only look at the chip's eFuses. See efuse_report.h.

#include "efuse_report.h"

#include "esp_efuse_table.h"
#include "esp_flash_encrypt.h"
#include "esp_secure_boot.h"

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
    }

    report->download_mode_disabled = esp_efuse_read_field_bit(ESP_EFUSE_DIS_DOWNLOAD_MODE);
    report->secure_download_mode = esp_efuse_read_field_bit(ESP_EFUSE_ENABLE_SECURITY_DOWNLOAD);
    report->usb_serial_jtag_disabled = esp_efuse_read_field_bit(ESP_EFUSE_DIS_USB_SERIAL_JTAG);
    report->usb_download_disabled =
        esp_efuse_read_field_bit(ESP_EFUSE_DIS_USB_SERIAL_JTAG_DOWNLOAD_MODE);
    report->secure_boot_on = esp_secure_boot_enabled();
    report->flash_encryption_on = esp_flash_encryption_enabled();
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
