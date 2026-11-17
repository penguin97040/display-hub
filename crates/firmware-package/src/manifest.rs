//! The manifest: a small JSON object, covered by the signature, that describes the image.
//!
//! [`Manifest::parse`] is the strict reading the hub, the app and the signing tool use. A display
//! ignores `usb_install`, so [`Manifest::parse_for_display`] checks everything else the same way and
//! leaves that member's contents alone (the JSON rules still apply to it, as to any member).

use std::collections::BTreeSet;

use hub_protocol::text::{check_text, TextProblem};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use crate::hex;
use crate::json::{parse_object, JsonProblem};
use crate::key::PublicKey;
use crate::limits::*;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Manifest {
    pub device_type: String,
    pub hardware_revisions: Vec<String>,
    pub firmware_version: String,
    pub security_counter: u32,
    pub image_size: u64,
    pub image_sha256: [u8; SHA256_LEN],
    pub summary: Option<String>,
    /// The complete set of keys displays should trust once this release is installed and good.
    pub key_set: Option<Vec<PublicKey>>,
    /// Always `None` from [`Manifest::parse_for_display`].
    pub usb_install: Option<UsbInstall>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UsbInstall {
    pub chip: String,
    pub min_flash_bytes: u64,
    pub app_offset: u64,
    /// In the order they follow the image in the package.
    pub parts: Vec<UsbPart>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UsbPart {
    pub name: String,
    pub offset: u64,
    pub size: u64,
    pub sha256: [u8; SHA256_LEN],
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ManifestError {
    #[error("the manifest isn't acceptable: {0}")]
    Json(JsonProblem),
    /// A required member is missing, or a member has the wrong JSON type. The text is serde's.
    #[error("the manifest has a missing or mistyped member: {0}")]
    Shape(String),
    #[error("the manifest's {field} {problem}")]
    Invalid { field: String, problem: String },
}

fn invalid(field: impl Into<String>, problem: impl Into<String>) -> ManifestError {
    ManifestError::Invalid {
        field: field.into(),
        problem: problem.into(),
    }
}

/// An optional member that, when present, must have its proper type: `null` is refused rather than
/// read as "absent".
fn present<'de, D: Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Option<T>, D::Error> {
    T::deserialize(d).map(Some)
}

// Unknown members are ignored (serde's default), so the format can grow.
#[derive(Deserialize)]
struct RawManifest {
    device_type: String,
    hardware_revisions: Vec<String>,
    firmware_version: String,
    security_counter: u64,
    image_size: u64,
    image_sha256: String,
    #[serde(default, deserialize_with = "present")]
    summary: Option<String>,
    #[serde(default, deserialize_with = "present")]
    key_set: Option<Vec<String>>,
    #[serde(default, deserialize_with = "present")]
    usb_install: Option<Value>,
}

#[derive(Deserialize)]
struct RawUsbInstall {
    chip: String,
    min_flash_bytes: u64,
    app_offset: u64,
    parts: Vec<RawUsbPart>,
}

#[derive(Deserialize)]
struct RawUsbPart {
    name: String,
    offset: u64,
    size: u64,
    sha256: String,
}

impl Manifest {
    /// The strict reading, for the hub, the app and the signing tool.
    pub fn parse(bytes: &[u8]) -> Result<Self, ManifestError> {
        Self::read(bytes, true)
    }

    /// A display's reading: every rule except `usb_install`, whose contents it ignores.
    pub fn parse_for_display(bytes: &[u8]) -> Result<Self, ManifestError> {
        Self::read(bytes, false)
    }

    fn read(bytes: &[u8], with_usb: bool) -> Result<Self, ManifestError> {
        if bytes.len() > MAX_MANIFEST_LEN {
            return Err(invalid(
                "length",
                format!("is {} bytes, over the limit of {MAX_MANIFEST_LEN}", bytes.len()),
            ));
        }
        let object = parse_object(bytes, MAX_JSON_DEPTH).map_err(ManifestError::Json)?;
        let raw: RawManifest =
            serde_json::from_value(Value::Object(object)).map_err(|e| ManifestError::Shape(e.to_string()))?;

        text("device_type", &raw.device_type, MAX_DEVICE_TYPE_LEN)?;
        hardware_revisions(&raw.hardware_revisions)?;
        check_firmware_version(&raw.firmware_version).map_err(|p| invalid("firmware_version", p))?;
        let security_counter = u32::try_from(raw.security_counter)
            .map_err(|_| invalid("security_counter", format!("is above {}", u32::MAX)))?;
        if !(MIN_IMAGE_SIZE..=MAX_IMAGE_SIZE).contains(&raw.image_size) {
            return Err(invalid(
                "image_size",
                format!("must be from {MIN_IMAGE_SIZE} to {MAX_IMAGE_SIZE}"),
            ));
        }
        let image_sha256 = digest("image_sha256", &raw.image_sha256)?;
        if let Some(s) = &raw.summary {
            text("summary", s, MAX_SUMMARY_LEN)?;
        }
        let key_set = raw.key_set.as_deref().map(key_set).transpose()?;
        let usb_install = match raw.usb_install {
            Some(v) if with_usb => Some(usb_install(v)?),
            _ => None,
        };
        Ok(Self {
            device_type: raw.device_type,
            hardware_revisions: raw.hardware_revisions,
            firmware_version: raw.firmware_version,
            security_counter,
            image_size: raw.image_size,
            image_sha256,
            summary: raw.summary,
            key_set,
            usb_install,
        })
    }

    /// The total length of the USB-install parts, zero without `usb_install`. Parts are bounded when
    /// parsed, so this can't overflow.
    pub fn parts_len(&self) -> u64 {
        self.usb_install
            .as_ref()
            .map_or(0, |u| u.parts.iter().map(|p| p.size).sum())
    }

    /// Compact JSON with the members in the document's order and absent options left out. Used by
    /// [`crate::build_and_sign`]; nothing ever re-serialises a manifest to check a signature.
    pub fn to_json(&self) -> Vec<u8> {
        #[derive(Serialize)]
        struct Out<'a> {
            device_type: &'a str,
            hardware_revisions: &'a [String],
            firmware_version: &'a str,
            security_counter: u32,
            image_size: u64,
            image_sha256: String,
            #[serde(skip_serializing_if = "Option::is_none")]
            summary: Option<&'a str>,
            #[serde(skip_serializing_if = "Option::is_none")]
            key_set: Option<Vec<String>>,
            #[serde(skip_serializing_if = "Option::is_none")]
            usb_install: Option<UsbOut<'a>>,
        }
        #[derive(Serialize)]
        struct UsbOut<'a> {
            chip: &'a str,
            min_flash_bytes: u64,
            app_offset: u64,
            parts: Vec<PartOut<'a>>,
        }
        #[derive(Serialize)]
        struct PartOut<'a> {
            name: &'a str,
            offset: u64,
            size: u64,
            sha256: String,
        }
        let out = Out {
            device_type: &self.device_type,
            hardware_revisions: &self.hardware_revisions,
            firmware_version: &self.firmware_version,
            security_counter: self.security_counter,
            image_size: self.image_size,
            image_sha256: hex::encode(&self.image_sha256),
            summary: self.summary.as_deref(),
            key_set: self
                .key_set
                .as_ref()
                .map(|ks| ks.iter().map(PublicKey::to_hex).collect()),
            usb_install: self.usb_install.as_ref().map(|u| UsbOut {
                chip: &u.chip,
                min_flash_bytes: u.min_flash_bytes,
                app_offset: u.app_offset,
                parts: u
                    .parts
                    .iter()
                    .map(|p| PartOut {
                        name: &p.name,
                        offset: p.offset,
                        size: p.size,
                        sha256: hex::encode(&p.sha256),
                    })
                    .collect(),
            }),
        };
        serde_json::to_vec(&out).expect("a manifest always serialises")
    }
}

