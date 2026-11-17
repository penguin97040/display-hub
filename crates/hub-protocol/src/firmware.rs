//! Signed firmware updates through the hub: what a display reports about its trusted keys, the offer
//! in its snapshot, the report it sends back, and the firmware section of a provisioning bundle.
//! `docs/firmware-updates.md` ("Protocol additions") is the specification.
//!
//! Everything here is an addition to protocol v1. Every new member of an existing message is optional
//! and left out when absent, so older displays and hubs read the same messages as before; new values
//! of the report's `state` and `reason` read as [`ReportState::Unrecognised`] and
//! [`ReportReason::Unrecognised`].

use serde::{Deserialize, Serialize};

use crate::device::MAX_DEVICE_FIELD_LEN;
use crate::limits::{MAX_FIRMWARE_UPDATE_BYTES, MAX_TRUSTED_KEYS};
use crate::text::check_text;

/// Hex digits in a key id (the SHA-256 of a public key) and in a package id (of a whole package).
pub const KEY_ID_HEX_LEN: usize = 64;
pub const PACKAGE_ID_HEX_LEN: usize = 64;
/// Hex digits in an offer id: 128 random bits.
pub const OFFER_ID_HEX_LEN: usize = 32;
/// Hex digits in a public key: the 65 bytes of an uncompressed P-256 point, starting `04`.
pub const PUBLIC_KEY_HEX_LEN: usize = 130;

/// Whether `s` is exactly `len` lower-case hex digits.
pub fn is_lower_hex(s: &str, len: usize) -> bool {
    s.len() == len
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The `firmware` member of a display's `device` object (enrolment, heartbeat and USB `hello`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FirmwareInfo {
    /// The key ids of the display's trusted keys, 0 to 3.
    pub key_ids: Vec<String>,
    /// The highest security counter the display has confirmed.
    pub security_floor: u32,
    /// The size of the display's spare app partition, in bytes.
    pub slot_bytes: u64,
}

impl FirmwareInfo {
    /// Problems with what the display reported, if any.
    pub fn problems(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.key_ids.len() > MAX_TRUSTED_KEYS {
            out.push(format!(
                "firmware.key_ids holds at most {MAX_TRUSTED_KEYS} key ids"
            ));
        }
        if !self.key_ids.iter().all(|k| is_lower_hex(k, KEY_ID_HEX_LEN)) {
            out.push(format!(
                "each of firmware.key_ids must be {KEY_ID_HEX_LEN} lower-case hex digits"
            ));
        }
        if has_repeats(&self.key_ids) {
            out.push("firmware.key_ids must not repeat a key id".into());
        }
        out
    }
}

/// The `firmware_update` member of a snapshot: present only while the display has an open offer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FirmwareUpdate {
    pub offer_id: String,
    pub package_id: String,
    /// The length of the update form: what `GET /v1/device/firmware/{package_id}` will send.
    pub size: u64,
}

impl FirmwareUpdate {
    /// Problems with an offer a hub is about to send, if any.
    pub fn problems(&self) -> Vec<String> {
        let mut out = Vec::new();
        if !is_lower_hex(&self.offer_id, OFFER_ID_HEX_LEN) {
            out.push(format!(
                "offer_id must be {OFFER_ID_HEX_LEN} lower-case hex digits"
            ));
        }
        if !is_lower_hex(&self.package_id, PACKAGE_ID_HEX_LEN) {
            out.push(format!(
                "package_id must be {PACKAGE_ID_HEX_LEN} lower-case hex digits"
            ));
        }
        if self.size == 0 || self.size > MAX_FIRMWARE_UPDATE_BYTES {
            out.push(format!("size must be 1-{MAX_FIRMWARE_UPDATE_BYTES}"));
        }
        out
    }
}

/// `POST /v1/device/firmware/report`: what became of an offer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FirmwareReport {
    pub offer_id: String,
    pub state: ReportState,
    /// Why, for `refused` and `failed`. Absent otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<ReportReason>,
    /// What the display is running as it sends the report.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub firmware_version: Option<String>,
}

impl FirmwareReport {
    /// Problems with the report's form, if any. A state or reason from a newer revision is not a
    /// problem: the hub records it as a failure.
    pub fn problems(&self) -> Vec<String> {
        let mut out = Vec::new();
        if !is_lower_hex(&self.offer_id, OFFER_ID_HEX_LEN) {
            out.push(format!(
                "offer_id must be {OFFER_ID_HEX_LEN} lower-case hex digits"
            ));
        }
        if let Some(v) = &self.firmware_version {
            if check_text(v, MAX_DEVICE_FIELD_LEN).is_err() {
                out.push(format!(
                    "firmware_version must be 1-{MAX_DEVICE_FIELD_LEN} characters of printable text"
                ));
            }
        }
        out
    }
}

/// The `state` of a report.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReportState {
    /// The manifest passed every check and the image is arriving.
    Downloading,
    /// The new firmware started, reached the hub and was marked good.
    Installed,
    /// The display would not install it. Nothing was written.
    Refused,
    /// Something went wrong after the checks. The old firmware is still running.
    Failed,
    /// The new firmware didn't prove itself, and the display went back.
    RolledBack,
    /// A state from a newer revision. The hub treats it as a failure. A display never sends it.
    #[serde(other)]
    Unrecognised,
}

