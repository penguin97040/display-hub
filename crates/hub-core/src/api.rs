//! Admin API bodies, shared by the hub and its clients. Admin endpoints need an admin token.

use hub_protocol::Timestamp;
use serde::{Deserialize, Serialize};

use crate::model::{Settings, Source, StoredPage};

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
    /// The template the page is made from, as `name/version` such as `usage-limits/1`
    /// ([`valid_template_id`](crate::model::valid_template_id)). Only recorded: the hub doesn't
    /// read templates.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template: Option<String>,
    /// The answers to the template's settings form, kept with the page for its helper to read
    /// ([`settings_problem`](crate::model::settings_problem)). Not secret: never API keys or
    /// passwords. An empty object is the same as none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings: Option<Settings>,
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
    /// Replaces the page's settings; an empty object removes them. The template it was made from
    /// can't change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings: Option<Settings>,
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
    /// The ids of the pages the display shows (its ticks), in the shared page order. Left out by a
    /// hub that doesn't say, so the manager never reads that as "none".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pages: Option<Vec<String>>,
    /// Its firmware: what it reported, its offer, the best package for it and `auto_update`. Left
    /// out by a hub without signed updates.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub firmware: Option<DisplayFirmware>,
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

/// `POST /v1/admin/helpers`: invites a helper, such as the usage helper, to send pages to some feeds.
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

/// `GET /v1/helper/feeds/{feed}/settings`: the settings of every page of the feed whose source is
/// `helper`, in page order, so a helper can do what each page was set up for. Nothing else about the
/// pages is readable this way.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct FeedSettings {
    pub pages: Vec<PageSettings>,
}

/// One helper page's template and settings.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PageSettings {
    pub page_id: String,
    /// The template it was made from, if any.
    #[serde(default)]
    pub template: Option<String>,
    /// Its settings; empty when it has none.
    #[serde(default)]
    pub settings: Settings,
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

// ----- signed firmware updates (`docs/firmware-updates.md`) -----

/// One package the hub holds, as `GET /v1/admin/firmware` (and the publisher's list) shows it, and
/// as an upload answers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FirmwarePackageView {
    /// The SHA-256 of the whole file, as 64 lower-case hex digits. Only a name for the file.
    pub package_id: String,
    pub device_type: String,
    pub hardware_revisions: Vec<String>,
    pub firmware_version: String,
    pub security_counter: u32,
    #[serde(default)]
    pub summary: Option<String>,
    /// The key id of the key that signed it (64 hex digits), its fingerprint, and the owner's label
    /// for it if there is one. A label is only as trustworthy as the hub.
    pub signer_key_id: String,
    pub signer_fingerprint: String,
    #[serde(default)]
    pub signer_label: Option<String>,
    /// The whole file, and the part a display receives (without the USB-install parts).
    pub size: u64,
    pub update_size: u64,
    /// The app image alone: what a display compares with its spare space.
    pub image_size: u64,
    /// Whether it has what a first install over USB needs.
    pub usb_install: bool,
    /// The key ids (64 hex digits each) of the keys its manifest's `key_set` names: the keys a
    /// display trusts once it has installed it. `None` when it has no `key_set` (a display keeps
    /// the keys it has), or from a hub that doesn't say.
    #[serde(default)]
    pub key_set: Option<Vec<String>>,
    /// The hub couldn't read what it holds about this package (its `key_set`), so it never offers
    /// it, and a manager treats it as fit for no display. Left out when false, as from an older hub.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub damaged: bool,
    pub uploaded_at: Timestamp,
    /// `admin` or `publisher`, with that credential's id and label.
    pub uploaded_by_role: String,
    pub uploaded_by: String,
    #[serde(default)]
    pub uploaded_by_label: Option<String>,
    /// The displays it is on offer to now. Always empty in a publisher's list.
    #[serde(default)]
    pub offered_to: Vec<String>,
    /// Only in an upload's answer: the package the hub removed to make room for this one, if it
    /// removed one. Left out otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evicted: Option<EvictedPackage>,
}

