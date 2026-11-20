// One page of a snapshot, drawn with LVGL. See ui_page.h.

#include "ui_page.h"

#include <string.h>

#include "ui_font.h"
#include "ui_text.h"

// A meter with more bars than this is drawn smaller, so six fit on the screen.
#define METER_ROOMY_BARS 3
// A text page of up to this many characters is drawn larger.
#define TEXT_LARGE_CHARS 100
// A list with more lines than this is drawn smaller, so eight fit on the screen.
#define LIST_ROOMY_LINES 5
// How much of a list's line its detail may take, in per cent.
#define LIST_DETAIL_PERCENT 45

// A plain container: no theme styles, not scrollable, and a swipe passes through it to the screen.
static lv_obj_t *box(lv_obj_t *parent, lv_flex_flow_t flow)
{
    lv_obj_t *obj = lv_obj_create(parent);
    lv_obj_remove_style_all(obj);
    lv_obj_set_size(obj, LV_PCT(100), LV_SIZE_CONTENT);
    lv_obj_set_flex_flow(obj, flow);
    lv_obj_set_scrollable(obj, false);
    lv_obj_set_clickable(obj, false);
    return obj;
}

// A label as wide as its parent, with wrapped text.
static lv_obj_t *text(lv_obj_t *parent, const lv_font_t *font, lv_color_t colour, const char *s)
{
    lv_obj_t *label = lv_label_create(parent);
    lv_obj_set_width(label, LV_PCT(100));
    lv_label_set_long_mode(label, LV_LABEL_LONG_MODE_WRAP);
    lv_obj_set_style_text_font(label, font, 0);
    lv_obj_set_style_text_color(label, colour, 0);
    lv_label_set_text(label, s);
    return label;
}

// The data quality, in a thin frame as the desktop app draws it.
static void quality_tag(lv_obj_t *parent, dh_quality quality)
{
    lv_obj_t *tag = lv_label_create(parent);
    lv_obj_set_style_text_font(tag, ui_fonts()->small_bold, 0);
    lv_obj_set_style_text_color(tag, UI_COLOUR_QUIET, 0);
    lv_obj_set_style_border_color(tag, UI_COLOUR_DIM, 0);
    lv_obj_set_style_border_width(tag, 1, 0);
    lv_obj_set_style_pad_hor(tag, 6, 0);
    lv_obj_set_style_pad_ver(tag, 2, 0);
    lv_label_set_text(tag, dh_quality_label(quality));
}

static lv_obj_t *figures_area(lv_obj_t *parent)
{
    lv_obj_t *area = box(parent, LV_FLEX_FLOW_COLUMN);
    lv_obj_set_flex_grow(area, 1);
    lv_obj_set_style_pad_row(area, 10, 0);
    // Up and down only, so a sideways swipe still moves between pages.
    lv_obj_set_scrollable(area, true);
    lv_obj_set_scroll_dir(area, LV_DIR_VER);
    lv_obj_set_scrollbar_mode(area, LV_SCROLLBAR_MODE_AUTO);
    lv_obj_set_style_bg_color(area, UI_COLOUR_DIM, LV_PART_SCROLLBAR);
    lv_obj_set_style_bg_opa(area, LV_OPA_COVER, LV_PART_SCROLLBAR);
    lv_obj_set_style_width(area, 3, LV_PART_SCROLLBAR);
    return area;
}

