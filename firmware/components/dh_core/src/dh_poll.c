#include "dh_poll.h"

uint32_t dh_backoff_seconds(uint32_t failures)
{
    uint32_t doublings = failures > 0 ? failures - 1 : 0;
    if (doublings > 10) {
        doublings = 10;
    }
    uint32_t s = DH_BACKOFF_FIRST_SECONDS << doublings;
    return s < DH_BACKOFF_MAX_SECONDS ? s : DH_BACKOFF_MAX_SECONDS;
}

uint32_t dh_jitter_ms(uint32_t base_ms, uint32_t percent, dh_random_fn rnd, void *ctx)
{
    uint64_t spread = (uint64_t)base_ms * percent / 100u;
    /* A uniform pick from 0 to 2 * spread, without the bias of a remainder. */
    uint64_t pick = ((uint64_t)rnd(ctx) * (2u * spread + 1u)) >> 32;
    return (uint32_t)((uint64_t)base_ms - spread + pick);
}

const char *dh_problem_headline(dh_problem p)
{
    switch (p) {
    case DH_PROBLEM_NONE: return "";
    case DH_PROBLEM_UNREACHABLE: return "Can't reach the hub";
    case DH_PROBLEM_WRONG_HUB: return "Something else is answering as the hub";
    case DH_PROBLEM_UNREADABLE: return "The hub sent data this display can't read";
    case DH_PROBLEM_BUSY: return "The hub is busy";
    case DH_PROBLEM_NEEDS_SETUP: return "This display needs setting up again";
    case DH_PROBLEM_INCOMPATIBLE: return "Hub and display versions don't match";
    }
    return "";
}

static uint32_t failed(dh_poll_state *st, dh_problem problem, bool hourly, dh_random_fn rnd,
                       void *ctx)
{
    if (st->failures < UINT32_MAX) {
        st->failures++;
    }
    st->problem = problem;
    uint32_t base = hourly ? DH_HOURLY_SECONDS : dh_backoff_seconds(st->failures);
    return dh_jitter_ms(base * 1000u, 20, rnd, ctx);
}

uint32_t dh_poll_answered(dh_poll_state *st, const dh_response *r, uint32_t refresh_after_seconds,
                          dh_random_fn rnd, void *ctx)
{
    switch (r->outcome) {
    case DH_OUTCOME_SHOW:
        st->failures = 0;
        st->problem = DH_PROBLEM_NONE;
        return dh_jitter_ms(refresh_after_seconds * 1000u, 10, rnd, ctx);
    case DH_OUTCOME_REPROVISION:
        return failed(st, DH_PROBLEM_NEEDS_SETUP, true, rnd, ctx);
    case DH_OUTCOME_INCOMPATIBLE:
        return failed(st, DH_PROBLEM_INCOMPATIBLE, true, rnd, ctx);
    case DH_OUTCOME_RETRY:
        return failed(st, DH_PROBLEM_BUSY, false, rnd, ctx);
    case DH_OUTCOME_UNREADABLE:
    case DH_OUTCOME_FAULT:
        return failed(st, DH_PROBLEM_UNREADABLE, false, rnd, ctx);
    }
    return failed(st, DH_PROBLEM_UNREADABLE, false, rnd, ctx);
}

uint32_t dh_poll_failed(dh_poll_state *st, dh_problem problem, dh_random_fn rnd, void *ctx)
{
    return failed(st, problem, false, rnd, ctx);
}
