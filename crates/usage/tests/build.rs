//! Usage pages through the hub's snapshot builder: they are ordinary meter pages there.

use hub_core::{build_snapshot, Source};
use hub_protocol::{DataQuality, PageContent, Timestamp};
use usage::{MetricValue, PageData, Service, Subscription, Window};

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

fn meter(p: &hub_protocol::Page) -> hub_protocol::Meter {
    match p.content().unwrap() {
        Some(PageContent::Meter(m)) => m,
        other => panic!("not a meter: {other:?}"),
    }
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
    let page = sub("claude-max", Service::ClaudeMax, Source::Helper).to_page(Some(&stored));
    let s = build_snapshot(now, 300, &[page]);
    let p = &s.pages[0];
    let m = meter(p);
    assert_eq!(m.items[0].percent, None);
    assert_eq!(m.items[0].resets_at, None);
    assert_eq!(m.items[1].percent, Some(30.0));
    assert_eq!(p.note.as_deref(), Some(hub_core::build::RESET_NOTE));
    s.validate().unwrap();
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
    let s = build_snapshot(now, 5, &[s1.to_page(Some(&stored))]);
    let p = &s.pages[0];
    assert_eq!(p.title, "Mine");
    assert_eq!(p.updated_at, Some(now));
    assert_eq!(meter(p).items[0].percent, None);
    assert_eq!(s.refresh_after_seconds, 30);
    s.validate().unwrap();
}

#[test]
fn demo_usage_pages_show_the_sample_figures_as_fresh() {
    let now = ts("2026-11-10T20:00:00Z");
    let mut page = sub("opencode-go-plus", Service::OpencodeGoPlus, Source::Demo).to_page(None);
    let sample = usage::provider::demo_content(Service::OpencodeGoPlus);
    page.content = Some(hub_core::StoredContent {
        quality: DataQuality::Demo,
        updated_at: Some(ts("2026-01-01T00:00:00Z")),
        data: sample.data,
        note: sample.note,
    });
    let s = build_snapshot(now, 300, &[page]);
    let p = &s.pages[0];
    assert_eq!((p.data_quality, p.updated_at), (DataQuality::Demo, Some(now)));
    assert_eq!(p.note.as_deref(), Some(usage::provider::DEMO_NOTE));
    let shares: Vec<Option<f64>> = meter(p).items.iter().map(|i| i.percent).collect();
    assert_eq!(shares, [Some(17.0), Some(31.0), Some(9.0)]);
    s.validate().unwrap();
}
