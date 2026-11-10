//! Display Hub device protocol, version 1.
//!
//! This crate is the contract between the hub and its displays, and has no I/O. `docs/protocol-v1.md`
//! describes it for firmware authors, and `protocol/` holds the JSON Schema and example messages that
//! the tests here check against.

pub mod error;
pub mod limits;
pub mod snapshot;
pub mod text;
pub mod timestamp;

pub use error::{ErrorAction, ErrorBody, ErrorCode, ErrorDetail};
pub use snapshot::{receive, DataQuality, Metric, Page, ReceiveError, Snapshot, Violation};
pub use timestamp::Timestamp;

/// The wire protocol version this crate speaks.
pub const PROTOCOL_VERSION: u32 = 1;
/// The oldest protocol version a hub still serves. Raise it only with a firmware release plan.
pub const MIN_SUPPORTED_PROTOCOL_VERSION: u32 = 1;
/// Request header a display sends with its protocol version.
pub const PROTOCOL_HEADER: &str = "x-display-protocol";

/// `GET /v1/health`: operational health only. Never credentials or account details.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Health {
    pub status: String,
    pub protocol_versions: Vec<u32>,
}

impl Health {
    pub fn ok() -> Self {
        Self {
            status: "ok".into(),
            protocol_versions: (MIN_SUPPORTED_PROTOCOL_VERSION..=PROTOCOL_VERSION).collect(),
        }
    }
}
