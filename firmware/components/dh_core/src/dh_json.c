/*
 * The strict JSON reader. See dh_json.h for what it checks.
 *
 * Parsing is one pass with an explicit stack of one bit per level (object or array), so it never
 * recurses and never allocates. Reading afterwards works on the original bytes, which the parse
 * has already proved well formed; that is why the readers below can skip values without checking
 * them again.
 */
#include "dh_json.h"

#include <math.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "dh_text.h"

/* ----- parsing ----- */

typedef struct {
    const uint8_t *b;
    size_t n;
    size_t i;
    unsigned flags;
} cursor;

static bool is_ws(uint8_t c)
{
    return c == ' ' || c == '\t' || c == '\n' || c == '\r';
}

static void skip_ws(cursor *c)
{
    while (c->i < c->n && is_ws(c->b[c->i])) {
        c->i++;
    }
}

static bool is_digit(uint8_t c)
{
    return c >= '0' && c <= '9';
}

static int hex_value(uint8_t c)
{
    if (c >= '0' && c <= '9') {
        return c - '0';
    }
    if (c >= 'a' && c <= 'f') {
        return c - 'a' + 10;
    }
    if (c >= 'A' && c <= 'F') {
        return c - 'A' + 10;
    }
    return -1;
}

/* Reads four hex digits at b[i..i+4). */
static bool hex4(const uint8_t *b, size_t n, size_t i, uint32_t *out)
{
    if (n < 4 || i > n - 4) {
        return false;
    }
    uint32_t v = 0;
    for (size_t k = 0; k < 4; k++) {
        int h = hex_value(b[i + k]);
        if (h < 0) {
            return false;
        }
        v = (v << 4) | (uint32_t)h;
    }
    *out = v;
    return true;
}

static dh_json_status parse_string(cursor *c)
{
    c->i++; /* the opening quote */
    for (;;) {
        if (c->i >= c->n) {
            return DH_JSON_E_SYNTAX;
        }
        uint8_t ch = c->b[c->i];
        if (ch == '"') {
            c->i++;
            return DH_JSON_OK;
        }
        if (ch == '\\') {
            if (c->i + 1 >= c->n) {
                return DH_JSON_E_SYNTAX;
            }
            uint8_t e = c->b[c->i + 1];
            switch (e) {
            case '"': case '\\': case '/': case 'b': case 'f': case 'n': case 'r': case 't':
                c->i += 2;
                continue;
            case 'u': {
                uint32_t u;
                if (!hex4(c->b, c->n, c->i + 2, &u)) {
                    return DH_JSON_E_ESCAPE;
                }
                c->i += 6;
                if (u >= 0xDC00 && u <= 0xDFFF) {
                    return DH_JSON_E_ESCAPE;
                }
                if (u >= 0xD800 && u <= 0xDBFF) {
                    uint32_t lo;
                    if (c->i + 1 >= c->n || c->b[c->i] != '\\' || c->b[c->i + 1] != 'u' ||
                        !hex4(c->b, c->n, c->i + 2, &lo) || lo < 0xDC00 || lo > 0xDFFF) {
                        return DH_JSON_E_ESCAPE;
                    }
                    c->i += 6;
                }
                continue;
            }
            default:
                return DH_JSON_E_ESCAPE;
            }
        }
        if (ch < 0x20) {
            return DH_JSON_E_CONTROL;
        }
        if (ch < 0x80) {
            c->i++;
            continue;
        }
        uint32_t cp;
        size_t len = dh_utf8_decode(c->b + c->i, c->n - c->i, &cp);
        if (len == 0) {
            return DH_JSON_E_UTF8;
        }
        c->i += len;
    }
}

/* Whether the number written at b[start..end) is beyond the range of a double, deciding cheaply
 * when its magnitude is clear and converting it otherwise. */
static bool number_overflows(const uint8_t *b, size_t start, size_t end);

