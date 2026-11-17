//! How long the display has been running: a monotonic timer, separate from the wall clock, as
//! firmware's `esp_timer_get_time` is. The attempt budget, the trial boot's five minutes and the
//! download time limits are measured with it, never with a date or the hub's clock, so nothing a hub
//! sends and no change to the date can shorten a wait.
//!
//! [`Firmware`](super::Firmware) counts from the reading at each start, so every start (a real one,
//! or the restart after an install or a rollback) begins again at zero. A new `display-simulator`
//! process is a new start.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// A timer that only goes forward.
pub trait Uptime: Send + Sync + 'static {
    /// Time since some fixed moment, such as the process starting. Never goes backwards.
    fn elapsed(&self) -> Duration;
}

/// The operating system's monotonic clock, from when this was made.
#[derive(Clone, Copy, Debug)]
pub struct SystemUptime(Instant);

impl SystemUptime {
    pub fn new() -> Self {
        Self(Instant::now())
    }
}

impl Default for SystemUptime {
    fn default() -> Self {
        Self::new()
    }
}

impl Uptime for SystemUptime {
    fn elapsed(&self) -> Duration {
        self.0.elapsed()
    }
}

/// A timer that only moves when told to, for tests. Clones share one reading.
#[derive(Clone, Debug, Default)]
pub struct FakeUptime(Arc<AtomicU64>);

impl FakeUptime {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn advance(&self, seconds: u64) {
        self.0.fetch_add(seconds, Ordering::SeqCst);
    }
}

impl Uptime for FakeUptime {
    fn elapsed(&self) -> Duration {
        Duration::from_secs(self.0.load(Ordering::SeqCst))
    }
}
