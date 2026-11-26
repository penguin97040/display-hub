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

#include <stdbool.h>
#include <stddef.h>
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

/* ---- A radio that is joined but gets nothing through ----
 *
 * Seen on real boards after a first install: Wi-Fi joins and an address is given, but nothing
 * the display sends reaches the hub, for as long as it is left. Unplugging it fixes it at once,
 * and a restart (esp_restart) is the nearest the firmware can come to that. So when every
 * address has gone unanswered for several full rounds of enrolments or polls in a row while
 * Wi-Fi stayed joined, and at least once an IP address (not a name) sent nothing back at all, not
 * even a refusal, the display restarts. It does so at most DH_RADIO_RESTARTS_MAX times until a
 * hub answers again, or a new setup is stored.
 *
 * Only on a start that has heard nothing at all from any hub address yet. A display that has
 * reached the hub since it started has a radio that works: a hub that goes away later (switched
 * off, say) never restarts it, and it goes on showing the pages it has. And never while a device
 * token the hub has just issued is held only in memory: a restart would lose it, and the
 * display would need setting up again.
 *
 * With the usual back-off (15 s, then 30 s, each +-20%) the third round ends about 75 s after the
 * first began with one IP address (10 s for a connection that never comes), and about 100 s with
 * a name that can't be looked up ahead of it (7 s more): both restarts are over within about
 * four minutes of joining, well inside an enrolment secret's ten minutes.
 */

#define DH_RADIO_RESTART_ROUNDS 3u
#define DH_RADIO_RESTARTS_MAX 2u

typedef struct {
    /* Full rounds of enrolments or polls in a row, every address tried, with nothing back from
     * any of them. */
    uint32_t silent_rounds;
    /* Wi-Fi stayed joined, with no drop, since the first of those rounds began. */
    bool wifi_steady;
    /* In at least one of them, an IP address was tried and nothing came back from it in the time
     * allowed: no connection, and no refusal. */
    bool address_silent;
    /* Restarts made for this since something last came back from a hub address, or the setup
     * was changed. */
    uint32_t restarts;
    /* Something has come back from a hub address since this start: an answer, an error, or a
     * connection made or refused. */
    bool heard_this_start;
    /* The hub has issued a device token that couldn't be written to flash yet. */
    bool token_unsaved;
    /* The running firmware is on its trial boot: a restart would send it back. */
    bool on_trial;
    /* An update is being downloaded or put in place. */
    bool updating;
} dh_radio_facts;

/* Whether to restart the display now, after a round that no address answered. */
bool dh_radio_restart_due(const dh_radio_facts *f);

/* Whether a hub address is an IP address, IPv4 in dotted decimal or IPv6, and not a name to be
 * looked up. */
bool dh_host_is_address(const char *host, size_t len);

#ifdef __cplusplus
}
#endif

#endif
