// Small pieces the screens share: the colours and the plain label and gap every page is built
// from. They are defined in ui_board_test.c.

#pragma once

#include "lvgl.h"

#ifdef __cplusplus
extern "C" {
#endif

#define UI_COLOUR_TEXT    lv_color_white()
#define UI_COLOUR_QUIET   lv_color_hex(0xA0A0A0)
#define UI_COLOUR_WARNING lv_color_hex(0xFFB000)
#define UI_COLOUR_GOOD    lv_color_hex(0x40D060)
#define UI_COLOUR_BAD     lv_color_hex(0xFF5040)

// A label as wide as its parent, with wrapped text.
lv_obj_t *ui_add_label(lv_obj_t *parent, const lv_font_t *font, lv_color_t colour,
                       const char *text);

// An empty gap in a column.
void ui_add_gap(lv_obj_t *parent, int height);

#ifdef __cplusplus
}
#endif
