/*
 * UTF-8 and the text rules, with an exact NFC check.
 *
 * Text is NFC when normalising it changes nothing. Most text answers that from the quick check
 * alone: no character whose quick check is No, and nothing that might compose or reorder. Where
 * something might, the text is split into segments, each starting at a character whose quick
 * check is Yes and whose combining class is 0 (nothing after such a character can reach back past
 * it; the table generator checks that this holds), and each such segment is decomposed,
 * reordered and composed again, then compared with the original.
 */
#include "dh_text.h"

#include <string.h>

#include "dh_unicode_tables.h"

size_t dh_utf8_decode(const uint8_t *s, size_t n, uint32_t *cp)
{
    if (n == 0) {
        return 0;
    }
    uint8_t b0 = s[0];
    if (b0 < 0x80) {
        *cp = b0;
        return 1;
    }
    size_t len;
    uint32_t v;
    uint8_t lo = 0x80, hi = 0xBF; /* the allowed range of the second byte */
    if (b0 >= 0xC2 && b0 <= 0xDF) {
        len = 2;
        v = b0 & 0x1Fu;
    } else if (b0 >= 0xE0 && b0 <= 0xEF) {
        len = 3;
        v = b0 & 0x0Fu;
        if (b0 == 0xE0) {
            lo = 0xA0;
        } else if (b0 == 0xED) {
            hi = 0x9F; /* no surrogates */
        }
    } else if (b0 >= 0xF0 && b0 <= 0xF4) {
        len = 4;
        v = b0 & 0x07u;
        if (b0 == 0xF0) {
            lo = 0x90;
        } else if (b0 == 0xF4) {
            hi = 0x8F;
        }
    } else {
        return 0;
    }
    if (n < len) {
        return 0;
    }
    for (size_t i = 1; i < len; i++) {
        uint8_t b = s[i];
        uint8_t min = i == 1 ? lo : 0x80, max = i == 1 ? hi : 0xBF;
        if (b < min || b > max) {
            return 0;
        }
        v = (v << 6) | (b & 0x3Fu);
    }
    *cp = v;
    return len;
}

size_t dh_utf8_encode(uint32_t cp, uint8_t *out)
{
    if (cp < 0x80) {
        out[0] = (uint8_t)cp;
        return 1;
    }
    if (cp < 0x800) {
        out[0] = (uint8_t)(0xC0u | (cp >> 6));
        out[1] = (uint8_t)(0x80u | (cp & 0x3Fu));
        return 2;
    }
    if (cp < 0x10000) {
        out[0] = (uint8_t)(0xE0u | (cp >> 12));
        out[1] = (uint8_t)(0x80u | ((cp >> 6) & 0x3Fu));
        out[2] = (uint8_t)(0x80u | (cp & 0x3Fu));
        return 3;
    }
    out[0] = (uint8_t)(0xF0u | (cp >> 18));
    out[1] = (uint8_t)(0x80u | ((cp >> 12) & 0x3Fu));
    out[2] = (uint8_t)(0x80u | ((cp >> 6) & 0x3Fu));
    out[3] = (uint8_t)(0x80u | (cp & 0x3Fu));
    return 4;
}

size_t dh_utf8_count(const uint8_t *s, size_t n)
{
    size_t count = 0;
    for (size_t i = 0; i < n; i++) {
        if ((s[i] & 0xC0u) != 0x80u) {
            count++;
        }
    }
    return count;
}

/* ----- character properties ----- */

/* Looks cp up in a range table; 0 if it is in no range. */
static uint8_t range_value(const uint32_t (*table)[2], size_t count, uint32_t cp)
{
    size_t lo = 0, hi = count;
    while (lo < hi) {
        size_t mid = lo + (hi - lo) / 2;
        uint32_t first = table[mid][0] & 0xFFFFFFu;
        if (cp < first) {
            hi = mid;
        } else if (cp > table[mid][1]) {
            lo = mid + 1;
        } else {
            return (uint8_t)(table[mid][0] >> 24);
        }
    }
    return 0;
}

uint8_t dh_char_ccc(uint32_t cp)
{
    if (cp < 0x300) {
        return 0;
    }
    return range_value(dh_ccc_ranges, dh_ccc_range_count, cp);
}

enum { QC_YES = 0, QC_NO = 1, QC_MAYBE = 2 };

static uint8_t nfc_qc(uint32_t cp)
{
    if (cp < 0x300) {
        return QC_YES;
    }
    return range_value(dh_nfc_qc_ranges, dh_nfc_qc_range_count, cp);
}

