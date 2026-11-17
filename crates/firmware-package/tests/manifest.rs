//! Each manifest field's rules and bounds, read the strict way and a display's way.

mod common;

use common::*;
use firmware_package::limits::*;
use firmware_package::{Manifest, ManifestError};
use serde_json::{json, Value};

fn parse(v: &Value) -> Result<Manifest, ManifestError> {
    Manifest::parse(&json_bytes(v))
}

fn with(field: &str, value: Value) -> Value {
    let mut v = manifest_value();
    v[field] = value;
    v
}

fn without(field: &str) -> Value {
    let mut v = manifest_value();
    v.as_object_mut().unwrap().remove(field);
    v
}

fn invalid(v: &Value) -> bool {
    matches!(parse(v), Err(ManifestError::Invalid { .. }))
}

fn shape(v: &Value) -> bool {
    matches!(parse(v), Err(ManifestError::Shape(_)))
}

#[test]
fn the_base_manifest_is_valid() {
    let m = parse(&manifest_value()).unwrap();
    assert_eq!(m.device_type, DEVICE_TYPE);
    assert_eq!(m.summary, None);
    assert_eq!(m.key_set, None);
    assert_eq!(m.usb_install, None);
}

#[test]
fn every_required_field_is_required() {
    for field in [
        "device_type",
        "hardware_revisions",
        "firmware_version",
        "security_counter",
        "image_size",
        "image_sha256",
    ] {
        assert!(shape(&without(field)), "{field}");
        assert!(shape(&with(field, Value::Null)), "{field} as null");
    }
    // Mistyped.
    assert!(shape(&with("security_counter", json!("3"))));
    assert!(shape(&with("image_size", json!("5000"))));
    assert!(shape(&with("device_type", json!(5))));
    assert!(shape(&with("hardware_revisions", json!(HARDWARE))));
    assert!(shape(&with("hardware_revisions", json!([1]))));
    assert!(shape(&with("image_sha256", json!(true))));
    // Optional members, when present, have their own types: null isn't "absent".
    assert!(shape(&with("summary", Value::Null)));
    assert!(shape(&with("key_set", Value::Null)));
    assert!(shape(&with("usb_install", Value::Null)));
    assert!(shape(&with("summary", json!(1))));
}

#[test]
fn device_type() {
    assert!(parse(&with("device_type", json!("x".repeat(MAX_DEVICE_TYPE_LEN)))).is_ok());
    assert!(parse(&with("device_type", json!("ā".repeat(MAX_DEVICE_TYPE_LEN)))).is_ok());
    assert!(invalid(&with(
        "device_type",
        json!("x".repeat(MAX_DEVICE_TYPE_LEN + 1))
    )));
    assert!(invalid(&with("device_type", json!(""))));
    assert!(invalid(&with("device_type", json!("a\u{7}b"))));
    assert!(invalid(&with("device_type", json!("a\u{202e}b"))));
    assert!(invalid(&with("device_type", json!(" padded"))));
}

#[test]
fn hardware_revisions() {
    let list = |n: usize| json!((0..n).map(|i| format!("rev-{i}")).collect::<Vec<_>>());
    assert!(parse(&with("hardware_revisions", list(1))).is_ok());
    assert!(parse(&with("hardware_revisions", list(MAX_HARDWARE_REVISIONS))).is_ok());
    assert!(invalid(&with("hardware_revisions", list(0))));
    assert!(invalid(&with(
        "hardware_revisions",
        list(MAX_HARDWARE_REVISIONS + 1)
    )));
    assert!(invalid(&with("hardware_revisions", json!(["a", "b", "a"]))));
    assert!(invalid(&with("hardware_revisions", json!([""]))));
    assert!(parse(&with(
        "hardware_revisions",
        json!(["x".repeat(MAX_HARDWARE_REVISION_LEN)])
    ))
    .is_ok());
    assert!(invalid(&with(
        "hardware_revisions",
        json!(["x".repeat(MAX_HARDWARE_REVISION_LEN + 1)])
    )));
}

#[test]
fn firmware_version() {
    assert!(parse(&with("firmware_version", json!("1.3.0-beta.1"))).is_ok());
    for bad in ["1.2", "v1.2.0", "1.2.0-", "1.02.0", ""] {
        assert!(invalid(&with("firmware_version", json!(bad))), "{bad}");
    }
    let at_limit = format!("1.0.0-{}", "a".repeat(MAX_FIRMWARE_VERSION_LEN - 6));
    assert!(parse(&with("firmware_version", json!(at_limit))).is_ok());
    let over = format!("1.0.0-{}", "a".repeat(MAX_FIRMWARE_VERSION_LEN - 5));
    assert!(invalid(&with("firmware_version", json!(over))));
}

