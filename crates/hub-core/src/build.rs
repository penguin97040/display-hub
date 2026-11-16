//! Turns subscriptions and their stored figures into the snapshot a display receives.

use hub_protocol::limits::{MAX_METER_ITEMS, MAX_PAGES};
use hub_protocol::shapes::KIND_METER;
use hub_protocol::snapshot::MESSAGE_TYPE;
use hub_protocol::{CantShow, DataQuality, Meter, MeterItem, Page, Snapshot, Timestamp, PROTOCOL_VERSION};

use crate::model::{clean_note, clean_title, PageData, Source, Subscription};
use crate::provider::generated;

pub const RESET_NOTE: &str = "Window reset since the last report";
pub const NO_SOURCE_NOTE: &str = "No supported source connected yet";
pub const NEVER_COLLECTED_NOTE: &str = "Waiting for the first report";
/// A page with a source but no figures in it: a meter needs at least one item.
pub const NO_FIGURES_NOTE: &str = "No figures yet";
/// Stand-ins for a page that breaks the page-level rules, so one bad page never blanks the rest.
pub const TOO_BIG_NOTE: &str = "This page is too big to show";
pub const INVALID_DATA_NOTE: &str = "This page's data isn't valid";
/// The stand-in's title when the original can't be cleaned into one.
pub const UNTITLED: &str = "Untitled page";
pub const DEFAULT_REFRESH_SECONDS: u32 = 300;

/// Builds a snapshot. `pages` are in display order, each with its latest stored figures (if any).
/// Demo pages are generated fresh. Pages past the limit are left off.
pub fn build_snapshot(
    now: Timestamp,
    refresh_after_seconds: u32,
    pages: &[(Subscription, Option<PageData>)],
) -> Snapshot {
    let mut snap = Snapshot {
        protocol_version: PROTOCOL_VERSION,
        message_type: MESSAGE_TYPE.into(),
        generated_at: now,
        refresh_after_seconds: refresh_after_seconds.clamp(
            *hub_protocol::limits::REFRESH_RANGE.start(),
            *hub_protocol::limits::REFRESH_RANGE.end(),
        ),
        pages: Vec::new(),
    };
    let guarded: Vec<Page> = pages
        .iter()
        .take(MAX_PAGES)
        .map(|(sub, data)| guard(page(now, sub, data.as_ref())))
        .collect();
    // A page that is still unsendable has a bad id or other common field, which only a damaged
    // database can give. Leaving it off keeps every other page on the displays.
    snap.pages = guarded
        .into_iter()
        .filter(|p| {
            Snapshot {
                pages: vec![p.clone()],
                ..snap.clone()
            }
            .violations()
            .is_empty()
        })
        .collect();
    snap
}

