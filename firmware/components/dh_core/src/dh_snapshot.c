/*
 * Reading a snapshot or an error answer, as the reference display does (tools/display-simulator,
 * src/conform.rs, with crates/hub-protocol for the parsing).
 *
 * The reference reads JSON with serde, and some of what it accepts follows from that:
 *  - a member the reference knows (such as a page's "title") may appear only once, but inside a
 *    page's "data" and the "firmware_update" offer, which it reads as a generic JSON value first,
 *    a repeated member name is allowed and the last one wins;
 *  - a missing optional member and null are the same;
 *  - a number is read as a double wherever a figure is expected, but the integer fields
 *    (protocol_version, refresh_after_seconds, stale_after_seconds, size) take only a plain
 *    integer in range;
 *  - the 3072-byte page limit counts the page as serde writes it out again, compactly, with only
 *    the fields a page has, so unknown fields don't count and an absent note counts as null.
 */
#include "dh_snapshot.h"

#include <math.h>
#include <stdio.h>
#include <string.h>

#include "dh_hub.h"
#include "dh_json.h"
#include "dh_time.h"

#define LIT(s) s, (sizeof s - 1u)

static size_t find(const dh_json_doc *doc, dh_json_value obj, const char *name, size_t n,
                   dh_json_value *v)
{
    return dh_json_object_find(doc, obj, name, n, v);
}

/* Copies a string value if it fits; false otherwise (and for a value that isn't a string). */
static bool copy_string(const dh_json_doc *doc, dh_json_value v, char *buf, size_t cap,
                        size_t *len)
{
    if (dh_json_type_of(doc, v) != DH_JSON_STRING) {
        return false;
    }
    if (dh_json_string_length(doc, v) >= cap) {
        return false;
    }
    return dh_json_string_copy(doc, v, buf, cap, len);
}

/* A plain integer from 0 to UINT32_MAX, as serde reads a u32. */
static bool read_u32(const dh_json_doc *doc, dh_json_value v, uint32_t *out)
{
    uint64_t x;
    if (dh_json_type_of(doc, v) != DH_JSON_NUMBER ||
        dh_json_number_serde_class(doc, v) != DH_JSON_SERDE_U64 || !dh_json_number_u64(doc, v, &x) ||
        x > UINT32_MAX) {
        return false;
    }
    *out = (uint32_t)x;
    return true;
}

static bool read_time(const dh_json_doc *doc, dh_json_value v, int64_t *out)
{
    char buf[21];
    size_t len;
    return copy_string(doc, v, buf, sizeof buf, &len) && dh_time_parse(buf, len, out);
}

static bool is_null(const dh_json_doc *doc, dh_json_value v)
{
    return dh_json_type_of(doc, v) == DH_JSON_NULL;
}

/* Page and item ids: 1 to 64 of a-z, 0-9 and '-', not starting with '-'. */
static bool id_ok(const char *s, size_t n)
{
    if (n == 0 || n > DH_MAX_ID_BYTES || s[0] == '-') {
        return false;
    }
    for (size_t i = 0; i < n; i++) {
        char c = s[i];
        if (!((c >= 'a' && c <= 'z') || (c >= '0' && c <= '9') || c == '-')) {
            return false;
        }
    }
    return true;
}

static bool text_ok(const char *s, size_t n, size_t max, dh_text_scratch *scratch)
{
    return dh_text_check((const uint8_t *)s, n, max, scratch) == DH_TEXT_OK;
}

typedef enum { KIND_INVALID, KIND_SHAPE, KIND_CUSTOM } kind_class;

static bool kind_label_ok(const char *s, size_t n)
{
    if (n == 0) {
        return false;
    }
    for (size_t i = 0; i < n; i++) {
        char c = s[i];
        bool lower_or_digit = (c >= 'a' && c <= 'z') || (c >= '0' && c <= '9');
        if (!(lower_or_digit || (i > 0 && c == '-'))) {
            return false;
        }
    }
    return true;
}

/* ^[a-z0-9][a-z0-9-]*(\.[a-z0-9][a-z0-9-]*)*\/[1-9][0-9]{0,3}$, at most 64 bytes. */
static bool custom_kind_ok(const char *s, size_t n)
{
    if (n > DH_MAX_KIND_BYTES) {
        return false;
    }
    const char *slash = memchr(s, '/', n);
    if (!slash) {
        return false;
    }
    size_t name_len = (size_t)(slash - s);
    const char *ver = slash + 1;
    size_t ver_len = n - name_len - 1;
    if (ver_len < 1 || ver_len > 4 || ver[0] == '0') {
        return false;
    }
    for (size_t i = 0; i < ver_len; i++) {
        if (ver[i] < '0' || ver[i] > '9') {
            return false;
        }
    }
    size_t start = 0;
    for (size_t i = 0; i <= name_len; i++) {
        if (i == name_len || s[i] == '.') {
            if (!kind_label_ok(s + start, i - start)) {
                return false;
            }
            start = i + 1;
        }
    }
    return true;
}

