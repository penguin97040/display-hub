//! Per-address limits on failed sign-ins and on pairing and enrolment attempts, so guessing a
//! token or code from the network is hopeless. Held in memory: a restart forgives everyone, which
//! is fine for a home hub.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub struct Limiter {
    max: u32,
    window: Duration,
    hits: Mutex<HashMap<IpAddr, (Instant, u32)>>,
}

impl Limiter {
    /// At most `max` events per address in each `window`.
    pub fn new(max: u32, window: Duration) -> Self {
        Self {
            max,
            window,
            hits: Mutex::new(HashMap::new()),
        }
    }

    /// Whether `ip` is currently blocked.
    pub fn blocked(&self, ip: IpAddr, now: Instant) -> bool {
        let hits = self.hits.lock().unwrap_or_else(|e| e.into_inner());
        hits.get(&ip)
            .is_some_and(|(start, n)| now.duration_since(*start) < self.window && *n >= self.max)
    }

    /// Records one event for `ip`. Returns whether it is now blocked.
    pub fn hit(&self, ip: IpAddr, now: Instant) -> bool {
        let mut hits = self.hits.lock().unwrap_or_else(|e| e.into_inner());
        if hits.len() > 4096 {
            let window = self.window;
            hits.retain(|_, (start, _)| now.duration_since(*start) < window);
        }
        let entry = hits.entry(ip).or_insert((now, 0));
        if now.duration_since(entry.0) >= self.window {
            *entry = (now, 0);
        }
        entry.1 = entry.1.saturating_add(1);
        entry.1 >= self.max
    }

    /// How long until `ip` may try again.
    pub fn retry_after(&self, ip: IpAddr, now: Instant) -> Duration {
        let hits = self.hits.lock().unwrap_or_else(|e| e.into_inner());
        hits.get(&ip)
            .map(|(start, _)| self.window.saturating_sub(now.duration_since(*start)))
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_after_the_limit_and_forgives_later() {
        let l = Limiter::new(3, Duration::from_secs(60));
        let ip: IpAddr = "192.168.1.9".parse().unwrap();
        let other: IpAddr = "192.168.1.10".parse().unwrap();
        let t = Instant::now();
        assert!(!l.hit(ip, t));
        assert!(!l.hit(ip, t));
        assert!(l.hit(ip, t));
        assert!(l.blocked(ip, t));
        assert!(!l.blocked(other, t));
        assert!(!l.blocked(ip, t + Duration::from_secs(61)));
        assert!(!l.hit(ip, t + Duration::from_secs(61)));
    }
}