static dh_json_status parse_number(cursor *c)
{
    size_t start = c->i;
    unsigned form = 0;
    if (c->b[c->i] == '-') {
        form |= DH_JSON_NUM_NEGATIVE;
        c->i++;
    }
    if (c->i >= c->n || !is_digit(c->b[c->i])) {
        return DH_JSON_E_SYNTAX;
    }
    if (c->b[c->i] == '0') {
        c->i++;
    } else {
        while (c->i < c->n && is_digit(c->b[c->i])) {
            c->i++;
        }
    }
    if (c->i < c->n && c->b[c->i] == '.') {
        form |= DH_JSON_NUM_FRACTION;
        c->i++;
        if (c->i >= c->n || !is_digit(c->b[c->i])) {
            return DH_JSON_E_SYNTAX;
        }
        while (c->i < c->n && is_digit(c->b[c->i])) {
            c->i++;
        }
    }
    if (c->i < c->n && (c->b[c->i] == 'e' || c->b[c->i] == 'E')) {
        form |= DH_JSON_NUM_EXPONENT;
        c->i++;
        if (c->i < c->n && (c->b[c->i] == '+' || c->b[c->i] == '-')) {
            c->i++;
        }
        if (c->i >= c->n || !is_digit(c->b[c->i])) {
            return DH_JSON_E_SYNTAX;
        }
        while (c->i < c->n && is_digit(c->b[c->i])) {
            c->i++;
        }
    }
    if ((c->flags & DH_JSON_PLAIN_INTEGERS) && form != 0) {
        return DH_JSON_E_NUMBER_FORM;
    }
    if (number_overflows(c->b, start, c->i)) {
        return DH_JSON_E_NUMBER_RANGE;
    }
    return DH_JSON_OK;
}

static dh_json_status parse_literal(cursor *c, const char *word)
{
    size_t len = strlen(word);
    if (c->n - c->i < len || memcmp(c->b + c->i, word, len) != 0) {
        return DH_JSON_E_SYNTAX;
    }
    c->i += len;
    return DH_JSON_OK;
}

static bool has_duplicates(const dh_json_doc *doc);

dh_json_status dh_json_parse(dh_json_doc *doc, const uint8_t *buf, size_t len,
                             const dh_json_options *opts, dh_json_value *root)
{
    size_t max_bytes = opts && opts->max_bytes ? opts->max_bytes : (size_t)-1;
    unsigned max_depth = opts && opts->max_depth ? opts->max_depth : DH_JSON_DEFAULT_MAX_DEPTH;
    if (max_depth > DH_JSON_DEPTH_LIMIT) {
        max_depth = DH_JSON_DEPTH_LIMIT;
    }
    doc->buf = buf;
    doc->len = len;
    doc->error_offset = 0;
    if (len > max_bytes) {
        return DH_JSON_E_TOO_LARGE;
    }
    cursor c = {buf, len, 0, opts ? opts->flags : 0u};
    /* One bit per open container: set for an object. */
    uint8_t is_object[(DH_JSON_DEPTH_LIMIT + 8u) / 8u] = {0};
    unsigned depth = 0;
    dh_json_status st = DH_JSON_OK;

    skip_ws(&c);
    if (c.i >= c.n) {
        return DH_JSON_E_EMPTY;
    }
    *root = c.i;

    for (;;) {
        /* A value is expected at c.i. */
        skip_ws(&c);
        if (c.i >= c.n) {
            st = DH_JSON_E_SYNTAX;
            goto fail;
        }
        uint8_t ch = c.b[c.i];
        if (ch == '{' || ch == '[') {
            if (depth >= max_depth) {
                st = DH_JSON_E_DEPTH;
                goto fail;
            }
            uint8_t bit = (uint8_t)(1u << (depth & 7u));
            if (ch == '{') {
                is_object[depth >> 3] = (uint8_t)(is_object[depth >> 3] | bit);
            } else {
                is_object[depth >> 3] = (uint8_t)(is_object[depth >> 3] & ~bit);
            }
            depth++;
            c.i++;
            skip_ws(&c);
            if (c.i < c.n && c.b[c.i] == (ch == '{' ? '}' : ']')) {
                c.i++;
                depth--;
                goto after_value;
            }
            if (ch == '{') {
                goto member;
            }
            continue;
        } else if (ch == '"') {
            st = parse_string(&c);
        } else if (ch == '-' || is_digit(ch)) {
            st = parse_number(&c);
        } else if (ch == 't') {
            st = parse_literal(&c, "true");
        } else if (ch == 'f') {
            st = parse_literal(&c, "false");
        } else if (ch == 'n') {
            st = parse_literal(&c, "null");
        } else {
            st = DH_JSON_E_SYNTAX;
        }
        if (st != DH_JSON_OK) {
            goto fail;
        }

    after_value:
        for (;;) {
            if (depth == 0) {
                skip_ws(&c);
                if (c.i != c.n) {
                    st = DH_JSON_E_SYNTAX;
                    goto fail;
                }
                if ((c.flags & DH_JSON_NO_DUPLICATES) && has_duplicates(doc)) {
                    return DH_JSON_E_DUPLICATE;
                }
                return DH_JSON_OK;
            }
            skip_ws(&c);
            if (c.i >= c.n) {
                st = DH_JSON_E_SYNTAX;
                goto fail;
            }
            unsigned top = depth - 1;
            bool obj = ((unsigned)is_object[top >> 3] >> (top & 7u)) & 1u;
            ch = c.b[c.i];
            if (ch == ',') {
                c.i++;
                if (obj) {
                    skip_ws(&c);
                    goto member;
                }
                break; /* the next array element */
            }
            if (ch == (obj ? '}' : ']')) {
                c.i++;
                depth--;
                continue;
            }
            st = DH_JSON_E_SYNTAX;
            goto fail;
        }
        continue;

    member:
        /* A member name, then a colon; the value follows. */
        if (c.i >= c.n || c.b[c.i] != '"') {
            st = DH_JSON_E_SYNTAX;
            goto fail;
        }
        st = parse_string(&c);
        if (st != DH_JSON_OK) {
            goto fail;
        }
        skip_ws(&c);
        if (c.i >= c.n || c.b[c.i] != ':') {
            st = DH_JSON_E_SYNTAX;
            goto fail;
        }
        c.i++;
    }

fail:
    doc->error_offset = c.i;
    return st;
}

