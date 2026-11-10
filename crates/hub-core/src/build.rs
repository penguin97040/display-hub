//! Turns subscriptions and their stored figures into the snapshot a display receives.

use hub_protocol::limits::MAX_PAGES;
use hub_protocol::snapshot::{DASHBOARD_LLM_USAGE, MESSAGE_TYPE};
use hub_protocol::{DataQuality, Metric, Page, Snapshot, Timestamp, PROTOCOL_VERSION};

use crate::model::{clean_note, clean_title, PageData, Source, Subscription};
use crate::provider::generated;

pub const RESET_NOTE: &str = "Window reset since the last report";
pub const NO_SOURCE_NOTE: &str = "No supported source connected yet";
pub const NEVER_COLLECTED_NOTE: &str = "Waiting for the first report";
pub const DEFAULT_REFRESH_SECONDS: u32 = 300;

/// Builds a snapshot. `pages` are in display order, each with its latest stored figures (if any).
/// Demo pages are generated fresh. Pages past the limit are left off.
pub fn build_snapshot(
    now: Timestamp,
    refresh_after_seconds: u32,
    pages: &[(Subscription, Option<PageData>)],
) -> Snapshot {
    Snapshot {
        protocol_version: PROTOCOL_VERSION,
        message_type: MESSAGE_TYPE.into(),
        dashboard_id: DASHBOARD_LLM_USAGE.into(),
        generated_at: now,
        refresh_after_seconds: refresh_after_seconds.clamp(
            *hub_protocol::limits::REFRESH_RANGE.start(),
            *hub_protocol::limits::REFRESH_RANGE.end(),
        ),
        pages: pages
            .iter()
            .take(MAX_PAGES)
            .map(|(sub, data)| page(now, sub, data.as_ref()))
            .collect(),
    }
}

