// The storage key screen. See ui_storage_key.h.
//
// The burn is permanent, so asking for it takes two taps on two different buttons in two different
// places: "Burn the key", then "Yes, burn it now". "Cancel" sits where "Burn the key" was, so a
// double tap cancels. A tap only counts if the finger went down on the button after it had been
// showing for a second, stayed put, and came up on the same button. A swipe never counts, nor does
// a touch that began somewhere else and slid onto a button.

#include "ui_storage_key.h"

#include <inttypes.h>
#include <stdlib.h>

#include "esp_log.h"
#include "esp_system.h"
#include "ui_parts.h"

static const char *TAG = "storage-key";

// A button must have been showing this long before a touch on it counts.
#define SETTLE_MS 1000
// A tap's finger may wander this far, in pixels (about 2 mm on this screen), and no further.
#define TAP_SLOP 24
// A button pressed this long after the finger first went down was slid onto, not tapped.
#define SLID_ON_MS 150
// A press held longer than this isn't a tap.
#define TAP_MAX_MS 3000
// The second step goes back to the first if it is left alone this long.
#define CONFIRM_TIMEOUT_MS 30000

#define BUTTON_HEIGHT 60

typedef enum {
    VIEW_INFO = 0, // why the burn isn't offered, or the checks' results
    VIEW_OFFER,    // the explanation, and "Burn the key"
    VIEW_CONFIRM,  // "Yes, burn it now" and "Cancel"
    VIEW_BUSY,     // the burn is under way
    VIEW_COUNT,
} view_t;

typedef enum {
    BUTTON_BURN = 1,
    BUTTON_YES,
    BUTTON_CANCEL,
    BUTTON_RESTART,
} button_t;

static lv_obj_t *s_views[VIEW_COUNT];
static lv_obj_t *s_info_text; // the part of VIEW_INFO that is rebuilt
static view_t s_view = VIEW_INFO;
static uint32_t s_view_since;
static bool s_page_shown;
static bool s_can_burn;       // the state allows the offer
static bool s_confirmed;      // the second tap has been made; never ask twice
static ui_storage_key_confirmed_t s_on_confirmed;

// The touch in progress on a button.
static struct {
    lv_obj_t *button;
    lv_point_t down_at;
    uint32_t down_tick;
    bool spoilt;
} s_tap;

// When the finger last went down anywhere on the screen, watched by a timer. It backs up the
// press lock: a button that reports a press well after the touch began was slid onto.
static bool s_touching;
static uint32_t s_touch_down_tick;

static void watch_touch(lv_timer_t *timer)
{
    (void)timer;
    bool touching = false;
    for (lv_indev_t *indev = lv_indev_get_next(NULL); indev != NULL;
         indev = lv_indev_get_next(indev)) {
        if (lv_indev_get_state(indev) == LV_INDEV_STATE_PRESSED) {
            touching = true;
        }
    }
    if (touching && !s_touching) {
        s_touch_down_tick = lv_tick_get();
    }
    s_touching = touching;
}

static void show_view(view_t view)
{
    s_view = view;
    s_view_since = lv_tick_get();
    s_tap.button = NULL;
    for (int i = 0; i < VIEW_COUNT; i++) {
        lv_obj_set_hidden(s_views[i], i != (int)view);
    }
}

void ui_storage_key_page_shown(bool shown)
{
    if (s_views[0] == NULL) {
        return;
    }
    s_page_shown = shown;
    if (s_view == VIEW_CONFIRM) {
        // Arriving or leaving, a half-made confirmation starts again from the first step.
        show_view(VIEW_OFFER);
    } else {
        // The settle time starts again each time the screen is swiped to.
        s_view_since = lv_tick_get();
        s_tap.button = NULL;
    }
}

void ui_storage_key_swipe_seen(void)
{
    s_tap.spoilt = true;
}

static void on_confirm_timeout(lv_timer_t *timer)
{
    (void)timer;
    if (s_view == VIEW_CONFIRM && lv_tick_elaps(s_view_since) > CONFIRM_TIMEOUT_MS) {
        ESP_LOGI(TAG, "the confirmation timed out; back to the first step");
        show_view(VIEW_OFFER);
    }
}

