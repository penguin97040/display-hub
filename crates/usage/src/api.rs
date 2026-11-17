//! Usage pages as the manager and the usage helper see them, and the checks their figures pass before
//! they are sent. The hub takes these as ordinary meter pages.

use hub_core::api::{FeedContent, NewPage, PageContentInput, PageView};
use hub_core::Source;
use hub_protocol::shapes::KIND_METER;
use hub_protocol::Timestamp;
use serde::{Deserialize, Serialize};

use crate::model::{clean_percent, meter_data, to_subscription, MetricValue, PageData, Service, Window};

/// One usage page, with its stored figures.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SubscriptionView {
    pub id: String,
    pub service: Service,
    pub title: String,
    pub source: Source,
    pub data: Option<PageData>,
}

impl SubscriptionView {
    /// A usage page as the manager sees it; `None` for any other page.
    pub fn from_page(page: &PageView) -> Option<Self> {
        let (s, data) = to_subscription(page)?;
        Some(Self {
            id: s.id,
            service: s.service,
            title: s.title,
            source: s.source,
            data,
        })
    }
}

/// A new usage page.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NewSubscription {
    pub service: Service,
    pub source: Source,
    /// Defaults to the service's name.
    pub title: Option<String>,
    /// Defaults to the service's slug, made unique.
    pub id: Option<String>,
}

impl NewSubscription {
    pub fn to_new_page(&self) -> NewPage {
        NewPage {
            title: self
                .title
                .clone()
                .unwrap_or_else(|| self.service.default_title().into()),
            kind: KIND_METER.into(),
            source: self.source,
            feed: Some(self.service.slug().into()),
            id: self.id.clone(),
            stale_after_seconds: None,
            template: None,
            settings: None,
        }
    }
}

/// A change to a usage page's title or source.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SubscriptionChange {
    pub title: Option<String>,
    pub source: Option<Source>,
}

/// Figures typed in by the owner for a usage page.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ManualFigures {
    pub metrics: Vec<ManualMetric>,
    pub note: Option<String>,
}

impl ManualFigures {
    /// The figures as a usage page's meter content, once they pass [`clean_figures`]: shares
    /// rounded to one decimal place, and each window's own id and label. The error is a sentence
    /// for the owner.
    pub fn to_content(&self, service: Service) -> Result<PageContentInput, String> {
        let metrics = clean_figures(service, &self.metrics)?;
        Ok(PageContentInput {
            data: Some(meter_data(&metrics)),
            note: self.note.clone(),
        })
    }
}

/// Checks typed-in or reported figures against the service's windows and bounds, and rounds shares
/// to one decimal place. Unknown stays unknown. The error is a sentence for the owner.
pub fn clean_figures(service: Service, input: &[ManualMetric]) -> Result<Vec<MetricValue>, String> {
    let mut metrics: Vec<MetricValue> = Vec::new();
    for m in input {
        if !service.windows().contains(&m.window) {
            return Err(format!(
                "{} has no {} window.",
                service.default_title(),
                m.window.id()
            ));
        }
        if metrics.iter().any(|x| x.window == m.window) {
            return Err(format!("The {} window is listed twice.", m.window.id()));
        }
        let used_percent = match m.used_percent {
            Some(p) => Some(clean_percent(p).ok_or("Percentages must be 0 to 100.")?),
            None => None,
        };
        metrics.push(MetricValue {
            window: m.window,
            used_percent,
            resets_at: m.resets_at,
        });
    }
    Ok(metrics)
}

/// One allowance figure, as typed in or reported by a helper.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ManualMetric {
    pub window: Window,
    pub used_percent: Option<f64>,
    pub resets_at: Option<Timestamp>,
}

/// Figures a provider's own tool reported on the PC, for one service.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UsageReport {
    pub service: Service,
    /// When the tool reported them, by the PC's clock.
    pub collected_at: Timestamp,
    pub metrics: Vec<ManualMetric>,
}

