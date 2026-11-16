//! Admin API bodies, shared by the hub and its clients. Admin endpoints need an admin token.

use hub_protocol::Timestamp;
use serde::{Deserialize, Serialize};

use crate::model::{Source, StoredPage};

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
    /// The named feed that fills the page ([`valid_feed`](crate::model::valid_feed)). A page whose
    /// source is `helper` needs one.
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

/// `PUT /v1/admin/pages/{id}/content`: what a manual or demo page shows, checked against its kind's
/// rules and the page size cap. It is stored as `manual` or `demo` (after the page's source),
/// updated now.
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

/// `POST /v1/admin/helpers`: invites a helper, such as the PC helper, to send pages to some feeds.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NewHelper {
    /// A name for the helper, such as the PC's name. Plain text.
    pub label: String,
    /// The feeds it may send to ([`valid_feed`](crate::model::valid_feed)). What it sends goes to
    /// every page of these feeds whose source is `helper`.
    pub feeds: Vec<String>,
}

/// The one-time secret for pairing a helper (`hub-send login` or `usage-helper login`), valid for
/// 10 minutes.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct HelperInvite {
    pub secret: String,
    pub expires_at: Timestamp,
    pub identity: HubIdentity,
}

impl std::fmt::Debug for HelperInvite {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HelperInvite")
            .field("secret", &"[redacted]")
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

/// `POST /v1/helper/pair`: exchanges the invite's secret for a helper token.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct HelperPairRequest {
    pub secret: String,
}

impl std::fmt::Debug for HelperPairRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HelperPairRequest")
            .field("secret", &"[redacted]")
            .finish()
    }
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct HelperPairResponse {
    pub helper_id: String,
    pub helper_token: String,
    /// The feeds it may send to.
    pub feeds: Vec<String>,
}

impl std::fmt::Debug for HelperPairResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HelperPairResponse")
            .field("helper_id", &self.helper_id)
            .field("helper_token", &"[redacted]")
            .field("feeds", &self.feeds)
            .finish()
    }
}

/// `PUT /v1/helper/feeds/{feed}`: a whole page body for every page of the feed whose source is
/// `helper` and whose kind is `kind`. The hub checks it with the same rules and size cap as any
/// page, and stores it as `provider_reported`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FeedContent {
    /// The kind the content is in. Pages of the feed that take another kind are skipped.
    pub kind: String,
    /// The page's `data`, in the shape `kind` names.
    pub data: serde_json::Value,
    /// When the helper collected it, by its own clock. Defaults to now; never later than the hub's
    /// clock, and refused if more than five minutes ahead of it.
    #[serde(default)]
    pub collected_at: Option<Timestamp>,
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FeedAccepted {
    /// The pages that now show this content. Empty when no page of the feed takes it from a helper.
    pub updated: Vec<String>,
    /// Pages of the feed that take content from a helper, but of another kind, left as they were.
    pub skipped: Vec<String>,
}

/// One helper, as the manager sees it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HelperView {
    pub id: String,
    pub label: String,
    pub created_at: Timestamp,
    pub revoked: bool,
    pub feeds: Vec<String>,
}