const char *dh_json_status_name(dh_json_status s)
{
    switch (s) {
    case DH_JSON_OK: return "ok";
    case DH_JSON_E_EMPTY: return "empty";
    case DH_JSON_E_TOO_LARGE: return "too_large";
    case DH_JSON_E_SYNTAX: return "syntax";
    case DH_JSON_E_DEPTH: return "depth";
    case DH_JSON_E_UTF8: return "utf8";
    case DH_JSON_E_ESCAPE: return "escape";
    case DH_JSON_E_CONTROL: return "control";
    case DH_JSON_E_NUMBER_RANGE: return "number_range";
    case DH_JSON_E_NUMBER_FORM: return "number_form";
    case DH_JSON_E_DUPLICATE: return "duplicate";
    }
    return "unknown";
}

/* ----- reading a parsed document ----- */

static size_t ws_from(const dh_json_doc *doc, size_t i)
{
    while (i < doc->len && is_ws(doc->buf[i])) {
        i++;
    }
    return i;
}

static size_t string_end(const dh_json_doc *doc, size_t i)
{
    i++;
    while (doc->buf[i] != '"') {
        i += doc->buf[i] == '\\' ? 2u : 1u;
    }
    return i + 1;
}

static size_t number_end(const dh_json_doc *doc, size_t i)
{
    while (i < doc->len) {
        uint8_t ch = doc->buf[i];
        if (!(is_digit(ch) || ch == '-' || ch == '+' || ch == '.' || ch == 'e' || ch == 'E')) {
            break;
        }
        i++;
    }
    return i;
}

dh_json_type dh_json_type_of(const dh_json_doc *doc, dh_json_value v)
{
    switch (doc->buf[v]) {
    case '{': return DH_JSON_OBJECT;
    case '[': return DH_JSON_ARRAY;
    case '"': return DH_JSON_STRING;
    case 't': return DH_JSON_TRUE;
    case 'f': return DH_JSON_FALSE;
    case 'n': return DH_JSON_NULL;
    default: return DH_JSON_NUMBER;
    }
}