static void button_tapped(button_t button)
{
    lv_indev_t *indev = lv_indev_active();
    switch (button) {
    case BUTTON_BURN:
        if (s_view == VIEW_OFFER && s_can_burn && !s_confirmed) {
            ESP_LOGI(TAG, "first tap: \"Burn the key\". Asking again");
            show_view(VIEW_CONFIRM);
        }
        break;
    case BUTTON_YES:
        if (s_view == VIEW_CONFIRM && s_can_burn && !s_confirmed && s_on_confirmed != NULL) {
            ESP_LOGW(TAG, "second tap: \"Yes, burn it now\". Confirmed on the screen");
            s_confirmed = true;
            show_view(VIEW_BUSY);
            s_on_confirmed();
        }
        break;
    case BUTTON_CANCEL:
        if (s_view == VIEW_CONFIRM) {
            ESP_LOGI(TAG, "cancelled; nothing was burnt");
            show_view(VIEW_OFFER);
        }
        break;
    case BUTTON_RESTART:
        if (s_view == VIEW_INFO) {
            ESP_LOGI(TAG, "restarting, as asked on the screen");
            esp_restart();
        }
        break;
    }
    // The rest of this touch does nothing more.
    if (indev != NULL) {
        lv_indev_wait_release(indev);
    }
}

static void on_button_event(lv_event_t *e)
{
    lv_obj_t *button = lv_event_get_current_target_obj(e);
    lv_indev_t *indev = lv_indev_active();
    lv_point_t point = {0, 0};
    if (indev != NULL) {
        lv_indev_get_point(indev, &point);
    }

    switch (lv_event_get_code(e)) {
    case LV_EVENT_PRESSED:
        s_tap.button = button;
        s_tap.down_at = point;
        s_tap.down_tick = lv_tick_get();
        // A touch that lands before the button has settled never counts, however it ends.
        s_tap.spoilt = indev == NULL || !s_page_shown || lv_tick_elaps(s_view_since) < SETTLE_MS ||
                       (s_touching && lv_tick_elaps(s_touch_down_tick) > SLID_ON_MS);
        break;
    case LV_EVENT_PRESSING:
        if (s_tap.button == button && (abs(point.x - s_tap.down_at.x) > TAP_SLOP ||
                                       abs(point.y - s_tap.down_at.y) > TAP_SLOP)) {
            s_tap.spoilt = true;
        }
        break;
    case LV_EVENT_PRESS_LOST:
        s_tap.spoilt = true;
        break;
    case LV_EVENT_CLICKED: {
        const bool deliberate =
            s_tap.button == button && !s_tap.spoilt && s_page_shown && indev != NULL &&
            lv_tick_elaps(s_tap.down_tick) <= TAP_MAX_MS &&
            abs(point.x - s_tap.down_at.x) <= TAP_SLOP &&
            abs(point.y - s_tap.down_at.y) <= TAP_SLOP && lv_obj_hit_test(button, &point);
        s_tap.button = NULL;
        if (deliberate) {
            button_tapped((button_t)(uintptr_t)lv_event_get_user_data(e));
        }
        break;
    }
    default:
        break;
    }
}

static lv_obj_t *add_button(lv_obj_t *parent, button_t id, const char *text, lv_color_t fill,
                            lv_color_t edge)
{
    lv_obj_t *button = lv_button_create(parent);
    lv_obj_remove_style_all(button);
    lv_obj_set_size(button, LV_PCT(100), BUTTON_HEIGHT);
    lv_obj_set_style_radius(button, 12, 0);
    lv_obj_set_style_bg_color(button, fill, 0);
    lv_obj_set_style_bg_opa(button, LV_OPA_COVER, 0);
    lv_obj_set_style_border_color(button, edge, 0);
    lv_obj_set_style_border_width(button, 2, 0);
    // Show the press.
    lv_obj_set_style_bg_color(button, edge, LV_STATE_PRESSED);
    // A touch stays with the object it went down on, so sliding onto a button never presses it.
    // (LVGL's default for every object; set here because the confirmation relies on it.)
    lv_obj_set_press_lock(button, true);
    lv_obj_set_scrollable(button, false);

    lv_obj_t *label = lv_label_create(button);
    lv_obj_set_style_text_font(label, &lv_font_montserrat_24, 0);
    lv_obj_set_style_text_color(label, UI_COLOUR_TEXT, 0);
    lv_label_set_text(label, text);
    lv_obj_center(label);

    lv_obj_add_event_cb(button, on_button_event, LV_EVENT_ALL, (void *)(uintptr_t)id);
    return button;
}