#[test]
fn security_counter() {
    assert_eq!(
        parse(&with("security_counter", json!(0)))
            .unwrap()
            .security_counter,
        0
    );
    assert_eq!(
        parse(&with("security_counter", json!(u32::MAX)))
            .unwrap()
            .security_counter,
        u32::MAX
    );
    assert!(invalid(&with("security_counter", json!(u64::from(u32::MAX) + 1))));
}

#[test]
fn image_size_and_hash() {
    assert!(parse(&with("image_size", json!(1))).is_ok());
    assert!(parse(&with("image_size", json!(MAX_IMAGE_SIZE))).is_ok());
    assert!(invalid(&with("image_size", json!(0))));
    assert!(invalid(&with("image_size", json!(MAX_IMAGE_SIZE + 1))));
    let h = sha256_hex(b"x");
    assert!(parse(&with("image_sha256", json!(h))).is_ok());
    assert!(invalid(&with("image_sha256", json!(h.to_uppercase()))));
    assert!(invalid(&with("image_sha256", json!(&h[1..]))));
    assert!(invalid(&with("image_sha256", json!(format!("{h}00")))));
}

#[test]
fn summary() {
    assert!(parse(&with("summary", json!("x".repeat(MAX_SUMMARY_LEN)))).is_ok());
    assert!(invalid(&with("summary", json!("x".repeat(MAX_SUMMARY_LEN + 1)))));
    assert!(invalid(&with("summary", json!(""))));
    assert!(invalid(&with("summary", json!("line\nbreak"))));
}

#[test]
fn key_set() {
    let keys: Vec<String> = (0..4).map(|_| key().public_key().to_hex()).collect();
    assert_eq!(
        parse(&with("key_set", json!(&keys[..1])))
            .unwrap()
            .key_set
            .unwrap()
            .len(),
        1
    );
    assert!(parse(&with("key_set", json!(&keys[..MAX_KEY_SET]))).is_ok());
    assert!(invalid(&with("key_set", json!(&keys[..MAX_KEY_SET + 1]))));
    assert!(invalid(&with("key_set", json!([]))));
    assert!(invalid(&with("key_set", json!([&keys[0], &keys[0]]))));
    assert!(invalid(&with("key_set", json!([keys[0].to_uppercase()]))));
    assert!(invalid(&with("key_set", json!([&keys[0][2..]]))));
    // The right length of hex, but not a point on the curve.
    let mut off_curve = keys[0].clone();
    let last = if off_curve.ends_with('0') { "1" } else { "0" };
    off_curve.replace_range(129.., last);
    assert!(invalid(&with("key_set", json!([off_curve]))));
}

fn usb(v: Value) -> Value {
    let mut m = manifest_value();
    m["usb_install"] = v;
    m
}

fn usb_base() -> Value {
    json!({
        "chip": "esp32s3",
        "min_flash_bytes": 16777216,
        "app_offset": 65536,
        "parts": [
            { "name": "bootloader", "offset": 0, "size": 21072, "sha256": sha256_hex(b"b") },
            { "name": "partition-table", "offset": 32768, "size": 3072, "sha256": sha256_hex(b"p") }
        ]
    })
}

fn usb_with(path: &[&str], value: Value) -> Value {
    let mut u = usb_base();
    let mut at = &mut u;
    for p in &path[..path.len() - 1] {
        at = match p.parse::<usize>() {
            Ok(i) => &mut at[i],
            Err(_) => &mut at[*p],
        };
    }
    at[path[path.len() - 1]] = value;
    usb(u)
}