static kind_class classify_kind(const char *s, size_t n)
{
    static const char *const shapes[] = {"meter", "big_number", "text", "list"};
    for (size_t i = 0; i < 4; i++) {
        if (strlen(shapes[i]) == n && memcmp(shapes[i], s, n) == 0) {
            return KIND_SHAPE;
        }
    }
    return custom_kind_ok(s, n) ? KIND_CUSTOM : KIND_INVALID;
}

static const struct {
    const char *name;
    dh_quality q;
} qualities[] = {
    {"demo", DH_QUALITY_DEMO},
    {"provider_reported", DH_QUALITY_PROVIDER_REPORTED},
    {"estimated", DH_QUALITY_ESTIMATED},
    {"manual", DH_QUALITY_MANUAL},
    {"unavailable", DH_QUALITY_UNAVAILABLE},
};

/* The name serde writes for a quality; "unrecognised" for one it didn't know. */
static const char *quality_wire_name(dh_quality q)
{
    for (size_t i = 0; i < sizeof qualities / sizeof qualities[0]; i++) {
        if (qualities[i].q == q) {
            return qualities[i].name;
        }
    }
    return "unrecognised";
}

const char *dh_quality_label(dh_quality q)
{
    switch (q) {
    case DH_QUALITY_DEMO: return "DEMO";
    case DH_QUALITY_PROVIDER_REPORTED: return "REPORTED";
    case DH_QUALITY_ESTIMATED: return "ESTIMATE";
    case DH_QUALITY_MANUAL: return "MANUAL";
    case DH_QUALITY_UNAVAILABLE:
    case DH_QUALITY_UNRECOGNISED: return "UNAVAILABLE";
    }
    return "UNAVAILABLE";
}

bool dh_quality_shows_figures(dh_quality q)
{
    return q != DH_QUALITY_UNAVAILABLE && q != DH_QUALITY_UNRECOGNISED;
}

size_t dh_card_lines(dh_cant_show reason, const char *lines[2])
{
    if (reason == DH_CANT_SHOW_NONE) {
        return 0;
    }
    lines[0] = DH_CANT_SHOW_TEXT;
    if (reason == DH_CANT_SHOW_UNKNOWN_KIND) {
        lines[1] = DH_CANT_SHOW_UPDATE_HINT;
        return 2;
    }
    return 1;
}

const char *dh_cant_show_name(dh_cant_show reason)
{
    switch (reason) {
    case DH_CANT_SHOW_NONE: return NULL;
    case DH_CANT_SHOW_UNKNOWN_KIND: return "unknown_kind";
    case DH_CANT_SHOW_BAD_DATA: return "bad_data";
    case DH_CANT_SHOW_TOO_LARGE: return "too_large";
    }
    return NULL;
}

/* ----- the error answer ----- */

static const struct {
    const char *name;
    dh_error_code code;
    dh_outcome outcome;
} error_codes[] = {
    {"unauthorised", DH_ERROR_UNAUTHORISED, DH_OUTCOME_FAULT},
    {"forbidden", DH_ERROR_FORBIDDEN, DH_OUTCOME_FAULT},
    {"token_revoked", DH_ERROR_TOKEN_REVOKED, DH_OUTCOME_REPROVISION},
    {"unknown_device", DH_ERROR_UNKNOWN_DEVICE, DH_OUTCOME_REPROVISION},
    {"enrolment_used", DH_ERROR_ENROLMENT_USED, DH_OUTCOME_REPROVISION},
    {"enrolment_expired", DH_ERROR_ENROLMENT_EXPIRED, DH_OUTCOME_REPROVISION},
    {"incompatible_version", DH_ERROR_INCOMPATIBLE_VERSION, DH_OUTCOME_INCOMPATIBLE},
    {"bad_request", DH_ERROR_BAD_REQUEST, DH_OUTCOME_FAULT},
    {"payload_too_large", DH_ERROR_PAYLOAD_TOO_LARGE, DH_OUTCOME_FAULT},
    {"not_found", DH_ERROR_NOT_FOUND, DH_OUTCOME_FAULT},
    {"conflict", DH_ERROR_CONFLICT, DH_OUTCOME_FAULT},
    {"rate_limited", DH_ERROR_RATE_LIMITED, DH_OUTCOME_RETRY},
    {"unavailable", DH_ERROR_UNAVAILABLE, DH_OUTCOME_RETRY},
};

