/*
 * Timestamps on the wire: always YYYY-MM-DDTHH:MM:SSZ, UTC, exactly 20 characters, with no
 * fractions or offsets (docs/protocol-v1.md, "Timestamps").
 */
#ifndef DH_TIME_H
#define DH_TIME_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Parses exactly the wire form into seconds since 1970-01-01T00:00:00Z. Years 0000 to 9999 of the
 * proleptic Gregorian calendar; real dates only; seconds 00 to 59. */
bool dh_time_parse(const char *s, size_t n, int64_t *unix_seconds);

#ifdef __cplusplus
}
#endif

#endif
