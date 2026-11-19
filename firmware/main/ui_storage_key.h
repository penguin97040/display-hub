// The fourth screen: the storage key. It explains the burn and asks for it in two deliberate
// taps, or says why it isn't offered, or shows the result of the checks. See storage_key.h.
//
// Every function here must be called while holding the LVGL lock (or from LVGL's own task).

#pragma once

#include "lvgl.h"
#include "storage_key.h"

#ifdef __cplusplus
extern "C" {
#endif

// Called, in LVGL's task, when the second confirming tap has been made. It should only wake the
// task that does the burn.
typedef void (*ui_storage_key_confirmed_t)(void);

// Builds the screen's parts inside `page`. Nothing is shown until ui_storage_key_show().
void ui_storage_key_build(lv_obj_t *page);

// Shows what fits the state: the offer to burn, why it isn't offered, or the checks' results.
// `burn_failure` is NULL, or says why a burn that was just asked for didn't happen.
void ui_storage_key_show(const storage_key_status_t *status, const storage_key_checks_t *checks,
                         const char *burn_failure, ui_storage_key_confirmed_t on_confirmed);

// The screen was swiped to (true) or away from (false). Leaving it cancels a half-made
// confirmation.
void ui_storage_key_page_shown(bool shown);

// A swipe was recognised. The touch that made it can't count as a tap.
void ui_storage_key_swipe_seen(void);

#ifdef __cplusplus
}
#endif
