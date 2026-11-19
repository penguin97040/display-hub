// A read-only look at the chip's eFuses: the six key blocks and the security settings. It is read
// before and after the storage key is burnt (see storage_key.h), so the log shows what changed.
//
// Only ESP-IDF's eFuse getters are called. Nothing here writes, burns or protects anything.

#pragma once

#include <stdbool.h>
#include <stdint.h>

#include "esp_efuse.h"

#ifdef __cplusplus
extern "C" {
#endif

#define EFUSE_REPORT_KEYS         6 // KEY0 to KEY5
#define EFUSE_REPORT_BLOCK0_WORDS 6 // eFuse block 0 is six 32-bit words

typedef struct {
    esp_efuse_purpose_t purpose;
    bool read_protected;    // esp_efuse_get_key_dis_read
    bool write_protected;   // esp_efuse_get_key_dis_write
    bool purpose_locked;    // esp_efuse_get_keypurpose_dis_write
    bool unused;            // esp_efuse_key_block_unused: all zero, unprotected, purpose USER
    bool reads_as_zeros;    // esp_efuse_block_is_empty: every bit of the block reads back as 0.
                            // True for an empty block, and for one that is read-protected.
} efuse_key_report_t;

typedef struct {
    efuse_key_report_t keys[EFUSE_REPORT_KEYS];
    bool download_mode_disabled;        // DIS_DOWNLOAD_MODE
    bool secure_download_mode;          // ENABLE_SECURITY_DOWNLOAD
    bool usb_serial_jtag_disabled;      // DIS_USB_SERIAL_JTAG
    bool usb_download_disabled;         // DIS_USB_SERIAL_JTAG_DOWNLOAD_MODE
    bool usb_otg_download_disabled;     // DIS_USB_OTG_DOWNLOAD_MODE
    bool force_download_disabled;       // DIS_FORCE_DOWNLOAD
    bool direct_boot_disabled;          // DIS_DIRECT_BOOT
    bool pad_jtag_disabled;             // DIS_PAD_JTAG: JTAG on the pins, off for good
    bool usb_jtag_disabled;             // DIS_USB_JTAG: JTAG over USB, off for good
    uint8_t soft_jtag_disable;          // SOFT_DIS_JTAG (3 bits): an odd count of 1s turns JTAG off
    uint8_t flash_crypt_count;          // SPI_BOOT_CRYPT_CNT (3 bits)
    bool secure_boot_on;                // esp_secure_boot_enabled()
    bool flash_encryption_on;           // esp_flash_encryption_enabled()
    // The raw protection bits, and all of eFuse block 0 (where every setting above lives) as the
    // chip reads it back. Burning the storage key should change only KEY5's own bits in these.
    uint32_t write_disable;             // WR_DIS, 32 bits
    uint8_t read_disable;               // RD_DIS, 7 bits
    uint32_t block0[EFUSE_REPORT_BLOCK0_WORDS];
    // One bit per eFuse block, 0 to 10, that the chip's own error check flags as damaged. (KEY0
    // to KEY5 are blocks 4 to 9.)
    uint16_t damaged_blocks;
} efuse_report_t;

// Reads the report. Read-only.
void efuse_report_read(efuse_report_t *report);

// Prints the whole report to the console. `when` says which reading it is, such as "before the
// burn".
void efuse_report_log(const efuse_report_t *report, const char *when);

// True if `after` differs from `before` in nothing but key block `key`'s own purpose, protection
// and contents. Block 0's raw words are left out of this, because they hold that key's bits too:
// they are printed for a person to compare.
bool efuse_report_same_but_key(const efuse_report_t *before, const efuse_report_t *after, int key);

// ESP-IDF's name for a key purpose, such as "USER" or "HMAC_UP".
const char *efuse_purpose_name(esp_efuse_purpose_t purpose);

#ifdef __cplusplus
}
#endif