const char *dh_error_code_name(dh_error_code c)
{
    if (c == DH_ERROR_NONE) {
        return NULL;
    }
    for (size_t i = 0; i < sizeof error_codes / sizeof error_codes[0]; i++) {
        if (error_codes[i].code == c) {
            return error_codes[i].name;
        }
    }
    return "unrecognised";
}

static dh_next_poll next_for(dh_outcome o)
{
    switch (o) {
    case DH_OUTCOME_SHOW: return DH_NEXT_REFRESH;
    case DH_OUTCOME_REPROVISION:
    case DH_OUTCOME_INCOMPATIBLE: return DH_NEXT_HOURLY;
    case DH_OUTCOME_UNREADABLE:
    case DH_OUTCOME_RETRY:
    case DH_OUTCOME_FAULT: return DH_NEXT_BACKOFF;
    }
    return DH_NEXT_BACKOFF;
}

static dh_response response(dh_outcome o)
{
    dh_response r;
    memset(&r, 0, sizeof r);
    r.outcome = o;
    r.next_poll = next_for(o);
    r.error_code = DH_ERROR_NONE;
    r.unreadable = DH_UNREADABLE_NONE;
    return r;
}

static dh_response unreadable(dh_unreadable why)
{
    dh_response r = response(DH_OUTCOME_UNREADABLE);
    r.unreadable = why;
    return r;
}

/* {"error": {"code": "...", "message": "..."}}, with anything else ignored. */
static bool read_error_body(const dh_json_doc *doc, dh_json_value root, size_t *which)
{
    dh_json_value e, code, message;
    if (dh_json_type_of(doc, root) != DH_JSON_OBJECT || find(doc, root, LIT("error"), &e) != 1 ||
        dh_json_type_of(doc, e) != DH_JSON_OBJECT || find(doc, e, LIT("code"), &code) != 1 ||
        dh_json_type_of(doc, code) != DH_JSON_STRING ||
        find(doc, e, LIT("message"), &message) != 1 ||
        dh_json_type_of(doc, message) != DH_JSON_STRING) {
        return false;
    }
    *which = sizeof error_codes / sizeof error_codes[0];
    for (size_t i = 0; i < sizeof error_codes / sizeof error_codes[0]; i++) {
        if (dh_json_string_equals(doc, code, error_codes[i].name, strlen(error_codes[i].name))) {
            *which = i;
            break;
        }
    }
    return true;
}

static dh_response read_error(int status, const uint8_t *body, size_t len, size_t max_bytes)
{
    if (len <= max_bytes) {
        dh_json_doc doc;
        dh_json_value root;
        dh_json_options o = {max_bytes, 0, 0};
        size_t which;
        if (dh_json_parse(&doc, body, len, &o, &root) == DH_JSON_OK &&
            read_error_body(&doc, root, &which)) {
            if (which == sizeof error_codes / sizeof error_codes[0]) {
                dh_response r = response(DH_OUTCOME_RETRY);
                r.error_code = DH_ERROR_UNRECOGNISED;
                return r;
            }
            dh_response r = response(error_codes[which].outcome);
            r.error_code = error_codes[which].code;
            return r;
        }
    }
    /* No structured error, such as a proxy's page: a server error is transient, others faults. */
    return response(status >= 500 ? DH_OUTCOME_RETRY : DH_OUTCOME_FAULT);
}

/* ----- pages ----- */

/* Where a page's members are, once its common fields have been read. */
typedef struct {
    dh_json_value page, id, title, kind, note, data;
    bool has_note, has_data;
    kind_class kind_class;
} page_refs;

typedef enum { READ_OK, READ_MALFORMED, READ_INVALID } read_result;

/* Reads a page's common fields: types first (a wrong one makes the snapshot malformed), then the
 * rules (which make it invalid). p may be NULL, for pages past the tenth, which only need their
 * types checked: the snapshot is refused for having too many anyway. */
