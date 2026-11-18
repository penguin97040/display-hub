/*
 * Unit tests for dh_core: the JSON reader's edge cases, the text rules and NFC, timestamps, the
 * poll schedule, USB lines, and the parts of the snapshot reader the vectors don't reach.
 */
#include <inttypes.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "dh_json.h"
#include "dh_poll.h"
#include "dh_snapshot.h"
#include "dh_text.h"
#include "dh_time.h"
#include "dh_usb_line.h"

static int checks, failures;

#define CHECK(c) \
    do { \
        checks++; \
        if (!(c)) { \
            failures++; \
            fprintf(stderr, "%s:%d: CHECK(%s) failed\n", __FILE__, __LINE__, #c); \
        } \
    } while (0)

static dh_json_status parse_with(const char *s, size_t len, unsigned flags, unsigned depth)
{
    dh_json_doc doc;
    dh_json_value root;
    dh_json_options o = {0, depth, flags};
    return dh_json_parse(&doc, (const uint8_t *)s, len, &o, &root);
}

static dh_json_status parse(const char *s)
{
    return parse_with(s, strlen(s), 0, 0);
}

static dh_json_status parse_flags(const char *s, unsigned flags)
{
    return parse_with(s, strlen(s), flags, 0);
}

/* Parses s, which must be valid, and returns its root. */
static dh_json_value doc_of(dh_json_doc *doc, const char *s)
{
    dh_json_value root = 0;
    dh_json_status st = dh_json_parse(doc, (const uint8_t *)s, strlen(s), NULL, &root);
    CHECK(st == DH_JSON_OK);
    return root;
}

static void test_json_grammar(void)
{
    const char *good[] = {"{}", "[]", "0", "-0", "1e5", "1E+5", "-1.5e-3", "\"a\"", "true",
                          "false", "null", " \t\r\n[1, {\"a\": [true, null]}] \n",
                          "\"\\\"\\\\\\/\\b\\f\\n\\r\\t\\u00e9\"", "\"\x7f\"", "[[[]]]"};
    for (size_t i = 0; i < sizeof good / sizeof good[0]; i++) {
        CHECK(parse(good[i]) == DH_JSON_OK);
    }
    const char *bad[] = {"{", "[1,]", "{\"a\":1,}", "01", "1.", ".5", "1e", "+1", "tru", "nul",
                         "\"abc", "[1 2]", "{\"a\" 1}", "{1:2}", "1 2", "{}x", "\"\\x\"", "[",
                         "]", "{\"a\":}", "-", "--1", "0x10", "Infinity", "NaN", "'a'",
                         "[1,,2]", "{,}", "\"\\u12\"", "\"\\u12G4\""};
    for (size_t i = 0; i < sizeof bad / sizeof bad[0]; i++) {
        dh_json_status st = parse(bad[i]);
        CHECK(st == DH_JSON_E_SYNTAX || st == DH_JSON_E_ESCAPE);
    }
    CHECK(parse("") == DH_JSON_E_EMPTY);
    CHECK(parse("  \n") == DH_JSON_E_EMPTY);
    /* Only JSON's four white space characters. */
    CHECK(parse("\v1") == DH_JSON_E_SYNTAX);
    CHECK(parse("1\f") == DH_JSON_E_SYNTAX);
    /* A NUL byte is just a bad byte. */
    CHECK(parse_with("[1]\0", 4, 0, 0) == DH_JSON_E_SYNTAX);
    CHECK(parse_with("\"a\0\"", 4, 0, 0) == DH_JSON_E_CONTROL);
    /* The size limit. */
    dh_json_doc doc;
    dh_json_value root;
    dh_json_options small = {3, 0, 0};
    CHECK(dh_json_parse(&doc, (const uint8_t *)"[1]", 3, &small, &root) == DH_JSON_OK);
    CHECK(dh_json_parse(&doc, (const uint8_t *)"[1] ", 4, &small, &root) == DH_JSON_E_TOO_LARGE);
}

static void test_json_truncation(void)
{
    const char *full = "{\"a\":[1,-2.5e3,\"x\\u00e9\\ud83d\\ude00\",true,false,null],\"b\":{}}";
    size_t n = strlen(full);
    CHECK(parse(full) == DH_JSON_OK);
    for (size_t k = 0; k < n; k++) {
        dh_json_status st = parse_with(full, k, 0, 0);
        CHECK(st != DH_JSON_OK);
    }
}

static void test_json_duplicates(void)
{
    CHECK(parse("{\"a\":1,\"a\":2}") == DH_JSON_OK);
    CHECK(parse_flags("{\"a\":1,\"a\":2}", DH_JSON_NO_DUPLICATES) == DH_JSON_E_DUPLICATE);
    CHECK(parse_flags("{\"a\":1,\"\\u0061\":2}", DH_JSON_NO_DUPLICATES) == DH_JSON_E_DUPLICATE);
    CHECK(parse_flags("{\"x\":{\"b\":1,\"c\":2,\"b\":3}}", DH_JSON_NO_DUPLICATES) ==
          DH_JSON_E_DUPLICATE);
    CHECK(parse_flags("[{\"a\":1},{\"a\":1}]", DH_JSON_NO_DUPLICATES) == DH_JSON_OK);
    CHECK(parse_flags("{\"a\":{\"a\":1}}", DH_JSON_NO_DUPLICATES) == DH_JSON_OK);
    CHECK(parse_flags("{\"a\":\"{\\\"a\\\":1,\\\"a\\\":1}\"}", DH_JSON_NO_DUPLICATES) ==
          DH_JSON_OK);
    CHECK(parse_flags("{\"ab\":1,\"a\":2,\"abc\":3}", DH_JSON_NO_DUPLICATES) == DH_JSON_OK);

    dh_json_doc doc;
    dh_json_value root = doc_of(&doc, "{\"a\":1,\"b\":2,\"\\u0061\":3}"), last;
    CHECK(dh_json_object_find(&doc, root, "a", 1, &last) == 2);
    CHECK(dh_json_number_double(&doc, last) == 3.0);
    CHECK(dh_json_object_find(&doc, root, "c", 1, &last) == 0);
}

static void test_json_depth(void)
{
    char deep[600];
    for (unsigned d = 1; d <= 256; d++) {
        memset(deep, '[', d);
        memset(deep + d, ']', d);
        dh_json_status st = parse_with(deep, 2 * d, 0, 0);
        CHECK(st == (d <= DH_JSON_DEFAULT_MAX_DEPTH ? DH_JSON_OK : DH_JSON_E_DEPTH));
    }
    memset(deep, '[', 255);
    memset(deep + 255, ']', 255);
    CHECK(parse_with(deep, 510, 0, 255) == DH_JSON_OK);
    CHECK(parse_with(deep, 510, 0, 254) == DH_JSON_E_DEPTH);
    CHECK(parse_with(deep, 20, 0, 10) == DH_JSON_E_DEPTH);
    CHECK(parse_with("{\"a\":{\"b\":[1]}}", 15, 0, 3) == DH_JSON_OK);
    CHECK(parse_with("{\"a\":{\"b\":[1]}}", 15, 0, 2) == DH_JSON_E_DEPTH);
    /* Brackets inside strings don't count. */
    CHECK(parse_with("[\"[[[[[[[[[[[[\"]", 16, 0, 1) == DH_JSON_OK);
}

static void test_json_utf8(void)
{
    CHECK(parse("\"\xc3\xa9\xe2\x82\xac\xf0\x9f\x98\x80\"") == DH_JSON_OK);
    const char *bad[] = {
        "\"\xc0\x80\"",         /* overlong NUL */
        "\"\xc1\xbf\"",         /* overlong */
        "\"\xe0\x80\x80\"",     /* overlong */
        "\"\xe0\x9f\xbf\"",     /* overlong */
        "\"\xed\xa0\x80\"",     /* a surrogate */
        "\"\xed\xbf\xbf\"",     /* a surrogate */
        "\"\xf0\x8f\xbf\xbf\"", /* overlong */
        "\"\xf4\x90\x80\x80\"", /* beyond U+10FFFF */
        "\"\xf5\x80\x80\x80\"", "\"\xff\"", "\"\x80\"", "\"\xe2\x82\"", "\"\xe2\x82",
        "\"\xc3\"", "\"\xc3\x28\"",
    };
    for (size_t i = 0; i < sizeof bad / sizeof bad[0]; i++) {
        CHECK(parse(bad[i]) == DH_JSON_E_UTF8 || parse(bad[i]) == DH_JSON_E_SYNTAX);
    }
    CHECK(parse("\"\xed\x9f\xbf\xee\x80\x80\xf4\x8f\xbf\xbf\"") == DH_JSON_OK);
    /* Outside strings, any non-ASCII byte is a syntax error. */
    CHECK(parse("\xc3\xa9") == DH_JSON_E_SYNTAX);
    CHECK(parse("\xef\xbb\xbf{}") == DH_JSON_E_SYNTAX); /* no byte order mark */
}

static void test_json_surrogates(void)
{
    CHECK(parse("\"\\ud83d\\ude00\"") == DH_JSON_OK);
    CHECK(parse("\"\\uD83D\\uDE00\"") == DH_JSON_OK);
    CHECK(parse("\"\\ud83d\"") == DH_JSON_E_ESCAPE);
    CHECK(parse("\"\\ude00\"") == DH_JSON_E_ESCAPE);
    CHECK(parse("\"\\ud83dx\"") == DH_JSON_E_ESCAPE);
    CHECK(parse("\"\\ud83d\\u0041\"") == DH_JSON_E_ESCAPE);
    CHECK(parse("\"\\ud83d\\ud83d\"") == DH_JSON_E_ESCAPE);
    CHECK(parse("\"\\ude00\\ud83d\"") == DH_JSON_E_ESCAPE);
    CHECK(parse("\"\\ud83d\\") == DH_JSON_E_ESCAPE);

    dh_json_doc doc;
    dh_json_value v = doc_of(&doc, "\"a\\ud83d\\ude00\\u0000b\"");
    char buf[16];
    size_t len;
    CHECK(dh_json_string_length(&doc, v) == 7);
    CHECK(dh_json_string_copy(&doc, v, buf, sizeof buf, &len) && len == 7);
    CHECK(memcmp(buf, "a\xf0\x9f\x98\x80\0b", 7) == 0 && buf[7] == '\0');
    CHECK(dh_json_string_equals(&doc, v, "a\xf0\x9f\x98\x80\0b", 7));
    CHECK(!dh_json_string_equals(&doc, v, "a\xf0\x9f\x98\x80", 5));
    CHECK(!dh_json_string_copy(&doc, v, buf, 7, &len) && buf[0] == '\0');
    CHECK(dh_json_string_copy(&doc, v, buf, 8, &len));
}

static void test_json_numbers(void)
{
    CHECK(parse("1e308") == DH_JSON_OK);
    CHECK(parse("1.7976931348623157e308") == DH_JSON_OK);
    CHECK(parse("-1.7976931348623157e308") == DH_JSON_OK);
    CHECK(parse("1.7976931348623159e308") == DH_JSON_E_NUMBER_RANGE);
    CHECK(parse("1e309") == DH_JSON_E_NUMBER_RANGE);
    CHECK(parse("-1e400") == DH_JSON_E_NUMBER_RANGE);
    CHECK(parse("[0,1e99999999999999999999]") == DH_JSON_E_NUMBER_RANGE);
    CHECK(parse("1e-400") == DH_JSON_OK);
    CHECK(parse("0e99999999999999") == DH_JSON_OK);
    CHECK(parse("0.0000e400") == DH_JSON_OK);
    CHECK(parse("0.001e311") == DH_JSON_OK);
    CHECK(parse("0.001e312") == DH_JSON_E_NUMBER_RANGE);
    char big[420];
    memset(big, '9', 308);
    big[308] = '\0';
    CHECK(parse(big) == DH_JSON_OK);
    memset(big, '9', 309);
    big[309] = '\0';
    CHECK(parse(big) == DH_JSON_E_NUMBER_RANGE);

    CHECK(parse_flags("[0, 7, 18446744073709551615]", DH_JSON_PLAIN_INTEGERS) == DH_JSON_OK);
    CHECK(parse_flags("-1", DH_JSON_PLAIN_INTEGERS) == DH_JSON_E_NUMBER_FORM);
    CHECK(parse_flags("-0", DH_JSON_PLAIN_INTEGERS) == DH_JSON_E_NUMBER_FORM);
    CHECK(parse_flags("1.0", DH_JSON_PLAIN_INTEGERS) == DH_JSON_E_NUMBER_FORM);
    CHECK(parse_flags("1e3", DH_JSON_PLAIN_INTEGERS) == DH_JSON_E_NUMBER_FORM);
    CHECK(parse_flags("{\"a\":[1,{\"b\":2E0}]}", DH_JSON_PLAIN_INTEGERS) ==
          DH_JSON_E_NUMBER_FORM);

    dh_json_doc doc;
    dh_json_value v;
    uint64_t u;
    v = doc_of(&doc, "18446744073709551615");
    CHECK(dh_json_number_u64(&doc, v, &u) && u == UINT64_MAX);
    CHECK(dh_json_number_serde_class(&doc, v) == DH_JSON_SERDE_U64);
    v = doc_of(&doc, "18446744073709551616");
    CHECK(!dh_json_number_u64(&doc, v, &u));
    CHECK(dh_json_number_serde_class(&doc, v) == DH_JSON_SERDE_F64);
    CHECK(dh_json_number_double(&doc, v) == 18446744073709551616.0);
    v = doc_of(&doc, "-9223372036854775808");
    CHECK(dh_json_number_serde_class(&doc, v) == DH_JSON_SERDE_I64);
    v = doc_of(&doc, "-9223372036854775809");
    CHECK(dh_json_number_serde_class(&doc, v) == DH_JSON_SERDE_F64);
    v = doc_of(&doc, "-0");
    CHECK(dh_json_number_serde_class(&doc, v) == DH_JSON_SERDE_F64);
    CHECK(dh_json_number_form(&doc, v) == DH_JSON_NUM_NEGATIVE);
    v = doc_of(&doc, "-1.5e2");
    CHECK(dh_json_number_form(&doc, v) ==
          (DH_JSON_NUM_NEGATIVE | DH_JSON_NUM_FRACTION | DH_JSON_NUM_EXPONENT));
    CHECK(dh_json_number_double(&doc, v) == -150.0);
    v = doc_of(&doc, "0.1");
    CHECK(dh_json_number_double(&doc, v) == 0.1);
    v = doc_of(&doc, "12345678901234567890123456789012345678901234567890e-49");
    CHECK(dh_json_number_double(&doc, v) == 1.2345678901234567);
    v = doc_of(&doc, "0.000000000000000000000000000000000000000000000000000012345");
    CHECK(dh_json_number_double(&doc, v) == 1.2345e-53);
    v = doc_of(&doc, "4.9e-324");
    CHECK(dh_json_number_double(&doc, v) > 0.0);
}

static size_t serde_length_of(const char *s)
{
    dh_json_doc doc;
    dh_json_value v = doc_of(&doc, s);
    size_t size;
    unsigned depth;
    dh_json_serde_measure(&doc, v, (size_t)-2, &size, &depth);
    return size;
}

static void test_json_serde_lengths(void)
{
    /* Numbers as serde_json writes them back. */
    CHECK(serde_length_of("123") == 3);     /* 123 */
    CHECK(serde_length_of("-45") == 3);     /* -45 */
    CHECK(serde_length_of("1e2") == 5);     /* 100.0 */
    CHECK(serde_length_of("1E5") == 8);     /* 100000.0 */
    CHECK(serde_length_of("-0") == 4);      /* -0.0 */
    CHECK(serde_length_of("0.0") == 3);     /* 0.0 */
    CHECK(serde_length_of("0.1") == 3);     /* 0.1 */
    CHECK(serde_length_of("42.50") == 4);   /* 42.5 */
    CHECK(serde_length_of("1e15") == 18);   /* 1000000000000000.0 */
    CHECK(serde_length_of("1e16") == 4);    /* 1e16 */
    CHECK(serde_length_of("1.5e300") == 7); /* 1.5e300 */
    CHECK(serde_length_of("1e-5") == 7);    /* 0.00001 */
    CHECK(serde_length_of("1e-6") == 4);    /* 1e-6 */
    CHECK(serde_length_of("1.25e-7") == 7); /* 1.25e-7 */
    CHECK(serde_length_of("-1234.5") == 7); /* -1234.5 */
    CHECK(serde_length_of("123456789012345678901234567890") == 21); /* 1.2345678901234568e29 */
    CHECK(serde_length_of("5e-324") == 6);                          /* 5e-324 */
    /* Strings, re-escaped. */
    CHECK(serde_length_of("\"\\u00e9\"") == 4);
    CHECK(serde_length_of("\"\\/\"") == 3);
    CHECK(serde_length_of("\"\\u0001\"") == 8);
    CHECK(serde_length_of("\"\\n\\\"\"") == 6);
    CHECK(serde_length_of("\"\x7f\"") == 3);
    /* Containers, compact, with repeated names collapsed to the last. */
    CHECK(serde_length_of(" { \"a\" : [ 1 , 2 ] , \"b\" : { } } ") == 18);
    CHECK(serde_length_of("{\"a\":[1,2,3],\"a\":1}") == 7);
    CHECK(serde_length_of("{\"a\":1,\"b\":2,\"a\":3}") == 13);
    CHECK(serde_length_of("[]") == 2);

    dh_json_doc doc;
    size_t size;
    unsigned depth;
    dh_json_value v = doc_of(&doc, "{\"a\":[[[[1]]]],\"a\":{}}");
    dh_json_serde_measure(&doc, v, (size_t)-2, &size, &depth);
    CHECK(depth == 2 && size == 8);
    v = doc_of(&doc, "{\"a\":[[[[1]]]]}");
    dh_json_serde_measure(&doc, v, (size_t)-2, &size, &depth);
    CHECK(depth == 5);
    dh_json_serde_measure(&doc, v, 5, &size, &depth);
    CHECK(size == 6);
    CHECK(serde_length_of("5") == 1);
}

static dh_text_scratch scratch;

static dh_text_problem text(const char *s, size_t max)
{
    return dh_text_check((const uint8_t *)s, strlen(s), max, &scratch);
}

static void test_text(void)
{
    CHECK(text("Kitchen Display", 48) == DH_TEXT_OK);
    CHECK(text("T\xc4\x93n\xc4\x81 koe", 48) == DH_TEXT_OK);
    CHECK(text("", 48) == DH_TEXT_EMPTY);
    CHECK(text("a\x07"
               "b",
               48) == DH_TEXT_CONTROL);
    CHECK(text("a\xe2\x80\xae"
               "b",
               48) == DH_TEXT_CONTROL); /* U+202E */
    CHECK(text("a\xc2\x85"
               "b",
               48) == DH_TEXT_CONTROL); /* U+0085 */
    CHECK(text("\xef\xbb\xbf"
               "a",
               48) == DH_TEXT_CONTROL); /* U+FEFF */
    CHECK(text(" padded", 48) == DH_TEXT_NOT_NORMALISED);
    CHECK(text("padded ", 48) == DH_TEXT_NOT_NORMALISED);
    CHECK(text("a\xc2\xa0", 48) == DH_TEXT_NOT_NORMALISED);     /* no-break space */
    CHECK(text("\xe3\x80\x80"
               "a",
               48) == DH_TEXT_NOT_NORMALISED);                 /* ideographic space */
    CHECK(text("a b", 48) == DH_TEXT_OK);
    CHECK(text("e\xcc\x81", 48) == DH_TEXT_NOT_NORMALISED);     /* e + U+0301 */
    CHECK(text("\xc3\xa9", 48) == DH_TEXT_OK);                  /* U+00E9 */
    char long_text[64];
    memset(long_text, 'x', 49);
    long_text[49] = '\0';
    CHECK(text(long_text, 48) == DH_TEXT_TOO_LONG);
    long_text[48] = '\0';
    CHECK(text(long_text, 48) == DH_TEXT_OK);
    /* Characters, not bytes: 48 four-byte characters fit. */
    char emoji[48 * 4 + 1];
    for (int i = 0; i < 48; i++) {
        memcpy(emoji + 4 * i, "\xf0\x9f\x98\x80", 4);
    }
    emoji[192] = '\0';
    CHECK(text(emoji, 48) == DH_TEXT_OK);
    CHECK(text(emoji, 47) == DH_TEXT_TOO_LONG);
    CHECK(dh_text_check((const uint8_t *)"a\xff", 2, 48, &scratch) == DH_TEXT_BAD_UTF8);
}

static bool nfc(const char *s)
{
    return dh_text_is_nfc((const uint8_t *)s, strlen(s), &scratch);
}

static void test_nfc(void)
{
    CHECK(nfc("plain ASCII"));
    CHECK(nfc("\xc3\xa9"));          /* U+00E9 */
    CHECK(!nfc("e\xcc\x81"));        /* e U+0301 */
    CHECK(!nfc("\xef\xa4\x80"));     /* U+F900, a CJK compatibility ideograph */
    CHECK(!nfc("\xe2\x84\xab"));     /* U+212B ANGSTROM SIGN, a singleton */
    CHECK(nfc("\xc3\x85"));          /* U+00C5 */
    CHECK(!nfc("\xcd\x84"));         /* U+0344, a non-starter decomposition */
    /* U+00EA U+0323 reorders to e U+0323 U+0302 and composes to U+1EC7. */
    CHECK(!nfc("\xc3\xaa\xcc\xa3"));
    CHECK(nfc("\xe1\xbb\x87"));      /* U+1EC7 */
    /* a U+0323 U+0301 composes the first mark only: U+1EA1 U+0301. */
    CHECK(!nfc("a\xcc\xa3\xcc\x81"));
    CHECK(nfc("\xe1\xba\xa1\xcc\x81"));
    CHECK(!nfc("\xe1\xba\xa1\xcc\x81\xcc\xa3")); /* out of canonical order */
    CHECK(!nfc("a\xcc\x81\xcc\xa3"));            /* 230 before 220 */
    /* A mark blocked by one of the same class stays apart. */
    CHECK(nfc("\xc3\xa1\xcc\x81"));              /* U+00E1 U+0301 */
    CHECK(nfc("\xcc\x81"));                      /* a lone combining mark */
    CHECK(nfc("\xcc\xa3\xcc\x81"));
    CHECK(!nfc("\xcc\x81\xcc\xa3"));
    /* Hangul. */
    CHECK(nfc("\xea\xb0\x80"));                  /* U+AC00 */
    CHECK(!nfc("\xe1\x84\x80\xe1\x85\xa1"));     /* U+1100 U+1161 */
    CHECK(!nfc("\xea\xb0\x80\xe1\x86\xa8"));     /* U+AC00 U+11A8 */
    CHECK(nfc("\xea\xb0\x81"));                  /* U+AC01 */
    CHECK(nfc("\xea\xb0\x81\xe1\x86\xa8"));      /* U+AC01 U+11A8: LVT takes no more */
    CHECK(nfc("\xe1\x84\x80"));                  /* a lone leading jamo */
    /* b U+0323 composes to U+1E05, after a starter that doesn't take it. */
    CHECK(!nfc("ab\xcc\xa3"));
    CHECK(nfc("a\xe1\xb8\x85"));
    /* Text that is too long to check is refused. */
    char many[300 * 2 + 1];
    for (int i = 0; i < 300; i++) {
        memcpy(many + 2 * i, "\xcc\x81", 2);
    }
    many[600] = '\0';
    CHECK(!nfc(many));
    many[280 * 2] = '\0';
    CHECK(nfc(many)); /* 280 combining acutes in a row are NFC */
}

static void test_time(void)
{
    int64_t t;
    CHECK(dh_time_parse("2026-11-10T20:00:00Z", 20, &t) && t == 1794340800);
    CHECK(dh_time_parse("1970-01-01T00:00:00Z", 20, &t) && t == 0);
    CHECK(dh_time_parse("2000-02-29T00:00:00Z", 20, &t) && t == 951782400);
    CHECK(dh_time_parse("9999-12-31T23:59:59Z", 20, &t) && t == 253402300799);
    CHECK(dh_time_parse("0000-01-01T00:00:00Z", 20, &t) && t == -62167219200);
    CHECK(dh_time_parse("0000-02-29T00:00:00Z", 20, &t));
    const char *bad[] = {"2023-02-29T00:00:00Z", "1900-02-29T00:00:00Z", "2026-13-10T20:00:00Z",
                         "2026-00-10T20:00:00Z", "2026-11-00T20:00:00Z", "2026-11-31T20:00:00Z",
                         "2026-11-10T24:00:00Z", "2026-11-10T20:60:00Z", "2026-11-10T20:00:60Z",
                         "2026-11-10T20:00:00z", "2026-11-10 20:00:00Z", "2026-11-10t20:00:00Z",
                         "+026-11-10T20:00:00Z", "2026-1a-10T20:00:00Z", "2026-11-10T20:00:00+",
                         " 026-11-10T20:00:00Z"};
    for (size_t i = 0; i < sizeof bad / sizeof bad[0]; i++) {
        CHECK(!dh_time_parse(bad[i], 20, &t));
    }
    CHECK(!dh_time_parse("2026-11-10T20:00:00.5Z", 22, &t));
    CHECK(!dh_time_parse("2026-11-10T20:00:00+00:00", 25, &t));
    CHECK(!dh_time_parse("", 0, &t));
}

static uint32_t fixed_random(void *ctx)
{
    return *(const uint32_t *)ctx;
}

static void test_poll(void)
{
    uint32_t expect[] = {15, 30, 60, 120, 240, 480, 900, 900, 900};
    for (uint32_t i = 0; i < 9; i++) {
        CHECK(dh_backoff_seconds(i + 1) == expect[i]);
    }
    CHECK(dh_backoff_seconds(UINT32_MAX) == 900);
    uint32_t r = 0;
    CHECK(dh_jitter_ms(15000, 20, fixed_random, &r) == 12000);
    r = UINT32_MAX;
    CHECK(dh_jitter_ms(15000, 20, fixed_random, &r) == 18000);
    r = 0x80000000u;
    CHECK(dh_jitter_ms(15000, 20, fixed_random, &r) == 15000);
    r = 0;
    CHECK(dh_jitter_ms(300000, 10, fixed_random, &r) == 270000);

    dh_poll_state st = {0, DH_PROBLEM_NONE};
    dh_response retry = {DH_OUTCOME_RETRY, DH_NEXT_BACKOFF, DH_ERROR_UNAVAILABLE,
                         DH_UNREADABLE_NONE, 0};
    dh_response revoked = {DH_OUTCOME_REPROVISION, DH_NEXT_HOURLY, DH_ERROR_TOKEN_REVOKED,
                           DH_UNREADABLE_NONE, 0};
    dh_response ok = {DH_OUTCOME_SHOW, DH_NEXT_REFRESH, DH_ERROR_NONE, DH_UNREADABLE_NONE, 0};
    r = 0x80000000u;
    CHECK(dh_poll_answered(&st, &retry, 0, fixed_random, &r) == 15000);
    CHECK(st.problem == DH_PROBLEM_BUSY);
    CHECK(dh_poll_failed(&st, DH_PROBLEM_UNREACHABLE, fixed_random, &r) == 30000);
    CHECK(dh_poll_answered(&st, &retry, 0, fixed_random, &r) == 60000);
    CHECK(dh_poll_answered(&st, &revoked, 0, fixed_random, &r) == 3600000);
    CHECK(st.problem == DH_PROBLEM_NEEDS_SETUP && st.failures == 4);
    CHECK(dh_poll_answered(&st, &ok, 300, fixed_random, &r) == 300000);
    CHECK(st.failures == 0 && st.problem == DH_PROBLEM_NONE);
    CHECK(dh_poll_answered(&st, &retry, 0, fixed_random, &r) == 15000);
    CHECK(strcmp(dh_problem_headline(DH_PROBLEM_BUSY), "The hub is busy") == 0);
}

static dh_usb_line_reader reader;

/* Feeds s in pieces of `piece` bytes and writes the events and lines into out. */
static void feed(const uint8_t *s, size_t n, size_t piece, char *out, size_t cap)
{
    size_t o = 0;
    for (size_t i = 0; i < n;) {
        size_t take = n - i < piece ? n - i : piece;
        size_t used;
        const uint8_t *line;
        size_t line_len;
        while (take > 0) {
            dh_line_event e = dh_usb_line_feed(&reader, s + i, take, &used, &line, &line_len);
            if (e == DH_LINE_JSON) {
                o += (size_t)snprintf(out + o, cap - o, "[%.*s]", (int)line_len,
                                      (const char *)line);
            } else if (e == DH_LINE_OVERLONG) {
                o += (size_t)snprintf(out + o, cap - o, "<overlong>");
            }
            i += used;
            take -= used;
        }
    }
}

static void test_usb_lines(void)
{
    char out[256];
    const char *s = "I (312) boot: ESP-IDF v5.3\r\n{\"type\":\"provisioned\"}\r\n\n  "
                    "{\"not\":\"first\"}\n{\"partial\"";
    for (size_t piece = 1; piece <= 64; piece++) {
        dh_usb_line_init(&reader);
        out[0] = '\0';
        feed((const uint8_t *)s, strlen(s), piece, out, sizeof out);
        CHECK(strcmp(out, "[{\"type\":\"provisioned\"}]") == 0);
    }
    static uint8_t big[DH_MAX_USB_LINE_BYTES + 200];
    memset(big, '{', sizeof big);
    const char *tail = "tail\n{\"type\":\"provisioned\"}\n";
    dh_usb_line_init(&reader);
    out[0] = '\0';
    feed(big, DH_MAX_USB_LINE_BYTES + 100, 1000, out, sizeof out);
    feed((const uint8_t *)tail, strlen(tail), 7, out + strlen(out), sizeof out - strlen(out));
    CHECK(strcmp(out, "<overlong>[{\"type\":\"provisioned\"}]") == 0);
    /* 16383 bytes and a newline is a line; 16384 is too long. */
    dh_usb_line_init(&reader);
    big[DH_MAX_USB_LINE_BYTES - 1] = '\n';
    size_t used;
    const uint8_t *line;
    size_t line_len;
    CHECK(dh_usb_line_feed(&reader, big, DH_MAX_USB_LINE_BYTES, &used, &line, &line_len) ==
          DH_LINE_JSON);
    CHECK(line_len == DH_MAX_USB_LINE_BYTES - 1 && used == DH_MAX_USB_LINE_BYTES);
    big[DH_MAX_USB_LINE_BYTES - 1] = '{';
    big[DH_MAX_USB_LINE_BYTES] = '\n';
    CHECK(dh_usb_line_feed(&reader, big, DH_MAX_USB_LINE_BYTES + 1, &used, &line, &line_len) ==
          DH_LINE_OVERLONG);
    CHECK(used == DH_MAX_USB_LINE_BYTES);
    CHECK(dh_usb_line_feed(&reader, big + used, 1, &used, &line, &line_len) == DH_LINE_NONE);
}

/* ----- snapshot details the vectors don't cover ----- */

static dh_snapshot snap;

#define ENVELOPE "\"protocol_version\":1,\"message_type\":\"dashboard.snapshot\"," \
                 "\"generated_at\":\"2026-11-10T20:00:00Z\",\"refresh_after_seconds\":300"
#define PAGE(kind, data) "{\"id\":\"p\",\"title\":\"P\",\"kind\":\"" kind "\"," \
                         "\"data_quality\":\"manual\",\"updated_at\":\"2026-11-10T19:00:00Z\"," \
                         "\"stale_after_seconds\":600" data "}"

static dh_response read_body(const char *body, const dh_read_options *opts)
{
    return dh_read_response(200, (const uint8_t *)body, strlen(body), opts, &snap, &scratch);
}

static bool flights_only(const char *kind, void *ctx)
{
    (void)ctx;
    return strcmp(kind, "nearby-flights/1") == 0;
}

static void test_snapshot_details(void)
{
    dh_response r;
    /* A custom kind the caller draws. */
    const char *custom = "{" ENVELOPE ",\"pages\":[" PAGE("nearby-flights/1", ",\"data\":{\"n\":[1]}") "]}";
    dh_read_options opts = {flights_only, NULL};
    r = read_body(custom, &opts);
    CHECK(r.outcome == DH_OUTCOME_SHOW && snap.pages[0].content.shape == DH_SHAPE_CUSTOM);
    CHECK(strncmp(custom + snap.pages[0].content.u.custom.data_offset, "{\"n\":[1]}",
                  snap.pages[0].content.u.custom.data_length) == 0);
    CHECK(strcmp(snap.pages[0].kind, "nearby-flights/1") == 0);
    r = read_body(custom, NULL);
    CHECK(r.outcome == DH_OUTCOME_SHOW &&
          snap.pages[0].cant_show == DH_CANT_SHOW_UNKNOWN_KIND);

    /* A repeated page field makes the snapshot unreadable; inside data the last one wins. */
    r = read_body("{" ENVELOPE ",\"pages\":[" PAGE("text", ",\"data\":{\"text\":\"a\"},\"title\":\"Q\"") "]}", NULL);
    CHECK(r.outcome == DH_OUTCOME_UNREADABLE && r.unreadable == DH_UNREADABLE_MALFORMED);
    r = read_body("{" ENVELOPE ",\"pages\":[" PAGE("text", ",\"data\":{\"text\":\"\",\"text\":\"b\"}") "]}", NULL);
    CHECK(r.outcome == DH_OUTCOME_SHOW && strcmp(snap.pages[0].content.u.text.text, "b") == 0);
    r = read_body("{" ENVELOPE ",\"pages\":[" PAGE("text", ",\"data\":{\"text\":\"b\",\"text\":\"\"}") "]}", NULL);
    CHECK(r.outcome == DH_OUTCOME_SHOW && snap.pages[0].cant_show == DH_CANT_SHOW_BAD_DATA);
    r = read_body("{" ENVELOPE ",\"pages\":[],\"pages\":[]}", NULL);
    CHECK(r.outcome == DH_OUTCOME_UNREADABLE);

    /* The version is read before anything else that could be wrong. */
    r = read_body("{\"protocol_version\":2,\"pages\":7}", NULL);
    CHECK(r.outcome == DH_OUTCOME_INCOMPATIBLE && r.next_poll == DH_NEXT_HOURLY);
    r = read_body("{\"protocol_version\":2,\"message_type\":5}", NULL);
    CHECK(r.outcome == DH_OUTCOME_UNREADABLE);
    r = read_body("{\"protocol_version\":1.0}", NULL);
    CHECK(r.outcome == DH_OUTCOME_UNREADABLE);
    r = read_body("[1,\"dashboard.snapshot\"]", NULL);
    CHECK(r.outcome == DH_OUTCOME_UNREADABLE);

    /* The firmware offer. */
    r = read_body("{" ENVELOPE ",\"pages\":[],\"firmware_update\":{\"offer_id\":"
                  "\"0123456789abcdef0123456789abcdef\",\"package_id\":"
                  "\"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\","
                  "\"size\":4096}}", NULL);
    CHECK(r.outcome == DH_OUTCOME_SHOW && snap.firmware_update.present &&
          snap.firmware_update.usable && snap.firmware_update.size == 4096);
    r = read_body("{" ENVELOPE ",\"pages\":[],\"firmware_update\":{\"offer_id\":\"x\","
                  "\"package_id\":\"y\",\"size\":1}}", NULL);
    CHECK(r.outcome == DH_OUTCOME_SHOW && snap.firmware_update.present &&
          !snap.firmware_update.usable);
    r = read_body("{" ENVELOPE ",\"pages\":[],\"firmware_update\":{\"offer_id\":\"x\","
                  "\"package_id\":\"y\",\"size\":1.0}}", NULL);
    CHECK(r.outcome == DH_OUTCOME_SHOW && !snap.firmware_update.present);
    r = read_body("{" ENVELOPE ",\"pages\":[],\"firmware_update\":null}", NULL);
    CHECK(r.outcome == DH_OUTCOME_SHOW && !snap.firmware_update.present);
    r = read_body("{" ENVELOPE ",\"pages\":[],\"firmware_update\":1,\"firmware_update\":2}", NULL);
    CHECK(r.outcome == DH_OUTCOME_UNREADABLE);

    /* big_number text. */
    r = read_body("{" ENVELOPE ",\"pages\":[" PAGE("big_number", ",\"data\":{\"value\":-0.0}") "]}", NULL);
    CHECK(r.outcome == DH_OUTCOME_SHOW && strcmp(snap.pages[0].content.u.big_number.text, "-0") == 0);
    r = read_body("{" ENVELOPE ",\"pages\":[" PAGE("big_number", ",\"data\":{\"value\":2.5,\"decimals\":0}") "]}", NULL);
    CHECK(r.outcome == DH_OUTCOME_SHOW && strcmp(snap.pages[0].content.u.big_number.text, "2") == 0);
    r = read_body("{" ENVELOPE ",\"pages\":[" PAGE("big_number", ",\"data\":{\"value\":-1e15,\"decimals\":3}") "]}", NULL);
    CHECK(r.outcome == DH_OUTCOME_SHOW &&
          strcmp(snap.pages[0].content.u.big_number.text, "-1000000000000000.000") == 0);
    r = read_body("{" ENVELOPE ",\"pages\":[" PAGE("big_number", ",\"data\":{\"value\":1,\"decimals\":\"2\"}") "]}", NULL);
    CHECK(r.outcome == DH_OUTCOME_SHOW && snap.pages[0].cant_show == DH_CANT_SHOW_BAD_DATA);

    /* Ages and countdowns on the hub's clock, even for absurd held times. */
    r = read_body("{" ENVELOPE ",\"pages\":[" PAGE("meter", ",\"data\":{\"items\":[{\"id\":\"a\",\"label\":\"A\",\"resets_at\":\"2026-11-10T21:00:00Z\"}]}") "]}", NULL);
    CHECK(r.outcome == DH_OUTCOME_SHOW && snap.pages[0].content.shape == DH_SHAPE_METER);
    const dh_meter_item *item = &snap.pages[0].content.u.meter.items[0];
    dh_freshness f = dh_page_freshness(&snap, &snap.pages[0], 0);
    CHECK(f.known && f.age_seconds == 3600 && f.stale);
    f = dh_page_freshness(&snap, &snap.pages[0], -50);
    CHECK(f.age_seconds == 3600);
    f = dh_page_freshness(&snap, &snap.pages[0], INT64_MAX);
    CHECK(f.age_seconds == INT64_MAX && f.stale);
    dh_reset rs = dh_meter_item_reset(&snap, item, 0);
    CHECK(rs.counting && rs.resets_in_seconds == 3600 && !rs.reset_due);
    rs = dh_meter_item_reset(&snap, item, 3600);
    CHECK(!rs.counting && rs.reset_due);
    rs = dh_meter_item_reset(&snap, item, INT64_MAX);
    CHECK(rs.reset_due);
    rs = dh_meter_item_reset(&snap, item, INT64_MIN);
    CHECK(rs.counting && rs.resets_in_seconds == INT64_MAX);
    CHECK(!snap.pages[0].content.u.meter.items[0].has_percent);

    /* Errors. */
    const char *no_message = "{\"error\":{\"code\":\"token_revoked\"}}";
    r = dh_read_response(401, (const uint8_t *)no_message, strlen(no_message), NULL, &snap, &scratch);
    CHECK(r.outcome == DH_OUTCOME_FAULT && r.error_code == DH_ERROR_NONE);
    const char *revoked = "{\"error\":{\"message\":\"m\",\"code\":\"token_revoked\",\"x\":1},\"y\":2}";
    r = dh_read_response(401, (const uint8_t *)revoked, strlen(revoked), NULL, &snap, &scratch);
    CHECK(r.outcome == DH_OUTCOME_REPROVISION && r.error_code == DH_ERROR_TOKEN_REVOKED);
    CHECK(strcmp(dh_error_code_name(r.error_code), "token_revoked") == 0);
    r = dh_read_response(200, (const uint8_t *)revoked, strlen(revoked), NULL, &snap, &scratch);
    CHECK(r.outcome == DH_OUTCOME_UNREADABLE);
    r = dh_read_response(500, (const uint8_t *)"", 0, NULL, &snap, &scratch);
    CHECK(r.outcome == DH_OUTCOME_RETRY);
    r = dh_read_response(204, (const uint8_t *)"", 0, NULL, &snap, &scratch);
    CHECK(r.outcome == DH_OUTCOME_FAULT);
}

int main(void)
{
    test_json_grammar();
    test_json_truncation();
    test_json_duplicates();
    test_json_depth();
    test_json_utf8();
    test_json_surrogates();
    test_json_numbers();
    test_json_serde_lengths();
    test_text();
    test_nfc();
    test_time();
    test_poll();
    test_usb_lines();
    test_snapshot_details();
    printf("unit: %d checks, %d failed\n", checks, failures);
    return failures ? 1 : 0;
}
