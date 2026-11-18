/*
 * Prints what dh_core does with a hub response, as the "expect" object of a conformance vector
 * (protocol/v1/vectors/README.md), one JSON line per input. run_conformance.py compares them.
 *
 *   dh_conform vector <vector.json>...         reads each vector's response and held_seconds
 *   dh_conform body <status> <held> <file>...  reads each file as a raw response body
 */
#include <inttypes.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "dh_json.h"
#include "dh_snapshot.h"

static uint8_t *read_file(const char *path, size_t *len)
{
    FILE *f = fopen(path, "rb");
    if (!f) {
        return NULL;
    }
    size_t cap = 4096, n = 0;
    uint8_t *buf = malloc(cap);
    for (;;) {
        if (n == cap) {
            cap *= 2;
            uint8_t *bigger = realloc(buf, cap);
            if (!bigger) {
                free(buf);
                fclose(f);
                return NULL;
            }
            buf = bigger;
        }
        size_t got = fread(buf + n, 1, cap - n, f);
        if (got == 0) {
            break;
        }
        n += got;
    }
    fclose(f);
    *len = n;
    return buf;
}

static void put_string(const char *s)
{
    putchar('"');
    for (const unsigned char *p = (const unsigned char *)s; *p; p++) {
        if (*p == '"' || *p == '\\') {
            printf("\\%c", *p);
        } else if (*p < 0x20) {
            printf("\\u%04x", *p);
        } else {
            putchar(*p);
        }
    }
    putchar('"');
}

static void put_opt_string(bool has, const char *s)
{
    if (has) {
        put_string(s);
    } else {
        fputs("null", stdout);
    }
}

static void put_double(double d)
{
    printf("%.17g", d);
}

static void put_content(const dh_snapshot *s, const dh_page *p, int64_t held)
{
    const dh_content *c = &p->content;
    switch (c->shape) {
    case DH_SHAPE_METER:
        fputs("{\"shape\":\"meter\",\"items\":[", stdout);
        for (size_t i = 0; i < c->u.meter.count; i++) {
            const dh_meter_item *m = &c->u.meter.items[i];
            dh_reset r = dh_meter_item_reset(s, m, held);
            printf("%s{\"id\":", i ? "," : "");
            put_string(m->id);
            fputs(",\"label\":", stdout);
            put_string(m->label);
            fputs(",\"percent\":", stdout);
            if (m->has_percent) {
                put_double(m->percent);
            } else {
                fputs("null", stdout);
            }
            fputs(",\"resets_in_seconds\":", stdout);
            if (r.counting) {
                printf("%" PRId64, r.resets_in_seconds);
            } else {
                fputs("null", stdout);
            }
            printf(",\"reset_due\":%s}", r.reset_due ? "true" : "false");
        }
        fputs("]}", stdout);
        break;
    case DH_SHAPE_BIG_NUMBER: {
        const dh_big_number *b = &c->u.big_number;
        fputs("{\"shape\":\"big_number\",\"value\":", stdout);
        if (b->has_value) {
            put_double(b->value);
        } else {
            fputs("null", stdout);
        }
        fputs(",\"text\":", stdout);
        put_string(b->text);
        fputs(",\"unit\":", stdout);
        put_opt_string(b->has_unit, b->unit);
        fputs(",\"caption\":", stdout);
        put_opt_string(b->has_caption, b->caption);
        putchar('}');
        break;
    }
    case DH_SHAPE_TEXT:
        fputs("{\"shape\":\"text\",\"text\":", stdout);
        put_string(c->u.text.text);
        putchar('}');
        break;
    case DH_SHAPE_LIST:
        fputs("{\"shape\":\"list\",\"items\":[", stdout);
        for (size_t i = 0; i < c->u.list.count; i++) {
            const dh_list_item *l = &c->u.list.items[i];
            printf("%s{\"text\":", i ? "," : "");
            put_string(l->text);
            fputs(",\"detail\":", stdout);
            put_opt_string(l->has_detail, l->detail);
            putchar('}');
        }
        fputs("]}", stdout);
        break;
    case DH_SHAPE_CUSTOM:
    case DH_SHAPE_NONE:
        fputs("null", stdout);
        break;
    }
}

static dh_snapshot snap;
static dh_text_scratch scratch;

