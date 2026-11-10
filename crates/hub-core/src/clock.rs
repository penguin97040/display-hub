//! Time comes from a [`Clock`], so tests can fix it.

use std::sync::{Arc, Mutex};

use hub_protocol::Timestamp;
use time::OffsetDateTime;

pub trait Clock: Send + Sync + 'static {
    fn now(&self) -> Timestamp;
}

/// The system's wall clock.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Timestamp {
        Timestamp::new(OffsetDateTime::now_utc())
    }
}

/// A clock that only moves when told to.
#[derive(Clone, Debug)]
pub struct FakeClock(Arc<Mutex<Timestamp>>);

impl FakeClock {
    pub fn at(t: Timestamp) -> Self {
        Self(Arc::new(Mutex::new(t)))
    }

    pub fn parse(s: &str) -> Self {
        Self::at(Timestamp::parse(s).expect("a valid test timestamp"))
    }

    pub fn advance(&self, seconds: i64) {
        let mut t = self.0.lock().unwrap();
        *t = t.plus_seconds(seconds);
    }

    pub fn set(&self, to: Timestamp) {
        *self.0.lock().unwrap() = to;
    }
}

impl Clock for FakeClock {
    fn now(&self) -> Timestamp {
        *self.0.lock().unwrap()
    }
}