#[test]
fn usb_install_fields() {
    let m = parse(&usb(usb_base())).unwrap();
    let u = m.usb_install.unwrap();
    assert_eq!(u.chip, "esp32s3");
    assert_eq!(u.parts[1].offset, 32768);

    for chip in ["esp32", "esp32c6", "x"] {
        assert!(parse(&usb_with(&["chip"], json!(chip))).is_ok(), "{chip}");
    }
    for chip in ["", "ESP32", "esp-32", "32esp", "esp32 s3"] {
        assert!(invalid(&usb_with(&["chip"], json!(chip))), "{chip}");
    }
    assert!(parse(&usb_with(&["chip"], json!("e".repeat(MAX_CHIP_NAME_LEN)))).is_ok());
    assert!(invalid(&usb_with(
        &["chip"],
        json!("e".repeat(MAX_CHIP_NAME_LEN + 1))
    )));

    assert!(invalid(&usb_with(&["min_flash_bytes"], json!(0))));
    assert!(invalid(&usb_with(
        &["min_flash_bytes"],
        json!(MAX_FLASH_BYTES + 1)
    )));
    assert!(invalid(&usb_with(&["app_offset"], json!(4097))));
    assert!(parse(&usb_with(&["app_offset"], json!(0x10000))).is_ok());

    let part = |name: &str, offset: u64| json!({ "name": name, "offset": offset, "size": 1, "sha256": sha256_hex(b"") });
    assert!(invalid(&usb_with(&["parts"], json!([]))));
    let four: Vec<Value> = (0..4).map(|i| part(&format!("p{i}"), i * 4096)).collect();
    assert!(Manifest::parse(&json_bytes(&usb_with(&["parts"], json!(four)))).is_ok());
    let five: Vec<Value> = (0..5).map(|i| part(&format!("p{i}"), i * 4096)).collect();
    assert!(invalid(&usb_with(&["parts"], json!(five))));
    assert!(invalid(&usb_with(
        &["parts"],
        json!([part("a", 0), part("a", 4096)])
    )));

    for name in ["", "Boot", "boot_loader", "boot loader"] {
        assert!(invalid(&usb_with(&["parts", "0", "name"], json!(name))), "{name}");
    }
    assert!(parse(&usb_with(
        &["parts", "0", "name"],
        json!("a".repeat(MAX_PART_NAME_LEN))
    ))
    .is_ok());
    assert!(invalid(&usb_with(
        &["parts", "0", "name"],
        json!("a".repeat(MAX_PART_NAME_LEN + 1))
    )));
    assert!(invalid(&usb_with(&["parts", "0", "offset"], json!(100))));
    assert!(invalid(&usb_with(&["parts", "0", "size"], json!(0))));
    assert!(invalid(&usb_with(
        &["parts", "0", "size"],
        json!(MAX_PACKAGE_LEN + 1)
    )));
    assert!(invalid(&usb_with(&["parts", "0", "sha256"], json!("00"))));

    // Missing members inside it.
    for field in ["chip", "min_flash_bytes", "app_offset", "parts"] {
        let mut u = usb_base();
        u.as_object_mut().unwrap().remove(field);
        assert!(shape(&usb(u)), "{field}");
    }
    assert!(shape(&usb(json!("esp32"))));
    // Unknown members inside it are ignored.
    assert!(parse(&usb_with(&["future"], json!({"x": 1}))).is_ok());
}

#[test]
fn a_display_ignores_usb_install_but_not_the_json_rules() {
    let broken = usb(json!({"chip": 5}));
    assert!(parse(&broken).is_err());
    let m = Manifest::parse_for_display(&json_bytes(&broken)).unwrap();
    assert_eq!(m.usb_install, None);
    // Valid usb_install is ignored too.
    assert_eq!(
        Manifest::parse_for_display(&json_bytes(&usb(usb_base())))
            .unwrap()
            .usb_install,
        None
    );
    // The JSON rules still apply inside it.
    let text = String::from_utf8(json_bytes(&manifest_value())).unwrap();
    let body = &text[1..text.len() - 1];
    let repeated = format!(r#"{{{body},"usb_install":{{"chip":"a","chip":"b"}}}}"#);
    assert!(Manifest::parse_for_display(repeated.as_bytes()).is_err());
    let fraction = format!(r#"{{{body},"usb_install":{{"app_offset":1.5}}}}"#);
    assert!(Manifest::parse_for_display(fraction.as_bytes()).is_err());
    // Every other rule holds for a display too.
    assert!(Manifest::parse_for_display(&json_bytes(&with("image_size", json!(0)))).is_err());
}

#[test]
fn an_overlong_manifest_is_refused() {
    let mut v = manifest_value();
    v["padding"] = json!("x".repeat(MAX_MANIFEST_LEN));
    assert!(invalid(&v));
}

#[test]
fn to_json_round_trips() {
    let k = key();
    let built = build(&k, &with_usb(release()));
    let m = &built.checked.manifest;
    assert_eq!(Manifest::parse(&m.to_json()).unwrap(), *m);
    assert_eq!(m.to_json(), built.checked.manifest_bytes);
}