static read_result read_page(const dh_json_doc *doc, dh_json_value v, dh_page *p, page_refs *r,
                             dh_text_scratch *scratch)
{
    static const char *const members[] = {"id", "title", "kind", "data_quality", "updated_at",
                                          "stale_after_seconds", "note", "data"};
    dh_json_value at[8];
    size_t count[8];
    if (dh_json_type_of(doc, v) != DH_JSON_OBJECT) {
        return READ_MALFORMED;
    }
    for (size_t i = 0; i < 8; i++) {
        count[i] = find(doc, v, members[i], strlen(members[i]), &at[i]);
        if (count[i] > 1) {
            return READ_MALFORMED; /* serde refuses a repeated field */
        }
    }
    /* Required strings: id, title, kind, data_quality. */
    for (size_t i = 0; i < 4; i++) {
        if (count[i] != 1 || dh_json_type_of(doc, at[i]) != DH_JSON_STRING) {
            return READ_MALFORMED;
        }
    }
    bool has_updated = count[4] == 1 && !is_null(doc, at[4]);
    int64_t updated = 0;
    if (has_updated && !read_time(doc, at[4], &updated)) {
        return READ_MALFORMED;
    }
    uint32_t stale = 0;
    if (count[5] != 1 || !read_u32(doc, at[5], &stale)) {
        return READ_MALFORMED;
    }
    bool has_note = count[6] == 1 && !is_null(doc, at[6]);
    if (has_note && dh_json_type_of(doc, at[6]) != DH_JSON_STRING) {
        return READ_MALFORMED;
    }
    bool has_data = count[7] == 1 && !is_null(doc, at[7]);

    dh_quality q = DH_QUALITY_UNRECOGNISED;
    for (size_t i = 0; i < sizeof qualities / sizeof qualities[0]; i++) {
        if (dh_json_string_equals(doc, at[3], qualities[i].name, strlen(qualities[i].name))) {
            q = qualities[i].q;
        }
    }
    if (!p) {
        return READ_OK;
    }

    memset(p, 0, sizeof *p);
    r->page = v;
    r->id = at[0];
    r->title = at[1];
    r->kind = at[2];
    r->note = at[6];
    r->data = at[7];
    r->has_note = has_note;
    r->has_data = has_data;
    p->quality = q;
    p->has_updated_at = has_updated;
    p->updated_at = updated;
    p->stale_after_seconds = stale;
    p->has_note = has_note;

    size_t len;
    char kind[DH_MAX_KIND_BYTES + 1];
    r->kind_class = copy_string(doc, at[2], kind, sizeof kind, &len) ? classify_kind(kind, len)
                                                                     : KIND_INVALID;
    if (r->kind_class != KIND_INVALID) {
        memcpy(p->kind, kind, len + 1);
    }

    bool ok = copy_string(doc, at[0], p->id, sizeof p->id, &len) && id_ok(p->id, len);
    ok = ok && copy_string(doc, at[1], p->title, sizeof p->title, &len) &&
         text_ok(p->title, len, DH_MAX_TITLE_CHARS, scratch);
    if (has_note) {
        ok = ok && copy_string(doc, at[6], p->note, sizeof p->note, &len) &&
             text_ok(p->note, len, DH_MAX_NOTE_CHARS, scratch);
    }
    ok = ok && stale >= DH_STALE_MIN_SECONDS && stale <= DH_STALE_MAX_SECONDS;
    /* Figures need a time, unless the quality is one this display doesn't know. */
    ok = ok && (has_updated || !dh_quality_shows_figures(q));
    return ok ? READ_OK : READ_INVALID;
}

static size_t u32_digits(uint32_t v)
{
    size_t n = 1;
    while (v >= 10) {
        v /= 10;
        n++;
    }
    return n;
}

/* The page as serde writes it compactly: these members, in this order, and "data" only when
 * there is some. Returns the size, or more than DH_MAX_PAGE_BYTES; *depth is how deeply the data
 * nests, when the size is within the limit. */
static size_t page_size(const dh_json_doc *doc, const dh_page *p, const page_refs *r,
                        unsigned *depth)
{
    size_t n = sizeof "{\"id\":" - 1 + dh_json_serde_string_length(doc, r->id);
    n += sizeof ",\"title\":" - 1 + dh_json_serde_string_length(doc, r->title);
    n += sizeof ",\"kind\":" - 1 + dh_json_serde_string_length(doc, r->kind);
    n += sizeof ",\"data_quality\":" - 1 + strlen(quality_wire_name(p->quality)) + 2;
    n += sizeof ",\"updated_at\":" - 1 + (p->has_updated_at ? 22u : 4u);
    n += sizeof ",\"stale_after_seconds\":" - 1 + u32_digits(p->stale_after_seconds);
    n += sizeof ",\"note\":" - 1 + (r->has_note ? dh_json_serde_string_length(doc, r->note) : 4u);
    n += 1; /* } */
    *depth = 0;
    if (r->has_data && n <= DH_MAX_PAGE_BYTES) {
        n += sizeof ",\"data\":" - 1;
        if (n <= DH_MAX_PAGE_BYTES) {
            size_t data;
            dh_json_serde_measure(doc, r->data, DH_MAX_PAGE_BYTES - n, &data, depth);
            n += data;
        }
    }
    return n;
}

/* An optional member: absent and null are the same. True if present. */
static bool optional(const dh_json_doc *doc, dh_json_value obj, const char *name, size_t n,
                     dh_json_value *v)
{
    return find(doc, obj, name, n, v) > 0 && !is_null(doc, *v);
}