size_t dh_json_end(const dh_json_doc *doc, dh_json_value v)
{
    switch (doc->buf[v]) {
    case '"': return string_end(doc, v);
    case 't': return v + 4;
    case 'f': return v + 5;
    case 'n': return v + 4;
    case '{':
    case '[': {
        size_t depth = 0;
        size_t i = v;
        for (;;) {
            uint8_t ch = doc->buf[i];
            if (ch == '"') {
                i = string_end(doc, i);
                continue;
            }
            if (ch == '{' || ch == '[') {
                depth++;
            } else if (ch == '}' || ch == ']') {
                if (--depth == 0) {
                    return i + 1;
                }
            }
            i++;
        }
    }
    default:
        return number_end(doc, v);
    }
}

bool dh_json_object_begin(const dh_json_doc *doc, dh_json_value v, dh_json_iter *it)
{
    if (doc->buf[v] != '{') {
        return false;
    }
    it->pos = v + 1;
    return true;
}

bool dh_json_object_next(const dh_json_doc *doc, dh_json_iter *it, dh_json_value *key,
                         dh_json_value *value)
{
    size_t i = ws_from(doc, it->pos);
    if (doc->buf[i] == '}') {
        it->pos = i;
        return false;
    }
    if (doc->buf[i] == ',') {
        i = ws_from(doc, i + 1);
    }
    *key = i;
    i = ws_from(doc, string_end(doc, i));
    i = ws_from(doc, i + 1); /* the colon */
    *value = i;
    it->pos = dh_json_end(doc, i);
    return true;
}

bool dh_json_array_begin(const dh_json_doc *doc, dh_json_value v, dh_json_iter *it)
{
    if (doc->buf[v] != '[') {
        return false;
    }
    it->pos = v + 1;
    return true;
}

bool dh_json_array_next(const dh_json_doc *doc, dh_json_iter *it, dh_json_value *value)
{
    size_t i = ws_from(doc, it->pos);
    if (doc->buf[i] == ']') {
        it->pos = i;
        return false;
    }
    if (doc->buf[i] == ',') {
        i = ws_from(doc, i + 1);
    }
    *value = i;
    it->pos = dh_json_end(doc, i);
    return true;
}

size_t dh_json_array_length(const dh_json_doc *doc, dh_json_value v)
{
    dh_json_iter it;
    dh_json_value e;
    size_t n = 0;
    if (!dh_json_array_begin(doc, v, &it)) {
        return 0;
    }
    while (dh_json_array_next(doc, &it, &e)) {
        n++;
    }
    return n;
}

/* Steps through a string's decoded code points. *pos starts just past the opening quote. */
static bool string_next(const dh_json_doc *doc, size_t *pos, uint32_t *cp)
{
    size_t i = *pos;
    uint8_t ch = doc->buf[i];
    if (ch == '"') {
        return false;
    }
    if (ch == '\\') {
        uint8_t e = doc->buf[i + 1];
        switch (e) {
        case 'b': *cp = 0x08; break;
        case 'f': *cp = 0x0C; break;
        case 'n': *cp = 0x0A; break;
        case 'r': *cp = 0x0D; break;
        case 't': *cp = 0x09; break;
        case 'u': {
            uint32_t u = 0, lo = 0;
            (void)hex4(doc->buf, doc->len, i + 2, &u);
            if (u >= 0xD800 && u <= 0xDBFF) {
                (void)hex4(doc->buf, doc->len, i + 8, &lo);
                *cp = 0x10000u + ((u - 0xD800u) << 10) + (lo - 0xDC00u);
                *pos = i + 12;
                return true;
            }
            *cp = u;
            *pos = i + 6;
            return true;
        }
        default: *cp = e; break; /* '"', '\\' and '/' */
        }
        *pos = i + 2;
        return true;
    }
    if (ch < 0x80) {
        *cp = ch;
        *pos = i + 1;
        return true;
    }
    size_t len = dh_utf8_decode(doc->buf + i, doc->len - i, cp);
    *pos = i + len;
    return true;
}

bool dh_json_string_each(const dh_json_doc *doc, dh_json_value str,
                         bool (*f)(uint32_t cp, void *ctx), void *ctx)
{
    size_t pos = str + 1;
    uint32_t cp;
    while (string_next(doc, &pos, &cp)) {
        if (!f(cp, ctx)) {
            return false;
        }
    }
    return true;
}

static size_t utf8_length(uint32_t cp)
{
    return cp < 0x80 ? 1u : cp < 0x800 ? 2u : cp < 0x10000 ? 3u : 4u;
}

