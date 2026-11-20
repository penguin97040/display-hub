// The screens of this build: waiting for setup, how things stand with the hub (with the titles of
// the pages it sent), the factory reset countdown and the storage problem. They are plain text
// for now; drawing the pages themselves comes with a later milestone.
//
// White text on black throughout: an AMOLED pixel that is black is off. Nothing here shows a
// secret: not the Wi-Fi network's name or password, a hub address, or anything else from a bundle.
//
// Every function here must be called while holding the LVGL lock.

#pragma once

#include <stdbool.h>

#include "hub.h"
#include "storage.h"

#ifdef __cplusplus
extern "C" {
#endif

// Builds the screen. `version` is the firmware's, and `board_note` is NULL or a line about the
// board to keep at the bottom (a revision this firmware doesn't support). Both must stay valid.
void ui_start(const char *version, const char *board_note);

// Shows what fits the facts: waiting for setup, how things stand with the hub (for a display that
// is set up), or the storage problem. `note` is NULL or one line about what just happened ("The
// setup was erased.").
void ui_show_state(const storage_facts_t *facts, const storage_key_status_t *key,
                   const hub_status_t *hub, const char *note);

// The countdown while BOOT is held.
void ui_show_reset_countdown(int seconds_left);

#ifdef __cplusplus
}
#endif
