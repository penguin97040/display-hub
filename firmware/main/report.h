// The read-only facts about the firmware and the board: where it is running from, the board's
// parts, memory and the eFuses. Printed to the console.

#pragma once

#include <stdbool.h>
#include <stdint.h>

#include "board/board.h"
#include "efuse_report.h"

#ifdef __cplusplus
extern "C" {
#endif

typedef struct {
    const char *version;      // from esp_app_desc
    bool running_known;
    char running_label[17];   // "ota_0"
    uint32_t running_address; // 0x20000 after a first install
    uint32_t running_size;
    const char *boot_record;  // what the OTA data says about the running slot, in words
} system_report_t;

typedef struct {
    uint32_t psram_free;
    uint32_t psram_total;
    uint32_t internal_free;
    uint32_t internal_smallest; // the lowest internal free memory since start-up
} memory_report_t;

void system_report_read(system_report_t *report);
void memory_report_read(memory_report_t *report);

// Prints everything to the console (USB-Serial-JTAG), one fact per line.
void report_log(const board_t *board, const system_report_t *system, const efuse_report_t *efuse);

#ifdef __cplusplus
}
#endif
