//! Usage subscriptions (meter pages) with their figures.

use hub_core::{Source, StoredContent, StoredPage};
use hub_protocol::shapes::KIND_METER;
use hub_protocol::{DataQuality, Meter, MeterItem, Timestamp};
use serde::{Deserialize, Serialize};

/// A subscription service whose allowances can be shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Service {
    ClaudeMax,
    ChatgptPlus,
    OpencodeGoPlus,
}

impl Service {
    pub const ALL: [Service; 3] = [Service::ClaudeMax, Service::ChatgptPlus, Service::OpencodeGoPlus];

    /// Also the service's feed name and default page id.
    pub fn slug(self) -> &'static str {
        match self {
            Service::ClaudeMax => "claude-max",
            Service::ChatgptPlus => "chatgpt-plus",
            Service::OpencodeGoPlus => "opencode-go-plus",
        }
    }

    pub fn from_slug(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|x| x.slug() == s)
    }

    pub fn default_title(self) -> &'static str {
        match self {
            Service::ClaudeMax => "Claude Max",
            Service::ChatgptPlus => "ChatGPT Plus",
            Service::OpencodeGoPlus => "OpenCode Go Plus",
        }
    }

    /// The allowance windows this service is known to have. Helpers may report fewer.
    pub fn windows(self) -> &'static [Window] {
        match self {
            Service::ClaudeMax | Service::ChatgptPlus => &[Window::FiveHour, Window::Weekly],
            Service::OpencodeGoPlus => &[Window::FiveHour, Window::Weekly, Window::Monthly],
        }
    }
}

/// A usage allowance window.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Window {
    FiveHour,
    Weekly,
    Monthly,
}

impl Window {
    pub const ALL: [Window; 3] = [Window::FiveHour, Window::Weekly, Window::Monthly];

    /// Also the meter item's id.
    pub fn id(self) -> &'static str {
        match self {
            Window::FiveHour => "five-hour",
            Window::Weekly => "weekly",
            Window::Monthly => "monthly",
        }
    }

    pub fn from_id(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|w| w.id() == s)
    }

    pub fn label(self) -> &'static str {
        match self {
            Window::FiveHour => "Five-hour allowance used",
            Window::Weekly => "Weekly allowance used",
            Window::Monthly => "Monthly allowance used",
        }
    }
}

/// One usage page on the display.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Subscription {
    /// The page id: lower-case letters, digits and hyphens.
    pub id: String,
    pub service: Service,
    pub title: String,
    pub source: Source,
}

/// One allowance figure as stored.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MetricValue {
    pub window: Window,
    pub used_percent: Option<f64>,
    pub resets_at: Option<Timestamp>,
}

/// The latest figures for a subscription.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PageData {
    pub quality: DataQuality,
    pub updated_at: Option<Timestamp>,
    pub metrics: Vec<MetricValue>,
    pub note: Option<String>,
}

impl PageData {
    pub fn unavailable(note: &str) -> Self {
        Self {
            quality: DataQuality::Unavailable,
            updated_at: None,
            metrics: vec![],
            note: Some(note.into()),
        }
    }

    /// The figures as a meter page's content.
    pub fn to_content(&self) -> StoredContent {
        StoredContent {
            quality: self.quality,
            updated_at: self.updated_at,
            data: Some(meter_data(&self.metrics)),
            note: self.note.clone(),
        }
    }
}

/// Figures as a meter's `data`: one item per window, with the window's id and label.
pub fn meter_data(metrics: &[MetricValue]) -> serde_json::Value {
    let items: Vec<MeterItem> = metrics
        .iter()
        .map(|m| MeterItem::new(m.window.id(), m.window.label(), m.used_percent, m.resets_at))
        .collect();
    serde_json::to_value(Meter { items }).expect("meters always serialise")
}

