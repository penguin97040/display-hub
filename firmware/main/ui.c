// The screens of this build. See ui.h.

#include "ui.h"

#include "dh_poll.h"

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
static lv_obj_t *s_pages;
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
    s_pages = add_label(page, &lv_font_montserrat_16, COLOUR_TEXT);
    s_note = add_label(page, &lv_font_montserrat_20, COLOUR_WARNING);
    s_footer = add_label(page, &lv_font_montserrat_16, COLOUR_QUIET);

    if (board_note != NULL) {
        lv_label_set_text_fmt(s_footer, "Display Hub firmware %s\n%s", version, board_note);
    } else {
        lv_label_set_text_fmt(s_footer, "Display Hub firmware %s", version);
    }
}

// `pages` is NULL or the list of pages, shown under the body in smaller text.
static void show(const char *title, lv_color_t title_colour, const char *number, const char *body,
                 const char *pages, const char *note)
{
    lv_label_set_text(s_title, title);
    lv_obj_set_style_text_color(s_title, title_colour, 0);
    lv_label_set_text(s_number, number != NULL ? number : "");
    lv_obj_set_hidden(s_number, number == NULL);
    lv_label_set_text(s_body, body);
    lv_label_set_text(s_pages, pages != NULL ? pages : "");
    lv_obj_set_hidden(s_pages, pages == NULL);
    lv_label_set_text(s_note, note != NULL ? note : "");
    lv_obj_set_hidden(s_note, note == NULL);
}

#define RESET_HOW "hold the BOOT button for 10 seconds to erase its setup, then choose Add a " \
                  "display in Display Hub on your computer."
#define RETRYING  "It keeps trying by itself."

// The headline for a problem: dh_problem_headline()'s, except when the trouble is Wi-Fi, which
// is kept apart from what the hub last said.
static const char *headline(const hub_status_t *hub)
{
    switch (hub->detail) {
    case HUB_DETAIL_WIFI:
    case HUB_DETAIL_WIFI_START:
    case HUB_DETAIL_NO_WIFI_DETAILS:
        return "Can't join Wi-Fi";
    default:
        return dh_problem_headline(hub->problem);
    }
}

// What to do about a problem, in plain words, under headline()'s headline.
static const char *advice(const hub_status_t *hub)
{
    switch (hub->detail) {
    case HUB_DETAIL_WIFI:
        return "This display can't join the Wi-Fi network. It uses 2.4 GHz Wi-Fi only, so check "
               "the network offers 2.4 GHz and is in range. " RETRYING "\n\n"
               "If the network's name or password has changed, " RESET_HOW;
    case HUB_DETAIL_WIFI_START:
        return "This display's Wi-Fi didn't start. " RETRYING "\n\n"
               "If this doesn't clear, unplug the display and plug it back in.";
    case HUB_DETAIL_NO_WIFI_DETAILS:
        return "It was set up without Wi-Fi details, so it can't join a network.\n\n"
               "To set it up again, " RESET_HOW;
    case HUB_DETAIL_NO_ANCHOR:
        return "The hub's certificate in its setup can't be read.\n\n"
               "To set it up again, " RESET_HOW;
    case HUB_DETAIL_ENROL_REFUSED:
        return "The hub didn't accept this display's setup code: it was more than 10 minutes "
               "old, or had been used already.\n\n"
               "To start again, " RESET_HOW;
    case HUB_DETAIL_NO_TOKEN:
        return "Its setup is incomplete.\n\nTo start again, " RESET_HOW;
    case HUB_DETAIL_REVOKED:
        return "The hub no longer accepts this display. It may have been removed in Display "
               "Hub.\n\n"
               "To add it again, " RESET_HOW;
    default:
        break;
    }
    switch (hub->problem) {
    case DH_PROBLEM_UNREACHABLE:
        return "Check the hub (the Raspberry Pi) is switched on and on the same network as this "
               "display. " RETRYING;
    case DH_PROBLEM_WRONG_HUB:
        return "Its security key doesn't match, so nothing was sent to it. " RETRYING "\n\n"
               "If you have reinstalled the hub, " RESET_HOW " Otherwise, look for a device "
               "you don't know on your network.";
    case DH_PROBLEM_UNREADABLE:
    case DH_PROBLEM_BUSY:
        return RETRYING;
    case DH_PROBLEM_INCOMPATIBLE:
        return "The hub and this display's firmware are different versions that can't work "
               "together. Update the one that is older.";
    case DH_PROBLEM_NEEDS_SETUP:
        return "To set it up again, " RESET_HOW;
    case DH_PROBLEM_SETTINGS:
        // With no detail, this is only a request to the hub that couldn't be made (storage
        // trouble, say), not a bad setup: an hourly retry may clear it, so don't send the
        // owner to erase a setup that is probably fine.
        return "It couldn't prepare its request to the hub. " RETRYING "\n\n"
               "If this doesn't clear, unplug the display and plug it back in.";
    default:
        return "";
    }
}