/* Optional text: false if it is there but isn't a string, or breaks the text rules. */
static bool optional_text(const dh_json_doc *doc, dh_json_value obj, const char *name, size_t n,
                          char *buf, size_t cap, size_t max_chars, bool *has,
                          dh_text_scratch *scratch)
{
    dh_json_value v;
    size_t len;
    *has = optional(doc, obj, name, n, &v);
    if (!*has) {
        return true;
    }
    return copy_string(doc, v, buf, cap, &len) && text_ok(buf, len, max_chars, scratch);
}

static bool required_text(const dh_json_doc *doc, dh_json_value obj, const char *name, size_t n,
                          char *buf, size_t cap, size_t max_chars, dh_text_scratch *scratch)
{
    dh_json_value v;
    size_t len;
    return find(doc, obj, name, n, &v) > 0 && copy_string(doc, v, buf, cap, &len) &&
           text_ok(buf, len, max_chars, scratch);
}

/* An optional figure: false if it is there but isn't a number. */
static bool optional_number(const dh_json_doc *doc, dh_json_value obj, const char *name,
                            size_t n, bool *has, double *out)
{
    dh_json_value v;
    *has = optional(doc, obj, name, n, &v);
    if (!*has) {
        return true;
    }
    if (dh_json_type_of(doc, v) != DH_JSON_NUMBER) {
        return false;
    }
    *out = dh_json_number_double(doc, v);
    return true;
}

/* The items of a meter or list: an array of objects, min to max of them. */
static bool items_of(const dh_json_doc *doc, dh_json_value data, size_t min, size_t max,
                     dh_json_value *items, size_t *count)
{
    if (find(doc, data, LIT("items"), items) == 0 ||
        dh_json_type_of(doc, *items) != DH_JSON_ARRAY) {
        return false;
    }
    *count = dh_json_array_length(doc, *items);
    if (*count < min || *count > max) {
        return false;
    }
    dh_json_iter it;
    dh_json_value e;
    (void)dh_json_array_begin(doc, *items, &it);
    while (dh_json_array_next(doc, &it, &e)) {
        if (dh_json_type_of(doc, e) != DH_JSON_OBJECT) {
            return false;
        }
    }
    return true;
}

static bool read_meter(const dh_json_doc *doc, dh_json_value data, dh_content *c,
                       dh_text_scratch *scratch)
{
    dh_json_value items, e;
    size_t count;
    if (!items_of(doc, data, 1, DH_MAX_METER_ITEMS, &items, &count)) {
        return false;
    }
    c->u.meter.count = count;
    dh_json_iter it;
    size_t i = 0;
    (void)dh_json_array_begin(doc, items, &it);
    while (dh_json_array_next(doc, &it, &e)) {
        dh_meter_item *m = &c->u.meter.items[i++];
        dh_json_value v;
        size_t len;
        if (find(doc, e, LIT("id"), &v) == 0 || !copy_string(doc, v, m->id, sizeof m->id, &len) ||
            !id_ok(m->id, len)) {
            return false;
        }
        if (!required_text(doc, e, LIT("label"), m->label, sizeof m->label, DH_MAX_LABEL_CHARS,
                           scratch)) {
            return false;
        }
        if (!optional_number(doc, e, LIT("percent"), &m->has_percent, &m->percent) ||
            (m->has_percent && !(m->percent >= 0.0 && m->percent <= 100.0))) {
            return false;
        }
        m->has_resets_at = optional(doc, e, LIT("resets_at"), &v);
        if (m->has_resets_at && !read_time(doc, v, &m->resets_at)) {
            return false;
        }
    }
    for (size_t a = 0; a < count; a++) {
        for (size_t b = a + 1; b < count; b++) {
            if (strcmp(c->u.meter.items[a].id, c->u.meter.items[b].id) == 0) {
                return false;
            }
        }
    }
    return true;
}

static bool read_big_number(const dh_json_doc *doc, dh_json_value data, dh_content *c,
                            dh_text_scratch *scratch)
{
    dh_big_number *b = &c->u.big_number;
    double decimals = 0.0;
    bool has_decimals;
    if (!optional_number(doc, data, LIT("value"), &b->has_value, &b->value) ||
        !optional_number(doc, data, LIT("decimals"), &has_decimals, &decimals)) {
        return false;
    }
    if (has_decimals) {
        /* A whole number from 0 to 255 (2.0 counts), then at most 3. */
        if (!(decimals >= 0.0 && decimals <= 255.0) || decimals != (double)(unsigned)decimals) {
            return false;
        }
        if (decimals > DH_MAX_DECIMALS) {
            return false;
        }
        b->decimals = (uint8_t)decimals;
    }
    if (b->has_value && !(fabs(b->value) <= DH_MAX_BIG_NUMBER_ABS)) {
        return false;
    }
    if (!optional_text(doc, data, LIT("unit"), b->unit, sizeof b->unit, DH_MAX_UNIT_CHARS,
                       &b->has_unit, scratch) ||
        !optional_text(doc, data, LIT("caption"), b->caption, sizeof b->caption,
                       DH_MAX_CAPTION_CHARS, &b->has_caption, scratch)) {
        return false;
    }
    if (b->has_value) {
        (void)snprintf(b->text, sizeof b->text, "%.*f", (int)b->decimals, b->value);
    } else {
        memcpy(b->text, "-", 2);
    }
    return true;
}

