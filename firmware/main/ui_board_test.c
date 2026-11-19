// The check build's screens. See ui_board_test.h.
//
// White text on black throughout: an AMOLED pixel that is black is off, so this is easy on the
// screen and on power. Everything here runs in LVGL's task (or under its lock).

#include "ui_board_test.h"

#include <inttypes.h>

#include "esp_log.h"
#include "lvgl.h"
#include "ui_parts.h"
#include "ui_storage_key.h"

static const char *TAG = "board-test";

#define PAGE_COUNT    4
#define PAGE_PAD_SIDE 16
#define PAGE_PAD_TOP  18
#define DOT_SIZE      36

#define COLOUR_TEXT    UI_COLOUR_TEXT
#define COLOUR_QUIET   UI_COLOUR_QUIET
#define COLOUR_WARNING UI_COLOUR_WARNING

#define PAGE_EFUSE       2
#define PAGE_STORAGE_KEY 3

static lv_obj_t *s_pages[PAGE_COUNT];
static lv_obj_t *s_page_dots[PAGE_COUNT];
static int s_page;

static lv_obj_t *s_psram_label;
static lv_obj_t *s_heap_label;

static lv_obj_t *s_touch_dot;
static lv_obj_t *s_touch_point_label;
static lv_obj_t *s_swipe_label;

// Read by the console log from another task. A pointer to a string constant, so a read never
// sees half a value.
static const char *volatile s_last_swipe = "none";

const char *ui_board_test_last_swipe(void)
{
    return s_last_swipe;
}

lv_obj_t *ui_add_label(lv_obj_t *parent, const lv_font_t *font, lv_color_t colour,
                       const char *text)
{
    lv_obj_t *label = lv_label_create(parent);
    lv_obj_set_width(label, LV_PCT(100));
    lv_label_set_long_mode(label, LV_LABEL_LONG_MODE_WRAP);
    lv_obj_set_style_text_font(label, font, 0);
    lv_obj_set_style_text_color(label, colour, 0);
    lv_label_set_text(label, text);
    return label;
}

// A full-screen page: a plain column with no frame, background or scrolling of its own unless
// asked. Swipes reach the screen only from objects that don't scroll sideways.
static lv_obj_t *add_page(lv_obj_t *screen, bool scroll_up_down)
{
    lv_obj_t *page = lv_obj_create(screen);
    lv_obj_remove_style_all(page);
    lv_obj_set_size(page, LV_PCT(100), LV_PCT(100));
    lv_obj_set_flex_flow(page, LV_FLEX_FLOW_COLUMN);
    lv_obj_set_style_pad_hor(page, PAGE_PAD_SIDE, 0);
    lv_obj_set_style_pad_top(page, PAGE_PAD_TOP, 0);
    // Room at the bottom for the page dots.
    lv_obj_set_style_pad_bottom(page, 36, 0);
    lv_obj_set_style_pad_row(page, 4, 0);
    if (scroll_up_down) {
        lv_obj_set_scroll_dir(page, LV_DIR_VER);
        lv_obj_set_scrollbar_mode(page, LV_SCROLLBAR_MODE_OFF);
    } else {
        lv_obj_set_scrollable(page, false);
    }
    return page;
}

void ui_add_gap(lv_obj_t *page, int height)
{
    lv_obj_t *gap = lv_obj_create(page);
    lv_obj_remove_style_all(gap);
    lv_obj_set_size(gap, 1, height);
    lv_obj_set_clickable(gap, false);
}

static void show_page(int page)
{
    s_page = page;
    for (int i = 0; i < PAGE_COUNT; i++) {
        lv_obj_set_hidden(s_pages[i], i != page);
        lv_obj_set_style_bg_opa(s_page_dots[i], i == page ? LV_OPA_COVER : LV_OPA_TRANSP, 0);
    }
    ui_storage_key_page_shown(page == PAGE_STORAGE_KEY);
}

static void on_gesture(lv_event_t *e)
{
    (void)e;
    lv_indev_t *indev = lv_indev_active();
    if (indev == NULL) {
        return;
    }
    // A touch that turned into a swipe must never count as a tap on a button.
    ui_storage_key_swipe_seen();
    const lv_dir_t dir = lv_indev_get_gesture_dir(indev);
    switch (dir) {
    case LV_DIR_LEFT:
        s_last_swipe = "left";
        break;
    case LV_DIR_RIGHT:
        s_last_swipe = "right";
        break;
    case LV_DIR_TOP:
        s_last_swipe = "up";
        break;
    case LV_DIR_BOTTOM:
        s_last_swipe = "down";
        break;
    default:
        return;
    }
    lv_label_set_text_fmt(s_swipe_label, "Last swipe: %s", s_last_swipe);
    ESP_LOGI(TAG, "swipe %s", s_last_swipe);

    if (dir == LV_DIR_LEFT || dir == LV_DIR_RIGHT) {
        const int step = dir == LV_DIR_LEFT ? 1 : PAGE_COUNT - 1;
        show_page((s_page + step) % PAGE_COUNT);
        // Ignore the rest of this touch, so it doesn't land on the new page.
        lv_indev_wait_release(indev);
    }
}

