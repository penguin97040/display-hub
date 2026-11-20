// The screens of this build. See ui.h.

#include "ui.h"

#include <stdio.h>
#include <string.h>

#include "dh_poll.h"
#include "esp_timer.h"
#include "lvgl.h"
#include "ui_font.h"
#include "ui_page.h"
#include "ui_text.h"

// How far in from the screen's edges everything is laid out, with the burn-in shift at rest.
#define PAD_SIDE   16
#define PAD_TOP    22
#define PAD_BOTTOM 14

// Ages and countdowns are brought up to date this often, and the picture moves one step of its
// burn-in cycle every SHIFT_TICKS of these: every 3 minutes, round the cycle in under half an hour.
#define TICK_MS     30000
#define SHIFT_TICKS 6

// How far a finger must move before LVGL starts to scroll a long page up or down (its default is
// 10 px). LVGL takes whichever way the finger has moved further when this is reached, and a
// scroll that has started stops the swipe that turns the page. A sideways swipe often starts
// with a few pixels' wobble up or down, so a little more room keeps it a swipe.
#define SCROLL_START_PX 20

static lv_obj_t *s_root;   // everything; moved a few pixels now and then (ui_shift)
static lv_obj_t *s_status; // the status screen: a column of text, as it always was
static lv_obj_t *s_title;
static lv_obj_t *s_number;
static lv_obj_t *s_body;
static lv_obj_t *s_extra;
static lv_obj_t *s_note;
static lv_obj_t *s_footer;
static lv_obj_t *s_page; // one page, drawn afresh each time it changes
static lv_obj_t *s_dots; // where in the swipe row this is

static const hub_pages_t *s_pages; // the pages to draw, or NULL
static bool s_with_pages;          // set up and holding pages: the swipe row shows
static bool s_counting;            // the factory reset countdown covers everything
static size_t s_view;              // which view of the row: a page, or the status screen last
// The pages in the row, as last drawn: a new snapshot with the same pages (the same content_seq,
// setup and saved flag) only brings the ages up to date, so a swipe or a scroll in progress isn't
// lost.
static uint32_t s_drawn_content;
static uint32_t s_drawn_generation;
static bool s_drawn_saved;
static ui_page_ids s_ids;          // the ids of the pages in the row, to keep the view by
static char s_drawn_id[DH_MAX_ID_BYTES + 1]; // the page drawn last, to keep its scroll position

// The labels on the page shown that change with time, and the page they belong to.
static ui_page_live_t s_live;
static const dh_page *s_live_page;

// The short line on each page saying what is wrong, if anything, and whether it is a problem
// (which also colours the status screen's dot).
static char s_line[DH_UTF8_ROOM(80)];
static lv_color_t s_line_colour;
static bool s_line_problem;

static unsigned s_ticks;
static unsigned s_shift_step;

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

static lv_obj_t *add_column(lv_obj_t *parent)
{
    lv_obj_t *column = lv_obj_create(parent);
    lv_obj_remove_style_all(column);
    lv_obj_set_width(column, LV_PCT(100));
    lv_obj_set_flex_grow(column, 1);
    lv_obj_set_flex_flow(column, LV_FLEX_FLOW_COLUMN);
    lv_obj_set_scrollable(column, false);
    return column;
}

static void place_root(void)
{
    int32_t dx, dy;
    ui_shift(s_shift_step, &dx, &dy);
    lv_obj_set_pos(s_root, UI_SHIFT_MAX + dx, UI_SHIFT_MAX + dy);
}

static int64_t held_seconds(void)
{
    if (s_pages == NULL) {
        return 0;
    }
    const int64_t held_ms = esp_timer_get_time() / 1000 - s_pages->received_ms;
    return held_ms > 0 ? held_ms / 1000 : 0;
}

static size_t page_count(void)
{
    return s_pages != NULL && s_pages->snapshot.page_count <= DH_MAX_PAGES
               ? s_pages->snapshot.page_count
               : 0;
}

