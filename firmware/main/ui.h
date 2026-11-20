// The screens of this build: waiting for setup, the hub's pages, how things stand with the hub
// and with firmware updates, the factory reset countdown and the storage problem.
//
// Once the display is set up and holds pages, it shows one page at a time, in the hub's order;
// a swipe left or right moves between them, and the status screen comes after the last. Each
// page says what is wrong with the hub, if anything, in one short coloured line, and the status
// screen says what to do about it. Without pages (or setup, or working storage), the status
// screen is all there is, and the factory reset countdown always takes over the whole screen.
//
// White text on black throughout: an AMOLED pixel that is black is off. Every few minutes the
// whole picture moves a few pixels and back, so nothing stays lit in one place for good. Nothing
// here shows a secret: not the Wi-Fi network's name or password, a hub address, or anything else
// from a bundle.
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

// Shows what fits the facts: waiting for setup, the pages and how things stand with the hub (for
// a display that is set up), or the storage problem. `pages` is NULL or the pages to draw: the
// screen keeps drawing from it (its ages are kept up to date), so it must stay valid, and it may
// only be changed while holding the LVGL lock, followed by another call to this. `note` is NULL
// or one line about what just happened ("The setup was erased."). `fw` is how things stand with
// firmware updates: one being installed, a trial boot, or the last one's result.
void ui_show_state(const storage_facts_t *facts, const storage_key_status_t *key,
                   const hub_status_t *hub, const storage_fw_facts_t *fw,
                   const hub_pages_t *pages, const char *note);

// The countdown while BOOT is held. It covers everything else until ui_show_state() is called.
void ui_show_reset_countdown(int seconds_left);

#ifdef __cplusplus
}
#endif
