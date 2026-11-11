//! What a display does with each poll result: keep the last good snapshot, explain problems in
//! plain words, and decide when to poll next.

use std::path::PathBuf;
use std::time::Duration;

use hub_client::ClientError;
use hub_protocol::{ErrorAction, Snapshot, Timestamp};

use crate::cache::{self, Held};

/// Why the display isn't showing fresh data, worded for the screen.
#[derive(Clone, Debug, PartialEq)]
pub enum Problem {
    /// The hub couldn't be reached at all.
    Unreachable,
    /// Something answered at the hub's address, but with a key this display doesn't trust.
    WrongHub,
    /// The hub answered, but with something the display can't use. Last good data stays.
    BadResponse(String),
    /// The hub is busy or briefly unavailable.
    Busy(String),
    /// The display's credentials were refused or revoked: it must be set up again.
    NeedsSetup(String),
    /// Hub and display software don't speak the same protocol version.
    Incompatible(String),
    /// The display's own settings are wrong.
    Configuration(String),
}

impl Problem {
    pub fn headline(&self) -> &'static str {
        match self {
            Problem::Unreachable => "Can't reach the hub",
            Problem::WrongHub => "Something else is answering as the hub",
            Problem::BadResponse(_) => "The hub sent data this display can't read",
            Problem::Busy(_) => "The hub is busy",
            Problem::NeedsSetup(_) => "This display needs setting up again",
            Problem::Incompatible(_) => "Hub and display versions don't match",
            Problem::Configuration(_) => "This display's settings are wrong",
        }
    }

    pub fn advice(&self) -> String {
        match self {
            Problem::Unreachable => {
                "Check the Raspberry Pi is switched on and on the same Wi-Fi. Retrying automatically.".into()
            }
            Problem::WrongHub => "Its security key doesn't match, so nothing was sent. If you reinstalled the hub, \
                                  set this display up again; otherwise check for an unknown device on your network."
                .into(),
            Problem::BadResponse(d) => format!("{d}. Retrying automatically."),
            Problem::Busy(d) => format!("{d} Retrying automatically."),
            Problem::NeedsSetup(d) => format!("{d} Use the desktop manager to set it up again."),
            Problem::Incompatible(d) => d.clone(),
            Problem::Configuration(d) => d.clone(),
        }
    }

    pub fn from_error(e: &ClientError) -> Problem {
        match e {
            ClientError::AddressRefused(d) => Problem::Configuration(d.clone()),
            ClientError::Unreachable(_) | ClientError::Timeout => Problem::Unreachable,
            ClientError::IdentityMismatch => Problem::WrongHub,
            ClientError::TooLarge(_) => Problem::BadResponse("The hub's answer was too large".into()),
            ClientError::Snapshot(r) => match r {
                hub_protocol::ReceiveError::IncompatibleVersion(v) => Problem::Incompatible(format!(
                    "The hub sent protocol version {v}; this display understands version {}. \
                     Update the display's software.",
                    hub_protocol::PROTOCOL_VERSION
                )),
                other => Problem::BadResponse(capitalise(&other.to_string())),
            },
            ClientError::Malformed(d) => Problem::BadResponse(capitalise(d)),
            ClientError::Status { status, body } => {
                let message = body.as_ref().map(|b| b.error.message.clone()).unwrap_or_default();
                // Exactly the protocol's grouping (`ErrorCode::action`), so the vectors and this agree.
                match body.as_ref().map(|b| (b.error.code, b.error.code.action())) {
                    Some((_, ErrorAction::Reprovision)) => Problem::NeedsSetup(message),
                    Some((_, ErrorAction::Incompatible)) => Problem::Incompatible(message),
                    Some((_, ErrorAction::Retry)) => Problem::Busy(message),
                    Some((code, ErrorAction::Fault)) => {
                        Problem::BadResponse(format!("The hub refused the request ({})", code.as_str()))
                    }
                    // No structured error: a proxy or a crash. Server errors pass; others are faults.
                    None if *status >= 500 => {
                        Problem::Busy(format!("The hub had a problem (error {status})."))
                    }
                    None => Problem::BadResponse(format!("The hub answered with error {status}")),
                }
            }
        }
    }

    /// How long to wait before the next try, before jitter and backoff.
    fn base_retry(&self) -> Duration {
        match self {
            Problem::NeedsSetup(_) | Problem::Incompatible(_) | Problem::Configuration(_) => {
                Duration::from_secs(3600)
            }
            _ => Duration::from_secs(15),
        }
    }
}