static void draw_dots(size_t views)
{
    lv_obj_clean(s_dots);
    for (size_t i = 0; i < views; i++) {
        lv_obj_t *dot = lv_obj_create(s_dots);
        lv_obj_remove_style_all(dot);
        const bool here = i == s_view;
        const bool status = i == views - 1;
        lv_obj_set_size(dot, here ? 8 : 6, here ? 8 : 6);
        lv_obj_set_style_radius(dot, LV_RADIUS_CIRCLE, 0);
        lv_obj_set_style_bg_opa(dot, LV_OPA_COVER, 0);
        lv_obj_set_style_bg_color(dot, status && s_line_problem ? s_line_colour
                                       : here                   ? UI_COLOUR_TEXT
                                                                : UI_COLOUR_DIM,
                                  0);
        lv_obj_set_clickable(dot, false);
    }
}

// Shows what the state says: the status screen alone, or the view s_view of the swipe row.
// Room for the scroll bar when the figures don't all fit, so it never covers them. After every
// change to the page's text: an age that grows onto a second line can make the figures overflow.
static void make_room_for_scroll_bar(void)
{
    if (s_live.figures == NULL) {
        return;
    }
    lv_obj_update_layout(s_page);
    if (lv_obj_get_scroll_bottom(s_live.figures) > 0 &&
        lv_obj_get_style_pad_right(s_live.figures, 0) < 10) {
        lv_obj_set_style_pad_right(s_live.figures, 10, 0);
        lv_obj_update_layout(s_page);
    }
}

static void render(void)
{
    // The page drawn last, before it is cleared: to keep its place if it is drawn again.
    int32_t scroll_y = 0;
    if (s_live.figures != NULL) {
        scroll_y = lv_obj_get_scroll_y(s_live.figures);
    }
    memset(&s_live, 0, sizeof(s_live));
    s_live_page = NULL;

    if (s_counting || !s_with_pages) {
        s_drawn_id[0] = '\0';
        lv_obj_clean(s_page);
        lv_obj_set_hidden(s_page, true);
        lv_obj_set_hidden(s_dots, true);
        lv_obj_set_hidden(s_status, false);
        return;
    }

    const size_t count = page_count();
    const size_t views = ui_view_count(count);
    if (s_view >= views) {
        s_view = views - 1;
    }
    lv_obj_set_hidden(s_dots, false);
    draw_dots(views);
    lv_obj_clean(s_page);
    if (s_view == views - 1) {
        s_drawn_id[0] = '\0';
        lv_obj_set_hidden(s_page, true);
        lv_obj_set_hidden(s_status, false);
        lv_obj_scroll_to_y(s_status, 0, LV_ANIM_OFF);
        return;
    }
    lv_obj_set_hidden(s_status, true);
    lv_obj_set_hidden(s_page, false);

    if (count == 0) {
        ui_no_pages_draw(s_page);
    } else {
        const dh_page *page = &s_pages->snapshot.pages[s_view];
        ui_page_draw(s_page, &s_pages->snapshot, page, s_pages->saved, held_seconds(), &s_live);
        s_live_page = page;
    }
    if (s_line[0] != '\0') {
        add_label(s_page, ui_fonts()->small_bold, s_line_colour);
        lv_label_set_text(lv_obj_get_child(s_page, -1), s_line);
    }
    if (s_live.figures != NULL) {
        make_room_for_scroll_bar();
        // The same page again: where it was scrolled to.
        if (s_live_page != NULL && strcmp(s_drawn_id, s_live_page->id) == 0 && scroll_y > 0) {
            lv_obj_scroll_to_y(s_live.figures, scroll_y, LV_ANIM_OFF);
        }
    }
    snprintf(s_drawn_id, sizeof(s_drawn_id), "%s", s_live_page != NULL ? s_live_page->id : "");
}

static void render_later(void *arg)
{
    (void)arg;
    render();
}

// A swipe left shows the next view, a swipe right the one before. The row doesn't wrap round.
static void on_gesture(lv_event_t *e)
{
    (void)e;
    lv_indev_t *indev = lv_indev_active();
    if (indev == NULL || s_counting || !s_with_pages) {
        return;
    }
    const size_t views = ui_view_count(page_count());
    const lv_dir_t dir = lv_indev_get_gesture_dir(indev);
    size_t next = s_view;
    if (dir == LV_DIR_LEFT && s_view + 1 < views) {
        next = s_view + 1;
    } else if (dir == LV_DIR_RIGHT && s_view > 0) {
        next = s_view - 1;
    }
    if (next == s_view) {
        return;
    }
    s_view = next;
    lv_indev_wait_release(indev);
    // Not from inside the touch's own event: the object touched is about to be deleted.
    lv_async_call(render_later, NULL);
}

