//! The time-based rules, as pure functions of the update record and the time, so tests and the
//! conformance vectors need no waiting: "Limits on trying" and "The trial boot".

use hub_protocol::limits::{
    FIRMWARE_FAILURES_BEFORE_LONG_WAIT, FIRMWARE_LONG_WAIT_SECONDS, FIRMWARE_RETRY_WAIT_SECONDS,
    FIRMWARE_TRIAL_SECONDS,
};
use hub_protocol::{ErrorCode, Timestamp};

use super::store::Record;

/// What to do about an offer in a snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    /// This offer was acted on already. One attempt per offer.
    Ignore,
    /// Too soon after the last attempt: look again at a later poll.
    Wait {
        seconds: u64,
    },
    Attempt,
}

/// One attempt per offer; at least 10 minutes between attempts; at least 6 hours after three
/// failures in a row. A clock that has gone backwards counts as too soon.
pub fn decide(record: &Record, offer_id: &str, now: Timestamp) -> Decision {
    if record.last_offer_id.as_deref() == Some(offer_id) {
        return Decision::Ignore;
    }
    if let Some(at) = record.last_attempt_at {
        let gap = if record.failures_in_row >= FIRMWARE_FAILURES_BEFORE_LONG_WAIT {
            FIRMWARE_LONG_WAIT_SECONDS
        } else {
            FIRMWARE_RETRY_WAIT_SECONDS
        };
        let since = at.seconds_until(now);
        if since < gap as i64 {
            return Decision::Wait {
                seconds: (gap as i64 - since.max(0)) as u64,
            };
        }
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

/// New firmware on its trial boot, started at `started_at` by the display's own clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Trial {
    pub started_at: Timestamp,
}

impl Trial {
    fn expired(&self, now: Timestamp) -> bool {
        self.started_at.seconds_until(now) >= FIRMWARE_TRIAL_SECONDS as i64
    }

    /// A check-in's answer at `now`. An answer that comes after the five minutes is too late.
    pub fn check_in(&self, now: Timestamp, result: CheckIn) -> TrialStep {
        if self.expired(now) {
            return TrialStep::RollBack;
        }
        match result {
            CheckIn::Answered | CheckIn::SetUpAgain => TrialStep::MarkGood,
            CheckIn::Failed => TrialStep::Waiting,
        }
    }

    /// Time passing with no answer.
    pub fn tick(&self, now: Timestamp) -> TrialStep {
        if self.expired(now) {
            TrialStep::RollBack
        } else {
            TrialStep::Waiting
        }
    }
}