bool dh_json_string_equals(const dh_json_doc *doc, dh_json_value str, const char *s, size_t n)
{
    size_t pos = str + 1;
    size_t k = 0;
    uint32_t cp;
    while (string_next(doc, &pos, &cp)) {
        uint8_t enc[4];
        size_t len = dh_utf8_encode(cp, enc);
        if (n - k < len || memcmp(enc, s + k, len) != 0) {
            return false;
        }
        k += len;
    }
    return k == n;
}

bool dh_json_strings_equal(const dh_json_doc *doc, dh_json_value a, dh_json_value b)
{
    size_t pa = a + 1, pb = b + 1;
    for (;;) {
        uint32_t ca, cb;
        bool more_a = string_next(doc, &pa, &ca);
        bool more_b = string_next(doc, &pb, &cb);
        if (more_a != more_b) {
            return false;
        }
        if (!more_a) {
            return true;
        }
        if (ca != cb) {
            return false;
        }
    }
}

size_t dh_json_string_length(const dh_json_doc *doc, dh_json_value str)
{
    size_t pos = str + 1;
    size_t n = 0;
    uint32_t cp;
    while (string_next(doc, &pos, &cp)) {
        n += utf8_length(cp);
    }
    return n;
}

bool dh_json_string_copy(const dh_json_doc *doc, dh_json_value str, char *out, size_t cap,
                         size_t *len)
{
    size_t pos = str + 1;
    size_t k = 0;
    uint32_t cp;
    while (string_next(doc, &pos, &cp)) {
        uint8_t enc[4];
        size_t n = dh_utf8_encode(cp, enc);
        if (cap == 0 || cap - 1 - k < n) {
            if (cap > 0) {
                out[0] = '\0';
            }
            if (len) {
                *len = 0;
            }
            return false;
        }
        memcpy(out + k, enc, n);
        k += n;
    }
    if (cap == 0) {
        return false;
    }
    out[k] = '\0';
    if (len) {
        *len = k;
    }
    return true;
}

size_t dh_json_object_find(const dh_json_doc *doc, dh_json_value obj, const char *name,
                           size_t name_len, dh_json_value *last)
{
    dh_json_iter it;
    dh_json_value k, v;
    size_t count = 0;
    if (!dh_json_object_begin(doc, obj, &it)) {
        return 0;
    }
    while (dh_json_object_next(doc, &it, &k, &v)) {
        if (dh_json_string_equals(doc, k, name, name_len)) {
            count++;
            if (last) {
                *last = v;
            }
        }
    }
    return count;
}

/* Every object, pairwise. Quadratic, which is fine for the small documents that ask for it. */
static bool has_duplicates(const dh_json_doc *doc)
{
    for (size_t i = 0; i < doc->len; i++) {
        uint8_t ch = doc->buf[i];
        if (ch == '"') {
            i = string_end(doc, i) - 1;
            continue;
        }
        if (ch != '{') {
            continue;
        }
        dh_json_iter a;
        dh_json_value ka, va;
        (void)dh_json_object_begin(doc, i, &a);
        while (dh_json_object_next(doc, &a, &ka, &va)) {
            dh_json_iter b = a;
            dh_json_value kb, vb;
            while (dh_json_object_next(doc, &b, &kb, &vb)) {
                if (dh_json_strings_equal(doc, ka, kb)) {
                    return true;
                }
            }
        }
    }
    return false;
}

/* ----- numbers ----- */

typedef struct {
    bool negative;
    size_t int_start, int_end;   /* the integer digits */
    size_t frac_start, frac_end; /* the fraction digits, if any */
    bool has_exp;
    bool exp_negative;
    size_t exp_start, exp_end;
} number_parts;

static void split_number(const uint8_t *b, size_t start, size_t end, number_parts *p)
{
    size_t i = start;
    memset(p, 0, sizeof *p);
    if (b[i] == '-') {
        p->negative = true;
        i++;
    }
    p->int_start = i;
    while (i < end && is_digit(b[i])) {
        i++;
    }
    p->int_end = i;
    p->frac_start = p->frac_end = i;
    if (i < end && b[i] == '.') {
        i++;
        p->frac_start = i;
        while (i < end && is_digit(b[i])) {
            i++;
        }
        p->frac_end = i;
    }
    if (i < end && (b[i] == 'e' || b[i] == 'E')) {
        p->has_exp = true;
        i++;
        if (b[i] == '+' || b[i] == '-') {
            p->exp_negative = b[i] == '-';
            i++;
        }
        p->exp_start = i;
        p->exp_end = end;
    }
}