// Runs in LVGL's task, which holds the LVGL lock while it does.
static void on_tick(lv_timer_t *timer)
{
    (void)timer;
    if (++s_ticks % SHIFT_TICKS == 0) {
        s_shift_step++;
        place_root();
    }
    if (s_live_page != NULL && s_pages != NULL && !s_counting) {
        ui_page_tick(&s_live, &s_pages->snapshot, s_live_page, s_pages->saved, held_seconds());
        make_room_for_scroll_bar();
    }
}

void ui_start(const char *version, const char *board_note)
{
    ui_fonts_start();
    const ui_fonts_t *f = ui_fonts();

    lv_obj_t *screen = lv_screen_active();
    lv_obj_set_style_bg_color(screen, lv_color_black(), 0);
    lv_obj_set_style_bg_opa(screen, LV_OPA_COVER, 0);
    lv_obj_set_style_text_color(screen, UI_COLOUR_TEXT, 0);
    lv_obj_set_style_text_font(screen, f->body, 0);
    lv_obj_set_scrollable(screen, false);
    lv_obj_add_event_cb(screen, on_gesture, LV_EVENT_GESTURE, NULL);
    for (lv_indev_t *indev = lv_indev_get_next(NULL); indev != NULL;
         indev = lv_indev_get_next(indev)) {
        if (lv_indev_get_type(indev) == LV_INDEV_TYPE_POINTER) {
            lv_indev_set_scroll_limit(indev, SCROLL_START_PX);
        }
    }

    // Everything is laid out UI_SHIFT_MAX in from each edge, so the burn-in shift never pushes
    // anything off the screen.
    lv_display_t *display = lv_obj_get_display(screen);
    s_root = lv_obj_create(screen);
    lv_obj_remove_style_all(s_root);
    lv_obj_set_size(s_root, lv_display_get_horizontal_resolution(display) - 2 * UI_SHIFT_MAX,
                    lv_display_get_vertical_resolution(display) - 2 * UI_SHIFT_MAX);
    lv_obj_set_flex_flow(s_root, LV_FLEX_FLOW_COLUMN);
    lv_obj_set_style_pad_hor(s_root, PAD_SIDE - UI_SHIFT_MAX, 0);
    lv_obj_set_style_pad_top(s_root, PAD_TOP - UI_SHIFT_MAX, 0);
    lv_obj_set_style_pad_bottom(s_root, PAD_BOTTOM - UI_SHIFT_MAX, 0);
    lv_obj_set_style_pad_row(s_root, 8, 0);
    lv_obj_set_scrollable(s_root, false);
    place_root();

    // The status screen: one plain column, which scrolls up and down if a message is longer
    // than the screen.
    s_status = add_column(s_root);
    lv_obj_set_style_pad_row(s_status, 14, 0);
    lv_obj_set_scrollable(s_status, true);
    lv_obj_set_scroll_dir(s_status, LV_DIR_VER);
    lv_obj_set_scrollbar_mode(s_status, LV_SCROLLBAR_MODE_OFF);
    s_title = add_label(s_status, f->title, UI_COLOUR_TEXT);
    s_number = add_label(s_status, f->big[0], UI_COLOUR_WARNING);
    s_body = add_label(s_status, f->body, UI_COLOUR_TEXT);
    s_extra = add_label(s_status, f->small, UI_COLOUR_QUIET);
    s_note = add_label(s_status, f->body, UI_COLOUR_WARNING);
    s_footer = add_label(s_status, f->small, UI_COLOUR_QUIET);

    // A page.
    s_page = add_column(s_root);
    lv_obj_set_style_pad_row(s_page, 8, 0);
    lv_obj_set_clickable(s_page, false);
    lv_obj_set_hidden(s_page, true);

    // The dots along the bottom, one for each view of the row.
    s_dots = lv_obj_create(s_root);
    lv_obj_remove_style_all(s_dots);
    lv_obj_set_size(s_dots, LV_PCT(100), 8);
    lv_obj_set_flex_flow(s_dots, LV_FLEX_FLOW_ROW);
    lv_obj_set_flex_align(s_dots, LV_FLEX_ALIGN_CENTER, LV_FLEX_ALIGN_CENTER,
                          LV_FLEX_ALIGN_CENTER);
    lv_obj_set_style_pad_column(s_dots, 8, 0);
    lv_obj_set_scrollable(s_dots, false);
    lv_obj_set_clickable(s_dots, false);
    lv_obj_set_hidden(s_dots, true);

    if (board_note != NULL) {
        lv_label_set_text_fmt(s_footer, "Display Hub firmware %s\n%s", version, board_note);
    } else {
        lv_label_set_text_fmt(s_footer, "Display Hub firmware %s", version);
    }

    lv_timer_create(on_tick, TICK_MS, NULL);
}