static void on_touch_page_pressing(lv_event_t *e)
{
    (void)e;
    lv_indev_t *indev = lv_indev_active();
    if (indev == NULL) {
        return;
    }
    lv_point_t point;
    lv_indev_get_point(indev, &point);
    lv_obj_set_hidden(s_touch_dot, false);
    // The dot's position is measured from inside the page's padding; the point is on the screen.
    lv_obj_set_pos(s_touch_dot, point.x - PAGE_PAD_SIDE - DOT_SIZE / 2,
                   point.y - PAGE_PAD_TOP - DOT_SIZE / 2);
    lv_label_set_text_fmt(s_touch_point_label, "Touch at x %d, y %d", (int)point.x, (int)point.y);
}

static void update_memory(lv_timer_t *timer)
{
    (void)timer;
    memory_report_t mem;
    memory_report_read(&mem);
    lv_label_set_text_fmt(s_psram_label, "PSRAM free: %" PRIu32 " KB", mem.psram_free / 1024);
    lv_label_set_text_fmt(s_heap_label, "Heap free: %" PRIu32 " KB", mem.internal_free / 1024);
}

static void build_about_page(lv_obj_t *page, const board_t *board, const system_report_t *system)
{
    ui_add_label(page, &lv_font_montserrat_36, COLOUR_TEXT, "Display Hub");
    ui_add_label(page, &lv_font_montserrat_24, COLOUR_QUIET, "Storage key check");
    ui_add_gap(page, 10);

    lv_label_set_text_fmt(ui_add_label(page, &lv_font_montserrat_24, COLOUR_TEXT, ""),
                          "Firmware %s", system->version);
    if (system->running_known) {
        lv_label_set_text_fmt(ui_add_label(page, &lv_font_montserrat_24, COLOUR_TEXT, ""),
                              "Slot %s at 0x%" PRIx32, system->running_label,
                              system->running_address);
    } else {
        ui_add_label(page, &lv_font_montserrat_24, COLOUR_WARNING, "Slot: unknown");
    }
    lv_label_set_text_fmt(ui_add_label(page, &lv_font_montserrat_20, COLOUR_QUIET, ""),
                          "Boot record: %s", system->boot_record);

    switch (board->revision) {
    case BOARD_REVISION_V2:
        ui_add_label(page, &lv_font_montserrat_24, COLOUR_TEXT, "Board: V2");
        break;
    case BOARD_REVISION_V1:
        ui_add_label(page, &lv_font_montserrat_24, COLOUR_WARNING, "V1 board: not supported yet");
        break;
    default:
        ui_add_label(page, &lv_font_montserrat_24, COLOUR_WARNING, "Board: not recognised");
        break;
    }

    if (board->power.found) {
        ui_add_label(page, &lv_font_montserrat_24, COLOUR_TEXT,
                  board->power.usb_power_good ? "Power: USB good" : "Power: no USB power");
    } else {
        ui_add_label(page, &lv_font_montserrat_24, COLOUR_WARNING, "Power chip: not found");
    }
    if (!board->expander_found) {
        ui_add_label(page, &lv_font_montserrat_20, COLOUR_WARNING, "I/O expander: not found");
    }

    s_psram_label = ui_add_label(page, &lv_font_montserrat_24, COLOUR_TEXT, "");
    s_heap_label = ui_add_label(page, &lv_font_montserrat_24, COLOUR_TEXT, "");
    update_memory(NULL);
}

static void build_touch_page(lv_obj_t *page, const board_t *board)
{
    ui_add_label(page, &lv_font_montserrat_36, COLOUR_TEXT, "Touch test");
    if (board->touch_ready) {
        ui_add_label(page, &lv_font_montserrat_20, COLOUR_QUIET,
                  "Touch and drag: the dot follows your finger.");
        ui_add_label(page, &lv_font_montserrat_20, COLOUR_QUIET,
                  "Swipe up or down to test a swipe. Swipe left or right for the next screen.");
    } else {
        ui_add_label(page, &lv_font_montserrat_24, COLOUR_WARNING, "Touch isn't working.");
        ui_add_label(page, &lv_font_montserrat_20, COLOUR_QUIET,
                  "No touch chip answered, or it failed to start.");
    }
    ui_add_gap(page, 10);
    s_swipe_label = ui_add_label(page, &lv_font_montserrat_24, COLOUR_TEXT, "Last swipe: none");
    s_touch_point_label = ui_add_label(page, &lv_font_montserrat_20, COLOUR_QUIET, "Touch at: -");

    s_touch_dot = lv_obj_create(page);
    lv_obj_remove_style_all(s_touch_dot);
    lv_obj_set_ignore_layout(s_touch_dot, true);
    lv_obj_set_hidden(s_touch_dot, true);
    lv_obj_set_clickable(s_touch_dot, false);
    lv_obj_set_size(s_touch_dot, DOT_SIZE, DOT_SIZE);
    lv_obj_set_style_radius(s_touch_dot, LV_RADIUS_CIRCLE, 0);
    lv_obj_set_style_bg_color(s_touch_dot, COLOUR_TEXT, 0);
    lv_obj_set_style_bg_opa(s_touch_dot, LV_OPA_COVER, 0);

    lv_obj_add_event_cb(page, on_touch_page_pressing, LV_EVENT_PRESSING, NULL);
}