// A column that fills its parent's spare height.
static lv_obj_t *add_column(lv_obj_t *parent, bool scroll_up_down)
{
    lv_obj_t *column = lv_obj_create(parent);
    lv_obj_remove_style_all(column);
    lv_obj_set_width(column, LV_PCT(100));
    lv_obj_set_flex_grow(column, 1);
    lv_obj_set_flex_flow(column, LV_FLEX_FLOW_COLUMN);
    lv_obj_set_style_pad_row(column, 6, 0);
    if (scroll_up_down) {
        lv_obj_set_scroll_dir(column, LV_DIR_VER);
        lv_obj_set_scrollbar_mode(column, LV_SCROLLBAR_MODE_OFF);
    } else {
        lv_obj_set_scrollable(column, false);
    }
    return column;
}

void ui_storage_key_build(lv_obj_t *page)
{
    const lv_color_t grey = lv_color_hex(0x303030);
    const lv_color_t grey_edge = lv_color_hex(0x808080);

    for (int i = 0; i < VIEW_COUNT; i++) {
        s_views[i] = add_column(page, false);
    }

    // Why not, or the results. Filled in by ui_storage_key_show().
    s_info_text = add_column(s_views[VIEW_INFO], true);
    add_button(s_views[VIEW_INFO], BUTTON_RESTART, "Restart", grey, grey_edge);

    // The first step.
    lv_obj_t *text = add_column(s_views[VIEW_OFFER], true);
    ui_add_label(text, &lv_font_montserrat_24, UI_COLOUR_TEXT, "Storage key");
    ui_add_label(text, &lv_font_montserrat_20, UI_COLOUR_TEXT,
                 "This burns a secret key into eFuse key block KEY5, for encrypting the "
                 "display's saved settings.");
    ui_add_label(text, &lv_font_montserrat_20, UI_COLOUR_WARNING,
                 "It is permanent. KEY5 is used up for good and it cannot be undone.");
    ui_add_label(text, &lv_font_montserrat_20, UI_COLOUR_TEXT,
                 "It is expected not to affect reflashing over USB. That isn't proven yet: "
                 "this check is how we find out.");
    add_button(s_views[VIEW_OFFER], BUTTON_BURN, "Burn the key", lv_color_hex(0x402C00),
               UI_COLOUR_WARNING);

    // The second step. "Yes" is up in the text; "Cancel" is where "Burn the key" was.
    text = add_column(s_views[VIEW_CONFIRM], true);
    ui_add_label(text, &lv_font_montserrat_24, UI_COLOUR_WARNING, "Burn KEY5 now?");
    ui_add_label(text, &lv_font_montserrat_20, UI_COLOUR_TEXT,
                 "This cannot be undone. Leave the USB cable in until the result shows.");
    ui_add_gap(text, 8);
    add_button(text, BUTTON_YES, "Yes, burn it now", lv_color_hex(0x8C1C14), UI_COLOUR_BAD);
    add_button(s_views[VIEW_CONFIRM], BUTTON_CANCEL, "Cancel", grey, grey_edge);

    // Under way.
    text = add_column(s_views[VIEW_BUSY], false);
    ui_add_label(text, &lv_font_montserrat_24, UI_COLOUR_WARNING, "Burning the key...");
    ui_add_label(text, &lv_font_montserrat_20, UI_COLOUR_TEXT, "Leave the USB cable in.");

    show_view(VIEW_INFO);
    lv_timer_create(on_confirm_timeout, 1000, NULL);
    lv_timer_create(watch_touch, 20, NULL);
}

static void add_check(const char *text, check_result_t result)
{
    if (result == CHECK_NOT_RUN) {
        lv_label_set_text_fmt(ui_add_label(s_info_text, &lv_font_montserrat_16, UI_COLOUR_QUIET, ""),
                              "--  %s (not run)", text);
        return;
    }
    const bool ok = result == CHECK_PASS;
    lv_label_set_text_fmt(ui_add_label(s_info_text, &lv_font_montserrat_16,
                                       ok ? UI_COLOUR_GOOD : UI_COLOUR_BAD, ""),
                          "%s  %s", ok ? "OK" : "FAIL", text);
}

