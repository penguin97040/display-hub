// A read-only look at the chip's eFuses, for deciding on NVS encryption (milestone D2).
//
// Only ESP-IDF's eFuse getters are called. Nothing here writes, burns or protects anything.

#pragma once

#include <stdbool.h>

#include "esp_efuse.h"

#ifdef __cplusplus
extern "C" {
#endif

#define EFUSE_REPORT_KEYS 6 // KEY0 to KEY5

typedef struct {
    esp_efuse_purpose_t purpose;
    bool read_protected;    // esp_efuse_get_key_dis_read
    bool write_protected;   // esp_efuse_get_key_dis_write
    bool purpose_locked;    // esp_efuse_get_keypurpose_dis_write
    bool unused;            // esp_efuse_key_block_unused: all zero, unprotected, purpose USER
} efuse_key_report_t;

typedef struct {
    efuse_key_report_t keys[EFUSE_REPORT_KEYS];
    bool download_mode_disabled;        // DIS_DOWNLOAD_MODE
    bool secure_download_mode;          // ENABLE_SECURITY_DOWNLOAD
    bool usb_serial_jtag_disabled;      // DIS_USB_SERIAL_JTAG
    bool usb_download_disabled;         // DIS_USB_SERIAL_JTAG_DOWNLOAD_MODE
    bool secure_boot_on;                // esp_secure_boot_enabled()
    bool flash_encryption_on;           // esp_flash_encryption_enabled()
} efuse_report_t;

// Reads the report. Read-only.
void efuse_report_read(efuse_report_t *report);

// ESP-IDF's name for a key purpose, such as "USER" or "HMAC_UP".
const char *efuse_purpose_name(esp_efuse_purpose_t purpose);

#ifdef __cplusplus
}
#endif
