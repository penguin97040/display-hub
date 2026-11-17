//! The rules on trying, as pure functions of the update record and the display's own running time
//! (never the wall clock), so tests and the conformance vectors need no waiting: "Limits on trying"
//! and "The trial boot".

use hub_protocol::limits::{FIRMWARE_TRIAL_SECONDS, MAX_FIRMWARE_ATTEMPTS_LEFT};
use hub_protocol::ErrorCode;
use serde::{Deserialize, Serialize};

use super::store::Record;

/// A full attempt budget.
pub const FULL_BUDGET: u32 = MAX_FIRMWARE_ATTEMPTS_LEFT;
/// Credit hours that earn one attempt. Credit hours run from 0 to one less than this.
pub const CREDIT_HOURS_PER_ATTEMPT: u32 = 4;
/// One credit hour: a full hour of running, by the display's monotonic timer.
pub const CREDIT_HOUR_SECONDS: u64 = 3600;

/// The attempt budget kept in the update record.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Budget {
    /// Download attempts the display may still make: 0 to 5.
    pub attempts_left: u32,
    /// Full hours of running counted towards the next attempt: 0 to 3.
    pub credit_hours: u32,
}

impl Budget {
    pub const FULL: Self = Self {
        attempts_left: FULL_BUDGET,
        credit_hours: 0,
    };

    /// The budget with any out-of-range value (a damaged record) brought into range. A full budget
    /// counts nothing.
    pub fn normalised(self) -> Self {
        if self.attempts_left >= FULL_BUDGET {
            return Self::FULL;
        }
        Self {
            attempts_left: self.attempts_left,
            credit_hours: self.credit_hours.min(CREDIT_HOURS_PER_ATTEMPT - 1),
        }
    }

    pub fn is_full(self) -> bool {
        self.attempts_left >= FULL_BUDGET
    }

    /// One more full hour of running counted. At four, one more attempt.
    pub fn add_hour(self) -> Self {
        if self.is_full() {
            return Self::FULL;
        }
        let hours = self.credit_hours + 1;
        if hours >= CREDIT_HOURS_PER_ATTEMPT {
            Self {
                attempts_left: self.attempts_left + 1,
                credit_hours: 0,
            }
            .normalised()
        } else {
            Self {
                attempts_left: self.attempts_left,
                credit_hours: hours,
            }
        }
    }

    /// The budget after one attempt, or `None` with none left. Credit hours carry on.
    pub fn spend(self) -> Option<Self> {
        let left = self.attempts_left.checked_sub(1)?;
        Some(Self {
            attempts_left: left,
            credit_hours: if self.is_full() { 0 } else { self.credit_hours },
        })
    }

    /// Seconds of running until the next attempt is earned, with `uncounted` seconds already run
    /// towards the next credit hour. `None` for a full budget.
    pub fn seconds_to_next_attempt(self, uncounted: u64) -> Option<u64> {
        if self.is_full() {
            return None;
        }
        let hours = u64::from(CREDIT_HOURS_PER_ATTEMPT - self.credit_hours.min(CREDIT_HOURS_PER_ATTEMPT));
        Some((hours * CREDIT_HOUR_SECONDS).saturating_sub(uncounted))
    }
}

impl Default for Budget {
    fn default() -> Self {
        Self::FULL
    }
}

/// Counts running time towards the next credit hour. It lives in memory only and starts at zero at
/// every start, so time not yet saved as a credit hour is lost at a restart: a restart can only make
/// a wait longer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Meter {
    /// The running time, in seconds since this start, from which the current hour is counted.
    counted_from: u64,
}

impl Meter {
    /// The meter at a start.
    pub fn at_start() -> Self {
        Self::default()
    }

    /// Time passing, `uptime` seconds since this start. While the budget is below full, each full
    /// hour adds a credit hour; while it is full, nothing is counted. Returns the new budget and how
    /// many credit hours were added (each one a save), or `None` when nothing changed and nothing
    /// needs saving.
    pub fn count(&mut self, budget: Budget, uptime: u64) -> Option<(Budget, u32)> {
        let mut b = budget;
        let mut hours = 0;
        while !b.is_full() && uptime.saturating_sub(self.counted_from) >= CREDIT_HOUR_SECONDS {
            self.counted_from += CREDIT_HOUR_SECONDS;
            b = b.add_hour();
            hours += 1;
        }
        if b.is_full() {
            self.counted_from = uptime;
        }
        (hours > 0).then_some((b, hours))
    }

    /// Seconds run towards the next credit hour, not yet saved.
    pub fn uncounted(&self, uptime: u64) -> u64 {
        uptime.saturating_sub(self.counted_from)
    }
}