static void build_efuse_page(lv_obj_t *page, const efuse_report_t *efuse)
{
    ui_add_label(page, &lv_font_montserrat_24, COLOUR_TEXT, "eFuse keys");
    ui_add_label(page, &lv_font_montserrat_16, COLOUR_QUIET,
                 "This screen only reads. The next screen is the one that can burn a key.");
    ui_add_gap(page, 4);

    for (int i = 0; i < EFUSE_REPORT_KEYS; i++) {
        const efuse_key_report_t *key = &efuse->keys[i];
        lv_label_set_text_fmt(ui_add_label(page, &lv_font_montserrat_20, COLOUR_TEXT, ""),
                              "KEY%d  %s, %s", i, efuse_purpose_name(key->purpose),
                              key->unused ? "empty" : "in use");
        lv_label_set_text_fmt(ui_add_label(page, &lv_font_montserrat_16, COLOUR_QUIET, ""),
                              "read-protected: %s  write-protected: %s",
                              key->read_protected ? "yes" : "no",
                              key->write_protected ? "yes" : "no");
    }

    ui_add_gap(page, 4);
    lv_label_set_text_fmt(ui_add_label(page, &lv_font_montserrat_16, COLOUR_TEXT, ""),
                          "Download mode: %s. Secure download mode: %s.",
                          efuse->download_mode_disabled ? "disabled" : "allowed",
                          efuse->secure_download_mode ? "on" : "off");
    lv_label_set_text_fmt(ui_add_label(page, &lv_font_montserrat_16, COLOUR_TEXT, ""),
                          "USB-Serial-JTAG: %s. USB download: %s.",
                          efuse->usb_serial_jtag_disabled ? "disabled" : "allowed",
                          efuse->usb_download_disabled ? "disabled" : "allowed");
    lv_label_set_text_fmt(ui_add_label(page, &lv_font_montserrat_16, COLOUR_TEXT, ""),
                          "Secure Boot: %s. Flash encryption: %s.",
                          efuse->secure_boot_on ? "on" : "off",
                          efuse->flash_encryption_on ? "on" : "off");
}

static void build_page_dots(lv_obj_t *screen)
{
    lv_obj_t *row = lv_obj_create(screen);
    lv_obj_remove_style_all(row);
    lv_obj_set_size(row, LV_SIZE_CONTENT, LV_SIZE_CONTENT);
    lv_obj_set_flex_flow(row, LV_FLEX_FLOW_ROW);
    lv_obj_set_style_pad_column(row, 12, 0);
    lv_obj_set_clickable(row, false);
    lv_obj_set_scrollable(row, false);
    lv_obj_align(row, LV_ALIGN_BOTTOM_MID, 0, -12);

    for (int i = 0; i < PAGE_COUNT; i++) {
        lv_obj_t *dot = lv_obj_create(row);
        lv_obj_remove_style_all(dot);
        lv_obj_set_size(dot, 12, 12);
        lv_obj_set_style_radius(dot, LV_RADIUS_CIRCLE, 0);
        lv_obj_set_style_border_width(dot, 2, 0);
        lv_obj_set_style_border_color(dot, COLOUR_TEXT, 0);
        lv_obj_set_style_bg_color(dot, COLOUR_TEXT, 0);
        lv_obj_set_clickable(dot, false);
        s_page_dots[i] = dot;
    }
}

void ui_board_test_start(const board_t *board, const system_report_t *system,
                         const efuse_report_t *efuse)
{
    lv_obj_t *screen = lv_screen_active();
    lv_obj_set_style_bg_color(screen, lv_color_black(), 0);
    lv_obj_set_style_bg_opa(screen, LV_OPA_COVER, 0);
    lv_obj_set_style_text_color(screen, COLOUR_TEXT, 0);
    lv_obj_set_scrollable(screen, false);

    s_pages[0] = add_page(screen, true);
    s_pages[1] = add_page(screen, false);
    s_pages[PAGE_EFUSE] = add_page(screen, true);
    s_pages[PAGE_STORAGE_KEY] = add_page(screen, false);
    build_about_page(s_pages[0], board, system);
    build_touch_page(s_pages[1], board);
    build_efuse_page(s_pages[PAGE_EFUSE], efuse);
    ui_storage_key_build(s_pages[PAGE_STORAGE_KEY]);
    build_page_dots(screen);
    show_page(0);

    lv_obj_add_event_cb(screen, on_gesture, LV_EVENT_GESTURE, NULL);
    lv_timer_create(update_memory, 2000, NULL);
}

void ui_board_test_refresh_efuse(const efuse_report_t *efuse)
{
    lv_obj_clean(s_pages[PAGE_EFUSE]);
    build_efuse_page(s_pages[PAGE_EFUSE], efuse);
}