static void draw_meter(lv_obj_t *area, const dh_snapshot *s, const dh_page *p, bool saved,
                       int64_t held_seconds, ui_page_live_t *live)
{
    const ui_fonts_t *f = ui_fonts();
    const size_t count = p->content.u.meter.count;
    const bool roomy = count <= METER_ROOMY_BARS;
    const lv_font_t *font = roomy ? f->body : f->small;
    const lv_font_t *bold = roomy ? f->body_bold : f->small_bold;
    lv_obj_set_style_pad_row(area, roomy ? 16 : 6, 0);

    for (size_t i = 0; i < count && i < DH_MAX_METER_ITEMS; i++) {
        const dh_meter_item *item = &p->content.u.meter.items[i];
        lv_obj_t *one = box(area, LV_FLEX_FLOW_COLUMN);
        lv_obj_set_style_pad_row(one, roomy ? 6 : 3, 0);

        // The label, and the share at the end of the same line.
        lv_obj_t *row = box(one, LV_FLEX_FLOW_ROW);
        lv_obj_set_flex_align(row, LV_FLEX_ALIGN_SPACE_BETWEEN, LV_FLEX_ALIGN_START,
                              LV_FLEX_ALIGN_START);
        lv_obj_set_style_pad_column(row, 12, 0);
        lv_obj_t *label = text(row, font, UI_COLOUR_TEXT, item->label);
        lv_obj_set_width(label, 0);
        lv_obj_set_flex_grow(label, 1);
        char share[UI_TEXT_ROOM];
        ui_percent_text(item->has_percent, item->percent, share, sizeof(share));
        lv_obj_t *value = lv_label_create(row);
        lv_obj_set_style_text_font(value, bold, 0);
        lv_obj_set_style_text_color(value, item->has_percent ? UI_COLOUR_TEXT : UI_COLOUR_QUIET,
                                    0);
        lv_label_set_text(value, share);

        // The bar, and when it resets: under it when there is room, beside it when there are
        // many bars. An unknown share draws an empty bar, never one that reads as 0.
        lv_obj_t *under = one;
        if (!roomy) {
            under = box(one, LV_FLEX_FLOW_ROW);
            lv_obj_set_flex_align(under, LV_FLEX_ALIGN_START, LV_FLEX_ALIGN_CENTER,
                                  LV_FLEX_ALIGN_CENTER);
            lv_obj_set_style_pad_column(under, 10, 0);
        }
        const bool high = item->has_percent && item->percent >= UI_METER_HIGH_PERCENT;
        lv_obj_t *bar = lv_bar_create(under);
        lv_obj_remove_style_all(bar);
        lv_obj_set_size(bar, LV_PCT(100), roomy ? 12 : 8);
        if (!roomy) {
            lv_obj_set_width(bar, 0);
            lv_obj_set_flex_grow(bar, 1);
        }
        lv_obj_set_style_radius(bar, LV_RADIUS_CIRCLE, 0);
        lv_obj_set_style_bg_color(bar, UI_COLOUR_DIM, 0);
        lv_obj_set_style_bg_opa(bar, LV_OPA_COVER, 0);
        lv_obj_set_style_radius(bar, LV_RADIUS_CIRCLE, LV_PART_INDICATOR);
        lv_obj_set_style_bg_color(bar, high ? UI_COLOUR_WARNING : UI_COLOUR_TEXT,
                                  LV_PART_INDICATOR);
        lv_obj_set_style_bg_opa(bar, item->has_percent ? LV_OPA_COVER : LV_OPA_TRANSP,
                                LV_PART_INDICATOR);
        lv_obj_set_clickable(bar, false);
        lv_bar_set_range(bar, 0, 1000);
        lv_bar_set_value(bar, ui_percent_fill(item->has_percent, item->percent), LV_ANIM_OFF);

        // Kept up to date by ui_page_tick().
        if (item->has_resets_at) {
            char when[UI_TEXT_ROOM];
            ui_reset_text(s, item, saved, held_seconds, when, sizeof(when));
            lv_obj_t *reset = text(under, f->small, UI_COLOUR_QUIET, when);
            if (!roomy) {
                lv_obj_set_width(reset, LV_SIZE_CONTENT);
            }
            lv_obj_set_hidden(reset, when[0] == '\0');
            live->resets[live->reset_count++] = reset;
        }
    }
}

typedef struct {
    const char *text;
} measure_ctx;

static int32_t width_at(int32_t size, void *ctx)
{
    const measure_ctx *m = ctx;
    const lv_font_t *font = NULL;
    for (size_t i = 0; i < UI_BIG_SIZE_COUNT; i++) {
        if (ui_big_sizes[i] == size) {
            font = ui_fonts()->big[i];
        }
    }
    lv_point_t extent;
    lv_text_get_size(&extent, m->text, font, 0, 0, LV_COORD_MAX, LV_TEXT_FLAG_EXPAND);
    return extent.x;
}

static void draw_big_number(lv_obj_t *area, const dh_page *p)
{
    const ui_fonts_t *f = ui_fonts();
    const dh_big_number *b = &p->content.u.big_number;
    // Centred in the space there is, which the figure fills best.
    lv_obj_set_flex_align(area, LV_FLEX_ALIGN_CENTER, LV_FLEX_ALIGN_START, LV_FLEX_ALIGN_START);
    lv_obj_set_style_pad_row(area, 4, 0);

    char figure[UI_BIG_NUMBER_ROOM];
    ui_big_number_text(b, figure, sizeof(figure));
    // The largest size at which the figure and its unit fit on one line. If even the smallest
    // is too wide, the label wraps it onto a second line rather than cut any of it off.
    lv_obj_update_layout(area);
    const int32_t room = lv_obj_get_content_width(area);
    measure_ctx m = {figure};
    const size_t at = ui_pick_size(ui_big_sizes, UI_BIG_SIZE_COUNT, room, width_at, &m);
    text(area, f->big[at], b->has_value ? UI_COLOUR_TEXT : UI_COLOUR_QUIET, figure);
    if (b->has_caption) {
        text(area, f->body, UI_COLOUR_QUIET, b->caption);
    }
}

static void draw_text(lv_obj_t *area, const dh_page *p)
{
    const char *words = p->content.u.text.text;
    const size_t chars = dh_utf8_count((const uint8_t *)words, strlen(words));
    const ui_fonts_t *f = ui_fonts();
    lv_obj_t *label =
        text(area, chars <= TEXT_LARGE_CHARS ? f->text : f->body, UI_COLOUR_TEXT, words);
    lv_obj_set_style_text_line_space(label, 4, 0);
}

