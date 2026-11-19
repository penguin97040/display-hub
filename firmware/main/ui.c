// The screens of this build. See ui.h.

#include "ui.h"

#include <stdio.h>
#include <string.h>

#include "lvgl.h"

#define COLOUR_TEXT    lv_color_white()
#define COLOUR_QUIET   lv_color_hex(0xA0A0A0)
#define COLOUR_WARNING lv_color_hex(0xFFB000)
#define COLOUR_GOOD    lv_color_hex(0x40D060)
#define COLOUR_BAD     lv_color_hex(0xFF5040)

#define PAD_SIDE 16
#define PAD_TOP  22

static lv_obj_t *s_title;
static lv_obj_t *s_number;
static lv_obj_t *s_body;
static lv_obj_t *s_note;
static lv_obj_t *s_footer;

// A label as wide as its parent, with wrapped text.
static lv_obj_t *add_label(lv_obj_t *parent, const lv_font_t *font, lv_color_t colour)
{
    lv_obj_t *label = lv_label_create(parent);
    lv_obj_set_width(label, LV_PCT(100));
    lv_label_set_long_mode(label, LV_LABEL_LONG_MODE_WRAP);
    lv_obj_set_style_text_font(label, font, 0);
    lv_obj_set_style_text_color(label, colour, 0);
    lv_label_set_text(label, "");
    return label;
}

void ui_start(const char *version, const char *board_note)
{
    lv_obj_t *screen = lv_screen_active();
    lv_obj_set_style_bg_color(screen, lv_color_black(), 0);
    lv_obj_set_style_bg_opa(screen, LV_OPA_COVER, 0);
    lv_obj_set_style_text_color(screen, COLOUR_TEXT, 0);
    lv_obj_set_scrollable(screen, false);

    // One plain column, which scrolls up and down if a message is longer than the screen.
    lv_obj_t *page = lv_obj_create(screen);
    lv_obj_remove_style_all(page);
    lv_obj_set_size(page, LV_PCT(100), LV_PCT(100));
    lv_obj_set_flex_flow(page, LV_FLEX_FLOW_COLUMN);
    lv_obj_set_style_pad_hor(page, PAD_SIDE, 0);
    lv_obj_set_style_pad_top(page, PAD_TOP, 0);
    lv_obj_set_style_pad_bottom(page, PAD_TOP, 0);
    lv_obj_set_style_pad_row(page, 14, 0);
    lv_obj_set_scroll_dir(page, LV_DIR_VER);
    lv_obj_set_scrollbar_mode(page, LV_SCROLLBAR_MODE_OFF);

    s_title = add_label(page, &lv_font_montserrat_24, COLOUR_TEXT);
    s_number = add_label(page, &lv_font_montserrat_36, COLOUR_WARNING);
    s_body = add_label(page, &lv_font_montserrat_20, COLOUR_TEXT);
    s_note = add_label(page, &lv_font_montserrat_20, COLOUR_WARNING);
    s_footer = add_label(page, &lv_font_montserrat_16, COLOUR_QUIET);

    if (board_note != NULL) {
        lv_label_set_text_fmt(s_footer, "Display Hub firmware %s\n%s", version, board_note);
    } else {
        lv_label_set_text_fmt(s_footer, "Display Hub firmware %s", version);
    }
}

static void show(const char *title, lv_color_t title_colour, const char *number, const char *body,
                 const char *note)
{
    lv_label_set_text(s_title, title);
    lv_obj_set_style_text_color(s_title, title_colour, 0);
    lv_label_set_text(s_number, number != NULL ? number : "");
    lv_obj_set_hidden(s_number, number == NULL);
    lv_label_set_text(s_body, body);
    lv_label_set_text(s_note, note != NULL ? note : "");
    lv_obj_set_hidden(s_note, note == NULL);
}

void ui_show_state(const storage_facts_t *facts, const storage_key_status_t *key,
                   const char *note)
{
    static char body[640];

    if (facts->state == STORAGE_NO_KEY) {
        // Say why, in the storage key check's own plain words.
        size_t at = (size_t)snprintf(body, sizeof(body),
                                     "This display can't keep its setup safely, so it can't be "
                                     "set up.\n");
        for (int i = 0; i < key->blocker_count && at < sizeof(body); i++) {
            at += (size_t)snprintf(body + at, sizeof(body) - at, "\n%s", key->blockers[i]);
        }
        show("Storage problem", COLOUR_BAD, NULL, body, note);
        return;
    }
    if (facts->state != STORAGE_WORKING) {
        show("Storage problem", COLOUR_BAD, NULL,
             "This display's storage didn't start or couldn't be read, so it can't be set up.\n\n"
             "Open Display Hub on your computer and use Install firmware to install it again. "
             "That erases the display's storage.",
             note);
        return;
    }
    if (facts->setup_unreadable) {
        show("Setup can't be read", COLOUR_WARNING, NULL,
             "This display holds a setup that this firmware can't read.\n\n"
             "Hold the BOOT button for 10 seconds to erase it, then set the display up again.",
             note);
        return;
    }
    if (facts->set_up) {
        snprintf(body, sizeof(body),
                 "This display has its setup: %u hub %s, and %s.\n\n"
                 "It can't join the hub yet. That comes with a later version of this "
                 "firmware.\n\n"
                 "To erase the setup and start again, hold the BOOT button for 10 seconds.",
                 (unsigned)facts->hub_hosts, facts->hub_hosts == 1 ? "address" : "addresses",
                 facts->wifi_set ? "its Wi-Fi details" : "no Wi-Fi details");
        show("Set up", COLOUR_GOOD, NULL, body, note);
        return;
    }
    show("Waiting for setup", COLOUR_TEXT, NULL,
         "Open Display Hub on your computer and choose Add a display.\n\n"
         "Keep this display plugged in to the computer with its USB cable.",
         note);
}

void ui_show_reset_countdown(int seconds_left)
{
    char number[16];
    snprintf(number, sizeof(number), "%d", seconds_left);
    show("Factory reset", COLOUR_WARNING, number,
         "Keep holding BOOT to erase this display's setup.\n\nLet go now to cancel.", NULL);
}
