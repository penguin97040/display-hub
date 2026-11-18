/*
 * Text on the wire: UTF-8, and the rules for titles, labels and notes (docs/protocol-v1.md, "A
 * page"): never empty, no control or invisible formatting characters, no white space at either
 * end, Unicode NFC, and at most so many characters (Unicode scalar values, not bytes).
 *
 * These match check_text() in crates/hub-protocol/src/text.rs. The NFC check uses tables made
 * from the same unicode-normalization release (tools/unicode-tables).
 */
#ifndef DH_TEXT_H
#define DH_TEXT_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Decodes one UTF-8 sequence from s[0..n). Returns its length (1 to 4) and sets *cp, or returns
 * 0 if it isn't a shortest-form encoding of a scalar value. */
size_t dh_utf8_decode(const uint8_t *s, size_t n, uint32_t *cp);
/* Writes cp as UTF-8 into out (4 bytes of room) and returns the length. */
size_t dh_utf8_encode(uint32_t cp, uint8_t *out);

typedef enum {
    DH_TEXT_OK = 0,
    DH_TEXT_EMPTY,
    DH_TEXT_BAD_UTF8,
    DH_TEXT_CONTROL,        /* a control or invisible formatting character */
    DH_TEXT_TOO_LONG,
    DH_TEXT_NOT_NORMALISED, /* not NFC, or white space at either end */
} dh_text_problem;

/* The NFC check needs room for one segment of text, decomposed. A segment is at most as long as
 * the text, and a character decomposes to at most 4. Checks of text up to DH_TEXT_MAX_CHECKED
 * characters are exact; longer text is refused as too long before this matters. */
#define DH_TEXT_MAX_CHECKED 280u
typedef struct {
    uint32_t cp[DH_TEXT_MAX_CHECKED * 4u];
} dh_text_scratch;

/* Checks s[0..n) against the text rules, with at most max_chars characters. max_chars must be at
 * most DH_TEXT_MAX_CHECKED. The problems are checked in a cheaper order than the reference
 * does, so text with several problems may report a different one; whether there is a problem is
 * always the same. */
dh_text_problem dh_text_check(const uint8_t *s, size_t n, size_t max_chars,
                              dh_text_scratch *scratch);

/* Whether s[0..n) is in Unicode NFC. s must be valid UTF-8 of at most DH_TEXT_MAX_CHECKED
 * characters; otherwise this returns false. */
bool dh_text_is_nfc(const uint8_t *s, size_t n, dh_text_scratch *scratch);

/* The number of code points in valid UTF-8. */
size_t dh_utf8_count(const uint8_t *s, size_t n);

/* Character properties, as the reference has them. */
bool dh_char_is_forbidden(uint32_t cp); /* Rust's char::is_control, or an invisible format char */
bool dh_char_is_whitespace(uint32_t cp); /* Rust's char::is_whitespace (White_Space) */
uint8_t dh_char_ccc(uint32_t cp);        /* canonical combining class */

#ifdef __cplusplus
}
#endif

#endif