/* The exponent, clamped well beyond where any double runs out. */
#define EXP_CLAMP 1000000L
static long exponent_of(const uint8_t *b, const number_parts *p)
{
    long e = 0;
    if (!p->has_exp) {
        return 0;
    }
    for (size_t i = p->exp_start; i < p->exp_end; i++) {
        if (e < EXP_CLAMP) {
            e = e * 10 + (b[i] - '0');
        }
    }
    return p->exp_negative ? -e : e;
}

/* Digit k of the significand, counting integer then fraction digits. */
static uint8_t digit_at(const uint8_t *b, const number_parts *p, size_t k)
{
    size_t ni = p->int_end - p->int_start;
    return k < ni ? b[p->int_start + k] : b[p->frac_start + (k - ni)];
}

/* Finds the first non-zero digit. Returns false for zero. *pos is the power of ten of that digit
 * with the exponent applied (clamped). */
static bool leading_digit(const uint8_t *b, const number_parts *p, size_t *first, long *pos)
{
    size_t ni = p->int_end - p->int_start;
    size_t nd = ni + (p->frac_end - p->frac_start);
    for (size_t k = 0; k < nd; k++) {
        if (digit_at(b, p, k) != '0') {
            *first = k;
            /* Digit k sits at the power ni - 1 - k. */
            long place = (long)ni - 1 - (long)k;
            long e = exponent_of(b, p);
            *pos = place + e;
            return true;
        }
    }
    return false;
}

/* Rewrites the number as at most 40 significant digits, plus a sticky 1 if anything non-zero was
 * dropped, and a decimal exponent, then lets strtod round it. Writing the digits as an integer
 * with an exponent keeps the locale's decimal point out of it. */
static double parse_double(const uint8_t *b, size_t start, size_t end)
{
    number_parts p;
    split_number(b, start, end, &p);
    size_t first;
    long pos;
    if (!leading_digit(b, &p, &first, &pos)) {
        return p.negative ? -0.0 : 0.0;
    }
    char text[64];
    size_t t = 0;
    if (p.negative) {
        text[t++] = '-';
    }
    size_t ni = p.int_end - p.int_start;
    size_t nd = ni + (p.frac_end - p.frac_start);
    size_t kept = 0;
    size_t k = first;
    for (; k < nd && kept < 40; k++, kept++) {
        text[t++] = (char)digit_at(b, &p, k);
    }
    for (; k < nd; k++) {
        if (digit_at(b, &p, k) != '0') {
            text[t++] = '1';
            kept++;
            break;
        }
    }
    /* The value is text * 10^(pos - kept + 1). */
    long e = pos - (long)kept + 1;
    if (e > EXP_CLAMP) {
        e = EXP_CLAMP;
    } else if (e < -EXP_CLAMP) {
        e = -EXP_CLAMP;
    }
    (void)snprintf(text + t, sizeof text - t, "e%ld", e);
    return strtod(text, NULL);
}

static bool number_overflows(const uint8_t *b, size_t start, size_t end)
{
    number_parts p;
    split_number(b, start, end, &p);
    size_t first;
    long pos;
    if (!leading_digit(b, &p, &first, &pos)) {
        return false;
    }
    /* The value is at least 10^pos and below 10^(pos + 1); doubles end near 1.8e308. */
    if (pos <= 307) {
        return false;
    }
    if (pos >= 309) {
        return true;
    }
    return isinf(parse_double(b, start, end));
}

unsigned dh_json_number_form(const dh_json_doc *doc, dh_json_value num)
{
    number_parts p;
    split_number(doc->buf, num, number_end(doc, num), &p);
    unsigned form = 0;
    if (p.negative) {
        form |= DH_JSON_NUM_NEGATIVE;
    }
    if (p.frac_end > p.frac_start) {
        form |= DH_JSON_NUM_FRACTION;
    }
    if (p.has_exp) {
        form |= DH_JSON_NUM_EXPONENT;
    }
    return form;
}

