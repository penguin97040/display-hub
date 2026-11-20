/*
 * When to poll next, and the problem to show (docs/protocol-v1.md, "What a display does").
 *
 * After a success: refresh_after_seconds, +-10%. After a failure: 15 s, doubling with each failure
 * in a row to at most 15 minutes, +-20%; or an hour, +-20%, when the display needs setting up
 * again, the versions don't match or its settings can't be used. This is the reference display's schedule
 * (tools/display-simulator, src/device.rs).
 *
 * Randomness comes from the caller, so tests can fix it and firmware can use its own source.
 */
#ifndef DH_POLL_H
#define DH_POLL_H

#include <stdint.h>

#include "dh_snapshot.h"

#ifdef __cplusplus
extern "C" {
#endif

/* Returns 32 random bits. Need not be cryptographic. */
typedef uint32_t (*dh_random_fn)(void *ctx);

#define DH_BACKOFF_FIRST_SECONDS 15u
#define DH_BACKOFF_MAX_SECONDS 900u
#define DH_HOURLY_SECONDS 3600u

/* The delay before jitter after the n-th failure in a row (n >= 1). */
uint32_t dh_backoff_seconds(uint32_t failures);

/* base_ms spread by +-percent, uniformly. */
uint32_t dh_jitter_ms(uint32_t base_ms, uint32_t percent, dh_random_fn rnd, void *ctx);

/* What the screen says is wrong, in the reference display's words. */
typedef enum {
    DH_PROBLEM_NONE,
    DH_PROBLEM_UNREACHABLE,  /* no answer at all (set by the caller) */
    DH_PROBLEM_WRONG_HUB,    /* a certificate mismatch (set by the caller) */
    DH_PROBLEM_UNREADABLE,   /* the hub sent data this display can't read */
    DH_PROBLEM_BUSY,         /* the hub is busy */
    DH_PROBLEM_NEEDS_SETUP,  /* this display needs setting up again */
    DH_PROBLEM_INCOMPATIBLE, /* hub and display versions don't match */
    /* What the display was set up with can't be used: no Wi-Fi details, or no trust anchor that
     * reads as a certificate (set by the caller). Only setting it up again changes that. */
    DH_PROBLEM_SETTINGS,
} dh_problem;

const char *dh_problem_headline(dh_problem p);

typedef struct {
    uint32_t failures; /* failures in a row */
    dh_problem problem;
} dh_poll_state;

/* Records one answer and returns how long to wait, in milliseconds, before the next poll.
 * refresh_after_seconds is the new snapshot's, for DH_OUTCOME_SHOW. */
uint32_t dh_poll_answered(dh_poll_state *st, const dh_response *r, uint32_t refresh_after_seconds,
                          dh_random_fn rnd, void *ctx);

/* Records a poll that got no answer, or one from a hub that isn't ours (problem is
 * DH_PROBLEM_UNREACHABLE or DH_PROBLEM_WRONG_HUB), and returns the wait in milliseconds. For
 * DH_PROBLEM_SETTINGS the wait is an hour, as the reference waits when its settings are wrong. */
uint32_t dh_poll_failed(dh_poll_state *st, dh_problem problem, dh_random_fn rnd, void *ctx);

#ifdef __cplusplus
}
#endif

#endif