bool dh_char_is_forbidden(uint32_t cp)
{
    return cp < 0x20 || (cp >= 0x7F && cp <= 0x9F) || (cp >= 0x200B && cp <= 0x200F) ||
           (cp >= 0x202A && cp <= 0x202E) || (cp >= 0x2060 && cp <= 0x2064) ||
           (cp >= 0x2066 && cp <= 0x2069) || cp == 0xFEFF || (cp >= 0xFFF9 && cp <= 0xFFFB);
}

bool dh_char_is_whitespace(uint32_t cp)
{
    return (cp >= 0x09 && cp <= 0x0D) || cp == 0x20 || cp == 0x85 || cp == 0xA0 ||
           cp == 0x1680 || (cp >= 0x2000 && cp <= 0x200A) || cp == 0x2028 || cp == 0x2029 ||
           cp == 0x202F || cp == 0x205F || cp == 0x3000;
}

/* ----- composition and decomposition ----- */

#define HANGUL_S 0xAC00u
#define HANGUL_L 0x1100u
#define HANGUL_V 0x1161u
#define HANGUL_T 0x11A7u
#define HANGUL_L_COUNT 19u
#define HANGUL_V_COUNT 21u
#define HANGUL_T_COUNT 28u
#define HANGUL_N (HANGUL_V_COUNT * HANGUL_T_COUNT)
#define HANGUL_S_COUNT (HANGUL_L_COUNT * HANGUL_N)

static uint32_t pair_first(uint64_t e)
{
    return (uint32_t)(e >> 42) & 0x1FFFFFu;
}
static uint32_t pair_second(uint64_t e)
{
    return (uint32_t)(e >> 21) & 0x1FFFFFu;
}
static uint32_t pair_composite(uint64_t e)
{
    return (uint32_t)e & 0x1FFFFFu;
}

/* The primary composite of a and b, or 0. */
static uint32_t compose(uint32_t a, uint32_t b)
{
    if (a >= HANGUL_L && a < HANGUL_L + HANGUL_L_COUNT && b >= HANGUL_V &&
        b < HANGUL_V + HANGUL_V_COUNT) {
        return HANGUL_S + ((a - HANGUL_L) * HANGUL_V_COUNT + (b - HANGUL_V)) * HANGUL_T_COUNT;
    }
    if (a >= HANGUL_S && a < HANGUL_S + HANGUL_S_COUNT && (a - HANGUL_S) % HANGUL_T_COUNT == 0 &&
        b > HANGUL_T && b < HANGUL_T + HANGUL_T_COUNT) {
        return a + (b - HANGUL_T);
    }
    uint64_t key = ((uint64_t)a << 42) | ((uint64_t)b << 21);
    size_t lo = 0, hi = dh_compose_count;
    while (lo < hi) {
        size_t mid = lo + (hi - lo) / 2;
        uint64_t k = dh_compose_pairs[mid] & ~(uint64_t)0x1FFFFF;
        if (key < k) {
            hi = mid;
        } else if (key > k) {
            lo = mid + 1;
        } else {
            return pair_composite(dh_compose_pairs[mid]);
        }
    }
    return 0;
}

/* The pair a composite came from, if cp is one (outside Hangul). */
static bool decompose_pair(uint32_t cp, uint32_t *a, uint32_t *b)
{
    size_t lo = 0, hi = dh_compose_count;
    while (lo < hi) {
        size_t mid = lo + (hi - lo) / 2;
        uint64_t e = dh_compose_pairs[dh_compose_by_composite[mid]];
        uint32_t c = pair_composite(e);
        if (cp < c) {
            hi = mid;
        } else if (cp > c) {
            lo = mid + 1;
        } else {
            *a = pair_first(e);
            *b = pair_second(e);
            return true;
        }
    }
    return false;
}

/* Appends the full canonical decomposition of cp to out[*n..cap). It must have no character
 * whose quick check is No (those were refused already), so every character that decomposes is a
 * primary composite or Hangul. False if it doesn't fit. */
static bool decompose(uint32_t cp, uint32_t *out, size_t *n, size_t cap)
{
    uint32_t seconds[8];
    size_t ns = 0;
    for (;;) {
        if (cp >= HANGUL_S && cp < HANGUL_S + HANGUL_S_COUNT) {
            uint32_t s = cp - HANGUL_S;
            uint32_t t = s % HANGUL_T_COUNT;
            if (cap - *n < 3) {
                return false;
            }
            out[(*n)++] = HANGUL_L + s / HANGUL_N;
            out[(*n)++] = HANGUL_V + (s % HANGUL_N) / HANGUL_T_COUNT;
            if (t != 0) {
                out[(*n)++] = HANGUL_T + t;
            }
            break;
        }
        uint32_t a, b;
        if (ns < 8 && decompose_pair(cp, &a, &b)) {
            seconds[ns++] = b;
            cp = a;
            continue;
        }
        if (*n >= cap) {
            return false;
        }
        out[(*n)++] = cp;
        break;
    }
    while (ns > 0) {
        if (*n >= cap) {
            return false;
        }
        out[(*n)++] = seconds[--ns];
    }
    return true;
}