static void draw_list(lv_obj_t *area, const dh_page *p)
{
    const ui_fonts_t *f = ui_fonts();
    const size_t count = p->content.u.list.count;
    if (count == 0) {
        text(area, f->body, UI_COLOUR_QUIET, "Nothing listed");
        return;
    }
    const bool roomy = count <= LIST_ROOMY_LINES;
    const lv_font_t *font = roomy ? f->body : f->small;
    lv_obj_set_style_pad_row(area, roomy ? 10 : 7, 0);
    lv_obj_update_layout(area);
    const int32_t most = lv_obj_get_content_width(area) * LIST_DETAIL_PERCENT / 100;
    for (size_t i = 0; i < count && i < DH_MAX_LIST_ITEMS; i++) {
        const dh_list_item *item = &p->content.u.list.items[i];
        // The line, with its detail at the end of it in quieter text. Either wraps if need be.
        lv_obj_t *row = box(area, LV_FLEX_FLOW_ROW);
        lv_obj_set_flex_align(row, LV_FLEX_ALIGN_SPACE_BETWEEN, LV_FLEX_ALIGN_START,
                              LV_FLEX_ALIGN_START);
        lv_obj_set_style_pad_column(row, 12, 0);
        lv_obj_t *line = text(row, font, UI_COLOUR_TEXT, item->text);
        lv_obj_set_width(line, 0);
        lv_obj_set_flex_grow(line, 1);
        if (item->has_detail) {
            // As wide as it needs, up to its share of the line; past that it wraps.
            lv_point_t extent;
            lv_text_get_size(&extent, item->detail, font, 0, 0, LV_COORD_MAX,
                             LV_TEXT_FLAG_EXPAND);
            lv_obj_t *detail = text(row, font, UI_COLOUR_QUIET, item->detail);
            lv_obj_set_width(detail, extent.x + 1 < most ? extent.x + 1 : most);
            lv_obj_set_style_text_align(detail, LV_TEXT_ALIGN_RIGHT, 0);
        }
    }
}

void ui_page_draw(lv_obj_t *parent, const dh_snapshot *s, const dh_page *p, bool saved,
                  int64_t held_seconds, ui_page_live_t *live)
{
    const ui_fonts_t *f = ui_fonts();
    memset(live, 0, sizeof(*live));
    const ui_form form = ui_form_of(p);

    // A card says nothing about the page but its title and the card's lines.
    if (form != UI_FORM_CARD) {
        quality_tag(parent, p->quality);
    }
    text(parent, f->title, UI_COLOUR_TEXT, p->title);
    lv_obj_t *area = figures_area(parent);
    live->figures = area;

    switch (form) {
    case UI_FORM_CARD: {
        const char *lines[2];
        const size_t n = ui_card_lines(p, lines);
        for (size_t i = 0; i < n; i++) {
            text(area, f->body, i == 0 ? UI_COLOUR_TEXT : UI_COLOUR_QUIET, lines[i]);
        }
        return;
    }
    case UI_FORM_NO_FIGURES:
        // Its note, and nothing about figures or their age.
        text(area, f->body, UI_COLOUR_TEXT, ui_no_figures_text(p));
        return;
    case UI_FORM_FIGURES:
        break;
    }

    switch (p->content.shape) {
    case DH_SHAPE_METER:
        draw_meter(area, s, p, saved, held_seconds, live);
        break;
    case DH_SHAPE_BIG_NUMBER:
        draw_big_number(area, p);
        break;
    case DH_SHAPE_TEXT:
        draw_text(area, p);
        break;
    case DH_SHAPE_LIST:
        draw_list(area, p);
        break;
    default:
        break;
    }
    if (p->has_note) {
        text(parent, f->small, UI_COLOUR_QUIET, p->note);
    }
    live->age = text(parent, f->small, UI_COLOUR_QUIET, "");
    ui_page_tick(live, s, p, saved, held_seconds);
}

void ui_page_tick(const ui_page_live_t *live, const dh_snapshot *s, const dh_page *p, bool saved,
                  int64_t held_seconds)
{
    char words[UI_TEXT_ROOM];
    if (live->age != NULL) {
        const ui_age age = ui_age_text(s, p, saved, held_seconds, words, sizeof(words));
        lv_label_set_text(live->age, words);
        lv_obj_set_style_text_color(live->age,
                                    age == UI_AGE_STALE ? UI_COLOUR_WARNING : UI_COLOUR_QUIET, 0);
    }
    if (p->content.shape != DH_SHAPE_METER) {
        return;
    }
    size_t at = 0;
    for (size_t i = 0; i < p->content.u.meter.count && at < live->reset_count; i++) {
        const dh_meter_item *item = &p->content.u.meter.items[i];
        if (!item->has_resets_at) {
            continue;
        }
        ui_reset_text(s, item, saved, held_seconds, words, sizeof(words));
        lv_label_set_text(live->resets[at], words);
        lv_obj_set_hidden(live->resets[at], words[0] == '\0');
        at++;
    }
}

void ui_no_pages_draw(lv_obj_t *parent)
{
    const ui_fonts_t *f = ui_fonts();
    text(parent, f->title, UI_COLOUR_TEXT, "No pages yet");
    lv_obj_t *area = figures_area(parent);
    text(area, f->body, UI_COLOUR_TEXT, "Choose this display's pages in Display Hub.");
}
