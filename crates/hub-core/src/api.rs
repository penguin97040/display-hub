//! Admin API bodies, shared by the hub and its clients. Admin endpoints need an admin token.

use hub_protocol::Timestamp;
use serde::{Deserialize, Serialize};

use crate::model::{PageData, Service, Source, Window};

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
    pub subscriptions: usize,
    pub displays: usize,
    pub admins: usize,
}

/// One subscription as the manager sees it, with its stored figures.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SubscriptionView {
    pub id: String,
    pub service: Service,
    pub title: String,
    pub source: Source,
    pub data: Option<PageData>,
}

/// `POST /v1/admin/subscriptions`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NewSubscription {
    pub service: Service,
    pub source: Source,
    /// Defaults to the service's name.
    pub title: Option<String>,
    /// Defaults to the service's slug, made unique.
    pub id: Option<String>,
}

/// `PATCH /v1/admin/subscriptions/{id}`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SubscriptionChange {
    pub title: Option<String>,
    pub source: Option<Source>,
}

/// `PUT /v1/admin/subscriptions/order`: every subscription id, in the new order.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Order {
    pub ids: Vec<String>,
}

/// `PUT /v1/admin/subscriptions/{id}/manual`: figures typed in by the owner.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ManualFigures {
    pub metrics: Vec<ManualMetric>,
    pub note: Option<String>,
}

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
