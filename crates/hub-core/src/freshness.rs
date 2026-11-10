//! How old a page's figures are, and whether they're stale.
//!
//! A display may not know the time, so age is measured on the hub's clock: the gap between
//! `updated_at` and `generated_at`, plus however long the display has held the snapshot.

use hub_protocol::{Page, Timestamp};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Freshness {
    /// No figures have ever been collected.
    Never,
    Fresh {
        age_seconds: i64,
    },
    Stale {
        age_seconds: i64,
    },
}

impl Freshness {
    /// `held_seconds`: time since the snapshot was received, by a monotonic clock.
    pub fn of(page: &Page, generated_at: Timestamp, held_seconds: i64) -> Self {
        let Some(updated) = page.updated_at else {
            return Freshness::Never;
        };
        let age = updated.seconds_until(generated_at).max(0) + held_seconds.max(0);
        if age > page.stale_after_seconds as i64 {
            Freshness::Stale { age_seconds: age }
        } else {
            Freshness::Fresh { age_seconds: age }
        }
    }

    pub fn is_stale(self) -> bool {
        matches!(self, Freshness::Stale { .. })
    }
}

/// "45 s", "12 min", "3 h 5 min", "2 d 4 h": short, for a small screen.
pub fn short_duration(seconds: i64) -> String {
    let s = seconds.max(0);
    match s {
        0..=59 => format!("{s} s"),
        60..=3599 => format!("{} min", s / 60),
        3600..=86_399 => {
            let (h, m) = (s / 3600, (s % 3600) / 60);
            if m == 0 {
                format!("{h} h")
            } else {
                format!("{h} h {m} min")
            }
        }
        _ => {
            let (d, h) = (s / 86_400, (s % 86_400) / 3600);
            if h == 0 {
                format!("{d} d")
            } else {
                format!("{d} d {h} h")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hub_protocol::DataQuality;

    fn page(updated: Option<&str>, stale_after: u32) -> Page {
        Page {
            id: "p".into(),
            title: "P".into(),
            data_quality: DataQuality::Manual,
            updated_at: updated.map(|u| Timestamp::parse(u).unwrap()),
            stale_after_seconds: stale_after,
            note: None,
            metrics: vec![],
        }
    }

    #[test]
    fn age_uses_the_hub_clock_plus_time_held() {
        let gen = Timestamp::parse("2026-11-10T20:00:00Z").unwrap();
        let p = page(Some("2026-11-10T19:50:00Z"), 1800);
        assert_eq!(Freshness::of(&p, gen, 0), Freshness::Fresh { age_seconds: 600 });
        assert_eq!(
            Freshness::of(&p, gen, 1200),
            Freshness::Fresh { age_seconds: 1800 }
        );
        assert_eq!(
            Freshness::of(&p, gen, 1201),
            Freshness::Stale { age_seconds: 1801 }
        );
    }

    #[test]
    fn never_collected_is_not_stale_or_fresh() {
        let gen = Timestamp::parse("2026-11-10T20:00:00Z").unwrap();
        assert_eq!(Freshness::of(&page(None, 60), gen, 99_999), Freshness::Never);
    }

    #[test]
    fn durations_are_short() {
        assert_eq!(short_duration(45), "45 s");
        assert_eq!(short_duration(720), "12 min");
        assert_eq!(short_duration(3600), "1 h");
        assert_eq!(short_duration(11_100), "3 h 5 min");
        assert_eq!(short_duration(187_200), "2 d 4 h");
        assert_eq!(short_duration(-5), "0 s");
    }
}