fn capitalise(s: &str) -> String {
    let mut c = s.chars();
    c.next()
        .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
        .unwrap_or_default()
}

/// The display's connection state.
#[derive(Clone, Debug, PartialEq)]
pub enum Link {
    NeverConnected,
    Online {
        last_sync: Timestamp,
    },
    Trouble {
        problem: Problem,
        since: Timestamp,
        failures: u32,
    },
}

/// Random spread added to poll intervals so displays don't all poll at once.
pub trait Jitter: Send {
    /// A factor in `1 - spread ..= 1 + spread`.
    fn factor(&mut self, spread: f64) -> f64;
}

/// No spread, for tests.
pub struct NoJitter;

impl Jitter for NoJitter {
    fn factor(&mut self, _spread: f64) -> f64 {
        1.0
    }
}

/// A small xorshift generator seeded from the clock. Good enough to spread polls; not for secrets.
pub struct ClockJitter(u64);

impl Default for ClockJitter {
    fn default() -> Self {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9e37_79b9_7f4a_7c15);
        Self(seed | 1)
    }
}

impl Jitter for ClockJitter {
    fn factor(&mut self, spread: f64) -> f64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        let unit = (self.0 >> 11) as f64 / (1u64 << 53) as f64; // 0..1
        1.0 + spread * (2.0 * unit - 1.0)
    }
}

/// Retry delays grow 15 s, 30 s, 60 s ... up to this.
pub const MAX_BACKOFF: Duration = Duration::from_secs(900);

pub struct Device {
    pub held: Option<Held>,
    pub link: Link,
    cache: Option<PathBuf>,
    jitter: Box<dyn Jitter>,
}

impl Device {
    /// A display, restoring its last good snapshot from `cache` if there is one.
    pub fn new(cache: Option<PathBuf>, jitter: Box<dyn Jitter>) -> Self {
        let held = cache.as_deref().and_then(cache::load);
        Self {
            held,
            link: Link::NeverConnected,
            cache,
            jitter,
        }
    }

    /// Takes one poll's result at `now` and says how long to wait before the next.
    pub fn accept(&mut self, now: Timestamp, result: Result<Snapshot, ClientError>) -> Duration {
        match result {
            Ok(snapshot) => {
                let wait = Duration::from_secs(snapshot.refresh_after_seconds as u64);
                let held = Held {
                    snapshot,
                    received_at: now,
                };
                if let Some(path) = &self.cache {
                    if let Err(e) = cache::store(path, &held) {
                        eprintln!("display-simulator: couldn't save the cache: {e}");
                    }
                }
                self.held = Some(held);
                self.link = Link::Online { last_sync: now };
                wait.mul_f64(self.jitter.factor(0.1))
            }
            Err(e) => {
                let problem = Problem::from_error(&e);
                let (since, failures) = match &self.link {
                    Link::Trouble { since, failures, .. } => (*since, failures + 1),
                    _ => (now, 1),
                };
                let base = problem.base_retry();
                let backoff = if base >= MAX_BACKOFF {
                    base
                } else {
                    base.saturating_mul(1 << (failures - 1).min(10)).min(MAX_BACKOFF)
                };
                self.link = Link::Trouble {
                    problem,
                    since,
                    failures,
                };
                backoff.mul_f64(self.jitter.factor(0.2))
            }
        }
    }
}
