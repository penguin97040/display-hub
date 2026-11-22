/*
 * Tests for the screens' words and figures (firmware/main/ui_text.c): ages and countdowns on the
 * hub's clock, shares and big numbers (never 0 for unknown), how each page is drawn, which page
 * to keep when the pages change, and the burn-in shift. Pages are read with dh_core from real
 * snapshot bodies, so what is checked is what the firmware draws.
 */
#include <math.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "dh_snapshot.h"
#include "ui_text.h"

static int checks, failures;

#define CHECK(c) \
    do { \
        checks++; \
        if (!(c)) { \
            failures++; \
            fprintf(stderr, "%s:%d: CHECK(%s) failed\n", __FILE__, __LINE__, #c); \
        } \
    } while (0)

#define CHECK_STR(got, want) \
    do { \
        checks++; \
        if (strcmp((got), (want)) != 0) { \
            failures++; \
            fprintf(stderr, "%s:%d: got \"%s\", want \"%s\"\n", __FILE__, __LINE__, (got), \
                    (want)); \
        } \
    } while (0)

static dh_text_scratch scratch;
static dh_snapshot snap, snap2;
static char body[DH_MAX_SNAPSHOT_BYTES + 1];

#define GENERATED "2026-11-10T20:00:00Z"

/* Reads a snapshot with these pages (a JSON array's contents) into *out. */
static void read_pages(const char *pages, dh_snapshot *out)
{
    int n = snprintf(body, sizeof(body),
                     "{\"protocol_version\":1,\"message_type\":\"dashboard.snapshot\","
                     "\"generated_at\":\"" GENERATED "\",\"refresh_after_seconds\":300,"
                     "\"pages\":[%s]}",
                     pages);
    CHECK(n > 0 && (size_t)n < sizeof(body));
    dh_response r = dh_read_response(200, (const uint8_t *)body, (size_t)n, NULL, out, &scratch);
    CHECK(r.outcome == DH_OUTCOME_SHOW);
    if (r.outcome != DH_OUTCOME_SHOW) {
        fprintf(stderr, "unreadable snapshot (%d): %s\n", (int)r.unreadable, body);
        memset(out, 0, sizeof(*out));
    }
}

/* One page of the given kind, quality, updated time (or NULL) and data (or NULL). */
static void page_json(char *out, size_t size, const char *id, const char *kind,
                      const char *quality, const char *updated, unsigned stale_after,
                      const char *note, const char *data)
{
    char updated_s[40], note_s[200], data_s[3000];
    snprintf(updated_s, sizeof(updated_s), updated ? "\"%s\"" : "null", updated ? updated : "");
    snprintf(note_s, sizeof(note_s), note ? "\"%s\"" : "null", note ? note : "");
    snprintf(data_s, sizeof(data_s), "%s", data ? data : "null");
    snprintf(out, size,
             "{\"id\":\"%s\",\"title\":\"Page %s\",\"kind\":\"%s\",\"data_quality\":\"%s\","
             "\"updated_at\":%s,\"stale_after_seconds\":%u,\"note\":%s,\"data\":%s}",
             id, id, kind, quality, updated_s, stale_after, note_s, data_s);
}

static void test_duration(void)
{
    char s[UI_TEXT_ROOM];
    const struct {
        int64_t seconds;
        const char *want;
    } cases[] = {
        {-5, "0 s"},          {0, "0 s"},        {59, "59 s"},      {60, "1 min"},
        {3599, "59 min"},     {3600, "1 h"},     {12000, "3 h 20 min"}, {86399, "23 h 59 min"},
        {86400, "1 d"},       {187200, "2 d 4 h"}, {90000, "1 d 1 h"},
    };
    for (size_t i = 0; i < sizeof cases / sizeof cases[0]; i++) {
        ui_duration(cases[i].seconds, s, sizeof(s));
        CHECK_STR(s, cases[i].want);
    }
    ui_duration(INT64_MAX, s, sizeof(s));
    CHECK(strstr(s, " d") != NULL);
    char tiny[4];
    ui_duration(12000, tiny, sizeof(tiny)); /* cut short, never past the end */
    CHECK(strlen(tiny) == 3);
}

static void test_age(void)
{
    char pages[4][1200], all[5000];
    /* Ten minutes old when built, stale after half an hour. */
    page_json(pages[0], sizeof pages[0], "a", "text", "manual", "2026-11-10T19:50:00Z", 1800,
              NULL, "{\"text\":\"Hello\"}");
    /* Two days old: stale. */
    page_json(pages[1], sizeof pages[1], "b", "text", "provider_reported", "2026-11-08T20:00:00Z",
              3600, NULL, "{\"text\":\"Hello\"}");
    /* Never had figures. */
    page_json(pages[2], sizeof pages[2], "c", "meter", "unavailable", NULL, 600, "Not set up",
              NULL);
    snprintf(all, sizeof all, "%s,%s,%s", pages[0], pages[1], pages[2]);
    read_pages(all, &snap);
    CHECK(snap.page_count == 3);
    char s[UI_TEXT_ROOM];

    CHECK(ui_age_text(&snap, &snap.pages[0], false, 0, s, sizeof s) == UI_AGE_FRESH);
    CHECK_STR(s, "Updated 10 min ago");
    /* Time held by the display counts on the hub's clock. */
    CHECK(ui_age_text(&snap, &snap.pages[0], false, 1199, s, sizeof s) == UI_AGE_FRESH);
    CHECK_STR(s, "Updated 29 min ago");
    CHECK(ui_age_text(&snap, &snap.pages[0], false, 1201, s, sizeof s) == UI_AGE_STALE);
    CHECK_STR(s, "Stale. Updated 30 min ago");
    CHECK(ui_age_text(&snap, &snap.pages[1], false, 0, s, sizeof s) == UI_AGE_STALE);
    CHECK_STR(s, "Stale. Updated 2 d ago");
    CHECK(ui_age_text(&snap, &snap.pages[2], false, 0, s, sizeof s) == UI_AGE_NEVER);
    CHECK_STR(s, "Never updated");
    /* Pages restored from flash: the age isn't known, and nothing is guessed, stale or not. */
    for (size_t i = 0; i < 3; i++) {
        CHECK(ui_age_text(&snap, &snap.pages[i], true, 999999, s, sizeof s) == UI_AGE_UNKNOWN);
        CHECK_STR(s, "Saved; age unknown");
    }
    /* A negative held time (it can't be) counts as none. */
    CHECK(ui_age_text(&snap, &snap.pages[0], false, -100000, s, sizeof s) == UI_AGE_FRESH);
    CHECK_STR(s, "Updated 10 min ago");
}

static void test_meter(void)
{
    char page[1500];
    page_json(page, sizeof page, "m", "meter", "provider_reported", GENERATED, 1800, NULL,
              "{\"items\":["
              "{\"id\":\"a\",\"label\":\"Five-hour\",\"percent\":42.5,"
              "\"resets_at\":\"2026-11-10T23:20:00Z\"},"
              "{\"id\":\"b\",\"label\":\"Weekly\",\"percent\":null},"
              "{\"id\":\"c\",\"label\":\"Monthly\",\"resets_at\":\"2026-11-10T19:00:00Z\"},"
              "{\"id\":\"d\",\"label\":\"Full\",\"percent\":100},"
              "{\"id\":\"e\",\"label\":\"None\",\"percent\":0},"
              "{\"id\":\"f\",\"label\":\"Exactly now\",\"percent\":90,"
              "\"resets_at\":\"" GENERATED "\"}]}");
    read_pages(page, &snap);
    CHECK(snap.page_count == 1);
    const dh_page *p = &snap.pages[0];
    CHECK(ui_form_of(p) == UI_FORM_FIGURES);
    CHECK(p->content.shape == DH_SHAPE_METER && p->content.u.meter.count == 6);
    const dh_meter_item *it = p->content.u.meter.items;
    char s[UI_TEXT_ROOM];

    ui_percent_text(it[0].has_percent, it[0].percent, s, sizeof s);
    CHECK_STR(s, "42.5%");
    CHECK(ui_percent_fill(it[0].has_percent, it[0].percent) == 425);
    /* null and missing are both unknown: "unknown" and an empty bar, never 0%. */
    ui_percent_text(it[1].has_percent, it[1].percent, s, sizeof s);
    CHECK_STR(s, "unknown");
    CHECK(ui_percent_fill(it[1].has_percent, it[1].percent) == 0);
    ui_percent_text(it[2].has_percent, it[2].percent, s, sizeof s);
    CHECK_STR(s, "unknown");
    ui_percent_text(it[3].has_percent, it[3].percent, s, sizeof s);
    CHECK_STR(s, "100%");
    CHECK(ui_percent_fill(it[3].has_percent, it[3].percent) == 1000);
    ui_percent_text(it[4].has_percent, it[4].percent, s, sizeof s);
    CHECK_STR(s, "0%"); /* a real 0 is still 0 */

    ui_reset_text(&snap, &it[0], false, 0, s, sizeof s);
    CHECK_STR(s, "Resets in 3 h 20 min");
    ui_reset_text(&snap, &it[0], false, 1200, s, sizeof s); /* held 20 minutes */
    CHECK_STR(s, "Resets in 3 h");
    /* Under a minute: the screen refreshes every 30 s, so no seconds are counted. */
    ui_reset_text(&snap, &it[0], false, 11940, s, sizeof s); /* 60 s to go */
    CHECK_STR(s, "Resets in 1 min");
    ui_reset_text(&snap, &it[0], false, 11941, s, sizeof s); /* 59 s */
    CHECK_STR(s, "Resets in under a minute");
    ui_reset_text(&snap, &it[0], false, 11999, s, sizeof s); /* 1 s */
    CHECK_STR(s, "Resets in under a minute");
    ui_reset_text(&snap, &it[0], false, 12000, s, sizeof s);
    CHECK_STR(s, "Reset due");
    ui_reset_text(&snap, &it[2], false, 0, s, sizeof s); /* passed already */
    CHECK_STR(s, "Reset due");
    ui_reset_text(&snap, &it[5], false, 0, s, sizeof s); /* due this second */
    CHECK_STR(s, "Reset due");
    /* Its age, updated this second: no seconds counted under a minute either. */
    CHECK(ui_age_text(&snap, p, false, 0, s, sizeof s) == UI_AGE_FRESH);
    CHECK_STR(s, "Updated just now");
    CHECK(ui_age_text(&snap, p, false, 59, s, sizeof s) == UI_AGE_FRESH);
    CHECK_STR(s, "Updated just now");
    CHECK(ui_age_text(&snap, p, false, 60, s, sizeof s) == UI_AGE_FRESH);
    CHECK_STR(s, "Updated 1 min ago");
    ui_reset_text(&snap, &it[1], false, 0, s, sizeof s); /* no reset time */
    CHECK_STR(s, "");
    /* Saved pages: the hub's clock now isn't known, so no countdown is made up. */
    ui_reset_text(&snap, &it[0], true, 0, s, sizeof s);
    CHECK_STR(s, "");

    /* Out of range or not a number, in case one ever got this far. */
    ui_percent_text(true, NAN, s, sizeof s);
    CHECK_STR(s, "unknown");
    CHECK(ui_percent_fill(true, NAN) == 0);
    CHECK(ui_percent_fill(true, -5) == 0);
    CHECK(ui_percent_fill(true, 150) == 1000);
    ui_percent_text(true, 99.96, s, sizeof s);
    CHECK_STR(s, "100.0%");
}

static void test_numbers(void)
{
    char s[UI_NUMBER_ROOM];
    const struct {
        bool has;
        double value;
        unsigned decimals;
        const char *want;
    } cases[] = {
        {false, 0.0, 0, UI_DASH},
        {false, 5.0, 2, UI_DASH},
        {true, 0.0, 0, "0"},
        {true, 1234.0, 0, "1234"},
        {true, 1234.5, 1, "1234.5"},
        {true, 21.456, 1, "21.5"},
        {true, -1234.5678, 2, "-1234.57"},
        {true, -0.4, 0, "0"},
        {true, -0.0004, 3, "0.000"},
        {true, -0.0, 1, "0.0"},
        {true, -0.6, 0, "-1"},
        {true, 7.0, 3, "7.000"},
        {true, 7.0, 9, "7.000"}, /* more than 3 places can't come from the hub */
        {true, 1e15, 0, "1000000000000000"},
        {true, -1e15, 3, "-1000000000000000.000"},
        {true, 999999999999999.0, 2, "999999999999999.00"},
        {true, 1e15 * 1.0000001, 0, UI_DASH},
        {true, INFINITY, 0, UI_DASH},
        {true, -INFINITY, 0, UI_DASH},
        {true, NAN, 0, UI_DASH},
    };
    for (size_t i = 0; i < sizeof cases / sizeof cases[0]; i++) {
        ui_number_text(cases[i].has, cases[i].value, cases[i].decimals, s, sizeof s);
        CHECK_STR(s, cases[i].want);
    }
    /* The longest figure fits its room. */
    ui_number_text(true, -1e15, 3, s, sizeof s);
    CHECK(strlen(s) < UI_NUMBER_ROOM);

    /* Through dh_core: the figure with its unit, and the same figure dh_core writes. */
    char pages[3][800], all[2600];
    page_json(pages[0], sizeof pages[0], "n1", "big_number", "manual", GENERATED, 1800, NULL,
              "{\"value\":-1234567.891,\"decimals\":2,\"unit\":\"\xC2\xB0" "C\","
              "\"caption\":\"Outside\"}");
    page_json(pages[1], sizeof pages[1], "n2", "big_number", "estimated", GENERATED, 1800, NULL,
              "{\"value\":null,\"unit\":\"NZ$\"}");
    page_json(pages[2], sizeof pages[2], "n3", "big_number", "demo", GENERATED, 1800, NULL,
              "{\"value\":1200}");
    snprintf(all, sizeof all, "%s,%s,%s", pages[0], pages[1], pages[2]);
    read_pages(all, &snap);
    CHECK(snap.page_count == 3);
    char big[UI_BIG_NUMBER_ROOM];
    ui_big_number_text(&snap.pages[0].content.u.big_number, big, sizeof big);
    CHECK_STR(big, "-1234567.89 \xC2\xB0" "C");
    ui_number_text(true, snap.pages[0].content.u.big_number.value, 2, s, sizeof s);
    CHECK_STR(s, snap.pages[0].content.u.big_number.text);
    /* Unknown: a dash, and no unit after it. */
    ui_big_number_text(&snap.pages[1].content.u.big_number, big, sizeof big);
    CHECK_STR(big, UI_DASH);
    ui_big_number_text(&snap.pages[2].content.u.big_number, big, sizeof big);
    CHECK_STR(big, "1200");
}

static int32_t fake_width(int32_t size, void *ctx)
{
    return size * *(const int32_t *)ctx; /* a figure `ctx` characters long, a size wide each */
}

static void test_pick_size(void)
{
    const int32_t sizes[] = {88, 72, 60, 48, 38, 30};
    int32_t chars = 3;
    CHECK(ui_pick_size(sizes, 6, 336, fake_width, &chars) == 0); /* 264 fits */
    chars = 4;
    CHECK(ui_pick_size(sizes, 6, 336, fake_width, &chars) == 1); /* 352 no, 288 yes */
    chars = 9;
    CHECK(ui_pick_size(sizes, 6, 336, fake_width, &chars) == 5); /* 270 at 30 */
    chars = 30;
    CHECK(ui_pick_size(sizes, 6, 336, fake_width, &chars) == 5); /* none: the smallest, wrapped */
}

static void test_forms(void)
{
    char pages[6][800], all[5000];
    page_json(pages[0], sizeof pages[0], "k", "nearby-flights/1", "provider_reported", GENERATED,
              300, NULL, "{\"flights\":[]}");
    page_json(pages[1], sizeof pages[1], "bad", "meter", "manual", GENERATED, 300, "A note",
              "{\"items\":[]}");
    page_json(pages[2], sizeof pages[2], "u1", "big_number", "unavailable", NULL, 300,
              "Waiting for the helper", NULL);
    page_json(pages[3], sizeof pages[3], "u2", "list", "unavailable", NULL, 300, NULL, NULL);
    page_json(pages[4], sizeof pages[4], "q", "text", "from_the_future", NULL, 300, NULL, NULL);
    page_json(pages[5], sizeof pages[5], "l", "list", "manual", GENERATED, 300, NULL,
              "{\"items\":[]}");
    snprintf(all, sizeof all, "%s,%s,%s,%s,%s,%s", pages[0], pages[1], pages[2], pages[3],
             pages[4], pages[5]);
    read_pages(all, &snap);
    CHECK(snap.page_count == 6);
    const char *lines[2];

    /* An unknown kind: the card, with the update hint. */
    CHECK(ui_form_of(&snap.pages[0]) == UI_FORM_CARD);
    CHECK(ui_card_lines(&snap.pages[0], lines) == 2);
    CHECK_STR(lines[0], "This display can't show this page.");
    CHECK_STR(lines[1], "It may need a firmware update.");
    /* Data that breaks its shape: the card, one line only. */
    CHECK(ui_form_of(&snap.pages[1]) == UI_FORM_CARD);
    CHECK(ui_card_lines(&snap.pages[1], lines) == 1);
    CHECK_STR(lines[0], "This display can't show this page.");
    /* Unavailable with no data: its note, or a plain stand-in. */
    CHECK(ui_form_of(&snap.pages[2]) == UI_FORM_NO_FIGURES);
    CHECK_STR(ui_no_figures_text(&snap.pages[2]), "Waiting for the helper");
    CHECK_STR(dh_quality_label(snap.pages[2].quality), "UNAVAILABLE");
    CHECK(ui_form_of(&snap.pages[3]) == UI_FORM_NO_FIGURES);
    CHECK_STR(ui_no_figures_text(&snap.pages[3]), "No data available");
    /* A quality from a newer hub reads as unavailable. */
    CHECK(ui_form_of(&snap.pages[4]) == UI_FORM_NO_FIGURES);
    CHECK_STR(dh_quality_label(snap.pages[4].quality), "UNAVAILABLE");
    /* An empty list is a list. */
    CHECK(ui_form_of(&snap.pages[5]) == UI_FORM_FIGURES);
    CHECK(snap.pages[5].content.shape == DH_SHAPE_LIST && snap.pages[5].content.u.list.count == 0);

    /* The other labels. */
    CHECK_STR(dh_quality_label(DH_QUALITY_DEMO), "DEMO");
    CHECK_STR(dh_quality_label(DH_QUALITY_PROVIDER_REPORTED), "REPORTED");
    CHECK_STR(dh_quality_label(DH_QUALITY_ESTIMATED), "ESTIMATE");
    CHECK_STR(dh_quality_label(DH_QUALITY_MANUAL), "MANUAL");
    CHECK_STR(dh_quality_label(DH_QUALITY_UNRECOGNISED), "UNAVAILABLE");

    /* A custom kind this firmware was told it draws can still only be a card here. */
    dh_page custom = snap.pages[5];
    custom.content.shape = DH_SHAPE_CUSTOM;
    CHECK(ui_form_of(&custom) == UI_FORM_CARD);
    CHECK(ui_card_lines(&custom, lines) == 2);
}

/* A snapshot with pages of these ids, in order. */
static void with_ids(dh_snapshot *out, const char *const *ids, size_t n)
{
    char all[6000] = "";
    size_t used = 0;
    for (size_t i = 0; i < n; i++) {
        char one[600];
        page_json(one, sizeof one, ids[i], "text", "manual", GENERATED, 300, NULL,
                  "{\"text\":\"x\"}");
        used += (size_t)snprintf(all + used, sizeof all - used, "%s%s", i ? "," : "", one);
    }
    read_pages(all, out);
    CHECK(out->page_count == n);
}

static void test_keep_view(void)
{
    CHECK(ui_view_count(0) == 2); /* "No pages yet", then the status screen */
    CHECK(ui_view_count(1) == 2);
    CHECK(ui_view_count(10) == 11);

    ui_page_ids before;
    const char *abcde[] = {"a", "b", "c", "d", "e"};
    with_ids(&snap, abcde, 5);
    ui_page_ids_of(&snap, &before);
    CHECK(before.count == 5 && strcmp(before.id[4], "e") == 0);

    /* The same page, moved. */
    const char *moved[] = {"e", "d", "c", "b", "a"};
    with_ids(&snap2, moved, 5);
    CHECK(ui_keep_view(&before, 1, &snap2) == 3); /* b */
    /* Gone: the one after it, then the one before. */
    const char *no_c[] = {"a", "b", "d", "e"};
    with_ids(&snap2, no_c, 4);
    CHECK(ui_keep_view(&before, 2, &snap2) == 2); /* c gone: d */
    const char *no_cd[] = {"a", "b", "e"};
    with_ids(&snap2, no_cd, 3);
    CHECK(ui_keep_view(&before, 2, &snap2) == 1); /* c, d gone: b is nearer than e */
    const char *no_e[] = {"a", "b", "c", "d"};
    with_ids(&snap2, no_e, 4);
    CHECK(ui_keep_view(&before, 4, &snap2) == 3); /* the last went: the one before it */
    /* None of the old pages is left: the same place, as far as the new row goes. */
    const char *new_ones[] = {"x", "y"};
    with_ids(&snap2, new_ones, 2);
    CHECK(ui_keep_view(&before, 1, &snap2) == 1);
    CHECK(ui_keep_view(&before, 4, &snap2) == 1);
    /* The status screen stays the status screen, wherever it is now. */
    CHECK(ui_keep_view(&before, 5, &snap2) == 2);
    CHECK(ui_keep_view(&before, 99, &snap2) == 2);
    /* Down to no pages: "No pages yet", or the status screen if that was shown. */
    with_ids(&snap2, NULL, 0);
    CHECK(ui_keep_view(&before, 3, &snap2) == 0);
    CHECK(ui_keep_view(&before, 5, &snap2) == 1);
    /* From no pages to some. */
    ui_page_ids none;
    ui_page_ids_of(&snap2, &none);
    CHECK(none.count == 0);
    CHECK(ui_keep_view(&none, 0, &snap) == 0);
    CHECK(ui_keep_view(&none, 1, &snap) == 5);
    /* Unchanged. */
    for (size_t v = 0; v < 6; v++) {
        CHECK(ui_keep_view(&before, v, &snap) == v);
    }
}

static void test_shift(void)
{
    int32_t dx, dy, first_dx, first_dy;
    ui_shift(0, &first_dx, &first_dy);
    CHECK(first_dx == 0 && first_dy == 0);
    unsigned moved = 0, back_at = 0;
    for (unsigned step = 0; step < 1000; step++) {
        ui_shift(step, &dx, &dy);
        CHECK(dx >= -UI_SHIFT_MAX && dx <= UI_SHIFT_MAX);
        CHECK(dy >= -UI_SHIFT_MAX && dy <= UI_SHIFT_MAX);
        moved += dx != 0 || dy != 0;
        if (step > 0 && back_at == 0 && dx == 0 && dy == 0) {
            back_at = step;
        }
    }
    CHECK(moved > 500);              /* it does move */
    CHECK(back_at > 4 && back_at < 20); /* and comes round again */
    ui_shift(UINT32_MAX, &dx, &dy); /* no overflow at the end of the count */
    CHECK(dx >= -UI_SHIFT_MAX && dx <= UI_SHIFT_MAX);
}

/* The amber "Storage problem" screen: what it says for each reason storage is out of use. */
static void test_storage_stopped(void)
{
    static const ui_storage_stop all[] = {UI_STORAGE_NOT_STARTED, UI_STORAGE_ERASE_UNFINISHED,
                                          UI_STORAGE_ERASE_UNSURE, UI_STORAGE_ERASED,
                                          UI_STORAGE_WRITE_FAILED};
    for (size_t i = 0; i < sizeof all / sizeof all[0]; i++) {
        const char *text = ui_storage_stopped_text(all[i]);
        /* "Nothing was erased" only when nothing was. */
        CHECK((strstr(text, "Nothing was erased") != NULL) == (all[i] == UI_STORAGE_NOT_STARTED));
        /* Always a restart first, and Install firmware only as the last resort, after it. */
        const char *restart = strstr(text, "Unplug it and plug it back in");
        const char *reinstall = strstr(text, "Only if this keeps happening, use Install firmware");
        CHECK(restart != NULL && reinstall != NULL && restart < reinstall);
        /* Plain text, of a length the status screen shows without much scrolling. */
        CHECK(strlen(text) < 400);
        for (const char *c = text; *c != '\0'; c++) {
            CHECK(*c == '\n' || (*c >= 0x20 && *c < 0x7f));
        }
    }
    /* What was erased is said, and what the next start does about an erase cut short. */
    CHECK(strstr(ui_storage_stopped_text(UI_STORAGE_ERASED), "Its setup was erased") != NULL);
    CHECK(strstr(ui_storage_stopped_text(UI_STORAGE_ERASED), "Add a display") != NULL);
    CHECK(strstr(ui_storage_stopped_text(UI_STORAGE_ERASE_UNFINISHED), "couldn't finish") != NULL);
    CHECK(strstr(ui_storage_stopped_text(UI_STORAGE_ERASE_UNFINISHED),
                 "finishes the erase when it starts") != NULL);
    /* Only that one promises that a restart finishes the erase. A reset whose mark can't be
     * told to be in flash says the setup may still be there, and to reset again after the
     * restart. */
    for (size_t i = 0; i < sizeof all / sizeof all[0]; i++) {
        CHECK((strstr(ui_storage_stopped_text(all[i]), "finishes the erase") != NULL) ==
              (all[i] == UI_STORAGE_ERASE_UNFINISHED));
    }
    const char *unsure = ui_storage_stopped_text(UI_STORAGE_ERASE_UNSURE);
    const char *again = strstr(unsure, "hold the BOOT button for 10 seconds");
    CHECK(strstr(unsure, "may not have been erased") != NULL);
    CHECK(again != NULL && strstr(unsure, "Unplug it and plug it back in") < again);
    /* A write that failed isn't "didn't start", and promises nothing about what was erased. */
    CHECK(strstr(ui_storage_stopped_text(UI_STORAGE_WRITE_FAILED), "couldn't save") != NULL);
    CHECK(strstr(ui_storage_stopped_text(UI_STORAGE_WRITE_FAILED), "didn't start") == NULL);
    /* Anything else reads as the first. */
    CHECK(strcmp(ui_storage_stopped_text((ui_storage_stop)99),
                 ui_storage_stopped_text(UI_STORAGE_NOT_STARTED)) == 0);
}

/* The line at the foot of each page: a real problem is never hidden by a kept note. */
static void test_line(void)
{
    static const char KEPT[] = "Factory reset didn't happen";
    ui_line_facts f = {0};
    ui_line l = ui_line_pick(&f);
    CHECK(l.text == NULL && !l.problem && l.tone == UI_LINE_QUIET);

    /* Only news: the note, then what the display is doing, then an update. */
    f.note = "Factory reset cancelled.";
    l = ui_line_pick(&f);
    CHECK(l.text == f.note && !l.problem && l.tone == UI_LINE_QUIET);
    f.doing = "Joining Wi-Fi";
    l = ui_line_pick(&f);
    CHECK(l.text == f.doing && !l.problem && l.tone == UI_LINE_QUIET);
    f.update = "Updating its firmware";
    l = ui_line_pick(&f);
    CHECK(l.text == f.update && !l.problem && l.tone == UI_LINE_QUIET);

    /* A kept note comes ahead of all of that, as a problem. */
    f.kept = KEPT;
    l = ui_line_pick(&f);
    CHECK(l.text == KEPT && l.problem && l.tone == UI_LINE_WARNING);

    /* But not ahead of a token that isn't in flash, */
    f.token_unsaved = true;
    l = ui_line_pick(&f);
    CHECK(strcmp(l.text, UI_TOKEN_UNSAVED_LINE) == 0 && l.problem && l.tone == UI_LINE_WARNING);
    /* nor of a problem with the hub, in its own colour. */
    f.problem = "Can't reach the hub";
    l = ui_line_pick(&f);
    CHECK(l.text == f.problem && l.problem && l.tone == UI_LINE_WARNING);
    f.problem_is_bad = true;
    l = ui_line_pick(&f);
    CHECK(l.text == f.problem && l.problem && l.tone == UI_LINE_BAD);
    f.token_unsaved = false;
    l = ui_line_pick(&f);
    CHECK(l.text == f.problem && l.problem && l.tone == UI_LINE_BAD);

    /* With the problem gone, the kept note has the line again. */
    f.problem = NULL;
    l = ui_line_pick(&f);
    CHECK(l.text == KEPT && l.problem && l.tone == UI_LINE_WARNING);

    /* Every line fits the 80 characters the screen keeps for it. */
    CHECK(strlen(UI_TOKEN_UNSAVED_LINE) <= 80);
}

/* The status screen's notes: all that apply, none in place of another. */
static void test_notes(void)
{
    static const char KEPT[] = "The reset couldn't start, and nothing was erased.";
    static const char NOTE[] = "Its USB setup didn't start.";
    char out[UI_NOTES_ROOM];
    char small[60];

    CHECK(ui_notes_text(NULL, false, NULL, out, sizeof out) == NULL);
    CHECK(ui_notes_text(NULL, false, "", out, sizeof out) == NULL);
    CHECK(ui_notes_text(NULL, false, NOTE, out, sizeof out) == out && strcmp(out, NOTE) == 0);
    CHECK(ui_notes_text(KEPT, false, NULL, out, sizeof out) == out && strcmp(out, KEPT) == 0);
    CHECK(ui_notes_text(NULL, true, NULL, out, sizeof out) == out &&
          strcmp(out, UI_TOKEN_UNSAVED_NOTE) == 0);

    /* A kept note hides neither the token nor what didn't start: kept, token, note. */
    CHECK(ui_notes_text(KEPT, true, NOTE, out, sizeof out) == out);
    const char *kept = strstr(out, KEPT);
    const char *token = strstr(out, UI_TOKEN_UNSAVED_NOTE);
    const char *note = strstr(out, NOTE);
    CHECK(kept == out && token != NULL && note != NULL && kept < token && token < note);
    CHECK(strlen(out) == strlen(KEPT) + 2 + strlen(UI_TOKEN_UNSAVED_NOTE) + 2 + strlen(NOTE));
    CHECK(token != NULL && token[-1] == '\n' && token[-2] == '\n');
    CHECK(strstr(UI_TOKEN_UNSAVED_NOTE, "need setting up again") != NULL);

    /* The same note given twice is said once. */
    CHECK(ui_notes_text(KEPT, false, KEPT, out, sizeof out) == out && strcmp(out, KEPT) == 0);

    /* One that doesn't fit whole is left out, not cut short; the rest are still said. */
    CHECK(ui_notes_text(KEPT, true, NOTE, small, sizeof small) == small);
    CHECK(strcmp(small, KEPT) == 0);
    CHECK(ui_notes_text(NOTE, true, NULL, small, sizeof small) == small && strcmp(small, NOTE) == 0);
    CHECK(ui_notes_text(KEPT, false, NULL, small, strlen(KEPT)) == NULL);
    CHECK(ui_notes_text(KEPT, false, NULL, small, strlen(KEPT) + 1) == small);
    CHECK(ui_notes_text(KEPT, true, NOTE, small, 0) == NULL);

    /* The longest of each the firmware has, together, fit the room (main.c: the two reset
     * notes are under 200 bytes, and the start-up note's buffer is 200). */
    char longest[200];
    memset(longest, 'x', sizeof longest - 1);
    longest[sizeof longest - 1] = '\0';
    CHECK(ui_notes_text(longest, true, longest + 1, out, sizeof out) == out);
    CHECK(strlen(out) == 199 + 2 + strlen(UI_TOKEN_UNSAVED_NOTE) + 2 + 198);
}

static void test_unicode_example(void)
{
    /* The example with macrons in its titles reads, and keeps them as they are. */
    char path[1024];
    snprintf(path, sizeof path, "%s/examples/valid/unicode-titles.json", DH_PROTOCOL_DIR);
    FILE *f = fopen(path, "rb");
    CHECK(f != NULL);
    if (f == NULL) {
        return;
    }
    size_t n = fread(body, 1, sizeof(body) - 1, f);
    fclose(f);
    dh_response r = dh_read_response(200, (const uint8_t *)body, n, NULL, &snap, &scratch);
    CHECK(r.outcome == DH_OUTCOME_SHOW);
    CHECK(snap.page_count == 1);
    char s[UI_TEXT_ROOM];
    CHECK(ui_age_text(&snap, &snap.pages[0], false, 0, s, sizeof s) == UI_AGE_STALE);
    CHECK_STR(s, "Stale. Updated 1 h ago");
}

int main(void)
{
    test_duration();
    test_age();
    test_meter();
    test_numbers();
    test_pick_size();
    test_forms();
    test_keep_view();
    test_shift();
    test_storage_stopped();
    test_line();
    test_notes();
    test_unicode_example();
    printf("ui tests: %d checks, %d failures\n", checks, failures);
    return failures == 0 ? EXIT_SUCCESS : EXIT_FAILURE;
}
