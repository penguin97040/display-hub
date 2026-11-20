// The screens' fonts, and LVGL's memory. See ui_font.h.

#include "ui_font.h"

#include <string.h>

#include "esp_heap_caps.h"
#include "esp_log.h"

static const char *TAG = "ui";

// Built into the app by main/CMakeLists.txt (EMBED_FILES), from assets/fonts/.
extern const uint8_t _binary_Geist_Regular_ttf_start[];
extern const uint8_t _binary_Geist_Regular_ttf_end[];
extern const uint8_t _binary_Geist_SemiBold_ttf_start[];
extern const uint8_t _binary_Geist_SemiBold_ttf_end[];

const int32_t ui_big_sizes[UI_BIG_SIZE_COUNT] = {88, 72, 60, 48, 38, 30};

// How many glyphs each size keeps drawn. Text sizes meet a good part of an alphabet; a big
// number only digits, a sign, a point and a short unit.
#define TEXT_GLYPHS 96
#define BIG_GLYPHS  24

static ui_fonts_t s_fonts;

static const lv_font_t *make(bool semibold, int32_t size, size_t glyphs)
{
    const uint8_t *start = semibold ? _binary_Geist_SemiBold_ttf_start
                                    : _binary_Geist_Regular_ttf_start;
    const uint8_t *end = semibold ? _binary_Geist_SemiBold_ttf_end : _binary_Geist_Regular_ttf_end;
    lv_font_t *font = lv_tiny_ttf_create_data_ex(start, (size_t)(end - start), size,
                                                 LV_FONT_KERNING_NORMAL, glyphs);
    if (font == NULL) {
        ESP_LOGE(TAG, "the %s font at %d px couldn't be made; using the built-in font",
                 semibold ? "semibold" : "regular", (int)size);
        return LV_FONT_DEFAULT;
    }
    return font;
}

void ui_fonts_start(void)
{
    s_fonts.small = make(false, 16, TEXT_GLYPHS);
    s_fonts.small_bold = make(true, 16, TEXT_GLYPHS);
    s_fonts.body = make(false, 20, TEXT_GLYPHS);
    s_fonts.body_bold = make(true, 20, TEXT_GLYPHS);
    s_fonts.text = make(false, 24, TEXT_GLYPHS);
    s_fonts.title = make(true, 26, TEXT_GLYPHS);
    for (size_t i = 0; i < UI_BIG_SIZE_COUNT; i++) {
        s_fonts.big[i] = make(true, ui_big_sizes[i], BIG_GLYPHS);
    }
}

const ui_fonts_t *ui_fonts(void)
{
    return &s_fonts;
}

// ---- LVGL's memory (LV_USE_CUSTOM_MALLOC) ----
//
// PSRAM first, then any memory. LVGL's own draw buffers for the screen don't come from here:
// esp_lvgl_port takes those from DMA-capable internal memory (board.c).

#define FIRST_CAPS (MALLOC_CAP_SPIRAM | MALLOC_CAP_8BIT)

void lv_mem_init(void)
{
}

void lv_mem_deinit(void)
{
}

lv_mem_pool_t lv_mem_add_pool(void *mem, size_t bytes)
{
    (void)mem;
    (void)bytes;
    return NULL;
}

void lv_mem_remove_pool(lv_mem_pool_t pool)
{
    (void)pool;
}

void *lv_malloc_core(size_t size)
{
    void *p = heap_caps_malloc(size, FIRST_CAPS);
    return p != NULL ? p : heap_caps_malloc(size, MALLOC_CAP_DEFAULT);
}

void *lv_realloc_core(void *p, size_t new_size)
{
    void *q = heap_caps_realloc(p, new_size, FIRST_CAPS);
    return q != NULL ? q : heap_caps_realloc(p, new_size, MALLOC_CAP_DEFAULT);
}

void lv_free_core(void *p)
{
    heap_caps_free(p);
}

void lv_mem_monitor_core(lv_mem_monitor_t *mon_p)
{
    memset(mon_p, 0, sizeof(*mon_p));
    mon_p->total_size = heap_caps_get_total_size(FIRST_CAPS);
    mon_p->free_size = heap_caps_get_free_size(FIRST_CAPS);
    mon_p->free_biggest_size = heap_caps_get_largest_free_block(FIRST_CAPS);
}

lv_result_t lv_mem_test_core(void)
{
    return heap_caps_check_integrity_all(true) ? LV_RESULT_OK : LV_RESULT_INVALID;
}