static bool read_text(const dh_json_doc *doc, dh_json_value data, dh_content *c,
                      dh_text_scratch *scratch)
{
    return required_text(doc, data, LIT("text"), c->u.text.text, sizeof c->u.text.text,
                         DH_MAX_TEXT_CHARS, scratch);
}

static bool read_list(const dh_json_doc *doc, dh_json_value data, dh_content *c,
                      dh_text_scratch *scratch)
{
    dh_json_value items, e;
    size_t count;
    if (!items_of(doc, data, 0, DH_MAX_LIST_ITEMS, &items, &count)) {
        return false;
    }
    c->u.list.count = count;
    dh_json_iter it;
    size_t i = 0;
    (void)dh_json_array_begin(doc, items, &it);
    while (dh_json_array_next(doc, &it, &e)) {
        dh_list_item *l = &c->u.list.items[i++];
        if (!required_text(doc, e, LIT("text"), l->text, sizeof l->text, DH_MAX_LIST_TEXT_CHARS,
                           scratch) ||
            !optional_text(doc, e, LIT("detail"), l->detail, sizeof l->detail,
                           DH_MAX_LIST_TEXT_CHARS, &l->has_detail, scratch)) {
            return false;
        }
    }
    return true;
}

/* What the display draws for one page of an accepted snapshot. */
static void read_content(const dh_json_doc *doc, dh_page *p, const page_refs *r,
                         const dh_read_options *opts, dh_text_scratch *scratch)
{
    unsigned depth;
    memset(&p->content, 0, sizeof p->content);
    p->content.shape = DH_SHAPE_NONE;
    if (page_size(doc, p, r, &depth) > DH_MAX_PAGE_BYTES) {
        p->cant_show = DH_CANT_SHOW_TOO_LARGE;
        return;
    }
    if (p->quality == DH_QUALITY_UNAVAILABLE && r->has_data) {
        p->cant_show = DH_CANT_SHOW_BAD_DATA; /* an unavailable page carries no data */
        return;
    }
    if (!dh_quality_shows_figures(p->quality)) {
        return; /* the title and note only, whatever the kind */
    }
    if (r->kind_class == KIND_INVALID) {
        p->cant_show = DH_CANT_SHOW_UNKNOWN_KIND;
        return;
    }
    if (!r->has_data) {
        p->cant_show = DH_CANT_SHOW_BAD_DATA;
        return;
    }
    if (dh_json_type_of(doc, r->data) != DH_JSON_OBJECT) {
        p->cant_show = DH_CANT_SHOW_BAD_DATA; /* every kind's data is an object */
        return;
    }
    if (r->kind_class == KIND_CUSTOM) {
        if (depth > DH_MAX_CUSTOM_DEPTH) {
            p->cant_show = DH_CANT_SHOW_BAD_DATA;
        } else if (opts && opts->custom_kind_supported &&
                   opts->custom_kind_supported(p->kind, opts->ctx)) {
            p->content.shape = DH_SHAPE_CUSTOM;
            p->content.u.custom.data_offset = r->data;
            p->content.u.custom.data_length = dh_json_end(doc, r->data) - r->data;
        } else {
            p->cant_show = DH_CANT_SHOW_UNKNOWN_KIND;
        }
        return;
    }
    bool ok;
    dh_shape shape;
    if (strcmp(p->kind, "meter") == 0) {
        shape = DH_SHAPE_METER;
        ok = read_meter(doc, r->data, &p->content, scratch);
    } else if (strcmp(p->kind, "big_number") == 0) {
        shape = DH_SHAPE_BIG_NUMBER;
        ok = read_big_number(doc, r->data, &p->content, scratch);
    } else if (strcmp(p->kind, "text") == 0) {
        shape = DH_SHAPE_TEXT;
        ok = read_text(doc, r->data, &p->content, scratch);
    } else {
        shape = DH_SHAPE_LIST;
        ok = read_list(doc, r->data, &p->content, scratch);
    }
    if (!ok) {
        memset(&p->content, 0, sizeof p->content);
        p->content.shape = DH_SHAPE_NONE;
        p->cant_show = DH_CANT_SHOW_BAD_DATA;
        return;
    }
    p->content.shape = shape;
}

/* ----- the snapshot ----- */

