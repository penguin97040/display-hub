//! Pages as the hub stores them, and usage subscriptions (meter pages) with their figures.

use hub_protocol::limits::{MAX_LABEL_LEN, MAX_NOTE_LEN, MAX_TITLE_LEN};
use hub_protocol::shapes::KIND_METER;
use hub_protocol::text::sanitise_text;
use hub_protocol::{DataQuality, Meter, MeterItem, Timestamp};
use serde::{Deserialize, Serialize};

/// A subscription service the hub knows how to describe.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Service {
    ClaudeMax,
    ChatgptPlus,
    OpencodeGoPlus,
}

impl Service {
    pub const ALL: [Service; 3] = [Service::ClaudeMax, Service::ChatgptPlus, Service::OpencodeGoPlus];

    /// Also the default page id.
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

    /// The allowance windows this service is known to have. Collectors may report fewer.
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

/// Where a subscription's figures come from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Source {
    /// Sample figures. Always labelled demo.
    Demo,
    /// Typed in by the owner through the manager.
    Manual,
    /// Pushed by a paired collector, such as the PC helper.
    Collector,
    /// Nothing yet. The page shows as unavailable, with the reason.
    None,
}

impl Source {
    pub const ALL: [Source; 4] = [Source::Demo, Source::Manual, Source::Collector, Source::None];

    pub fn slug(self) -> &'static str {
        match self {
            Source::Demo => "demo",
            Source::Manual => "manual",
            Source::Collector => "collector",
            Source::None => "none",
        }
    }

    pub fn from_slug(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|x| x.slug() == s)
    }

    /// How long figures from this source stay fresh.
    pub fn stale_after_seconds(self) -> u32 {
        match self {
            Source::Demo => 3600,
            Source::Manual => 86_400,
            Source::Collector => 1800,
            Source::None => 3600,
        }
    }
}

/// One page on the display.
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
}

/// One page as the hub stores it: any kind, with whatever was last stored for it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StoredPage {
    /// The page id: lower-case letters, digits and hyphens.
    pub id: String,
    pub title: String,
    /// A ready-made shape (`meter`, `big_number`, `text`, `list`) or a custom `name/version` kind.
    pub kind: String,
    pub source: Source,
    /// The named feed that fills the page, such as a usage service. A collector may write only to
    /// pages whose feed it is scoped to and whose source is `collector`.
    pub feed: Option<String>,
    pub stale_after_seconds: u32,
    /// What was last stored for the page. `None` until something is.
    pub content: Option<StoredContent>,
}

/// What was last stored for a page.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StoredContent {
    pub quality: DataQuality,
    pub updated_at: Option<Timestamp>,
    /// The page's `data`, in the shape its kind names. `None` when there is nothing to show.
    pub data: Option<serde_json::Value>,
    pub note: Option<String>,
}

// Usage subscriptions are meter pages: the service is the page's feed, and each allowance window is
// a meter item.

impl PageData {
    /// The figures as a meter page's content.
    pub fn to_content(&self) -> StoredContent {
        let items: Vec<MeterItem> = self
            .metrics
            .iter()
            .map(|m| MeterItem::new(m.window.id(), m.window.label(), m.used_percent, m.resets_at))
            .collect();
        StoredContent {
            quality: self.quality,
            updated_at: self.updated_at,
            data: Some(serde_json::to_value(Meter { items }).expect("meters always serialise")),
            note: self.note.clone(),
        }
    }
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
        }
    }
}

impl StoredPage {
    /// The usage service a meter page belongs to, if it is a usage page.
    pub fn usage_service(&self) -> Option<Service> {
        (self.kind == KIND_METER)
            .then(|| Service::from_slug(self.feed.as_deref()?))
            .flatten()
    }

    /// A usage page as a subscription with its figures. Meter items that aren't allowance windows
    /// are left out.
    pub fn to_subscription(&self) -> Option<(Subscription, Option<PageData>)> {
        let service = self.usage_service()?;
        let data = self.content.as_ref().map(|c| PageData {
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
                id: self.id.clone(),
                service,
                title: self.title.clone(),
                source: self.source,
            },
            data,
        ))
    }
}

/// Owner-supplied page titles and notes, made safe for the wire.
pub fn clean_title(s: &str) -> Option<String> {
    sanitise_text(s, MAX_TITLE_LEN)
}

pub fn clean_note(s: &str) -> Option<String> {
    sanitise_text(s, MAX_NOTE_LEN)
}

pub fn clean_label(s: &str) -> Option<String> {
    sanitise_text(s, MAX_LABEL_LEN)
}

/// A page id is valid wire text and short.
pub fn valid_page_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 48
        && !id.starts_with('-')
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Rounds a percentage to one decimal place and checks it is 0-100.
pub fn clean_percent(p: f64) -> Option<f64> {
    (p.is_finite() && (0.0..=100.0).contains(&p)).then(|| (p * 10.0).round() / 10.0)
}