/// A package removed from the shelf to make room for an upload.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvictedPackage {
    pub package_id: String,
    pub device_type: String,
    pub firmware_version: String,
}

/// `PUT /v1/admin/devices/{id}/firmware-update`: offer this package to the display (the **Update**
/// button).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FirmwareOfferRequest {
    pub package_id: String,
}

/// One offer and what became of it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FirmwareOfferView {
    pub offer_id: String,
    pub package_id: String,
    /// The version on offer.
    pub firmware_version: String,
    /// `offered` (waiting for the display to check in), `downloading`, `installed`, `refused`,
    /// `failed`, `rolled_back`, `cancelled` or `expired`.
    pub state: String,
    /// For `refused` and `failed`, the display's reason (`unrecognised` for one this hub doesn't
    /// know); for `cancelled`, why (`by_admin`, `replaced`, `package_deleted`, `display_removed`,
    /// `package_missing`); for `installed`, `seen_in_heartbeat` when the display's report was lost.
    #[serde(default)]
    pub reason: Option<String>,
    /// What the display said it was running in its last report about this offer.
    #[serde(default)]
    pub reported_version: Option<String>,
    pub made_at: Timestamp,
    pub expires_at: Timestamp,
    pub updated_at: Timestamp,
    #[serde(default)]
    pub ended_at: Option<Timestamp>,
}

/// A display's firmware, in `GET /v1/admin/devices`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisplayFirmware {
    /// What the display last reported: its trusted keys, security floor and spare partition.
    /// `None` for firmware made before signed updates, which can't take them.
    #[serde(default)]
    pub reported: Option<hub_protocol::firmware::FirmwareInfo>,
    /// Its open offer, or else its most recent one.
    #[serde(default)]
    pub offer: Option<FirmwareOfferView>,
    /// The newest package the hub holds that passes every check the hub can make for this display
    /// and is newer than what it runs, if any.
    #[serde(default)]
    pub available_package_id: Option<String>,
    /// Whether the hub may offer updates by itself. Always off for now.
    pub auto_update: bool,
}

/// The owner's label for a signing key.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignerView {
    pub key_id: String,
    pub fingerprint: String,
    pub label: String,
    pub updated_at: Timestamp,
}

/// `PUT /v1/admin/firmware/signers/{key_id}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignerLabel {
    /// Plain text, such as the maker's name.
    pub label: String,
}

/// `POST /v1/admin/publishers`: invites a firmware maker's `hub-firmware` tool to upload packages.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewPublisher {
    /// A name for the publisher, such as the maker or their computer. Plain text.
    pub label: String,
}

/// The one-time secret for pairing a publisher (`hub-firmware login`), valid for 10 minutes.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct PublisherInvite {
    pub secret: String,
    pub expires_at: Timestamp,
    pub identity: HubIdentity,
}

impl std::fmt::Debug for PublisherInvite {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PublisherInvite")
            .field("secret", &"[redacted]")
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

/// `POST /v1/publisher/pair`: exchanges the invite's secret for a publisher token.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct PublisherPairRequest {
    pub secret: String,
}

impl std::fmt::Debug for PublisherPairRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PublisherPairRequest")
            .field("secret", &"[redacted]")
            .finish()
    }
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct PublisherPairResponse {
    pub publisher_id: String,
    /// `dhp_…`: may upload and list packages, and nothing else.
    pub publisher_token: String,
}

impl std::fmt::Debug for PublisherPairResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PublisherPairResponse")
            .field("publisher_id", &self.publisher_id)
            .field("publisher_token", &"[redacted]")
            .finish()
    }
}

/// One publisher, as the app sees it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PublisherView {
    pub id: String,
    pub label: String,
    pub created_at: Timestamp,
    pub revoked: bool,
}
