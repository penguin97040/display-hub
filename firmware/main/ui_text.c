// The words and figures the screens show. See ui_text.h.

#include "ui_text.h"

#include <math.h>
#include <stdio.h>
#include <string.h>

void ui_duration(int64_t seconds, char *out, size_t size)
{
    const long long s = seconds > 0 ? (long long)seconds : 0;
    if (s < 60) {
        snprintf(out, size, "%lld s", s);
    } else if (s < 3600) {
        snprintf(out, size, "%lld min", s / 60);
    } else if (s < 86400) {
        const long long h = s / 3600, m = (s % 3600) / 60;
        if (m == 0) {
            snprintf(out, size, "%lld h", h);
        } else {
            snprintf(out, size, "%lld h %lld min", h, m);
        }
    } else {
        const long long d = s / 86400, h = (s % 86400) / 3600;
        if (h == 0) {
            snprintf(out, size, "%lld d", d);
        } else {
            snprintf(out, size, "%lld d %lld h", d, h);
        }
    }
}

ui_age ui_age_text(const dh_snapshot *s, const dh_page *p, bool saved, int64_t held_seconds,
                   char *out, size_t size)
{
    if (saved) {
        snprintf(out, size, "Saved; age unknown");
        return UI_AGE_UNKNOWN;
    }
    const dh_freshness f = dh_page_freshness(s, p, held_seconds);
    if (!f.known) {
        snprintf(out, size, "Never updated");
        return UI_AGE_NEVER;
    }
    // The screen brings this up to date every 30 seconds, so seconds would only ever be wrong.
    char age[UI_TEXT_ROOM + 8];
    if (f.age_seconds < 60) {
        snprintf(age, sizeof(age), "just now");
    } else {
        char d[UI_TEXT_ROOM];
        ui_duration(f.age_seconds, d, sizeof(d));
        snprintf(age, sizeof(age), "%s ago", d);
    }
    if (f.stale) {
        snprintf(out, size, "Stale. Updated %s", age);
        return UI_AGE_STALE;
    }
    snprintf(out, size, "Updated %s", age);
    return UI_AGE_FRESH;
}

void ui_reset_text(const dh_snapshot *s, const dh_meter_item *item, bool saved,
                   int64_t held_seconds, char *out, size_t size)
{
    if (size > 0) {
        out[0] = '\0';
    }
    // A saved snapshot's countdown would need the hub's clock now, which isn't known.
    if (!item->has_resets_at || saved) {
        return;
    }
    const dh_reset r = dh_meter_item_reset(s, item, held_seconds);
    if (r.counting && r.resets_in_seconds < 60) {
        snprintf(out, size, "Resets in under a minute");
    } else if (r.counting) {
        char left[UI_TEXT_ROOM];
        ui_duration(r.resets_in_seconds, left, sizeof(left));
        snprintf(out, size, "Resets in %s", left);
    } else if (r.reset_due) {
        snprintf(out, size, "Reset due");
    }
}

static double clamp_percent(double percent)
{
    if (!(percent >= 0.0)) { // also NaN
        return 0.0;
    }
    return percent > 100.0 ? 100.0 : percent;
}

void ui_percent_text(bool has_percent, double percent, char *out, size_t size)
{
    if (!has_percent || !isfinite(percent)) {
        snprintf(out, size, "unknown");
        return;
    }
    const double p = clamp_percent(percent);
    // As the desktop app: whole numbers without a decimal place, anything else with one.
    if (p == floor(p)) {
        snprintf(out, size, "%.0f%%", p);
    } else {
        snprintf(out, size, "%.1f%%", p);
    }
}

int ui_percent_fill(bool has_percent, double percent)
{
    if (!has_percent || !isfinite(percent)) {
        return 0;
    }
    return (int)lround(clamp_percent(percent) * 10.0);
}