fn page(now: Timestamp, sub: &Subscription, stored: Option<&PageData>) -> Page {
    let generated_data = generated(sub.service, sub.source).and_then(|p| p.fetch(now).ok());
    let data = match (sub.source, generated_data.as_ref(), stored) {
        (Source::None, _, _) => PageData::unavailable(NO_SOURCE_NOTE),
        (_, Some(g), _) => g.clone(),
        (_, None, Some(s)) => s.clone(),
        (Source::Collector, None, None) => PageData::unavailable(NEVER_COLLECTED_NOTE),
        (_, None, None) => PageData::unavailable(NO_SOURCE_NOTE),
    };

    let title = clean_title(&sub.title).unwrap_or_else(|| sub.service.default_title().to_string());
    let mut note = data.note.as_deref().and_then(clean_note);
    let shows = data.quality.shows_figures() && data.quality != DataQuality::Unrecognised;
    // A figure from the future is a clock fault somewhere: don't let it look fresh.
    let updated_at = data.updated_at.map(|u| u.min(now));

    let mut cleared = false;
    let metrics = if shows && updated_at.is_some() {
        data.metrics
            .iter()
            .map(|m| {
                // Once a window has reset, the old share no longer describes it, and the new one
                // is unknown until the next report.
                let reset = m.resets_at.is_some_and(|r| r <= now);
                cleared |= reset && m.used_percent.is_some();
                Metric {
                    id: m.window.id().into(),
                    label: m.window.label().into(),
                    used_percent: if reset {
                        None
                    } else {
                        m.used_percent.and_then(crate::model::clean_percent)
                    },
                    resets_at: if reset { None } else { m.resets_at },
                }
            })
            .take(hub_protocol::limits::MAX_METRICS)
            .collect()
    } else {
        Vec::new()
    };
    if cleared && note.is_none() {
        note = Some(RESET_NOTE.into());
    }
    let (quality, updated_at) = if shows && updated_at.is_some() {
        (data.quality, updated_at)
    } else {
        if note.is_none() {
            note = Some(NO_SOURCE_NOTE.into());
        }
        (DataQuality::Unavailable, None)
    };

    Page {
        id: sub.id.clone(),
        title,
        data_quality: quality,
        updated_at,
        stale_after_seconds: sub.source.stale_after_seconds(),
        note,
        metrics,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{MetricValue, Service, Window};

    fn ts(s: &str) -> Timestamp {
        Timestamp::parse(s).unwrap()
    }

    fn sub(id: &str, service: Service, source: Source) -> Subscription {
        Subscription {
            id: id.into(),
            service,
            title: service.default_title().into(),
            source,
        }
    }

    #[test]
    fn keeps_order_and_produces_a_valid_snapshot() {
        let now = ts("2026-11-10T20:00:00Z");
        let pages = vec![
            (
                sub("opencode-go-plus", Service::OpencodeGoPlus, Source::Demo),
                None,
            ),
            (sub("chatgpt-plus", Service::ChatgptPlus, Source::None), None),
            (sub("claude-max", Service::ClaudeMax, Source::Demo), None),
        ];
        let s = build_snapshot(now, 300, &pages);
        let ids: Vec<_> = s.pages.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, ["opencode-go-plus", "chatgpt-plus", "claude-max"]);
        assert_eq!(s.pages[0].data_quality, DataQuality::Demo);
        assert_eq!(s.pages[1].data_quality, DataQuality::Unavailable);
        assert!(s.pages[1].metrics.is_empty());
        s.validate().unwrap();
    }

    #[test]
    fn a_reset_window_loses_its_old_figure() {
        let now = ts("2026-11-10T20:00:00Z");
        let stored = PageData {
            quality: DataQuality::ProviderReported,
            updated_at: Some(ts("2026-11-10T14:00:00Z")),
            metrics: vec![
                MetricValue {
                    window: Window::FiveHour,
                    used_percent: Some(80.0),
                    resets_at: Some(ts("2026-11-10T19:00:00Z")),
                },
                MetricValue {
                    window: Window::Weekly,
                    used_percent: Some(30.0),
                    resets_at: Some(ts("2026-11-15T00:00:00Z")),
                },
            ],
            note: None,
        };
        let s = build_snapshot(
            now,
            300,
            &[(
                sub("claude-max", Service::ClaudeMax, Source::Collector),
                Some(stored),
            )],
        );
        let p = &s.pages[0];
        assert_eq!(p.metrics[0].used_percent, None);
        assert_eq!(p.metrics[0].resets_at, None);
        assert_eq!(p.metrics[1].used_percent, Some(30.0));
        assert_eq!(p.note.as_deref(), Some(RESET_NOTE));
        s.validate().unwrap();
    }

    #[test]
    fn collector_pages_wait_for_their_first_report() {
        let s = build_snapshot(
            ts("2026-11-10T20:00:00Z"),
            300,
            &[(sub("claude-max", Service::ClaudeMax, Source::Collector), None)],
        );
        assert_eq!(s.pages[0].data_quality, DataQuality::Unavailable);
        assert_eq!(s.pages[0].note.as_deref(), Some(NEVER_COLLECTED_NOTE));
    }

    #[test]
    fn hostile_titles_and_figures_are_cleaned() {
        let now = ts("2026-11-10T20:00:00Z");
        let mut s1 = sub("x", Service::ClaudeMax, Source::Manual);
        s1.title = "\u{202e}  Mine\u{7}  ".into();
        let stored = PageData {
            quality: DataQuality::Manual,
            updated_at: Some(ts("2026-11-11T20:00:00Z")), // in the future
            metrics: vec![MetricValue {
                window: Window::FiveHour,
                used_percent: Some(f64::NAN),
                resets_at: None,
            }],
            note: Some("\n".into()),
        };
        let s = build_snapshot(now, 5, &[(s1, Some(stored))]);
        let p = &s.pages[0];
        assert_eq!(p.title, "Mine");
        assert_eq!(p.updated_at, Some(now));
        assert_eq!(p.metrics[0].used_percent, None);
        assert_eq!(s.refresh_after_seconds, 30);
        s.validate().unwrap();
    }

    #[test]
    fn unavailable_figures_are_never_shown() {
        let stored = PageData {
            quality: DataQuality::Unavailable,
            updated_at: Some(ts("2026-11-10T19:00:00Z")),
            metrics: vec![MetricValue {
                window: Window::FiveHour,
                used_percent: Some(50.0),
                resets_at: None,
            }],
            note: None,
        };
        let s = build_snapshot(
            ts("2026-11-10T20:00:00Z"),
            300,
            &[(sub("a", Service::ClaudeMax, Source::Manual), Some(stored))],
        );
        assert!(s.pages[0].metrics.is_empty());
        assert_eq!(s.pages[0].updated_at, None);
        s.validate().unwrap();
    }
}
