//! Requests a display makes besides polling: enrolling once, and an optional heartbeat.

use serde::{Deserialize, Serialize};

use crate::text::check_text;

/// Short free-text fields a display reports about itself.
pub const MAX_DEVICE_FIELD_LEN: usize = 48;

/// `POST /v1/device/enrol`. The secret comes from the provisioning bundle and works once.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct EnrolRequest {
    /// The enrolment secret, as written in the provisioning bundle.
    pub enrolment_secret: String,
    pub device: DeviceInfo,
}

impl std::fmt::Debug for EnrolRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EnrolRequest")
            .field("enrolment_secret", &"[redacted]")
            .field("device", &self.device)
            .finish()
    }
}

/// What a display says about itself, at enrolment and in heartbeats. Nothing personal.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceInfo {
    /// For example `llm-usage-display`.
    pub device_type: String,
    /// For example `1.0.0`, or `0.0.0-simulator`.
    pub firmware_version: String,
    /// For example `waveshare-amoled-1.8-v2`, or `simulated`.
    pub hardware_revision: String,
    /// The protocol version the firmware speaks.
    pub protocol_version: u32,
}

impl DeviceInfo {
    /// Problems with the reported fields, if any.
    pub fn problems(&self) -> Vec<String> {
        let mut out = Vec::new();
        for (name, value) in [
            ("device_type", &self.device_type),
            ("firmware_version", &self.firmware_version),
            ("hardware_revision", &self.hardware_revision),
        ] {
            if check_text(value, MAX_DEVICE_FIELD_LEN).is_err() {
                out.push(format!(
                    "{name} must be 1-{MAX_DEVICE_FIELD_LEN} characters of printable text"
                ));
            }
        }
        out
    }
}

/// The answer to a successful enrolment. The token is the display's only credential: store it in
/// encrypted flash and never log it.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct EnrolResponse {
    pub device_id: String,
    pub device_token: String,
}

impl std::fmt::Debug for EnrolResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EnrolResponse")
            .field("device_id", &self.device_id)
            .field("device_token", &"[redacted]")
            .finish()
    }
}

/// `POST /v1/device/heartbeat`. Optional; at most one a minute is recorded.
pub type Heartbeat = DeviceInfo;