static bool lower_hex(const char *s, size_t n, size_t want)
{
    if (n != want) {
        return false;
    }
    for (size_t i = 0; i < n; i++) {
        if (!((s[i] >= '0' && s[i] <= '9') || (s[i] >= 'a' && s[i] <= 'f'))) {
            return false;
        }
    }
    return true;
}

/* The offer, read leniently: anything that doesn't read reads as no offer. */
static void read_offer(const dh_json_doc *doc, dh_json_value v, dh_firmware_offer *o)
{
    dh_json_value offer, package, size;
    memset(o, 0, sizeof *o);
    if (dh_json_type_of(doc, v) != DH_JSON_OBJECT || find(doc, v, LIT("offer_id"), &offer) == 0 ||
        dh_json_type_of(doc, offer) != DH_JSON_STRING ||
        find(doc, v, LIT("package_id"), &package) == 0 ||
        dh_json_type_of(doc, package) != DH_JSON_STRING || find(doc, v, LIT("size"), &size) == 0 ||
        dh_json_type_of(doc, size) != DH_JSON_NUMBER ||
        dh_json_number_serde_class(doc, size) != DH_JSON_SERDE_U64 ||
        !dh_json_number_u64(doc, size, &o->size)) {
        o->size = 0;
        return;
    }
    o->present = true;
    size_t ol, pl;
    o->usable = copy_string(doc, offer, o->offer_id, sizeof o->offer_id, &ol) &&
                copy_string(doc, package, o->package_id, sizeof o->package_id, &pl) &&
                lower_hex(o->offer_id, ol, DH_OFFER_ID_HEX_LEN) &&
                lower_hex(o->package_id, pl, DH_PACKAGE_ID_HEX_LEN) && o->size >= 1 &&
                o->size <= DH_MAX_FIRMWARE_UPDATE_BYTES;
    if (!o->usable) {
        o->offer_id[0] = '\0';
        o->package_id[0] = '\0';
    }
}

static dh_response read_snapshot(const uint8_t *body, size_t len, const dh_read_options *opts,
                                 dh_snapshot *out, dh_text_scratch *scratch)
{
    if (len > DH_MAX_SNAPSHOT_BYTES) {
        return unreadable(DH_UNREADABLE_TOO_LARGE);
    }
    dh_json_doc doc;
    dh_json_value root;
    dh_json_options o = {DH_MAX_SNAPSHOT_BYTES, 0, 0};
    dh_json_status st = dh_json_parse(&doc, body, len, &o, &root);
    if (st != DH_JSON_OK) {
        dh_response r = unreadable(DH_UNREADABLE_NOT_JSON);
        r.json_status = (int)st;
        return r;
    }
    if (dh_json_type_of(&doc, root) != DH_JSON_OBJECT) {
        return unreadable(DH_UNREADABLE_MALFORMED);
    }

    /* The version first, as the reference does: a snapshot from a newer protocol is reported as
     * such, whatever else is in it, as long as the version and message type read. */
    dh_json_value pv, mt;
    uint32_t version;
    if (find(&doc, root, LIT("protocol_version"), &pv) != 1 || !read_u32(&doc, pv, &version)) {
        return unreadable(DH_UNREADABLE_MALFORMED);
    }
    size_t mt_count = find(&doc, root, LIT("message_type"), &mt);
    if (mt_count > 1 || (mt_count == 1 && !is_null(&doc, mt) &&
                         dh_json_type_of(&doc, mt) != DH_JSON_STRING)) {
        return unreadable(DH_UNREADABLE_MALFORMED);
    }
    if (version != DH_PROTOCOL_VERSION) {
        return response(DH_OUTCOME_INCOMPATIBLE);
    }
    if (mt_count != 1 || is_null(&doc, mt) || !dh_json_string_equals(&doc, mt, LIT(DH_MESSAGE_TYPE))) {
        return unreadable(DH_UNREADABLE_MESSAGE_TYPE);
    }

    memset(out, 0, sizeof *out);
    bool invalid = false;
    dh_json_value gen, refresh, pages, offer;
    if (find(&doc, root, LIT("generated_at"), &gen) != 1 ||
        !read_time(&doc, gen, &out->generated_at) ||
        find(&doc, root, LIT("refresh_after_seconds"), &refresh) != 1 ||
        !read_u32(&doc, refresh, &out->refresh_after_seconds) ||
        find(&doc, root, LIT("pages"), &pages) != 1 ||
        dh_json_type_of(&doc, pages) != DH_JSON_ARRAY) {
        return unreadable(DH_UNREADABLE_MALFORMED);
    }
    size_t offer_count = find(&doc, root, LIT("firmware_update"), &offer);
    if (offer_count > 1) {
        return unreadable(DH_UNREADABLE_MALFORMED);
    }
    if (out->refresh_after_seconds < DH_REFRESH_MIN_SECONDS ||
        out->refresh_after_seconds > DH_REFRESH_MAX_SECONDS) {
        invalid = true;
    }

    page_refs refs[DH_MAX_PAGES];
    dh_json_iter it;
    dh_json_value pv_page;
    size_t n = 0;
    (void)dh_json_array_begin(&doc, pages, &it);
    while (dh_json_array_next(&doc, &it, &pv_page)) {
        dh_page *p = n < DH_MAX_PAGES ? &out->pages[n] : NULL;
        read_result rr = read_page(&doc, pv_page, p, n < DH_MAX_PAGES ? &refs[n] : NULL, scratch);
        if (rr == READ_MALFORMED) {
            return unreadable(DH_UNREADABLE_MALFORMED);
        }
        if (rr == READ_INVALID) {
            invalid = true;
        }
        n++;
    }
    if (n > DH_MAX_PAGES) {
        return unreadable(DH_UNREADABLE_INVALID);
    }
    out->page_count = n;
    for (size_t i = 0; i < n && !invalid; i++) {
        const dh_page *p = &out->pages[i];
        if (p->has_updated_at && p->updated_at > out->generated_at) {
            invalid = true;
        }
        for (size_t j = 0; j < i; j++) {
            if (strcmp(out->pages[j].id, p->id) == 0) {
                invalid = true;
            }
        }
    }
    if (invalid) {
        return unreadable(DH_UNREADABLE_INVALID);
    }

    if (offer_count == 1) {
        read_offer(&doc, offer, &out->firmware_update);
    }
    for (size_t i = 0; i < n; i++) {
        read_content(&doc, &out->pages[i], &refs[i], opts, scratch);
    }
    return response(DH_OUTCOME_SHOW);
}