/* The integer digits as a u64, if there are no more than fit. */
static bool digits_u64(const uint8_t *b, size_t start, size_t end, uint64_t *out)
{
    uint64_t v = 0;
    for (size_t i = start; i < end; i++) {
        unsigned d = (unsigned)(b[i] - '0');
        if (v > (UINT64_MAX - d) / 10u) {
            return false;
        }
        v = v * 10u + d;
    }
    *out = v;
    return true;
}

bool dh_json_number_u64(const dh_json_doc *doc, dh_json_value num, uint64_t *out)
{
    if (dh_json_type_of(doc, num) != DH_JSON_NUMBER || dh_json_number_form(doc, num) != 0) {
        return false;
    }
    return digits_u64(doc->buf, num, number_end(doc, num), out);
}

double dh_json_number_double(const dh_json_doc *doc, dh_json_value num)
{
    return parse_double(doc->buf, num, number_end(doc, num));
}

dh_json_serde_number dh_json_number_serde_class(const dh_json_doc *doc, dh_json_value num)
{
    number_parts p;
    split_number(doc->buf, num, number_end(doc, num), &p);
    if (p.frac_end > p.frac_start || p.has_exp) {
        return DH_JSON_SERDE_F64;
    }
    uint64_t mag;
    if (!digits_u64(doc->buf, p.int_start, p.int_end, &mag)) {
        return DH_JSON_SERDE_F64; /* too long for a u64: serde_json reads it as a double */
    }
    if (!p.negative) {
        return DH_JSON_SERDE_U64;
    }
    if (mag == 0 || mag > (uint64_t)1 << 63) {
        return DH_JSON_SERDE_F64; /* "-0", or below the range of an i64 */
    }
    return DH_JSON_SERDE_I64;
}

/* ----- serde_json's way of writing values ----- */

static size_t decimal_digits(long v)
{
    size_t n = 1;
    while (v >= 10) {
        v /= 10;
        n++;
    }
    return n;
}

/* The length of a finite double as the ryu crate writes it (serde_json's writer). The shortest
 * digits that read back as the same double come from printf and strtod, trying more digits
 * until they round-trip; ryu's layout rules then give the length. */
static size_t ryu_length(double d)
{
    size_t sign = signbit(d) ? 1u : 0u;
    if (d == 0.0) {
        return sign + 3; /* "0.0" */
    }
    char buf[40];
    int prec;
    for (prec = 0; prec < 17; prec++) {
        (void)snprintf(buf, sizeof buf, "%.*e", prec, d);
        if (strtod(buf, NULL) == d) {
            break;
        }
    }
    /* buf is "[-]D[.DDD]e[+-]XX". */
    const char *s = buf + sign;
    long length = 0;
    long last_nonzero = 0;
    const char *q = s;
    for (; *q && *q != 'e'; q++) {
        if (*q >= '0' && *q <= '9') {
            length++;
            if (*q != '0') {
                last_nonzero = length;
            }
        }
    }
    length = last_nonzero; /* the shortest form has no trailing zeros */
    long x = strtol(q + 1, NULL, 10);
    long k = x - (length - 1); /* the value is digits * 10^k */
    long kk = length + k;      /* 10^(kk-1) <= v < 10^kk */
    size_t out;
    if (0 <= k && kk <= 16) {
        out = (size_t)kk + 2; /* 1234e7 -> 12340000000.0 */
    } else if (0 < kk && kk <= 16) {
        out = (size_t)length + 1; /* 1234e-2 -> 12.34 */
    } else if (-5 < kk && kk <= 0) {
        out = (size_t)(length + 2 - kk); /* 1234e-6 -> 0.001234 */
    } else {
        long e = kk - 1;
        size_t elen = (e < 0 ? 1u : 0u) + decimal_digits(e < 0 ? -e : e);
        out = (length == 1 ? 2u : (size_t)length + 2u) + elen; /* 1e30, 1.234e33 */
    }
    return sign + out;
}

size_t dh_json_serde_number_length(const dh_json_doc *doc, dh_json_value num)
{
    size_t end = number_end(doc, num);
    if (dh_json_number_serde_class(doc, num) != DH_JSON_SERDE_F64) {
        return end - num; /* written back exactly as it came */
    }
    return ryu_length(parse_double(doc->buf, num, end));
}

