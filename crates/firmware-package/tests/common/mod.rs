//! Helpers shared by the integration tests. Every key is made at test time and never kept.

#![allow(dead_code)]

use firmware_package::header::{signed_bytes, signed_prefix};
use firmware_package::limits::*;
use firmware_package::{build_and_sign, Built, PublicKey, Release, SigningKey, UsbLayout, UsbPartFile};
use serde_json::{json, Value};

pub fn key() -> SigningKey {
    SigningKey::generate().unwrap().0
}

pub const DEVICE_TYPE: &str = "llm-usage-display";
pub const HARDWARE: &str = "waveshare-amoled-1.8-v2";

pub fn release() -> Release {
    Release {
        device_type: DEVICE_TYPE.into(),
        hardware_revisions: vec!["waveshare-amoled-1.8-v1".into(), HARDWARE.into()],
        firmware_version: "1.2.0".into(),
        security_counter: 3,
        summary: Some("Fixes the clock drifting after a long outage.".into()),
        key_set: None,
        usb_install: None,
    }
}

pub fn image() -> Vec<u8> {
    (0..5000u32).map(|i| (i * 7 + 3) as u8).collect()
}

pub fn with_usb(mut r: Release) -> Release {
    r.usb_install = Some(UsbLayout {
        chip: "esp32s3".into(),
        min_flash_bytes: 16 * 1024 * 1024,
        app_offset: 65536,
        parts: vec![
            UsbPartFile {
                name: "bootloader".into(),
                offset: 0,
                bytes: vec![0xb0; 2100],
            },
            UsbPartFile {
                name: "partition-table".into(),
                offset: 32768,
                bytes: vec![0x7a; 3072],
            },
        ],
    });
    r
}

pub fn build(key: &SigningKey, r: &Release) -> Built {
    build_and_sign(key, r, &image()).unwrap()
}

pub fn sha256_hex(b: &[u8]) -> String {
    firmware_package::hex::encode(ring::digest::digest(&ring::digest::SHA256, b).as_ref())
}

/// A valid manifest for [`image`], as JSON, for tests to change.
pub fn manifest_value() -> Value {
    let img = image();
    json!({
        "device_type": DEVICE_TYPE,
        "hardware_revisions": ["waveshare-amoled-1.8-v1", HARDWARE],
        "firmware_version": "1.2.0",
        "security_counter": 3,
        "image_size": img.len(),
        "image_sha256": sha256_hex(&img),
    })
}

/// A manifest with `usb_install` for [`image`] and the given parts.
pub fn usb_manifest_value(parts: &[(&str, u64, &[u8])]) -> Value {
    let mut v = manifest_value();
    v["usb_install"] = json!({
        "chip": "esp32s3",
        "min_flash_bytes": 16 * 1024 * 1024,
        "app_offset": 65536,
        "parts": parts.iter().map(|(name, offset, bytes)| json!({
            "name": name, "offset": offset, "size": bytes.len(), "sha256": sha256_hex(bytes),
        })).collect::<Vec<_>>(),
    });
    v
}

/// The signed bytes for any manifest text, signed correctly, so the tests can reach every check
/// after the signature.
pub fn package(key: &SigningKey, manifest: &[u8], image: &[u8], parts: &[&[u8]]) -> Vec<u8> {
    package_signed_as(key, &key.public_key(), manifest, image, parts)
}

/// As [`package`], but the header names `named` while `key` signs.
pub fn package_signed_as(
    key: &SigningKey,
    named: &PublicKey,
    manifest: &[u8],
    image: &[u8],
    parts: &[&[u8]],
) -> Vec<u8> {
    let manifest_len = manifest.len() as u16;
    let prefix = signed_prefix(manifest_len, named);
    let signature = key.sign(&signed_bytes(&prefix, manifest)).unwrap();
    let mut out = prefix.to_vec();
    out.extend_from_slice(&signature);
    out.extend_from_slice(manifest);
    out.extend_from_slice(image);
    for p in parts {
        out.extend_from_slice(p);
    }
    assert_eq!(out[..HEADER_LEN].len(), HEADER_LEN);
    out
}

pub fn json_bytes(v: &Value) -> Vec<u8> {
    serde_json::to_vec(v).unwrap()
}
