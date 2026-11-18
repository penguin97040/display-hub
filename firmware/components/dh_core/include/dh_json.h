/*
 * A small, strict JSON reader for untrusted input.
 *
 * dh_json_parse() checks a whole document in one pass without allocating: the grammar of RFC 8259,
 * UTF-8 (shortest forms only, no surrogates), escapes (a \u surrogate must be one half of a pair),
 * a nesting limit, a size limit, and that every number is within the range of a double. Optional
 * flags also refuse repeated member names, or any number that isn't a plain unsigned integer.
 *
 * After that, the document is read in place: a value is the offset of its first byte in the
 * buffer, and the functions below find members, walk arrays and decode strings and numbers into
 * caller-provided storage. They must only be given values from a document that parsed with
 * DH_JSON_OK, and the buffer must not change while they are used.
 */
#ifndef DH_JSON_H
#define DH_JSON_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef enum {
    DH_JSON_OK = 0,
    DH_JSON_E_EMPTY,        /* nothing but white space */
    DH_JSON_E_TOO_LARGE,    /* longer than max_bytes */
    DH_JSON_E_SYNTAX,       /* not JSON, or something after the value */
    DH_JSON_E_DEPTH,        /* nested deeper than max_depth */
    DH_JSON_E_UTF8,         /* bytes that aren't UTF-8 in a string */
    DH_JSON_E_ESCAPE,       /* a bad escape, or half a surrogate pair */
    DH_JSON_E_CONTROL,      /* a raw control character in a string */
    DH_JSON_E_NUMBER_RANGE, /* a number beyond the range of a double */
    DH_JSON_E_NUMBER_FORM,  /* with DH_JSON_PLAIN_INTEGERS: a sign, fraction or exponent */
    DH_JSON_E_DUPLICATE,    /* with DH_JSON_NO_DUPLICATES: a member name repeated in an object */
} dh_json_status;

/* Refuse an object that names the same member twice (compared after decoding escapes). */
#define DH_JSON_NO_DUPLICATES 0x1u
/* Refuse any number with a minus sign, a fraction or an exponent. */
#define DH_JSON_PLAIN_INTEGERS 0x2u

/* Containers nested deeper than this are refused unless the options say otherwise. 127 is what
 * serde_json, which the reference implementation uses, allows. */
#define DH_JSON_DEFAULT_MAX_DEPTH 127u
/* The most max_depth may be. */
#define DH_JSON_DEPTH_LIMIT 255u

typedef struct {
    size_t max_bytes;   /* 0: no limit beyond what a size_t holds */
    unsigned max_depth; /* 0: DH_JSON_DEFAULT_MAX_DEPTH; at most DH_JSON_DEPTH_LIMIT */
    unsigned flags;     /* DH_JSON_NO_DUPLICATES, DH_JSON_PLAIN_INTEGERS */
} dh_json_options;

typedef struct {
    const uint8_t *buf;
    size_t len;
    size_t error_offset; /* where parsing stopped, when it failed */
} dh_json_doc;

/* A value: the offset of its first byte in doc->buf. */
typedef size_t dh_json_value;

typedef enum {
    DH_JSON_NULL,
    DH_JSON_FALSE,
    DH_JSON_TRUE,
    DH_JSON_NUMBER,
    DH_JSON_STRING,
    DH_JSON_ARRAY,
    DH_JSON_OBJECT,
} dh_json_type;

/* Checks buf[0..len) and, on success, fills *doc and *root. opts may be NULL for the defaults. */
dh_json_status dh_json_parse(dh_json_doc *doc, const uint8_t *buf, size_t len,
                             const dh_json_options *opts, dh_json_value *root);

const char *dh_json_status_name(dh_json_status s);

dh_json_type dh_json_type_of(const dh_json_doc *doc, dh_json_value v);

/* The offset just past a value. */
size_t dh_json_end(const dh_json_doc *doc, dh_json_value v);