/* Normalises one segment (UTF-8 in s[0..n)) and compares it with itself. */
static bool segment_is_nfc(const uint8_t *s, size_t n, dh_text_scratch *scratch)
{
    uint32_t *buf = scratch->cp;
    const size_t cap = sizeof scratch->cp / sizeof scratch->cp[0];
    size_t len = 0;
    for (size_t i = 0; i < n;) {
        uint32_t cp;
        i += dh_utf8_decode(s + i, n - i, &cp);
        if (!decompose(cp, buf, &len, cap)) {
            return false;
        }
    }
    /* Canonical ordering: a stable sort of each run of non-starters by class. */
    for (size_t i = 1; i < len; i++) {
        uint32_t c = buf[i];
        uint8_t cc = dh_char_ccc(c);
        if (cc == 0) {
            continue;
        }
        size_t j = i;
        while (j > 0) {
            uint8_t prev = dh_char_ccc(buf[j - 1]);
            if (prev == 0 || prev <= cc) {
                break;
            }
            buf[j] = buf[j - 1];
            j--;
        }
        buf[j] = c;
    }
    /* Canonical composition (Unicode Standard Annex #15). */
    size_t out = 1;
    long starter = dh_char_ccc(buf[0]) == 0 ? 0 : -1;
    int last_cc = starter == 0 ? -1 : 256; /* -1: nothing since the starter */
    for (size_t i = 1; i < len; i++) {
        uint32_t c = buf[i];
        int cc = dh_char_ccc(c);
        if (starter >= 0 && last_cc < cc) {
            uint32_t comp = compose(buf[starter], c);
            if (comp != 0) {
                buf[starter] = comp;
                continue;
            }
        }
        if (cc == 0) {
            starter = (long)out;
            last_cc = -1;
        } else {
            last_cc = cc;
        }
        buf[out++] = c;
    }
    /* Compare with the original. */
    size_t k = 0;
    for (size_t i = 0; i < n; k++) {
        uint32_t cp;
        i += dh_utf8_decode(s + i, n - i, &cp);
        if (k >= out || buf[k] != cp) {
            return false;
        }
    }
    return k == out;
}

bool dh_text_is_nfc(const uint8_t *s, size_t n, dh_text_scratch *scratch)
{
    if (dh_utf8_count(s, n) > DH_TEXT_MAX_CHECKED) {
        return false;
    }
    size_t seg = 0;          /* where the current segment starts */
    bool seg_needs = false;  /* whether it holds anything that might change */
    uint8_t last_cc = 0;
    for (size_t i = 0; i < n;) {
        uint32_t cp;
        size_t len = dh_utf8_decode(s + i, n - i, &cp);
        if (len == 0) {
            return false;
        }
        uint8_t qc = nfc_qc(cp);
        uint8_t cc = dh_char_ccc(cp);
        if (qc == QC_NO || (cc != 0 && last_cc > cc)) {
            return false;
        }
        if (qc == QC_YES && cc == 0) {
            if (seg_needs && !segment_is_nfc(s + seg, i - seg, scratch)) {
                return false;
            }
            seg = i;
            seg_needs = false;
        } else {
            seg_needs = true;
        }
        last_cc = cc;
        i += len;
    }
    return !seg_needs || segment_is_nfc(s + seg, n - seg, scratch);
}

dh_text_problem dh_text_check(const uint8_t *s, size_t n, size_t max_chars,
                              dh_text_scratch *scratch)
{
    if (n == 0) {
        return DH_TEXT_EMPTY;
    }
    size_t chars = 0;
    uint32_t first = 0, last = 0;
    for (size_t i = 0; i < n;) {
        uint32_t cp;
        size_t len = dh_utf8_decode(s + i, n - i, &cp);
        if (len == 0) {
            return DH_TEXT_BAD_UTF8;
        }
        if (dh_char_is_forbidden(cp)) {
            return DH_TEXT_CONTROL;
        }
        if (chars == 0) {
            first = cp;
        }
        last = cp;
        chars++;
        i += len;
    }
    if (chars > max_chars || max_chars > DH_TEXT_MAX_CHECKED) {
        return DH_TEXT_TOO_LONG;
    }
    if (dh_char_is_whitespace(first) || dh_char_is_whitespace(last)) {
        return DH_TEXT_NOT_NORMALISED;
    }
    if (!dh_text_is_nfc(s, n, scratch)) {
        return DH_TEXT_NOT_NORMALISED;
    }
    return DH_TEXT_OK;
}
