//! The interface every source of usage figures implements, and the demo source.
//!
//! No provider runs untrusted code or reaches the internet from the hub. Real usage arrives from
//! helpers on the owner's PC (see `docs/providers.md`).

use hub_core::api::PageContentInput;
use hub_core::Source;
use hub_protocol::{DataQuality, Timestamp};

use crate::model::{meter_data, MetricValue, PageData, Service, Window};

/// What a provider can do, so the manager can explain it before the owner chooses it.
#[derive(Clone, Debug, PartialEq)]
pub struct Capabilities {
    pub service: Service,
    pub source: Source,
    /// The quality label its figures carry.
    pub quality: DataQuality,
    /// Whether the hub keeps getting new figures while the owner's PC is off.
    pub works_with_pc_off: bool,
    /// The fastest it may be refreshed.
    pub min_refresh_seconds: u32,
    /// One line, en-NZ, for the manager.
    pub summary: &'static str,
}

/// Why a provider has no figures right now. Never carries credentials.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum ProviderError {
    /// No supported source. Shown to the owner as the reason.
    #[error("{0}")]
    Unavailable(String),
    /// Try again later; keep the last figures meanwhile.
    #[error("temporarily unavailable, retry after {retry_after_seconds} s")]
    Transient { retry_after_seconds: u32 },
}

pub trait Provider: Send + Sync {
    fn capabilities(&self) -> Capabilities;
    /// The current figures. Called no more often than `min_refresh_seconds`.
    fn fetch(&self, now: Timestamp) -> Result<PageData, ProviderError>;
}

/// Fixed sample figures, always labelled demo. `age_seconds` makes them look collected that long
/// ago, to show stale states.
#[derive(Clone, Debug)]
pub struct DemoProvider {
    pub service: Service,
    pub age_seconds: i64,
}

pub const DEMO_NOTE: &str = "Sample figures, not real usage";

impl Provider for DemoProvider {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            service: self.service,
            source: Source::Demo,
            quality: DataQuality::Demo,
            works_with_pc_off: true,
            min_refresh_seconds: 30,
            summary: "Sample figures for trying the hub and displays out. Never real usage.",
        }
    }

    fn fetch(&self, now: Timestamp) -> Result<PageData, ProviderError> {
        let updated = now.plus_seconds(-self.age_seconds.max(0));
        let figures: &[(Window, f64)] = match self.service {
            Service::ClaudeMax => &[(Window::FiveHour, 42.0), (Window::Weekly, 24.0)],
            Service::ChatgptPlus => &[(Window::FiveHour, 63.0), (Window::Weekly, 38.0)],
            Service::OpencodeGoPlus => &[
                (Window::FiveHour, 17.0),
                (Window::Weekly, 31.0),
                (Window::Monthly, 9.0),
            ],
        };
        Ok(PageData {
            quality: DataQuality::Demo,
            updated_at: Some(updated),
            metrics: figures
                .iter()
                .map(|&(window, pct)| MetricValue {
                    window,
                    used_percent: Some(pct),
                    resets_at: Some(next_boundary(updated, window)),
                })
                .collect(),
            note: Some(DEMO_NOTE.into()),
        })
    }
}

/// What a demo usage page holds on the hub: the service's sample figures and note. The hub shows a
/// demo page's content as fresh for as long as it keeps it, so the figures carry no reset times,
/// which would pass and clear them.
pub fn demo_content(service: Service) -> PageContentInput {
    let mut figures = DemoProvider {
        service,
        age_seconds: 0,
    }
    .fetch(Timestamp::from_unix(0).expect("in range"))
    .expect("demo figures never fail");
    for m in &mut figures.metrics {
        m.resets_at = None;
    }
    PageContentInput {
        data: Some(meter_data(&figures.metrics)),
        note: figures.note,
    }
}

/// The next five-hour, weekly (Monday 00:00 UTC) or monthly (1st, 00:00 UTC) boundary after `t`.
pub fn next_boundary(t: Timestamp, window: Window) -> Timestamp {
    let s = t.unix();
    match window {
        Window::FiveHour => Timestamp::from_unix((s / 18_000 + 1) * 18_000).expect("in range"),
        // 1970-01-05 was a Monday.
        Window::Weekly => {
            let monday = 4 * 86_400;
            Timestamp::from_unix(((s - monday) / 604_800 + 1) * 604_800 + monday).expect("in range")
        }
        Window::Monthly => {
            let d = t.get().date();
            let (y, m) = if d.month() == time::Month::December {
                (d.year() + 1, time::Month::January)
            } else {
                (d.year(), d.month().next())
            };
            let first = time::Date::from_calendar_date(y, m, 1).expect("valid date");
            Timestamp::new(first.midnight().assume_utc())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demo_figures_are_labelled_and_deterministic() {
        let now = Timestamp::parse("2026-11-10T20:00:00Z").unwrap();
        let p = DemoProvider {
            service: Service::ClaudeMax,
            age_seconds: 0,
        };
        let a = p.fetch(now).unwrap();
        assert_eq!(a, p.fetch(now).unwrap());
        assert_eq!(a.quality, DataQuality::Demo);
        assert_eq!(a.note.as_deref(), Some(DEMO_NOTE));
        assert_eq!(
            a.metrics[0].resets_at.unwrap().to_string(),
            "2026-11-10T22:00:00Z"
        );
    }

    #[test]
    fn demo_content_has_the_figures_but_no_reset_times() {
        let c = demo_content(Service::OpencodeGoPlus);
        let items = c.data.unwrap()["items"].as_array().unwrap().clone();
        let got: Vec<(&str, f64)> = items
            .iter()
            .map(|i| (i["id"].as_str().unwrap(), i["percent"].as_f64().unwrap()))
            .collect();
        assert_eq!(got, [("five-hour", 17.0), ("weekly", 31.0), ("monthly", 9.0)]);
        assert!(items.iter().all(|i| i.get("resets_at").is_none()));
        assert_eq!(c.note.as_deref(), Some(DEMO_NOTE));
    }

    #[test]
    fn boundaries() {
        let t = Timestamp::parse("2026-11-10T20:00:00Z").unwrap(); // a Tuesday
        assert_eq!(
            next_boundary(t, Window::FiveHour).to_string(),
            "2026-11-10T22:00:00Z"
        );
        assert_eq!(
            next_boundary(t, Window::Weekly).to_string(),
            "2026-11-16T00:00:00Z"
        );
        assert_eq!(
            next_boundary(t, Window::Monthly).to_string(),
            "2026-12-01T00:00:00Z"
        );
        let dec = Timestamp::parse("2026-12-31T23:59:59Z").unwrap();
        assert_eq!(
            next_boundary(dec, Window::Monthly).to_string(),
            "2027-01-01T00:00:00Z"
        );
    }
}