/// What to do about an offer in a snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    /// This is the offer last acted on.
    Ignore,
    /// No attempts left: leave the offer open, report nothing, and look again later. `seconds` is
    /// how much more running earns the next attempt.
    Wait { seconds: u64 },
    /// Spend an attempt and request the download.
    Attempt,
}

/// Whether to act on `offer_id`, from the update record and the seconds of running not yet counted
/// as a credit hour. Nothing here reads a date or the hub's time, and an attempt's result never
/// changes the budget.
pub fn decide(record: &Record, offer_id: &str, uncounted: u64) -> Decision {
    if record.last_offer_id.as_deref() == Some(offer_id) {
        return Decision::Ignore;
    }
    let budget = record.budget();
    if budget.attempts_left == 0 {
        return Decision::Wait {
            seconds: budget.seconds_to_next_attempt(uncounted).unwrap_or(0),
        };
    }
    Decision::Attempt
}

/// What the hub answered a trial boot's authenticated check-in with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CheckIn {
    /// Success.
    Answered,
    /// An error in the "set up again" group a check-in can get: `token_revoked` or `unknown_device`.
    /// Rolling back wouldn't fix it, so it counts as reaching the hub.
    SetUpAgain,
    /// No answer, or any other error.
    Failed,
}

impl CheckIn {
    pub fn from_error(code: Option<ErrorCode>) -> Self {
        match code {
            Some(ErrorCode::TokenRevoked | ErrorCode::UnknownDevice) => Self::SetUpAgain,
            _ => Self::Failed,
        }
    }
}

/// What the trial boot does next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrialStep {
    Waiting,
    /// Mark the new firmware good, then apply its floor and keys.
    MarkGood,
    /// Give up and go back to the previous firmware.
    RollBack,
}

/// New firmware on its trial boot, which began `started_at` seconds into this start by the
/// display's monotonic timer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Trial {
    pub started_at: u64,
}

impl Trial {
    fn expired(&self, uptime: u64) -> bool {
        uptime.saturating_sub(self.started_at) >= FIRMWARE_TRIAL_SECONDS
    }

    /// A check-in's answer, `uptime` seconds into this start. An answer that comes after the five
    /// minutes is too late.
    pub fn check_in(&self, uptime: u64, result: CheckIn) -> TrialStep {
        if self.expired(uptime) {
            return TrialStep::RollBack;
        }
        match result {
            CheckIn::Answered | CheckIn::SetUpAgain => TrialStep::MarkGood,
            CheckIn::Failed => TrialStep::Waiting,
        }
    }

    /// Time passing with no answer.
    pub fn tick(&self, uptime: u64) -> TrialStep {
        if self.expired(uptime) {
            TrialStep::RollBack
        } else {
            TrialStep::Waiting
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn four_counted_hours_earn_one_attempt_and_a_full_budget_counts_nothing() {
        let mut m = Meter::at_start();
        let empty = Budget {
            attempts_left: 0,
            credit_hours: 0,
        };
        assert_eq!(m.count(empty, 3599), None);
        assert_eq!(
            m.count(empty, 3600),
            Some((
                Budget {
                    attempts_left: 0,
                    credit_hours: 1
                },
                1
            ))
        );
        assert_eq!(
            m.count(
                Budget {
                    attempts_left: 0,
                    credit_hours: 1
                },
                4 * 3600
            ),
            Some((
                Budget {
                    attempts_left: 1,
                    credit_hours: 0
                },
                3
            ))
        );
        // Full: nothing counted, and the count starts afresh when an attempt is spent.
        let mut m = Meter::at_start();
        assert_eq!(m.count(Budget::FULL, 100 * 3600), None);
        let spent = Budget::FULL.spend().unwrap();
        assert_eq!(m.count(spent, 100 * 3600 + 3599), None);
        assert_eq!(m.count(spent, 101 * 3600).unwrap().0.credit_hours, 1);
        // Refilling past full stops.
        let mut m = Meter::at_start();
        let (b, hours) = m
            .count(
                Budget {
                    attempts_left: 4,
                    credit_hours: 3,
                },
                10 * 3600,
            )
            .unwrap();
        assert_eq!((b, hours), (Budget::FULL, 1));
    }

    #[test]
    fn a_damaged_budget_is_brought_into_range() {
        let b = Budget {
            attempts_left: 9,
            credit_hours: 9,
        };
        assert_eq!(b.normalised(), Budget::FULL);
        let b = Budget {
            attempts_left: 2,
            credit_hours: 9,
        };
        assert_eq!(b.normalised().credit_hours, 3);
        assert_eq!(
            Budget {
                attempts_left: 0,
                credit_hours: 3
            }
            .seconds_to_next_attempt(600),
            Some(3000)
        );
    }
}
