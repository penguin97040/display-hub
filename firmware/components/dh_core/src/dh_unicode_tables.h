/* The tables tools/unicode-tables writes into dh_unicode_tables.c. Internal to dh_core. */
#ifndef DH_UNICODE_TABLES_H
#define DH_UNICODE_TABLES_H

#include <stddef.h>
#include <stdint.h>

/* {first | (value << 24), last}, sorted, for runs of code points sharing a non-zero value. */
extern const uint32_t dh_ccc_ranges[][2];
extern const size_t dh_ccc_range_count;
/* NFC quick check: 1 for No, 2 for Maybe; absent is Yes. */
extern const uint32_t dh_nfc_qc_ranges[][2];
extern const size_t dh_nfc_qc_range_count;
/* Primary composites outside Hangul: (first << 42) | (second << 21) | composite, sorted. */
extern const uint64_t dh_compose_pairs[];
extern const size_t dh_compose_count;
/* Indexes into dh_compose_pairs, sorted by composite. */
extern const uint16_t dh_compose_by_composite[];

#endif