dh_response dh_read_response(int http_status, const uint8_t *body, size_t len,
                             const dh_read_options *opts, dh_snapshot *out,
                             dh_text_scratch *scratch)
{
    if (http_status == 200) {
        return read_snapshot(body, len, opts, out, scratch);
    }
    return read_error(http_status, body, len, DH_MAX_SNAPSHOT_BYTES);
}

/* Declared in dh_hub.h: an error answer from any route, with the smaller limit. */
dh_response dh_read_error_response(int http_status, const uint8_t *body, size_t len)
{
    return read_error(http_status, body, len, DH_MAX_HUB_ANSWER_BYTES);
}

/* ----- on the hub's clock ----- */

/* a + b, held at the ends of the range rather than overflowing. Timestamps are within years 0
 * to 9999, so only a held time from a confused caller could get there. */
static int64_t add_saturating(int64_t a, int64_t b)
{
    if (b > 0 && a > INT64_MAX - b) {
        return INT64_MAX;
    }
    if (b < 0 && a < INT64_MIN - b) {
        return INT64_MIN;
    }
    return a + b;
}

dh_freshness dh_page_freshness(const dh_snapshot *s, const dh_page *p, int64_t held_seconds)
{
    dh_freshness f = {false, 0, false};
    if (!p->has_updated_at) {
        return f;
    }
    int64_t behind = s->generated_at - p->updated_at;
    f.known = true;
    f.age_seconds = add_saturating(behind > 0 ? behind : 0, held_seconds > 0 ? held_seconds : 0);
    f.stale = f.age_seconds > (int64_t)p->stale_after_seconds;
    return f;
}

dh_reset dh_meter_item_reset(const dh_snapshot *s, const dh_meter_item *item,
                             int64_t held_seconds)
{
    dh_reset r = {false, 0, false};
    if (!item->has_resets_at) {
        return r;
    }
    int64_t now = add_saturating(s->generated_at, held_seconds);
    int64_t left = now == INT64_MIN ? INT64_MAX : add_saturating(item->resets_at, -now);
    if (left > 0) {
        r.counting = true;
        r.resets_in_seconds = left;
    } else {
        r.reset_due = true;
    }
    return r;
}

const char *dh_outcome_name(dh_outcome o)
{
    switch (o) {
    case DH_OUTCOME_SHOW: return "show";
    case DH_OUTCOME_UNREADABLE: return "unreadable";
    case DH_OUTCOME_REPROVISION: return "reprovision";
    case DH_OUTCOME_INCOMPATIBLE: return "incompatible";
    case DH_OUTCOME_RETRY: return "retry";
    case DH_OUTCOME_FAULT: return "fault";
    }
    return "unknown";
}

const char *dh_next_poll_name(dh_next_poll n)
{
    switch (n) {
    case DH_NEXT_REFRESH: return "refresh";
    case DH_NEXT_BACKOFF: return "backoff";
    case DH_NEXT_HOURLY: return "hourly";
    }
    return "unknown";
}
