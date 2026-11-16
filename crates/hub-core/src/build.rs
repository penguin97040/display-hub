//! Turns stored pages into the snapshot a display receives.

use hub_protocol::limits::MAX_PAGES;
use hub_protocol::shapes::{classify_kind, KindClass, KIND_METER, KIND_TEXT};
use hub_protocol::snapshot::MESSAGE_TYPE;
use hub_protocol::{CantShow, Page, Snapshot, Timestamp, PROTOCOL_VERSION};
use serde_json::Value;

use crate::model::{clean_note, clean_title, PageData, Source, StoredContent, StoredPage};
use crate::provider::generated;

pub const RESET_NOTE: &str = "Window reset since the last report";
pub const NO_SOURCE_NOTE: &str = "No supported source connected yet";
pub const NEVER_COLLECTED_NOTE: &str = "Waiting for the first report";
/// A page with a source but nothing to show yet, such as a meter with no items.
pub const NO_FIGURES_NOTE: &str = "No figures yet";
/// Stand-ins for a page that breaks the page-level rules, so one bad page never blanks the rest.
pub const TOO_BIG_NOTE: &str = "This page is too big to show";
pub const INVALID_DATA_NOTE: &str = "This page's data isn't valid";
/// The stand-in's title when the original can't be cleaned into one.
pub const UNTITLED: &str = "Untitled page";
pub const DEFAULT_REFRESH_SECONDS: u32 = 300;