fn text(field: &str, s: &str, max: usize) -> Result<(), ManifestError> {
    check_text(s, max).map_err(|p| {
        invalid(
            field,
            match p {
                TextProblem::Empty => "is empty".to_string(),
                TextProblem::TooLong => format!("is longer than {max} characters"),
                TextProblem::ControlCharacter => {
                    "contains a control or invisible formatting character".into()
                }
                TextProblem::NotNormalised => {
                    "isn't normalised (it must be NFC, with no space at either end)".into()
                }
            },
        )
    })
}

fn hardware_revisions(list: &[String]) -> Result<(), ManifestError> {
    if list.is_empty() || list.len() > MAX_HARDWARE_REVISIONS {
        return Err(invalid(
            "hardware_revisions",
            format!("must list 1 to {MAX_HARDWARE_REVISIONS} revisions"),
        ));
    }
    let mut seen = BTreeSet::new();
    for r in list {
        text("hardware_revisions", r, MAX_HARDWARE_REVISION_LEN)?;
        if !seen.insert(r.as_str()) {
            return Err(invalid("hardware_revisions", format!("lists {r:?} twice")));
        }
    }
    Ok(())
}

/// `MAJOR.MINOR.PATCH`, optionally `-` and a pre-release tag of ASCII letters, digits, `.` and `-`,
/// at most [`MAX_FIRMWARE_VERSION_LEN`] characters. Each number is plain decimal with no leading zero
/// (as in Semantic Versioning) and fits in 32 bits, so every reader compares versions the same way.
pub fn check_firmware_version(s: &str) -> Result<(), String> {
    const SHAPE: &str = "must look like 1.2.0 or 1.3.0-beta.1";
    if s.len() > MAX_FIRMWARE_VERSION_LEN {
        return Err(format!("is longer than {MAX_FIRMWARE_VERSION_LEN} characters"));
    }
    let (core, pre) = match s.split_once('-') {
        Some((core, pre)) => (core, Some(pre)),
        None => (s, None),
    };
    let numbers: Vec<&str> = core.split('.').collect();
    if numbers.len() != 3 {
        return Err(SHAPE.into());
    }
    for n in numbers {
        let digits = !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit());
        if !digits || (n.len() > 1 && n.starts_with('0')) || n.parse::<u32>().is_err() {
            return Err(format!("{SHAPE}, with each number written plainly"));
        }
    }
    if let Some(pre) = pre {
        if pre.is_empty()
            || !pre
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
        {
            return Err(format!(
                "{SHAPE}: a pre-release tag is letters, digits, '.' and '-'"
            ));
        }
    }
    Ok(())
}