void ui_number_text(bool has_value, double value, unsigned decimals, char *out, size_t size)
{
    if (!has_value || !isfinite(value) || fabs(value) > DH_MAX_BIG_NUMBER_ABS) {
        snprintf(out, size, "%s", UI_DASH);
        return;
    }
    if (decimals > DH_MAX_DECIMALS) {
        decimals = DH_MAX_DECIMALS;
    }
    snprintf(out, size, "%.*f", (int)decimals, value);
    // A small negative figure that rounds to zero: "-0.0" says nothing a "0.0" doesn't.
    if (out[0] == '-' && strspn(out + 1, "0.") == strlen(out + 1)) {
        memmove(out, out + 1, strlen(out));
    }
}

void ui_big_number_text(const dh_big_number *b, char *out, size_t size)
{
    char figure[UI_NUMBER_ROOM];
    ui_number_text(b->has_value, b->value, b->decimals, figure, sizeof(figure));
    if (b->has_value && b->has_unit && b->unit[0] != '\0') {
        snprintf(out, size, "%s %s", figure, b->unit);
    } else {
        snprintf(out, size, "%s", figure);
    }
}

size_t ui_pick_size(const int32_t *sizes, size_t count, int32_t max_width,
                    int32_t (*width_at)(int32_t size, void *ctx), void *ctx)
{
    for (size_t i = 0; i < count; i++) {
        if (width_at(sizes[i], ctx) <= max_width) {
            return i;
        }
    }
    return count > 0 ? count - 1 : 0;
}

ui_form ui_form_of(const dh_page *p)
{
    if (p->cant_show != DH_CANT_SHOW_NONE) {
        return UI_FORM_CARD;
    }
    switch (p->content.shape) {
    case DH_SHAPE_NONE:
        return UI_FORM_NO_FIGURES;
    case DH_SHAPE_METER:
    case DH_SHAPE_BIG_NUMBER:
    case DH_SHAPE_TEXT:
    case DH_SHAPE_LIST:
        return UI_FORM_FIGURES;
    case DH_SHAPE_CUSTOM:
        break;
    }
    // This firmware draws no custom kinds, so one can only be a card.
    return UI_FORM_CARD;
}

size_t ui_card_lines(const dh_page *p, const char *lines[2])
{
    const dh_cant_show reason =
        p->cant_show != DH_CANT_SHOW_NONE ? p->cant_show : DH_CANT_SHOW_UNKNOWN_KIND;
    return dh_card_lines(reason, lines);
}

const char *ui_no_figures_text(const dh_page *p)
{
    return p->has_note && p->note[0] != '\0' ? p->note : "No data available";
}

size_t ui_view_count(size_t page_count)
{
    return (page_count == 0 ? 1 : page_count) + 1;
}

void ui_page_ids_of(const dh_snapshot *s, ui_page_ids *out)
{
    memset(out, 0, sizeof(*out));
    out->count = s->page_count < DH_MAX_PAGES ? s->page_count : DH_MAX_PAGES;
    for (size_t i = 0; i < out->count; i++) {
        snprintf(out->id[i], sizeof(out->id[i]), "%s", s->pages[i].id);
    }
}

// Where the page with this id is in the snapshot, or -1.
static long find_page(const dh_snapshot *s, const char *id)
{
    for (size_t i = 0; i < s->page_count && i < DH_MAX_PAGES; i++) {
        if (strcmp(s->pages[i].id, id) == 0) {
            return (long)i;
        }
    }
    return -1;
}

size_t ui_keep_view(const ui_page_ids *before, size_t view, const dh_snapshot *after)
{
    const size_t after_count = after->page_count < DH_MAX_PAGES ? after->page_count : DH_MAX_PAGES;
    const size_t status_after = ui_view_count(after_count) - 1;
    if (view >= ui_view_count(before->count) - 1) {
        return status_after;
    }
    if (before->count == 0 || after_count == 0) {
        return 0; // "No pages yet", or the first page of a list that has some at last
    }
    // The same page, or its nearest neighbour in the old row that is still there.
    for (size_t d = 0; d < before->count; d++) {
        if (view + d < before->count) {
            const long at = find_page(after, before->id[view + d]);
            if (at >= 0) {
                return (size_t)at;
            }
        }
        if (d > 0 && d <= view) {
            const long at = find_page(after, before->id[view - d]);
            if (at >= 0) {
                return (size_t)at;
            }
        }
    }
    return view < after_count ? view : after_count - 1;
}