/// Builds a snapshot. `pages` are in display order, each with what was last stored for it. Demo
/// pages are generated fresh. Pages past the limit are left off.
pub fn build_snapshot(now: Timestamp, refresh_after_seconds: u32, pages: &[StoredPage]) -> Snapshot {
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
        .map(|p| guard(page(now, p)))
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
/// unavailable page with the same id, kind and stale time, so the rest of the snapshot still goes
/// out. An unavailable page carries no data, so it passes the kind's own rules whatever they are.
fn guard(page: Page) -> Page {
    let note = match page.check() {
        Ok(()) => return page,
        Err(CantShow::TooLarge) => TOO_BIG_NOTE,
        Err(_) => INVALID_DATA_NOTE,
    };
    // The title may be what broke the page, so the stand-in gets a cleaned one. So may the kind:
    // one that is neither a shape nor a custom kind becomes `text`, the plainest shape.
    let title = clean_title(&page.title).unwrap_or_else(|| UNTITLED.to_string());
    let kind = match classify_kind(&page.kind) {
        KindClass::Invalid => KIND_TEXT.to_string(),
        KindClass::Shape | KindClass::Custom => page.kind,
    };
    Page::unavailable(page.id, title, kind, page.stale_after_seconds, Some(note.into()))
}

/// Fresh figures for a page whose source generates them (today, demo usage pages).
fn generated_content(now: Timestamp, page: &StoredPage) -> Option<StoredContent> {
    let service = page.usage_service()?;
    let data: PageData = generated(service, page.source)?.fetch(now).ok()?;
    Some(data.to_content())
}

fn page(now: Timestamp, p: &StoredPage) -> Page {
    let fresh = generated_content(now, p);
    let unavailable = |note: &str| StoredContent {
        quality: hub_protocol::DataQuality::Unavailable,
        updated_at: None,
        data: None,
        note: Some(note.into()),
    };
    let content = match (p.source, fresh, p.content.as_ref()) {
        (Source::None, _, _) => unavailable(NO_SOURCE_NOTE),
        (_, Some(g), _) => g,
        (_, None, Some(s)) => s.clone(),
        (Source::Collector, None, None) => unavailable(NEVER_COLLECTED_NOTE),
        (_, None, None) => unavailable(NO_SOURCE_NOTE),
    };

    let title = clean_title(&p.title).unwrap_or_else(|| UNTITLED.to_string());
    let mut note = content.note.as_deref().and_then(clean_note);
    let shows = content.quality.shows_figures();
    // A time from the future is a clock fault somewhere: don't let it look fresh.
    let updated_at = content.updated_at.map(|u| u.min(now));

    let mut data = if shows && updated_at.is_some() {
        content.data.clone().filter(|d| !d.is_null())
    } else {
        None
    };
    if p.kind == KIND_METER {
        if let Some(d) = data.as_mut() {
            if clear_reset_items(now, d) && note.is_none() {
                note = Some(RESET_NOTE.into());
            }
            // A meter with no items has nothing to show yet.
            if d.get("items")
                .and_then(Value::as_array)
                .is_some_and(Vec::is_empty)
            {
                data = None;
            }
        }
    }
    if let (Some(updated_at), Some(data)) = (updated_at, data) {
        return Page {
            id: p.id.clone(),
            title,
            kind: p.kind.clone(),
            data_quality: content.quality,
            updated_at: Some(updated_at),
            stale_after_seconds: p.stale_after_seconds,
            note,
            data: Some(data),
        };
    }
    if note.is_none() {
        note = Some(if shows { NO_FIGURES_NOTE } else { NO_SOURCE_NOTE }.into());
    }
    Page::unavailable(p.id.clone(), title, p.kind.clone(), p.stale_after_seconds, note)
}

/// Once a meter item's window has reset, its old share no longer describes it, and the new one is
/// unknown until the next report: the share becomes unknown and the reset time goes. True if a
/// known share was cleared. Data that isn't a well-formed meter is left for the page checks.
fn clear_reset_items(now: Timestamp, data: &mut Value) -> bool {
    let mut cleared = false;
    let Some(items) = data.get_mut("items").and_then(Value::as_array_mut) else {
        return false;
    };
    for item in items.iter_mut().filter_map(Value::as_object_mut) {
        let reset = item
            .get("resets_at")
            .and_then(Value::as_str)
            .and_then(|s| Timestamp::parse(s).ok())
            .is_some_and(|r| r <= now);
        if reset {
            cleared |= item.get("percent").is_some_and(|p| !p.is_null());
            item.insert("percent".into(), Value::Null);
            item.remove("resets_at");
        }
    }
    cleared
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{MetricValue, Service, Subscription, Window};
    use hub_protocol::{DataQuality, Meter, MeterItem, PageContent};
    use serde_json::json;

    fn ts(s: &str) -> Timestamp {
        Timestamp::parse(s).unwrap()
    }

    /// The builder, fed usage subscriptions as meter pages.
    fn build_snapshot(now: Timestamp, refresh: u32, pages: &[(Subscription, Option<PageData>)]) -> Snapshot {
        let pages: Vec<StoredPage> = pages.iter().map(|(s, d)| s.to_page(d.as_ref())).collect();
        super::build_snapshot(now, refresh, &pages)
    }

    fn stored(id: &str, kind: &str, data: Value) -> StoredPage {
        StoredPage {
            id: id.into(),
            title: "Page".into(),
            kind: kind.into(),
            source: Source::Manual,
            feed: None,
            stale_after_seconds: 600,
            content: Some(StoredContent {
                quality: DataQuality::Manual,
                updated_at: Some(ts("2026-11-10T19:00:00Z")),
                data: Some(data),
                note: None,
            }),
        }
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
            &sub("claude-max", Service::ClaudeMax, Source::Demo).to_page(None),
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

    #[test]
    fn pages_of_any_kind_go_out_as_stored() {
        let t = ts("2026-11-10T20:00:00Z");
        let pages = vec![
            stored(
                "flights",
                "nearby-flights/1",
                json!({"flights": [{"callsign": "ANZ1"}]}),
            ),
            stored("big", "big_number", json!({"value": 42, "unit": "kWh"})),
            stored("words", "text", json!({"text": "Back at 3 pm"})),
            stored(
                "todo",
                "list",
                json!({"items": [{"text": "Milk", "detail": "2 L"}]}),
            ),
        ];
        let snap = super::build_snapshot(t, 300, &pages);
        snap.validate().unwrap();
        for (out, inp) in snap.pages.iter().zip(&pages) {
            assert_eq!(out.kind, inp.kind);
            assert_eq!(out.data, inp.content.as_ref().unwrap().data);
            assert_eq!(out.stale_after_seconds, 600);
            assert_eq!(out.data_quality, DataQuality::Manual);
        }
        // An empty list is something to show; an empty meter isn't.
        let empty = vec![
            stored("l", "list", json!({"items": []})),
            stored("m", "meter", json!({"items": []})),
        ];
        let snap = super::build_snapshot(t, 300, &empty);
        assert_eq!(snap.pages[0].data_quality, DataQuality::Manual);
        assert_eq!(snap.pages[1].data_quality, DataQuality::Unavailable);
        assert_eq!(snap.pages[1].kind, "meter");
        assert_eq!(snap.pages[1].note.as_deref(), Some(NO_FIGURES_NOTE));
    }

    #[test]
    fn only_the_first_ten_pages_go_out() {
        let t = ts("2026-11-10T20:00:00Z");
        let pages: Vec<StoredPage> = (0..12)
            .map(|i| stored(&format!("p{i}"), "text", json!({"text": "x"})))
            .collect();
        let snap = super::build_snapshot(t, 300, &pages);
        assert_eq!(snap.pages.len(), MAX_PAGES);
        assert_eq!(snap.pages.last().unwrap().id, "p9");
    }

    #[test]
    fn a_stand_in_keeps_the_kind_of_the_page_it_replaces() {
        let t = ts("2026-11-10T20:00:00Z");
        let deep = (0..9).fold(json!(1), |v, _| json!({ "n": v }));
        let pages = vec![
            // Custom data nested too deeply, a number out of range, a page over the size cap and a
            // kind that is neither a shape nor a custom kind.
            stored("deep", "nearby-flights/1", deep),
            stored("big", "big_number", json!({"value": 1e300})),
            stored("huge", "text", json!({"text": "x".repeat(4000)})),
            stored("odd", "Not A Kind", json!({})),
        ];
        let snap = super::build_snapshot(t, 300, &pages);
        snap.validate().unwrap();
        let got: Vec<(&str, &str, Option<&str>)> = snap
            .pages
            .iter()
            .map(|p| (p.id.as_str(), p.kind.as_str(), p.note.as_deref()))
            .collect();
        assert_eq!(
            got,
            [
                ("deep", "nearby-flights/1", Some(INVALID_DATA_NOTE)),
                ("big", "big_number", Some(INVALID_DATA_NOTE)),
                ("huge", "text", Some(TOO_BIG_NOTE)),
                ("odd", "text", Some(INVALID_DATA_NOTE)),
            ]
        );
        for p in &snap.pages {
            assert_eq!(p.data_quality, DataQuality::Unavailable);
            assert!(p.data.is_none());
            assert_eq!(p.stale_after_seconds, 600);
            p.check().unwrap();
        }
    }

    #[test]
    fn a_kind_too_long_to_send_gets_a_plain_stand_in() {
        let t = ts("2026-11-10T20:00:00Z");
        let long = format!("{}/1", "a".repeat(4000));
        let bad = Page::with_content(
            "long",
            "Long",
            DataQuality::Manual,
            Some(t),
            600,
            None,
            hub_protocol::PageContent::custom(long, json!({})),
        );
        let stand_in = guard(bad);
        assert_eq!(stand_in.kind, KIND_TEXT);
        assert_eq!(stand_in.note.as_deref(), Some(TOO_BIG_NOTE));
        stand_in.check().unwrap();
    }

    #[test]
    fn meter_items_whose_window_reset_lose_their_share() {
        let t = ts("2026-11-10T20:00:00Z");
        let meter = Meter {
            items: vec![
                MeterItem::new("a", "A", Some(50.0), Some(ts("2026-11-10T19:59:59Z"))),
                MeterItem::new("b", "B", None, Some(ts("2026-11-10T20:00:00Z"))),
                MeterItem::new("c", "C", Some(5.0), Some(ts("2026-11-10T20:00:01Z"))),
            ],
        };
        let p = stored("m", "meter", serde_json::to_value(meter).unwrap());
        let snap = super::build_snapshot(t, 300, &[p]);
        let Some(PageContent::Meter(m)) = snap.pages[0].content().unwrap() else {
            panic!("not a meter")
        };
        assert_eq!(
            m.items,
            [
                MeterItem::new("a", "A", None, None),
                MeterItem::new("b", "B", None, None),
                MeterItem::new("c", "C", Some(5.0), Some(ts("2026-11-10T20:00:01Z"))),
            ]
        );
        assert_eq!(snap.pages[0].note.as_deref(), Some(RESET_NOTE));
    }
}
