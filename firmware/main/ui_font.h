// The screens' fonts: Geist, as the desktop app uses, drawn from its TrueType files by LVGL's
// built-in Tiny TTF (stb_truetype). The two files are built into the app (main/CMakeLists.txt);
// each glyph is drawn the first time it is needed and kept in a cache.
//
// Text from the hub is any Unicode. Geist covers Latin with its accents, macrons included (ā ē ī
// ō ū and their capitals), and basic Cyrillic, but not Greek, Chinese, Japanese or emoji. A
// character it lacks is drawn as a hollow box in its place; the rest of the text is drawn as
// usual.
//
// This file also gives LVGL its memory (LV_USE_CUSTOM_MALLOC): from PSRAM first, so that the
// widgets and the glyph caches leave the internal memory to Wi-Fi and TLS.
//
// Every function here must be called while holding the LVGL lock.

#pragma once

#include <stddef.h>
#include <stdint.h>

#include "lvgl.h"

#ifdef __cplusplus
extern "C" {
#endif

// The sizes of a big number, largest first. The largest that fits is used.
#define UI_BIG_SIZE_COUNT 6

typedef struct {
    const lv_font_t *small;      // 16 px: ages, notes, details, the status line
    const lv_font_t *small_bold; // 16 px, semibold: the quality label
    const lv_font_t *body;       // 20 px
    const lv_font_t *body_bold;  // 20 px, semibold
    const lv_font_t *text;       // 24 px: a short text page
    const lv_font_t *title;      // 26 px, semibold
    const lv_font_t *big[UI_BIG_SIZE_COUNT]; // semibold, at ui_big_sizes[]
} ui_fonts_t;

extern const int32_t ui_big_sizes[UI_BIG_SIZE_COUNT];

// Makes the fonts. If one can't be made (out of memory), LVGL's built-in font stands in for it,
// so the screen still works.
void ui_fonts_start(void);

const ui_fonts_t *ui_fonts(void);

#ifdef __cplusplus
}
#endif
