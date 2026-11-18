/*
 * Fuzzes the JSON reader: parses with and without the strict options, then reads every value
 * of an accepted document every way dh_json offers, checking the answers agree.
 */
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

#include "dh_json.h"

#define CHECK(c) \
    do { \
        if (!(c)) { \
            abort(); \
        } \
    } while (0)

static void walk(const dh_json_doc *doc, dh_json_value v, unsigned depth)
{
    size_t end = dh_json_end(doc, v);
    CHECK(end > v && end <= doc->len);
    size_t size;
    unsigned d;
    dh_json_serde_measure(doc, v, (size_t)-2, &size, &d);
    CHECK(size >= 1);
    switch (dh_json_type_of(doc, v)) {
    case DH_JSON_STRING: {
        char buf[64];
        size_t len = dh_json_string_length(doc, v), got = 0;
        bool fits = dh_json_string_copy(doc, v, buf, sizeof buf, &got);
        CHECK(fits == (len < sizeof buf));
        if (fits) {
            CHECK(got == len && buf[len] == '\0');
            CHECK(dh_json_string_equals(doc, v, buf, len));
        }
        CHECK(dh_json_strings_equal(doc, v, v));
        CHECK(dh_json_serde_string_length(doc, v) >= len + 2);
        CHECK(d == 0);
        break;
    }
    case DH_JSON_NUMBER: {
        double x = dh_json_number_double(doc, v);
        CHECK(x == x && x - x == 0.0); /* finite */
        uint64_t u;
        if (dh_json_number_u64(doc, v, &u)) {
            CHECK(dh_json_number_form(doc, v) == 0);
            CHECK(dh_json_number_serde_class(doc, v) == DH_JSON_SERDE_U64);
        }
        CHECK(dh_json_serde_number_length(doc, v) >= 1);
        break;
    }
    case DH_JSON_ARRAY: {
        CHECK(d >= 1);
        dh_json_iter it;
        dh_json_value e;
        size_t n = 0;
        CHECK(dh_json_array_begin(doc, v, &it));
        while (dh_json_array_next(doc, &it, &e)) {
            walk(doc, e, depth + 1);
            n++;
        }
        CHECK(n == dh_json_array_length(doc, v));
        break;
    }
    case DH_JSON_OBJECT: {
        CHECK(d >= 1);
        dh_json_iter it;
        dh_json_value k, e;
        CHECK(dh_json_object_begin(doc, v, &it));
        while (dh_json_object_next(doc, &it, &k, &e)) {
            CHECK(dh_json_type_of(doc, k) == DH_JSON_STRING);
            char name[32];
            size_t len;
            if (dh_json_string_copy(doc, k, name, sizeof name, &len)) {
                dh_json_value last;
                CHECK(dh_json_object_find(doc, v, name, len, &last) >= 1);
            }
            walk(doc, e, depth + 1);
        }
        break;
    }
    default:
        CHECK(d == 0);
        break;
    }
}

int LLVMFuzzerTestOneInput(const uint8_t *data, size_t size)
{
    dh_json_doc doc;
    dh_json_value root;
    dh_json_status loose = dh_json_parse(&doc, data, size, NULL, &root);
    if (loose == DH_JSON_OK) {
        walk(&doc, root, 0);
    }
    dh_json_options strict = {32768, 16, DH_JSON_NO_DUPLICATES | DH_JSON_PLAIN_INTEGERS};
    dh_json_status st = dh_json_parse(&doc, data, size, &strict, &root);
    if (st == DH_JSON_OK) {
        /* Anything the strict reading accepts, the loose one accepts too. */
        CHECK(loose == DH_JSON_OK);
        walk(&doc, root, 0);
    }
    return 0;
}