impl Subscription {
    /// The subscription as a meter page, with its figures if it has any.
    pub fn to_page(&self, data: Option<&PageData>) -> StoredPage {
        StoredPage {
            id: self.id.clone(),
            title: self.title.clone(),
            kind: KIND_METER.into(),
            source: self.source,
            feed: Some(self.service.slug().into()),
            stale_after_seconds: self.source.stale_after_seconds(),
            content: data.map(PageData::to_content),
            template: None,
            settings: None,
        }
    }
}

/// The usage service a page belongs to: a meter page whose feed is a service's slug.
pub fn usage_service(page: &StoredPage) -> Option<Service> {
    (page.kind == KIND_METER)
        .then(|| Service::from_slug(page.feed.as_deref()?))
        .flatten()
}

/// A usage page as a subscription with its figures. Meter items that aren't allowance windows are
/// left out. `None` for any other page.
pub fn to_subscription(page: &StoredPage) -> Option<(Subscription, Option<PageData>)> {
    let service = usage_service(page)?;
    let data = page.content.as_ref().map(|c| PageData {
        quality: c.quality,
        updated_at: c.updated_at,
        metrics: c
            .data
            .clone()
            .and_then(|d| serde_json::from_value::<Meter>(d).ok())
            .map(|m| {
                m.items
                    .into_iter()
                    .filter_map(|i| {
                        Some(MetricValue {
                            window: Window::from_id(&i.id)?,
                            used_percent: i.percent,
                            resets_at: i.resets_at,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default(),
        note: c.note.clone(),
    });
    Some((
        Subscription {
            id: page.id.clone(),
            service,
            title: page.title.clone(),
            source: page.source,
        },
        data,
    ))
}

/// Rounds a percentage to one decimal place and checks it is 0-100.
pub fn clean_percent(p: f64) -> Option<f64> {
    (p.is_finite() && (0.0..=100.0).contains(&p)).then(|| (p * 10.0).round() / 10.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_custom_or_unknown_feed_page_is_not_a_usage_page() {
        let page = |kind: &str, feed: Option<&str>| StoredPage {
            id: "p".into(),
            title: "P".into(),
            kind: kind.into(),
            source: Source::Manual,
            feed: feed.map(String::from),
            stale_after_seconds: 600,
            content: None,
            template: None,
            settings: None,
        };
        assert_eq!(
            usage_service(&page("meter", Some("claude-max"))),
            Some(Service::ClaudeMax)
        );
        assert_eq!(usage_service(&page("nearby-flights/1", Some("claude-max"))), None);
        assert_eq!(usage_service(&page("meter", Some("weather"))), None);
        assert_eq!(usage_service(&page("meter", None)), None);
    }

    #[test]
    fn a_usage_page_round_trips_and_drops_items_that_are_not_windows() {
        let now = Timestamp::parse("2026-11-10T20:00:00Z").unwrap();
        let sub = Subscription {
            id: "work".into(),
            service: Service::OpencodeGoPlus,
            title: "Work".into(),
            source: Source::Manual,
        };
        let data = PageData {
            quality: DataQuality::Manual,
            updated_at: Some(now),
            metrics: vec![MetricValue {
                window: Window::Monthly,
                used_percent: Some(9.5),
                resets_at: None,
            }],
            note: Some("Typed".into()),
        };
        let mut page = sub.to_page(Some(&data));
        assert_eq!(page.feed.as_deref(), Some("opencode-go-plus"));
        assert_eq!(to_subscription(&page), Some((sub.clone(), Some(data.clone()))));
        page.content.as_mut().unwrap().data = Some(json!({"items": [
            {"id": "daily", "label": "Daily", "percent": 1},
            {"id": "monthly", "label": "Mine", "percent": 9.5}
        ]}));
        assert_eq!(to_subscription(&page).unwrap().1.unwrap().metrics, data.metrics);
    }

    #[test]
    fn percentages_are_rounded_and_bounded() {
        assert_eq!(clean_percent(42.25), Some(42.3));
        assert_eq!(clean_percent(100.0), Some(100.0));
        assert_eq!(clean_percent(100.1), None);
        assert_eq!(clean_percent(-0.1), None);
        assert_eq!(clean_percent(f64::NAN), None);
    }
}
