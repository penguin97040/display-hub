//! Admin API bodies, shared by the hub and its clients. Admin endpoints need an admin token.

use hub_protocol::Timestamp;
use serde::{Deserialize, Serialize};

use crate::model::{PageData, Service, Source, StoredPage, Window};

/// `POST /v1/admin/pair`: exchanges the one-time secret from `hubd pair-admin` for an admin token.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct PairRequest {
    pub secret: String,
    /// What this admin is, for the hub's list (for example the PC's name). Plain text.
    pub label: String,
}

impl std::fmt::Debug for PairRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PairRequest")
            .field("secret", &"[redacted]")
            .field("label", &self.label)
            .finish()
    }
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct PairResponse {
    pub admin_token: String,
    pub identity: HubIdentity,
}

impl std::fmt::Debug for PairResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PairResponse")
            .field("admin_token", &"[redacted]")
            .field("identity", &self.identity)
            .finish()
    }
}

/// The hub's TLS identity, for building provisioning bundles.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HubIdentity {
    pub certificate_pem: String,
    /// Lower-case hex SHA-256 of the SubjectPublicKeyInfo.
    pub spki_sha256: String,
    pub server_name: String,
}

/// `GET /v1/admin/status`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HubStatus {
    pub version: String,
    pub started_at: Timestamp,
    pub now: Timestamp,
    /// Whether the operating system reports its clock as synchronised (NTP). Ages and reset times
    /// are only as right as this clock.
    pub clock_synchronised: Option<bool>,
    pub identity: HubIdentity,
    /// Where displays should look for the hub, in order (`advertise_hosts`, or `<hostname>.local`).
    #[serde(default)]
    pub hosts: Vec<String>,
    /// The port the hub listens on.
    #[serde(default)]
    pub port: u16,
    /// Pages the hub holds, of every kind.
    pub pages: usize,
    pub displays: usize,
    pub admins: usize,
}

/// `GET /v1/admin/pages` lists every page as a [`StoredPage`], in the shared display order, and
/// the other page routes answer with the page they changed.
pub type PageView = StoredPage;

/// `POST /v1/admin/pages`: a new page, added at the end of the order and ticked on every display.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NewPage {
    pub title: String,
    /// A ready-made shape (`meter`, `big_number`, `text`, `list`) or a custom `name/version` kind.
    /// It can't be changed later.
    pub kind: String,
    pub source: Source,
    /// The named feed that fills the page, such as a usage service.
    #[serde(default)]
    pub feed: Option<String>,
    /// Defaults to the feed, or else the title, made into an unused id.
    #[serde(default)]
    pub id: Option<String>,
    /// Defaults to the source's.
    #[serde(default)]
    pub stale_after_seconds: Option<u32>,
}

/// `PATCH /v1/admin/pages/{id}`. Changing the source drops what was stored and resets the stale
/// time to the new source's, unless one is given too.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PageChange {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub source: Option<Source>,
    #[serde(default)]
    pub stale_after_seconds: Option<u32>,
}

/// `PUT /v1/admin/pages/{id}/content`: what a manual page shows, checked against its kind's rules
/// and the page size cap. It is stored as `manual`, updated now.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PageContentInput {
    /// The page's `data`. Absent or `null` means nothing to show yet.
    #[serde(default)]
    pub data: Option<serde_json::Value>,
    #[serde(default)]
    pub note: Option<String>,
}

/// `PUT /v1/admin/pages/order`: every page id, in the new order.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Order {
    pub ids: Vec<String>,
}

/// `GET` and `PUT /v1/admin/devices/{id}/pages`: the pages a display shows (its ticks). Displays
/// share one page order, so this is a set; the hub answers in page order.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DisplayPages {
    pub ids: Vec<String>,
}

// Usage subscriptions, as the manager shows them: meter pages whose feed is the service. These are
// how clients see usage pages, not bodies the hub takes.

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
    pub fn from_page(page: &StoredPage) -> Option<Self> {
        let (s, data) = page.to_subscription()?;
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
            kind: hub_protocol::shapes::KIND_METER.into(),
            source: self.source,
            feed: Some(self.service.slug().into()),
            id: self.id.clone(),
            stale_after_seconds: None,
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
    /// The figures as a meter page's content. Shares are rounded to one decimal place; one out of
    /// range is passed on as it is, for the hub to refuse with its reason.
    pub fn to_content(&self) -> PageContentInput {
        let items: Vec<hub_protocol::MeterItem> = self
            .metrics
            .iter()
            .map(|m| {
                hub_protocol::MeterItem::new(
                    m.window.id(),
                    m.window.label(),
                    m.used_percent
                        .map(|p| crate::model::clean_percent(p).unwrap_or(p)),
                    m.resets_at,
                )
            })
            .collect();
        PageContentInput {
            data: Some(serde_json::to_value(hub_protocol::Meter { items }).expect("meters always serialise")),
            note: self.note.clone(),
        }
    }
}

/// One allowance figure, as typed in or reported by a PC helper.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ManualMetric {
    pub window: Window,
    pub used_percent: Option<f64>,
    pub resets_at: Option<Timestamp>,
}

/// One display, as the manager sees it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DisplayView {
    pub id: String,
    pub name: String,
    pub enrolled_at: Timestamp,
    pub last_seen_at: Option<Timestamp>,
    pub device: Option<hub_protocol::device::DeviceInfo>,
}

/// `POST /v1/admin/enrolments`: asks for a one-time secret to set up a display.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NewEnrolment {
    /// A name for the display, such as "Desk". Plain text.
    pub name: String,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct EnrolmentGrant {
    pub secret: String,
    pub expires_at: Timestamp,
    pub identity: HubIdentity,
}

impl std::fmt::Debug for EnrolmentGrant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EnrolmentGrant")
            .field("secret", &"[redacted]")
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

/// `POST /v1/admin/collectors`: invites a PC helper (collector) to report figures for some services.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NewCollector {
    /// A name for the helper, such as the PC's name. Plain text.
    pub label: String,
    /// The services it may report. Its figures go to every page of these services whose source is
    /// `collector`.
    pub services: Vec<Service>,
}

/// The one-time secret for `hubctl login`, valid for 10 minutes.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct CollectorInvite {
    pub secret: String,
    pub expires_at: Timestamp,
    pub identity: HubIdentity,
}

impl std::fmt::Debug for CollectorInvite {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CollectorInvite")
            .field("secret", &"[redacted]")
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

/// `POST /v1/collector/pair`: exchanges the invite's secret for a collector token.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct CollectorPairRequest {
    pub secret: String,
}

impl std::fmt::Debug for CollectorPairRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CollectorPairRequest")
            .field("secret", &"[redacted]")
            .finish()
    }
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct CollectorPairResponse {
    pub collector_id: String,
    pub collector_token: String,
    pub services: Vec<Service>,
}

impl std::fmt::Debug for CollectorPairResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CollectorPairResponse")
            .field("collector_id", &self.collector_id)
            .field("collector_token", &"[redacted]")
            .field("services", &self.services)
            .finish()
    }
}

/// `POST /v1/collector/usage`: figures a provider's own tool reported on the PC.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UsageReport {
    pub service: Service,
    /// When the tool reported them, by the PC's clock.
    pub collected_at: Timestamp,
    pub metrics: Vec<ManualMetric>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UsageAccepted {
    /// The pages that now show these figures. Empty when no page of this service uses a collector.
    pub updated: Vec<String>,
}

/// One PC helper, as the manager sees it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CollectorView {
    pub id: String,
    pub label: String,
    pub created_at: Timestamp,
    pub revoked: bool,
    pub services: Vec<Service>,
}