impl UsageReport {
    /// The report as the meter page body a helper sends to the service's feed, once its figures
    /// pass [`clean_figures`]. Gives the feed name and the body.
    pub fn to_feed_content(&self) -> Result<(&'static str, FeedContent), String> {
        let metrics = clean_figures(self.service, &self.metrics)?;
        Ok((
            self.service.slug(),
            FeedContent {
                kind: KIND_METER.into(),
                data: meter_data(&metrics),
                collected_at: Some(self.collected_at),
                note: None,
            },
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn metric(window: Window, pct: Option<f64>) -> ManualMetric {
        ManualMetric {
            window,
            used_percent: pct,
            resets_at: None,
        }
    }

    #[test]
    fn figures_must_be_the_services_windows_once_each_and_in_range() {
        let e = clean_figures(Service::ClaudeMax, &[metric(Window::Monthly, Some(1.0))]).unwrap_err();
        assert_eq!(e, "Claude Max has no monthly window.");
        let e = clean_figures(
            Service::ClaudeMax,
            &[
                metric(Window::FiveHour, Some(1.0)),
                metric(Window::FiveHour, None),
            ],
        )
        .unwrap_err();
        assert_eq!(e, "The five-hour window is listed twice.");
        for bad in [101.0, -1.0, f64::NAN, f64::INFINITY] {
            let e = clean_figures(Service::ChatgptPlus, &[metric(Window::Weekly, Some(bad))]).unwrap_err();
            assert_eq!(e, "Percentages must be 0 to 100.");
        }
        let ok = clean_figures(
            Service::OpencodeGoPlus,
            &[metric(Window::Monthly, Some(33.33)), metric(Window::Weekly, None)],
        )
        .unwrap();
        assert_eq!(ok[0].used_percent, Some(33.3));
        assert_eq!(ok[1].used_percent, None, "unknown stays unknown");
        assert!(clean_figures(Service::ClaudeMax, &[]).unwrap().is_empty());
    }

    #[test]
    fn typed_in_figures_become_a_meter_with_the_windows_own_labels() {
        let figures = ManualFigures {
            metrics: vec![metric(Window::Weekly, Some(42.25))],
            note: Some("Typed".into()),
        };
        let content = figures.to_content(Service::ClaudeMax).unwrap();
        assert_eq!(
            content.data,
            Some(json!({"items": [{"id": "weekly", "label": "Weekly allowance used", "percent": 42.3}]}))
        );
        assert_eq!(content.note.as_deref(), Some("Typed"));
        let bad = ManualFigures {
            metrics: vec![metric(Window::Monthly, Some(1.0))],
            note: None,
        };
        assert!(bad.to_content(Service::ChatgptPlus).is_err());
    }

    #[test]
    fn a_report_becomes_a_meter_body_for_the_services_feed() {
        let at = Timestamp::parse("2026-11-10T19:59:00Z").unwrap();
        let report = UsageReport {
            service: Service::ChatgptPlus,
            collected_at: at,
            metrics: vec![metric(Window::FiveHour, Some(12.34))],
        };
        let (feed, body) = report.to_feed_content().unwrap();
        assert_eq!(feed, "chatgpt-plus");
        assert_eq!(body.kind, "meter");
        assert_eq!(body.collected_at, Some(at));
        assert_eq!(body.data["items"][0]["percent"], json!(12.3));
        let bad = UsageReport {
            metrics: vec![metric(Window::FiveHour, Some(101.0))],
            ..report
        };
        assert!(bad.to_feed_content().is_err());
    }

    #[test]
    fn a_new_usage_page_is_a_meter_fed_by_its_service() {
        let page = NewSubscription {
            service: Service::OpencodeGoPlus,
            source: Source::Helper,
            title: None,
            id: None,
        }
        .to_new_page();
        assert_eq!(
            (page.title.as_str(), page.kind.as_str(), page.feed.as_deref()),
            ("OpenCode Go Plus", "meter", Some("opencode-go-plus"))
        );
    }
}