impl ReportState {
    pub fn as_str(self) -> &'static str {
        match self {
            ReportState::Downloading => "downloading",
            ReportState::Installed => "installed",
            ReportState::Refused => "refused",
            ReportState::Failed => "failed",
            ReportState::RolledBack => "rolled_back",
            ReportState::Unrecognised => "unrecognised",
        }
    }

    /// Whether the offer is settled by this report.
    pub fn is_final(self) -> bool {
        !matches!(self, ReportState::Downloading)
    }
}

/// The `reason` of a `refused` or `failed` report.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReportReason {
    NoTrustedKeys,
    TooLarge,
    BadFormat,
    UntrustedKey,
    BadSignature,
    WrongDeviceType,
    WrongHardware,
    Downgrade,
    Download,
    ImageHash,
    Flash,
    /// A reason from a newer revision. The hub treats it as a failure. A display never sends it.
    #[serde(other)]
    Unrecognised,
}

impl ReportReason {
    pub const ALL: [ReportReason; 11] = [
        ReportReason::NoTrustedKeys,
        ReportReason::TooLarge,
        ReportReason::BadFormat,
        ReportReason::UntrustedKey,
        ReportReason::BadSignature,
        ReportReason::WrongDeviceType,
        ReportReason::WrongHardware,
        ReportReason::Downgrade,
        ReportReason::Download,
        ReportReason::ImageHash,
        ReportReason::Flash,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            ReportReason::NoTrustedKeys => "no_trusted_keys",
            ReportReason::TooLarge => "too_large",
            ReportReason::BadFormat => "bad_format",
            ReportReason::UntrustedKey => "untrusted_key",
            ReportReason::BadSignature => "bad_signature",
            ReportReason::WrongDeviceType => "wrong_device_type",
            ReportReason::WrongHardware => "wrong_hardware",
            ReportReason::Downgrade => "downgrade",
            ReportReason::Download => "download",
            ReportReason::ImageHash => "image_hash",
            ReportReason::Flash => "flash",
            ReportReason::Unrecognised => "unrecognised",
        }
    }

    /// Whether a display sends this reason with `refused` (nothing written) rather than `failed`.
    pub fn is_refusal(self) -> bool {
        !matches!(
            self,
            ReportReason::Download
                | ReportReason::ImageHash
                | ReportReason::Flash
                | ReportReason::Unrecognised
        )
    }
}

/// The `firmware` member of a provisioning bundle: the keys a display with none should trust, and
/// its starting security floor. A display that already has keys accepts it only if it names the
/// same set (or the member is absent); otherwise it answers `firmware_keys_locked`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BundleFirmware {
    /// 1 to 3 public keys, as 130 lower-case hex digits each, no repeats.
    pub trusted_keys: Vec<String>,
    pub security_counter: u32,
}

impl BundleFirmware {
    pub fn problems(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.trusted_keys.is_empty() || self.trusted_keys.len() > MAX_TRUSTED_KEYS {
            out.push(format!("firmware.trusted_keys holds 1-{MAX_TRUSTED_KEYS} keys"));
        }
        if !self
            .trusted_keys
            .iter()
            .all(|k| is_lower_hex(k, PUBLIC_KEY_HEX_LEN) && k.starts_with("04"))
        {
            out.push(format!(
                "each of firmware.trusted_keys must be {PUBLIC_KEY_HEX_LEN} lower-case hex digits starting 04"
            ));
        }
        if has_repeats(&self.trusted_keys) {
            out.push("firmware.trusted_keys must not repeat a key".into());
        }
        out
    }
}

fn has_repeats(items: &[String]) -> bool {
    let mut seen = std::collections::HashSet::new();
    !items.iter().all(|i| seen.insert(i))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_states_and_reasons_read_as_unrecognised() {
        let r: FirmwareReport = serde_json::from_str(
            r#"{"offer_id":"00112233445566778899aabbccddeeff","state":"exploded","reason":"gremlins","extra":1}"#,
        )
        .unwrap();
        assert_eq!(r.state, ReportState::Unrecognised);
        assert_eq!(r.reason, Some(ReportReason::Unrecognised));
        assert!(r.problems().is_empty());
        for reason in ReportReason::ALL {
            let back: ReportReason = serde_json::from_value(serde_json::json!(reason.as_str())).unwrap();
            assert_eq!(back, reason);
        }
    }

    #[test]
    fn bad_forms_are_reported() {
        let info = FirmwareInfo {
            key_ids: vec!["A".repeat(64), "a".repeat(64), "a".repeat(64), "b".repeat(64)],
            security_floor: 0,
            slot_bytes: 0,
        };
        assert_eq!(info.problems().len(), 3);
        let offer = FirmwareUpdate {
            offer_id: "x".into(),
            package_id: "0".repeat(64),
            size: MAX_FIRMWARE_UPDATE_BYTES + 1,
        };
        assert_eq!(offer.problems().len(), 2);
        let keys = BundleFirmware {
            trusted_keys: vec![format!("05{}", "0".repeat(128))],
            security_counter: 0,
        };
        assert_eq!(keys.problems().len(), 1);
        assert_eq!(
            BundleFirmware {
                trusted_keys: vec![],
                security_counter: 0
            }
            .problems()
            .len(),
            1
        );
    }
}