/* Walking containers. Start an iterator with *_begin (false if v isn't that type), then call
 * *_next until it returns false. */
typedef struct {
    size_t pos;
} dh_json_iter;

bool dh_json_object_begin(const dh_json_doc *doc, dh_json_value v, dh_json_iter *it);
bool dh_json_object_next(const dh_json_doc *doc, dh_json_iter *it, dh_json_value *key,
                         dh_json_value *value);
bool dh_json_array_begin(const dh_json_doc *doc, dh_json_value v, dh_json_iter *it);
bool dh_json_array_next(const dh_json_doc *doc, dh_json_iter *it, dh_json_value *value);
size_t dh_json_array_length(const dh_json_doc *doc, dh_json_value v);

/* How many members of object obj are named name (name_len bytes, compared with the decoded
 * member name). If any, *last is the value of the last one. 0 if obj isn't an object. */
size_t dh_json_object_find(const dh_json_doc *doc, dh_json_value obj, const char *name,
                           size_t name_len, dh_json_value *last);

/* Strings. */
bool dh_json_string_equals(const dh_json_doc *doc, dh_json_value str, const char *s, size_t n);
bool dh_json_strings_equal(const dh_json_doc *doc, dh_json_value a, dh_json_value b);
/* The decoded length in bytes. */
size_t dh_json_string_length(const dh_json_doc *doc, dh_json_value str);
/* Decodes into out, NUL-terminated, and sets *len (without the NUL). False, with out[0] = 0 when
 * cap > 0, if it doesn't fit in cap bytes including the NUL. The decoded text may hold NUL bytes
 * from \u0000, so use *len rather than strlen. */
bool dh_json_string_copy(const dh_json_doc *doc, dh_json_value str, char *out, size_t cap,
                         size_t *len);
/* Calls f for each decoded code point in turn, stopping early if it returns false. */
bool dh_json_string_each(const dh_json_doc *doc, dh_json_value str,
                         bool (*f)(uint32_t cp, void *ctx), void *ctx);

/* Numbers. The form flags say how a number is written. */
#define DH_JSON_NUM_NEGATIVE 0x1u
#define DH_JSON_NUM_FRACTION 0x2u
#define DH_JSON_NUM_EXPONENT 0x4u
unsigned dh_json_number_form(const dh_json_doc *doc, dh_json_value num);
/* A plain unsigned integer (no sign, fraction or exponent) that fits in 64 bits. */
bool dh_json_number_u64(const dh_json_doc *doc, dh_json_value num, uint64_t *out);
/* The nearest double. Always finite for a parsed document. */
double dh_json_number_double(const dh_json_doc *doc, dh_json_value num);

/* How serde_json, which the reference implementation uses, holds a number: an unsigned or a
 * signed 64-bit integer when written as one and it fits, otherwise a double ("-0" is a double). */
typedef enum {
    DH_JSON_SERDE_U64,
    DH_JSON_SERDE_I64,
    DH_JSON_SERDE_F64,
} dh_json_serde_number;
dh_json_serde_number dh_json_number_serde_class(const dh_json_doc *doc, dh_json_value num);

/* What serde_json makes of a value when it reads it into its own Value type and writes it out
 * again compactly: for a repeated member name only the last one is kept, strings are escaped
 * serde_json's way and doubles are written in shortest form. *size is its length in bytes, or
 * size_limit + 1 once it is known to be longer than size_limit. *depth is how deeply containers
 * nest in it, counting v itself as 1 if it is a container. */
void dh_json_serde_measure(const dh_json_doc *doc, dh_json_value v, size_t size_limit,
                           size_t *size, unsigned *depth);
/* The length serde_json gives a string value, escaped and quoted. */
size_t dh_json_serde_string_length(const dh_json_doc *doc, dh_json_value str);
/* The length serde_json gives a number value, written out. */
size_t dh_json_serde_number_length(const dh_json_doc *doc, dh_json_value num);

#ifdef __cplusplus
}
#endif

#endif