fn digest(field: &str, s: &str) -> Result<[u8; SHA256_LEN], ManifestError> {
    hex::decode_exact::<SHA256_LEN>(s)
        .ok_or_else(|| invalid(field, format!("must be {SHA256_HEX_LEN} lower-case hex digits")))
}

fn key_set(list: &[String]) -> Result<Vec<PublicKey>, ManifestError> {
    if list.is_empty() || list.len() > MAX_KEY_SET {
        return Err(invalid("key_set", format!("must list 1 to {MAX_KEY_SET} keys")));
    }
    let mut keys: Vec<PublicKey> = Vec::with_capacity(list.len());
    for s in list {
        let key = PublicKey::from_hex(s).ok_or_else(|| {
            invalid(
                "key_set",
                format!("holds a key that isn't {PUBLIC_KEY_HEX_LEN} lower-case hex digits"),
            )
        })?;
        if !key.is_valid_point() {
            return Err(invalid(
                "key_set",
                "holds a key that isn't a valid P-256 public key",
            ));
        }
        if keys.contains(&key) {
            return Err(invalid("key_set", "lists a key twice"));
        }
        keys.push(key);
    }
    Ok(keys)
}

fn usb_install(v: Value) -> Result<UsbInstall, ManifestError> {
    let raw: RawUsbInstall =
        serde_json::from_value(v).map_err(|e| ManifestError::Shape(format!("usb_install: {e}")))?;
    let chip_ok = (1..=MAX_CHIP_NAME_LEN).contains(&raw.chip.len())
        && raw.chip.starts_with(|c: char| c.is_ascii_lowercase())
        && raw
            .chip
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
    if !chip_ok {
        return Err(invalid(
            "usb_install.chip",
            format!("must be a lower-case chip name like esp32s3, up to {MAX_CHIP_NAME_LEN} characters"),
        ));
    }
    if !(1..=MAX_FLASH_BYTES).contains(&raw.min_flash_bytes) {
        return Err(invalid(
            "usb_install.min_flash_bytes",
            format!("must be from 1 to {MAX_FLASH_BYTES}"),
        ));
    }
    if !raw.app_offset.is_multiple_of(FLASH_ALIGN) {
        return Err(invalid(
            "usb_install.app_offset",
            format!("must be a multiple of {FLASH_ALIGN}"),
        ));
    }
    if raw.parts.is_empty() || raw.parts.len() > MAX_USB_PARTS {
        return Err(invalid(
            "usb_install.parts",
            format!("must list 1 to {MAX_USB_PARTS} parts"),
        ));
    }
    let mut names = BTreeSet::new();
    let mut parts = Vec::with_capacity(raw.parts.len());
    for p in raw.parts {
        let name_ok = (1..=MAX_PART_NAME_LEN).contains(&p.name.len())
            && p.name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
        if !name_ok {
            return Err(invalid(
                "usb_install.parts",
                format!("has a name that isn't 1 to {MAX_PART_NAME_LEN} lower-case letters, digits and '-'"),
            ));
        }
        let field = format!("usb_install.parts {:?}", p.name);
        if !names.insert(p.name.clone()) {
            return Err(invalid(field, "appears twice"));
        }
        if !p.offset.is_multiple_of(FLASH_ALIGN) {
            return Err(invalid(
                field,
                format!("offset must be a multiple of {FLASH_ALIGN}"),
            ));
        }
        // No part can be longer than the whole package, which also keeps every sum in range.
        if !(1..=MAX_PACKAGE_LEN).contains(&p.size) {
            return Err(invalid(
                field,
                format!("size must be from 1 to {MAX_PACKAGE_LEN}"),
            ));
        }
        let sha256 = digest(&format!("{field} sha256"), &p.sha256)?;
        parts.push(UsbPart {
            name: p.name,
            offset: p.offset,
            size: p.size,
            sha256,
        });
    }
    Ok(UsbInstall {
        chip: raw.chip,
        min_flash_bytes: raw.min_flash_bytes,
        app_offset: raw.app_offset,
        parts,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn firmware_versions() {
        for ok in [
            "1.2.0",
            "0.0.0",
            "1.3.0-beta.1",
            "10.20.30-rc-2",
            "4294967295.0.0",
            "1.0.0-A.b",
        ] {
            assert_eq!(check_firmware_version(ok), Ok(()), "{ok}");
        }
        let long = format!("1.0.0-{}", "a".repeat(MAX_FIRMWARE_VERSION_LEN - 5));
        assert_eq!(long.len(), MAX_FIRMWARE_VERSION_LEN + 1);
        assert!(check_firmware_version(&long[..MAX_FIRMWARE_VERSION_LEN]).is_ok());
        for bad in [
            "",
            "1",
            "1.2",
            "1.2.3.4",
            "v1.2.0",
            "01.2.0",
            "1.02.0",
            "1.2.x",
            "1.2.0-",
            "1.2.0-beta_1",
            "1.2.0+build",
            "1..0",
            "4294967296.0.0",
            "1.2.0-bêta",
            " 1.2.0",
            long.as_str(),
        ] {
            assert!(check_firmware_version(bad).is_err(), "{bad}");
        }
    }
}