// The status screen's words. `extra` is NULL or a line in smaller text under the body.
static void show(const char *title, lv_color_t title_colour, const char *number, const char *body,
                 const char *extra, const char *note)
{
    lv_label_set_text(s_title, title);
    lv_obj_set_style_text_color(s_title, title_colour, 0);
    lv_label_set_text(s_number, number != NULL ? number : "");
    lv_obj_set_hidden(s_number, number == NULL);
    lv_label_set_text(s_body, body);
    lv_label_set_text(s_extra, extra != NULL ? extra : "");
    lv_obj_set_hidden(s_extra, extra == NULL);
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
        return "Can't join Wi-Fi";
    case HUB_DETAIL_WIFI_START:
        return "Wi-Fi didn't start";
    case HUB_DETAIL_NO_WIFI_DETAILS:
        // Not something to look for at the router: the setup itself has to be done again.
        return "No Wi-Fi details in its setup";
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

#define TOKEN_UNSAVED "This display couldn't save its place with the hub. If it restarts now, " \
                      "it will need setting up again."

// What a firmware update is doing, in a few words for the foot of each page. NULL for nothing.
static const char *update_line(const hub_status_t *hub, const storage_fw_facts_t *fw)
{
    if (hub->update == HUB_UPDATE_DOWNLOADING) {
        return "Updating its firmware";
    }
    if (hub->update == HUB_UPDATE_RESTARTING) {
        return "Restarting to finish the update";
    }
    return fw->on_trial ? "Checking the new firmware" : NULL;
}

// The same in full, for the status screen: an update under way, a trial boot, or how the last
// update ended. NULL for nothing to say.
static const char *update_words(const hub_status_t *hub, const storage_fw_facts_t *fw)
{
    if (hub->update == HUB_UPDATE_DOWNLOADING) {
        return "Installing a firmware update. Keep this display plugged in. It restarts by "
               "itself when the update is ready.";
    }
    if (hub->update == HUB_UPDATE_RESTARTING) {
        return "The firmware update is ready. Restarting now.";
    }
    if (fw->on_trial) {
        return "This is new firmware, on trial. It has 5 minutes from starting to reach the "
               "hub. If it can't, this display goes back to the version it had before.";
    }
    switch (fw->last_result) {
    case DH_FW_REPORT_INSTALLED:
        return "The last firmware update was installed.";
    case DH_FW_REPORT_ROLLED_BACK:
        return "The last firmware update didn't start properly, so this display went back to "
               "the version it had before. Nothing was lost.";
    case DH_FW_REPORT_FAILED:
        return "The last firmware update didn't finish, so this display kept the version it "
               "had. Nothing was lost.";
    case DH_FW_REPORT_REFUSED:
        return "This display refused the last firmware update it was offered. Nothing was "
               "changed.";
    default:
        return NULL;
    }
}

// The short line each page shows about the hub: a problem in its colour, what the display is
// doing while it isn't yet talking to the hub, or the note about what just happened.
static void set_line(const hub_status_t *hub, const storage_fw_facts_t *fw, const char *note)
{
    const char *line = NULL;
    lv_color_t colour = UI_COLOUR_QUIET;
    bool problem = false;
    switch (hub->state) {
    case HUB_TROUBLE:
    case HUB_NEEDS_SETUP:
        line = headline(hub);
        colour = hub->problem == DH_PROBLEM_WRONG_HUB ? UI_COLOUR_BAD : UI_COLOUR_WARNING;
        problem = true;
        break;
    case HUB_FAILED:
        line = "Can't connect";
        colour = UI_COLOUR_BAD;
        problem = true;
        break;
    case HUB_JOINING_WIFI:
        line = "Joining Wi-Fi";
        break;
    case HUB_ENROLLING:
    case HUB_WAITING:
        line = "Joining the hub";
        break;
    default:
        break;
    }
    if (!problem && hub->token_unsaved) {
        line = "Couldn't save its place with the hub";
        colour = UI_COLOUR_WARNING;
        problem = true;
    }
    // An update is said plainly, but never in place of a problem.
    if (!problem && update_line(hub, fw) != NULL) {
        line = update_line(hub, fw);
        colour = UI_COLOUR_QUIET;
    }
    if (line == NULL && note != NULL) {
        line = note;
    }
    snprintf(s_line, sizeof(s_line), "%s", line != NULL ? line : "");
    s_line_colour = colour;
    s_line_problem = problem;
}

// A display that is set up: how things stand with the hub. With pages, this is the screen at
// the end of the swipe row. `drawn` is NULL or the pages in the row; `no_memory` says there
// was no memory for the screen's copy of them, so none can be shown.
static void show_hub(const hub_status_t *hub, const storage_fw_facts_t *fw,
                     const hub_pages_t *drawn, bool no_memory, const char *note)
{
    static char body[640];
    static char extra_text[480];
    // What is said about the pages is about the ones on the screen.
    const size_t pages = drawn != NULL ? page_count() : 0;
    const char *extra = NULL;
    if (no_memory) {
        extra = "This display hasn't the memory to show its pages. Unplug it and plug it back "
                "in.";
    } else if (drawn != NULL && drawn->saved) {
        extra = "The pages shown were saved before the last restart, so their age isn't known "
                "until the hub answers.";
    }
    // Then how things stand with firmware updates, in the same small text.
    const char *update = update_words(hub, fw);
    if (update != NULL) {
        snprintf(extra_text, sizeof(extra_text), "%s%s%s", extra != NULL ? extra : "",
                 extra != NULL ? "\n\n" : "", update);
        extra = extra_text;
    }
    // A token that isn't in flash matters more than whatever just happened.
    if (hub->token_unsaved) {
        note = TOKEN_UNSAVED;
    }
    switch (hub->state) {
    case HUB_ONLINE:
        if (drawn == NULL && no_memory) {
            snprintf(body, sizeof(body), "This display is talking to the hub.");
        } else if (drawn == NULL) {
            snprintf(body, sizeof(body), "Asking the hub for this display's pages.");
        } else if (pages == 0) {
            snprintf(body, sizeof(body), "The hub hasn't given this display any pages yet.");
        } else {
            snprintf(body, sizeof(body), "Showing the %u %s the hub sent. Swipe right to see %s.",
                     (unsigned)pages, pages == 1 ? "page" : "pages", pages == 1 ? "it" : "them");
        }
        show("Connected to the hub", UI_COLOUR_GOOD, NULL, body, extra, note);
        return;
    case HUB_ENROLLING:
        show("Joining the hub", UI_COLOUR_TEXT, NULL,
             "On Wi-Fi. Asking the hub to add this display.", extra, note);
        return;
    case HUB_WAITING:
        show("Joined the hub", UI_COLOUR_TEXT, NULL, "Asking the hub for this display's pages.",
             extra, note);
        return;
    case HUB_TROUBLE:
    case HUB_NEEDS_SETUP:
        snprintf(body, sizeof(body), "%s%s", advice(hub),
                 pages > 0 && hub->state == HUB_TROUBLE
                     ? "\n\nShowing the last pages it was sent."
                     : "");
        show(headline(hub),
             hub->problem == DH_PROBLEM_WRONG_HUB ? UI_COLOUR_BAD : UI_COLOUR_WARNING, NULL, body,
             extra, note);
        return;
    case HUB_FAILED:
        show("Can't connect", UI_COLOUR_BAD, NULL,
             "This display couldn't start its connection to the hub.\n\n"
             "Unplug it and plug it back in.",
             extra, note);
        return;
    default:
        // HUB_JOINING_WIFI, and the moment before the hub task has looked at a new setup.
        show("Joining Wi-Fi", UI_COLOUR_TEXT, NULL,
             "This display has its setup and is joining your Wi-Fi network.", extra, note);
        return;
    }
}

// The status screen alone, for a display that isn't set up and running: the storage problem,
// an unreadable setup, or waiting for setup. Returns false for a display that is set up.
static bool show_not_running(const storage_facts_t *facts, const storage_key_status_t *key,
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
        show("Storage problem", UI_COLOUR_BAD, NULL, body, NULL, note);
        return true;
    }
    if (facts->state != STORAGE_WORKING) {
        show("Storage problem", UI_COLOUR_BAD, NULL,
             "This display's storage didn't start or couldn't be read, so it can't be set up.\n\n"
             "Open Display Hub on your computer and use Install firmware to install it again. "
             "That erases the display's storage.",
             NULL, note);
        return true;
    }
    if (facts->setup_unreadable) {
        show("Setup can't be read", UI_COLOUR_WARNING, NULL,
             "This display holds a setup that this firmware can't read.\n\n"
             "Hold the BOOT button for 10 seconds to erase it, then set the display up again.",
             NULL, note);
        return true;
    }
    if (!facts->set_up) {
        show("Waiting for setup", UI_COLOUR_TEXT, NULL,
             "Open Display Hub on your computer and choose Add a display.\n\n"
             "Keep this display plugged in to the computer with its USB cable.",
             NULL, note);
        return true;
    }
    return false;
}

