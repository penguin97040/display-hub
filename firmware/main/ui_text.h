// The words and figures the screens show, worked out in plain C with no LVGL and no ESP-IDF, so
// the host tests can check them (firmware/host-tests/ui_tests.c).
//
// The wording follows the desktop app's preview of a page (apps/hub-manager/src/shown.rs, and
// hub_core's short_duration): "Updated 10 min ago", "Stale. Updated 2 d ago", "Resets in 3 h 20
// min", "Reset due", and a dash for a figure that isn't known. One difference: the screen
// brings ages and countdowns up to date every 30 seconds, so under a minute it says "Updated just
// now" and "Resets in under a minute" where the app would count the seconds. Ages and countdowns are on the
// hub's clock: the snapshot's own times, plus how long it has been held by a monotonic timer.
// The display's own date is never used.

#pragma once

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#include "dh_snapshot.h"

#ifdef __cplusplus
extern "C" {
#endif

// The dash for a figure that isn't known (an en dash, as the desktop app draws it). Never 0.
#define UI_DASH "\xE2\x80\x93"

// Room for any of the texts below.
#define UI_TEXT_ROOM 64

// A duration as the desktop app writes it: "45 s", "12 min", "3 h", "3 h 20 min", "2 d", "2 d 4 h".
// A negative duration counts as 0.
void ui_duration(int64_t seconds, char *out, size_t size);

typedef enum {
    UI_AGE_FRESH,   // "Updated 10 min ago", or "Updated just now" under a minute
    UI_AGE_STALE,   // "Stale. Updated 2 d ago"
    UI_AGE_NEVER,   // "Never updated": the page has never had figures
    UI_AGE_UNKNOWN, // "Saved; age unknown": restored from flash, and no poll has succeeded since
} ui_age;

// The line about a page's age. `saved` is true for pages restored from flash after a restart:
// how old they are isn't known, so nothing is guessed. `held_seconds` is how long the snapshot
// has been held, by a monotonic timer.
ui_age ui_age_text(const dh_snapshot *s, const dh_page *p, bool saved, int64_t held_seconds,
                   char *out, size_t size);

// "Resets in 3 h 20 min", "Resets in under a minute", "Reset due", or "" when the item has no
// reset time or the hub's clock isn't known (saved pages).
void ui_reset_text(const dh_snapshot *s, const dh_meter_item *item, bool saved,
                   int64_t held_seconds, char *out, size_t size);

// A share: "42%", "42.5%", or "unknown". Never "0%" for unknown.
void ui_percent_text(bool has_percent, double percent, char *out, size_t size);

// How much of a bar to fill, from 0 to 1000. 0 for unknown, which draws no fill at all.
int ui_percent_fill(bool has_percent, double percent);

// A big number's figure: `decimals` places (0 to 3), or a dash when it isn't known. A figure
// outside +-1e15, or not a number at all, can't come from the hub and reads as a dash too. A
// negative figure that rounds to zero is written "0", not "-0". No thousands separators, as in
// the desktop app.
#define UI_NUMBER_ROOM 32
void ui_number_text(bool has_value, double value, unsigned decimals, char *out, size_t size);

// The figure with its unit, as the desktop app writes it: "21.5 °C", "1200", or a dash.
#define UI_BIG_NUMBER_ROOM (UI_NUMBER_ROOM + DH_UTF8_ROOM(DH_MAX_UNIT_CHARS) + 1)
void ui_big_number_text(const dh_big_number *b, char *out, size_t size);

// The biggest of `count` font sizes (largest first) at which `width_at(size)` fits in
// `max_width`. Returns the index of that size, or count - 1 (the smallest) when none fits: the
// caller then wraps the text rather than cut it off.
size_t ui_pick_size(const int32_t *sizes, size_t count, int32_t max_width,
                    int32_t (*width_at)(int32_t size, void *ctx), void *ctx);

// How a page is drawn.
typedef enum {
    UI_FORM_CARD,       // the "can't show this" card: the title and dh_card_lines(), nothing else
    UI_FORM_NO_FIGURES, // an unavailable page: its title, its note and its quality, no figures
    UI_FORM_FIGURES,    // one of the four shapes
} ui_form;

ui_form ui_form_of(const dh_page *p);

// The card's lines under the title: 1, or 2 for an unknown kind.
size_t ui_card_lines(const dh_page *p, const char *lines[2]);

// What an unavailable page says under its title: its note, or "No data available".
const char *ui_no_figures_text(const dh_page *p);

// The views in the swipe row: one per page (or one saying there are no pages yet), then the
// status screen at the end.
size_t ui_view_count(size_t page_count);

// The page ids being looked at, kept from one snapshot to the next.
typedef struct {
    size_t count;
    char id[DH_MAX_PAGES][DH_MAX_ID_BYTES + 1];
} ui_page_ids;

void ui_page_ids_of(const dh_snapshot *s, ui_page_ids *out);

// Which view to show once a new snapshot arrives, given the view shown before (an index into
// the old row). The same page by id if it is still there; if it has gone, the nearest page of
// the old row that is still there (the one after it first, then the one before, and so on);
// failing that, the same position, as far as the new row goes. The status screen stays the
// status screen.
size_t ui_keep_view(const ui_page_ids *before, size_t view, const dh_snapshot *after);

// Burn-in care: the content's offset, in pixels, at step `step` of a slow cycle. Each offset is
// within +-UI_SHIFT_MAX in both directions, so content laid out UI_SHIFT_MAX in from the edges
// is never cut off.
#define UI_SHIFT_MAX 4
void ui_shift(unsigned step, int32_t *dx, int32_t *dy);

#ifdef __cplusplus
}
#endif