static void fill_results(const storage_key_checks_t *checks)
{
    if (!checks->ran) {
        ui_add_label(s_info_text, &lv_font_montserrat_20, UI_COLOUR_WARNING,
                     "KEY5 holds a key, but the checks haven't run.");
        return;
    }
    ui_add_label(s_info_text, &lv_font_montserrat_36,
                 checks->all_passed ? UI_COLOUR_GOOD : UI_COLOUR_BAD,
                 checks->all_passed ? "Storage key: PASS" : "Storage key: FAIL");

    char line[96];
    add_check("KEY5 purpose is HMAC_UP", checks->key_purpose);
    add_check("KEY5 read- and write-protected", checks->key_protected);
    add_check(checks->key_readable ? "Key readable: YES" : "Key readable: no",
              checks->key_unreadable);
    if (checks->nvs_started == CHECK_FAIL) {
        lv_snprintf(line, sizeof(line), "Encrypted storage: error 0x%x",
                    (unsigned)checks->nvs_error);
        add_check(line, CHECK_FAIL);
    } else {
        add_check("Encrypted storage started", checks->nvs_started);
    }
    add_check(checks->first_start ? "First start since an erase"
                                  : "Last start's data read back",
              checks->stored_data);
    lv_snprintf(line, sizeof(line), "Boot count %" PRIu32 " (was %" PRIu32 "), saved",
                checks->boot_count, checks->boot_count > 0 ? checks->boot_count - 1 : 0);
    add_check(line, checks->written_data);
    if (checks->raw_flash == CHECK_FAIL) {
        add_check(checks->plain_text_found ? "Marker FOUND in raw flash: not encrypted"
                                           : "Raw flash couldn't be checked",
                  CHECK_FAIL);
    } else {
        add_check("Marker not found in raw flash", checks->raw_flash);
    }
    if (checks->burn_changed_only_key != CHECK_NOT_RUN) {
        add_check("The burn changed only KEY5", checks->burn_changed_only_key);
    }

    ui_add_gap(s_info_text, 4);
    ui_add_label(s_info_text, &lv_font_montserrat_16, UI_COLOUR_QUIET,
                 "Next: tap Restart (the count should go up by one). Then install this firmware "
                 "again (PASS again, count back at 1).");
}

void ui_storage_key_show(const storage_key_status_t *status, const storage_key_checks_t *checks,
                         const char *burn_failure, ui_storage_key_confirmed_t on_confirmed)
{
    s_on_confirmed = on_confirmed;
    s_can_burn = status->state == STORAGE_KEY_CAN_BURN && burn_failure == NULL && !s_confirmed;

    if (s_can_burn) {
        show_view(VIEW_OFFER);
        return;
    }

    lv_obj_clean(s_info_text);
    lv_obj_scroll_to_y(s_info_text, 0, LV_ANIM_OFF);
    // The results have their own big heading, and need the room.
    if (status->state != STORAGE_KEY_PRESENT) {
        ui_add_label(s_info_text, &lv_font_montserrat_24, UI_COLOUR_TEXT, "Storage key");
    }
    if (burn_failure != NULL) {
        ui_add_label(s_info_text, &lv_font_montserrat_20, UI_COLOUR_BAD, burn_failure);
    }

    switch (status->state) {
    case STORAGE_KEY_PRESENT:
        fill_results(checks);
        break;
    case STORAGE_KEY_BLOCKED:
        ui_add_label(s_info_text, &lv_font_montserrat_20, UI_COLOUR_TEXT,
                     "No key has been burnt. The burn isn't offered, because:");
        for (int i = 0; i < status->blocker_count; i++) {
            ui_add_label(s_info_text, &lv_font_montserrat_20, UI_COLOUR_WARNING,
                         status->blockers[i]);
        }
        break;
    default:
        ui_add_label(s_info_text, &lv_font_montserrat_20, UI_COLOUR_TEXT,
                     "KEY5 is still empty. Restart to be offered the burn again.");
        break;
    }
    show_view(VIEW_INFO);
}