/// A page a hub may not send (over its size cap, or with data that breaks its shape) becomes an
/// unavailable page with the same id and title, so the rest of the snapshot still goes out.
fn guard(page: Page) -> Page {
    let note = match page.check() {
        Ok(()) => return page,
        Err(CantShow::TooLarge) => TOO_BIG_NOTE,
        Err(_) => INVALID_DATA_NOTE,
    };
    // The title may be what broke the page, so the stand-in gets a cleaned one. The kind doesn't
    // matter on an unavailable page: a display shows only the title and the note.
    let title = clean_title(&page.title).unwrap_or_else(|| UNTITLED.to_string());
    Page::unavailable(
        page.id,
        title,
        KIND_METER,
        page.stale_after_seconds,
        Some(note.into()),
    )
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
    let items: Vec<MeterItem> = if shows && updated_at.is_some() {
        data.metrics
            .iter()
            .map(|m| {
                // Once a window has reset, the old share no longer describes it, and the new one
                // is unknown until the next report.
                let reset = m.resets_at.is_some_and(|r| r <= now);
                cleared |= reset && m.used_percent.is_some();
                MeterItem::new(
                    m.window.id(),
                    m.window.label(),
                    if reset {
                        None
                    } else {
                        m.used_percent.and_then(crate::model::clean_percent)
                    },
                    if reset { None } else { m.resets_at },
                )
            })
            .take(MAX_METER_ITEMS)
            .collect()
    } else {
        Vec::new()
    };
    if cleared && note.is_none() {
        note = Some(RESET_NOTE.into());
    }
    let stale_after_seconds = sub.source.stale_after_seconds();
    if let (true, Some(updated_at), false) = (shows, updated_at, items.is_empty()) {
        return Page::with_content(
            sub.id.clone(),
            title,
            data.quality,
            Some(updated_at),
            stale_after_seconds,
            note,
            Meter { items },
        );
    }
    if note.is_none() {
        note = Some(if shows { NO_FIGURES_NOTE } else { NO_SOURCE_NOTE }.into());
    }
    Page::unavailable(sub.id.clone(), title, KIND_METER, stale_after_seconds, note)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{MetricValue, Service, Window};
    use hub_protocol::PageContent;

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
        assert!(s.pages[1].data.is_none());
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
        let Some(PageContent::Meter(m)) = p.content().unwrap() else {
            panic!("not a meter")
        };
        assert_eq!(m.items[0].percent, None);
        assert_eq!(m.items[0].resets_at, None);
        assert_eq!(m.items[1].percent, Some(30.0));
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
        let Some(PageContent::Meter(m)) = p.content().unwrap() else {
            panic!("not a meter")
        };
        assert_eq!(m.items[0].percent, None);
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
        assert!(s.pages[0].data.is_none());
        assert_eq!(s.pages[0].updated_at, None);
        s.validate().unwrap();
    }

    #[test]
    fn a_page_with_a_source_but_no_figures_is_unavailable() {
        let stored = PageData {
            quality: DataQuality::Manual,
            updated_at: Some(ts("2026-11-10T19:00:00Z")),
            metrics: Vec::new(),
            note: None,
        };
        let s = build_snapshot(
            ts("2026-11-10T20:00:00Z"),
            300,
            &[(sub("a", Service::ClaudeMax, Source::Manual), Some(stored))],
        );
        assert_eq!(s.pages[0].data_quality, DataQuality::Unavailable);
        assert_eq!(s.pages[0].note.as_deref(), Some(NO_FIGURES_NOTE));
        s.validate().unwrap();
    }

    #[test]
    fn a_page_with_no_update_time_but_a_figure_showing_source_says_no_figures() {
        let stored = PageData {
            quality: DataQuality::Manual,
            updated_at: None,
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
        assert_eq!(s.pages[0].data_quality, DataQuality::Unavailable);
        assert_eq!(s.pages[0].note.as_deref(), Some(NO_FIGURES_NOTE));
        s.validate().unwrap();
    }

    #[test]
    fn a_page_with_a_bad_id_is_left_off_and_the_rest_still_go_out() {
        // Only a damaged database can give these: the API checks ids when a page is made.
        let t = ts("2026-11-10T20:00:00Z");
        let long = "x".repeat(4000);
        let pages = vec![
            (sub("Bad ID", Service::ClaudeMax, Source::Demo), None),
            (sub(&long, Service::ClaudeMax, Source::Demo), None),
            (sub("good", Service::ClaudeMax, Source::Demo), None),
        ];
        let snap = build_snapshot(t, 300, &pages);
        let ids: Vec<&str> = snap.pages.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, ["good"]);
        snap.to_wire().unwrap();
    }

    #[test]
    fn one_bad_page_is_replaced_and_the_rest_still_go_out() {
        let t = ts("2026-11-10T20:00:00Z");
        let item = |i: usize| MeterItem::new(format!("m{i}"), "M", Some(1.0), None);
        let meter = |items| Meter { items };
        // Over the page cap (here through a huge title) and with data that breaks its shape
        // (seven meter items).
        let big = Page::with_content(
            "big",
            "x".repeat(4000),
            DataQuality::Manual,
            Some(t),
            1800,
            None,
            meter(vec![item(0)]),
        );
        let bad = Page::with_content(
            "bad",
            "Bad",
            DataQuality::Manual,
            Some(t),
            1800,
            None,
            meter((0..7).map(item).collect()),
        );
        let fine = guard(page(
            t,
            &sub("claude-max", Service::ClaudeMax, Source::Demo),
            None,
        ));
        let (big, bad) = (guard(big), guard(bad));
        assert_eq!(big.note.as_deref(), Some(TOO_BIG_NOTE));
        assert_eq!(bad.note.as_deref(), Some(INVALID_DATA_NOTE));
        for p in [&big, &bad] {
            assert_eq!(p.data_quality, DataQuality::Unavailable);
            assert!(p.data.is_none());
        }
        assert_eq!(big.id, "big");
        assert_eq!(
            big.title,
            format!("{}…", "x".repeat(hub_protocol::limits::MAX_TITLE_LEN - 1))
        );
        assert_eq!(fine.data_quality, DataQuality::Demo);
        let snap = Snapshot {
            protocol_version: PROTOCOL_VERSION,
            message_type: MESSAGE_TYPE.into(),
            generated_at: t,
            refresh_after_seconds: 300,
            pages: vec![big, fine, bad],
        };
        snap.to_wire().unwrap();
    }
}
