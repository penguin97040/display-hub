/*
 * Fuzzes the snapshot reader with the input as a 200 body and as an error body, and checks what
 * comes back is something a screen can draw safely.
 */
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

#include "dh_snapshot.h"

#define CHECK(c) \
    do { \
        if (!(c)) { \
            abort(); \
        } \
    } while (0)

static dh_snapshot snap;
static dh_text_scratch scratch;

static size_t bounded_length(const char *s, size_t room)
{
    size_t n = 0;
    while (n < room && s[n] != '\0') {
        n++;
    }
    return n;
}

static void check_text(const char *s, size_t room, size_t max_chars)
{
    size_t n = bounded_length(s, room);
    CHECK(n < room);
    CHECK(dh_text_check((const uint8_t *)s, n, max_chars, &scratch) == DH_TEXT_OK);
}

static void check_optional(bool has, const char *s, size_t room, size_t max_chars)
{
    if (has) {
        check_text(s, room, max_chars);
    }
}

static bool supports_one(const char *kind, void *ctx)
{
    (void)ctx;
    return strcmp(kind, "nearby-flights/1") == 0;
}

int LLVMFuzzerTestOneInput(const uint8_t *data, size_t size)
{
    dh_read_options opts = {supports_one, NULL};
    dh_response r = dh_read_response(200, data, size, &opts, &snap, &scratch);
    if (r.outcome == DH_OUTCOME_SHOW) {
        CHECK(snap.page_count <= DH_MAX_PAGES);
        CHECK(snap.refresh_after_seconds >= 30 && snap.refresh_after_seconds <= 3600);
        for (size_t i = 0; i < snap.page_count; i++) {
            const dh_page *p = &snap.pages[i];
            CHECK(bounded_length(p->id, sizeof p->id) < sizeof p->id && p->id[0] != '\0');
            check_text(p->title, sizeof p->title, DH_MAX_TITLE_CHARS);
            check_optional(p->has_note, p->note, sizeof p->note, DH_MAX_NOTE_CHARS);
            const char *lines[2];
            size_t n = dh_card_lines(p->cant_show, lines);
            CHECK(n <= 2);
            if (p->cant_show != DH_CANT_SHOW_NONE) {
                CHECK(p->content.shape == DH_SHAPE_NONE);
            }
            (void)dh_page_freshness(&snap, p, (int64_t)size);
            const dh_content *c = &p->content;
            switch (c->shape) {
            case DH_SHAPE_METER:
                CHECK(c->u.meter.count >= 1 && c->u.meter.count <= DH_MAX_METER_ITEMS);
                for (size_t k = 0; k < c->u.meter.count; k++) {
                    const dh_meter_item *m = &c->u.meter.items[k];
                    check_text(m->label, sizeof m->label, DH_MAX_LABEL_CHARS);
                    CHECK(!m->has_percent || (m->percent >= 0.0 && m->percent <= 100.0));
                    (void)dh_meter_item_reset(&snap, m, 0);
                }
                break;
            case DH_SHAPE_BIG_NUMBER: {
                const dh_big_number *b = &c->u.big_number;
                CHECK(bounded_length(b->text, sizeof b->text) < sizeof b->text);
                CHECK(b->decimals <= DH_MAX_DECIMALS);
                check_optional(b->has_unit, b->unit, sizeof b->unit, DH_MAX_UNIT_CHARS);
                check_optional(b->has_caption, b->caption, sizeof b->caption,
                               DH_MAX_CAPTION_CHARS);
                break;
            }
            case DH_SHAPE_TEXT:
                check_text(c->u.text.text, sizeof c->u.text.text, DH_MAX_TEXT_CHARS);
                break;
            case DH_SHAPE_LIST:
                CHECK(c->u.list.count <= DH_MAX_LIST_ITEMS);
                for (size_t k = 0; k < c->u.list.count; k++) {
                    const dh_list_item *l = &c->u.list.items[k];
                    check_text(l->text, sizeof l->text, DH_MAX_LIST_TEXT_CHARS);
                    check_optional(l->has_detail, l->detail, sizeof l->detail,
                                   DH_MAX_LIST_TEXT_CHARS);
                }
                break;
            case DH_SHAPE_CUSTOM:
                CHECK(c->u.custom.data_offset < size);
                CHECK(c->u.custom.data_length <= size - c->u.custom.data_offset);
                CHECK(data[c->u.custom.data_offset] == '{');
                break;
            case DH_SHAPE_NONE:
                break;
            }
        }
    }
    r = dh_read_response(503, data, size, NULL, &snap, &scratch);
    CHECK(r.outcome == DH_OUTCOME_RETRY || r.outcome == DH_OUTCOME_FAULT ||
          r.outcome == DH_OUTCOME_REPROVISION || r.outcome == DH_OUTCOME_INCOMPATIBLE);
    return 0;
}