size_t dh_json_serde_string_length(const dh_json_doc *doc, dh_json_value str)
{
    size_t pos = str + 1;
    size_t n = 2;
    uint32_t cp;
    while (string_next(doc, &pos, &cp)) {
        if (cp == '"' || cp == '\\' || cp == 0x08 || cp == 0x09 || cp == 0x0A || cp == 0x0C ||
            cp == 0x0D) {
            n += 2;
        } else if (cp < 0x20) {
            n += 6;
        } else {
            n += utf8_length(cp);
        }
    }
    return n;
}

/* Whether a later member of the same object has the same name as the member whose name is at
 * key. serde_json keeps only the last, so this one doesn't count. */
static bool shadowed(const dh_json_doc *doc, dh_json_value key)
{
    size_t i = ws_from(doc, string_end(doc, key));
    i = ws_from(doc, i + 1);
    dh_json_iter it = {dh_json_end(doc, i)};
    dh_json_value k, v;
    while (dh_json_object_next(doc, &it, &k, &v)) {
        if (dh_json_strings_equal(doc, key, k)) {
            return true;
        }
    }
    return false;
}

void dh_json_serde_measure(const dh_json_doc *doc, dh_json_value v, size_t size_limit,
                           size_t *size, unsigned *depth)
{
    /* Per open container: whether it is an object, and whether anything has been written in
     * it yet (for the commas). */
    uint8_t is_object[(DH_JSON_DEPTH_LIMIT + 8u) / 8u] = {0};
    uint8_t started[(DH_JSON_DEPTH_LIMIT + 8u) / 8u] = {0};
    unsigned level = 0, deepest = 0;
    size_t total = 0;
    size_t i = v;

    for (;;) {
        /* A value at i. */
        uint8_t ch = doc->buf[i];
        if (ch == '{' || ch == '[') {
            uint8_t bit = (uint8_t)(1u << (level & 7u));
            if (ch == '{') {
                is_object[level >> 3] = (uint8_t)(is_object[level >> 3] | bit);
            } else {
                is_object[level >> 3] = (uint8_t)(is_object[level >> 3] & ~bit);
            }
            started[level >> 3] = (uint8_t)(started[level >> 3] & ~bit);
            level++;
            if (level > deepest) {
                deepest = level;
            }
            total += 1;
            i++;
        } else {
            switch (dh_json_type_of(doc, i)) {
            case DH_JSON_STRING: total += dh_json_serde_string_length(doc, i); break;
            case DH_JSON_NUMBER: total += dh_json_serde_number_length(doc, i); break;
            case DH_JSON_TRUE: total += 4; break;
            case DH_JSON_FALSE: total += 5; break;
            default: total += 4; break;
            }
            i = dh_json_end(doc, i);
            if (level == 0) {
                break;
            }
        }
        if (total > size_limit) {
            break;
        }

        /* The next element of the innermost open container, or its end. */
        bool have_value = false;
        while (!have_value && level > 0 && total <= size_limit) {
            unsigned top = level - 1;
            uint8_t bit = (uint8_t)(1u << (top & 7u));
            bool obj = is_object[top >> 3] & bit;
            i = ws_from(doc, i);
            if (doc->buf[i] == ',') {
                i = ws_from(doc, i + 1);
            }
            if (doc->buf[i] == '}' || doc->buf[i] == ']') {
                total += 1;
                i++;
                level--;
                continue;
            }
            if (obj) {
                dh_json_value key = i;
                size_t after = ws_from(doc, string_end(doc, key));
                dh_json_value value = ws_from(doc, after + 1);
                if (shadowed(doc, key)) {
                    i = dh_json_end(doc, value);
                    continue;
                }
                if (started[top >> 3] & bit) {
                    total += 1;
                }
                total += dh_json_serde_string_length(doc, key) + 1;
                i = value;
            } else if (started[top >> 3] & bit) {
                total += 1;
            }
            started[top >> 3] = (uint8_t)(started[top >> 3] | bit);
            have_value = true;
        }
        if (!have_value) {
            break;
        }
    }
    *size = total > size_limit ? size_limit + 1 : total;
    *depth = deepest;
}