void ui_show_state(const storage_facts_t *facts, const storage_key_status_t *key,
                   const hub_status_t *hub, const storage_fw_facts_t *fw,
                   const hub_pages_t *pages, const char *note)
{
    // Whatever was on the screen is drawn afresh unless only the ages have moved on.
    bool redraw = s_counting || !s_with_pages;
    s_counting = false;
    s_pages = pages;

    const bool running = !show_not_running(facts, key, note);
    const bool with_pages = running && pages != NULL && pages->has_snapshot;
    if (with_pages) {
        const bool fresh = !s_with_pages || pages->content_seq != s_drawn_content ||
                           pages->generation != s_drawn_generation ||
                           pages->saved != s_drawn_saved;
        if (!s_with_pages) {
            s_view = 0; // the first page, whenever the row appears
        } else if (fresh) {
            s_view = ui_keep_view(&s_ids, s_view, &pages->snapshot);
        }
        if (fresh) {
            ui_page_ids_of(&pages->snapshot, &s_ids);
            s_drawn_content = pages->content_seq;
            s_drawn_generation = pages->generation;
            s_drawn_saved = pages->saved;
            redraw = true;
        }
    } else {
        redraw = true;
    }
    s_with_pages = with_pages;

    // The line about the hub is part of the page: a new one means drawing the page again.
    char line_before[sizeof(s_line)];
    memcpy(line_before, s_line, sizeof(line_before));
    const lv_color_t colour_before = s_line_colour;
    const bool problem_before = s_line_problem;
    if (running) {
        show_hub(hub, fw, with_pages ? pages : NULL, pages == NULL, note);
        set_line(hub, fw, note);
    } else {
        s_line[0] = '\0';
        s_line_problem = false;
    }
    if (strcmp(line_before, s_line) != 0 || !lv_color_eq(colour_before, s_line_colour) ||
        problem_before != s_line_problem) {
        redraw = true;
    }

    if (redraw) {
        render();
    } else if (s_live_page != NULL) {
        // The same pages from a later snapshot: its generated_at and time of arrival.
        ui_page_tick(&s_live, &pages->snapshot, s_live_page, pages->saved, held_seconds());
        make_room_for_scroll_bar();
    }
}

void ui_show_reset_countdown(int seconds_left)
{
    char number[16];
    snprintf(number, sizeof(number), "%d", seconds_left);
    show("Factory reset", UI_COLOUR_WARNING, number,
         "Keep holding BOOT to erase this display's setup.\n\nLet go now to cancel.", NULL, NULL);
    s_counting = true;
    render();
}