static void run(int status, const uint8_t *body, size_t len, int64_t held)
{
    dh_response r = dh_read_response(status, body, len, NULL, &snap, &scratch);
    printf("{\"outcome\":\"%s\",\"next_poll\":\"%s\",\"refresh_after_seconds\":",
           dh_outcome_name(r.outcome), dh_next_poll_name(r.next_poll));
    if (r.outcome == DH_OUTCOME_SHOW) {
        printf("%" PRIu32, snap.refresh_after_seconds);
    } else {
        fputs("null", stdout);
    }
    fputs(",\"error_code\":", stdout);
    const char *code = dh_error_code_name(r.error_code);
    put_opt_string(code != NULL, code ? code : "");
    fputs(",\"pages\":[", stdout);
    for (size_t i = 0; r.outcome == DH_OUTCOME_SHOW && i < snap.page_count; i++) {
        const dh_page *p = &snap.pages[i];
        dh_freshness f = dh_page_freshness(&snap, p, held);
        const char *lines[2];
        size_t nlines = dh_card_lines(p->cant_show, lines);
        printf("%s{\"id\":", i ? "," : "");
        put_string(p->id);
        fputs(",\"title\":", stdout);
        put_string(p->title);
        printf(",\"show\":\"%s\",\"cant_show_reason\":",
               p->cant_show == DH_CANT_SHOW_NONE ? "show" : "cant_show");
        const char *why = dh_cant_show_name(p->cant_show);
        put_opt_string(why != NULL, why ? why : "");
        fputs(",\"card_lines\":[", stdout);
        for (size_t k = 0; k < nlines; k++) {
            if (k) {
                putchar(',');
            }
            put_string(lines[k]);
        }
        fputs("],\"quality_label\":", stdout);
        put_string(dh_quality_label(p->quality));
        fputs(",\"note\":", stdout);
        put_opt_string(p->has_note, p->note);
        fputs(",\"age_seconds\":", stdout);
        if (f.known) {
            printf("%" PRId64, f.age_seconds);
        } else {
            fputs("null", stdout);
        }
        printf(",\"stale\":%s,\"content\":", f.stale ? "true" : "false");
        put_content(&snap, p, held);
        putchar('}');
    }
    fputs("]}\n", stdout);
}

/* Reads a vector file with dh_json itself. */
static int run_vector(const char *path)
{
    size_t len;
    uint8_t *file = read_file(path, &len);
    if (!file) {
        fprintf(stderr, "%s: can't read\n", path);
        return 1;
    }
    dh_json_doc doc;
    dh_json_value root, response, status, body, held;
    dh_json_status st = dh_json_parse(&doc, file, len, NULL, &root);
    uint64_t status_value;
    if (st != DH_JSON_OK || dh_json_object_find(&doc, root, "response", 8, &response) != 1 ||
        dh_json_object_find(&doc, response, "status", 6, &status) != 1 ||
        dh_json_object_find(&doc, response, "body", 4, &body) != 1 ||
        dh_json_object_find(&doc, root, "held_seconds", 12, &held) != 1 ||
        !dh_json_number_u64(&doc, status, &status_value) ||
        dh_json_type_of(&doc, body) != DH_JSON_STRING) {
        fprintf(stderr, "%s: not a vector (%s)\n", path, dh_json_status_name(st));
        free(file);
        return 1;
    }
    size_t body_cap = dh_json_string_length(&doc, body) + 1;
    char *text = malloc(body_cap);
    size_t body_len = 0;
    if (!text || !dh_json_string_copy(&doc, body, text, body_cap, &body_len)) {
        fprintf(stderr, "%s: can't decode the body\n", path);
        free(text);
        free(file);
        return 1;
    }
    double held_seconds = dh_json_number_double(&doc, held);
    run((int)status_value, (const uint8_t *)text, body_len, (int64_t)held_seconds);
    free(text);
    free(file);
    return 0;
}

int main(int argc, char **argv)
{
    int failures = 0;
    if (argc >= 3 && strcmp(argv[1], "vector") == 0) {
        for (int i = 2; i < argc; i++) {
            failures += run_vector(argv[i]);
        }
        return failures ? 1 : 0;
    }
    if (argc >= 5 && strcmp(argv[1], "body") == 0) {
        int status = atoi(argv[2]);
        int64_t held = strtoll(argv[3], NULL, 10);
        for (int i = 4; i < argc; i++) {
            size_t len;
            uint8_t *body = read_file(argv[i], &len);
            if (!body) {
                fprintf(stderr, "%s: can't read\n", argv[i]);
                failures++;
                continue;
            }
            run(status, body, len, held);
            free(body);
        }
        return failures ? 1 : 0;
    }
    fprintf(stderr, "usage: dh_conform vector <file>... | body <status> <held> <file>...\n");
    return 2;
}
