// One page of a snapshot, drawn with LVGL: the four ready-made shapes, the "can't show this"
// card, and an unavailable page (docs/firmware-handoff.md, "Pages" and "On screen"). The words
// and figures come from ui_text.h; this file only lays them out.
//
// Every function here must be called while holding the LVGL lock.

#pragma once

#include <stdbool.h>
#include <stdint.h>

#include "dh_snapshot.h"
#include "lvgl.h"

#ifdef __cplusplus
extern "C" {
#endif

// The palette. White and quiet grey on black: an AMOLED pixel that is black is off.
#define UI_COLOUR_TEXT    lv_color_white()
#define UI_COLOUR_QUIET   lv_color_hex(0xA0A0A0)
#define UI_COLOUR_WARNING lv_color_hex(0xFFB000)
#define UI_COLOUR_GOOD    lv_color_hex(0x40D060)
#define UI_COLOUR_BAD     lv_color_hex(0xFF5040)
// An empty bar, a label's frame and the dots for the other pages: just visible, never bright.
#define UI_COLOUR_DIM     lv_color_hex(0x404040)

// A meter at or above this share is drawn in the warning colour, as the desktop app does.
#define UI_METER_HIGH_PERCENT 90.0

// The labels whose words change with time alone (the page's age and its meter's countdowns), and
// the figures area. Any of them may be NULL.
typedef struct {
    lv_obj_t *figures; // the area that scrolls, to keep its place across a redraw
    lv_obj_t *age;
    size_t reset_count;
    lv_obj_t *resets[DH_MAX_METER_ITEMS];
} ui_page_live_t;

// Draws page `p` of snapshot `s` into `parent`, an empty flex column, from the top down: its
// data quality, its title, its figures, its note and its age. `saved` and `held_seconds` are
// as for ui_age_text(). The figures area scrolls up and down if they don't all fit.
void ui_page_draw(lv_obj_t *parent, const dh_snapshot *s, const dh_page *p, bool saved,
                  int64_t held_seconds, ui_page_live_t *live);

// Brings the age and the countdowns of a page drawn by ui_page_draw up to date.
void ui_page_tick(const ui_page_live_t *live, const dh_snapshot *s, const dh_page *p, bool saved,
                  int64_t held_seconds);

// What shows in place of the pages when the hub has given this display none.
void ui_no_pages_draw(lv_obj_t *parent);

#ifdef __cplusplus
}
#endif
