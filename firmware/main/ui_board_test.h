// The check build's four screens: about the board, a touch test, the eFuse key report, and the
// storage key (ui_storage_key.h). Swiping left or right moves between them.

#pragma once

#include "board/board.h"
#include "efuse_report.h"
#include "report.h"

#ifdef __cplusplus
extern "C" {
#endif

// Builds the screens. Call it once, while holding the LVGL lock.
void ui_board_test_start(const board_t *board, const system_report_t *system,
                         const efuse_report_t *efuse);

// Redraws the eFuse screen from a new report. Call it while holding the LVGL lock.
void ui_board_test_refresh_efuse(const efuse_report_t *efuse);

// The last swipe seen, in words ("none", "left", "right", "up" or "down").
const char *ui_board_test_last_swipe(void);

#ifdef __cplusplus
}
#endif