// The pages the hub sent, by title: drawing them comes later.
static const char *page_list(const hub_status_t *hub)
{
    // Ten titles of up to 48 characters of up to four bytes, with their labels.
    static char text[160 + DH_MAX_PAGES * (DH_UTF8_ROOM(DH_MAX_TITLE_CHARS) + 48)];
    if (!hub->has_snapshot) {
        return NULL;
    }
    size_t at = 0;
    if (hub->page_count == 0) {
        at = (size_t)snprintf(text, sizeof(text),
                              "No pages yet. Choose this display's pages in Display Hub.");
    } else {
        at = (size_t)snprintf(text, sizeof(text), "%u %s from the hub:", (unsigned)hub->page_count,
                              hub->page_count == 1 ? "page" : "pages");
    }
    for (unsigned i = 0; i < hub->page_count && at < sizeof(text); i++) {
        const hub_page_t *page = &hub->pages[i];
        at += (size_t)snprintf(text + at, sizeof(text) - at, "\n%u. %s (%s%s)", i + 1, page->title,
                               page->cant_show ? "can't be shown" : page->quality,
                               page->stale ? ", stale" : "");
    }
    if (hub->saved_pages && at < sizeof(text)) {
        snprintf(text + at, sizeof(text) - at, "\nSaved before the last restart; age unknown.");
    }
    return text;
}

// A display that is set up: how things stand with the hub.
static void show_hub(const hub_status_t *hub, const char *note)
{
    static char body[640];
    const char *pages = page_list(hub);
    // A token that isn't in flash matters more than whatever just happened.
    if (hub->token_unsaved) {
        note = "This display couldn't save its place with the hub. If it restarts now, it will "
               "need setting up again.";
    }
    switch (hub->state) {
    case HUB_ONLINE:
        show("Connected to the hub", COLOUR_GOOD, NULL,
             "The pages themselves come with a later version of this firmware.", pages, note);
        return;
    case HUB_ENROLLING:
        show("Joining the hub", COLOUR_TEXT, NULL,
             "On Wi-Fi. Asking the hub to add this display.", pages, note);
        return;
    case HUB_WAITING:
        show("Joined the hub", COLOUR_TEXT, NULL, "Asking the hub for this display's pages.",
             pages, note);
        return;
    case HUB_TROUBLE:
    case HUB_NEEDS_SETUP:
        snprintf(body, sizeof(body), "%s%s", advice(hub),
                 hub->has_snapshot && hub->state == HUB_TROUBLE
                     ? "\n\nShowing the last pages it was sent."
                     : "");
        show(headline(hub),
             hub->problem == DH_PROBLEM_WRONG_HUB ? COLOUR_BAD : COLOUR_WARNING, NULL, body, pages,
             note);
        return;
    case HUB_FAILED:
        show("Can't connect", COLOUR_BAD, NULL,
             "This display couldn't start its connection to the hub.\n\n"
             "Unplug it and plug it back in.",
             pages, note);
        return;
    default:
        // HUB_JOINING_WIFI, and the moment before the hub task has looked at a new setup.
        show("Joining Wi-Fi", COLOUR_TEXT, NULL,
             "This display has its setup and is joining your Wi-Fi network.", pages, note);
        return;
    }
}

void ui_show_state(const storage_facts_t *facts, const storage_key_status_t *key,
                   const hub_status_t *hub, const char *note)
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
        show("Storage problem", COLOUR_BAD, NULL, body, NULL, note);
        return;
    }
    if (facts->state != STORAGE_WORKING) {
        show("Storage problem", COLOUR_BAD, NULL,
             "This display's storage didn't start or couldn't be read, so it can't be set up.\n\n"
             "Open Display Hub on your computer and use Install firmware to install it again. "
             "That erases the display's storage.",
             NULL, note);
        return;
    }
    if (facts->setup_unreadable) {
        show("Setup can't be read", COLOUR_WARNING, NULL,
             "This display holds a setup that this firmware can't read.\n\n"
             "Hold the BOOT button for 10 seconds to erase it, then set the display up again.",
             NULL, note);
        return;
    }
    if (facts->set_up) {
        show_hub(hub, note);
        return;
    }
    show("Waiting for setup", COLOUR_TEXT, NULL,
         "Open Display Hub on your computer and choose Add a display.\n\n"
         "Keep this display plugged in to the computer with its USB cable.",
         NULL, note);
}

void ui_show_reset_countdown(int seconds_left)
{
    char number[16];
    snprintf(number, sizeof(number), "%d", seconds_left);
    show("Factory reset", COLOUR_WARNING, number,
         "Keep holding BOOT to erase this display's setup.\n\nLet go now to cancel.", NULL, NULL);
}