const char *ui_storage_stopped_text(ui_storage_stop why)
{
// The last resort, the same every time.
#define IF_IT_KEEPS_HAPPENING \
    "Only if this keeps happening, use Install firmware in Display Hub on your computer, which " \
    "erases the display's storage."
    switch (why) {
    case UI_STORAGE_ERASE_UNFINISHED:
        // With the reset's mark in flash, so the next start does finish (or, after a second
        // fault, with a mark that couldn't be read: storage.h).
        return "This display's storage isn't working just now. It started erasing its setup "
               "and couldn't finish.\n\n"
               "Unplug it and plug it back in: it finishes the erase when it starts. "
               IF_IT_KEEPS_HAPPENING;
    case UI_STORAGE_ERASED:
        return "This display's storage isn't working just now. Its setup was erased.\n\n"
               "Unplug it and plug it back in, then choose Add a display in Display Hub on "
               "your computer. " IF_IT_KEEPS_HAPPENING;
    case UI_STORAGE_WRITE_FAILED:
        return "This display couldn't save to its storage, so it can't be used just now.\n\n"
               "Unplug it and plug it back in. " IF_IT_KEEPS_HAPPENING;
    default:
        // A fault that may pass, with nothing erased: a restart first, never a reinstall.
        return "This display's storage didn't start this time, so it can't be used just now. "
               "Nothing was erased.\n\n"
               "Unplug it and plug it back in. " IF_IT_KEEPS_HAPPENING;
    }
#undef IF_IT_KEEPS_HAPPENING
}

ui_line ui_line_pick(const ui_line_facts *facts)
{
    ui_line line = {.text = NULL, .tone = UI_LINE_QUIET, .problem = false};
    if (facts->problem != NULL) {
        line.text = facts->problem;
        line.tone = facts->problem_is_bad ? UI_LINE_BAD : UI_LINE_WARNING;
        line.problem = true;
    } else if (facts->token_unsaved) {
        line.text = UI_TOKEN_UNSAVED_LINE;
        line.tone = UI_LINE_WARNING;
        line.problem = true;
    } else if (facts->kept != NULL) {
        line.text = facts->kept;
        line.tone = UI_LINE_WARNING;
        line.problem = true;
    } else if (facts->update != NULL) {
        // An update is said plainly, but never in place of a problem.
        line.text = facts->update;
    } else if (facts->doing != NULL) {
        line.text = facts->doing;
    } else {
        line.text = facts->note;
    }
    return line;
}

const char *ui_notes_text(const char *kept, bool token_unsaved, const char *note, char *out,
                          size_t size)
{
    const char *const notes[] = {kept, token_unsaved ? UI_TOKEN_UNSAVED_NOTE : NULL,
                                 note != kept ? note : NULL};
    size_t at = 0;
    if (size == 0) {
        return NULL;
    }
    out[0] = '\0';
    for (size_t i = 0; i < sizeof(notes) / sizeof(notes[0]); i++) {
        if (notes[i] == NULL || notes[i][0] == '\0') {
            continue;
        }
        const size_t gap = at > 0 ? 2 : 0;
        const size_t len = strlen(notes[i]);
        if (len + gap >= size - at) {
            continue;
        }
        memcpy(out + at, "\n\n", gap);
        memcpy(out + at + gap, notes[i], len + 1);
        at += gap + len;
    }
    return at > 0 ? out : NULL;
}

void ui_shift(unsigned step, int32_t *dx, int32_t *dy)
{
    // The centre, then round a small square, one step at a time.
    static const int8_t offsets[][2] = {
        {0, 0}, {3, 0}, {3, 3}, {0, 3}, {-3, 3}, {-3, 0}, {-3, -3}, {0, -3}, {3, -3},
    };
    const unsigned n = (unsigned)(sizeof(offsets) / sizeof(offsets[0]));
    *dx = offsets[step % n][0];
    *dy = offsets[step % n][1];
}
