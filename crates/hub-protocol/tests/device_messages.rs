//! The enrolment, heartbeat, error and provisioning examples match their schemas and the Rust types.

use std::path::{Path, PathBuf};

use hub_protocol::device::{EnrolRequest, EnrolResponse, Heartbeat};
use hub_protocol::provisioning::Bundle;
use hub_protocol::ErrorBody;

fn dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../protocol/v1")
}

fn check(schema: &str, example: &str) -> serde_json::Value {
    let s: serde_json::Value = serde_json::from_slice(&std::fs::read(dir().join(schema)).unwrap()).unwrap();
    let v: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir().join("examples/device").join(example)).unwrap()).unwrap();
    let validator = jsonschema::validator_for(&s).unwrap();
    let errors: Vec<_> = validator.iter_errors(&v).map(|e| e.to_string()).collect();
    assert!(errors.is_empty(), "{example}: {errors:?}");
    v
}

fn round_trip<T: serde::de::DeserializeOwned + serde::Serialize>(v: serde_json::Value) -> T {
    let t: T = serde_json::from_value(v.clone()).unwrap();
    assert_eq!(serde_json::to_value(&t).unwrap(), v);
    t
}

#[test]
fn device_messages_match_their_schemas_and_types() {
    let h: Heartbeat = round_trip(check("heartbeat.schema.json", "heartbeat.json"));
    assert!(h.problems().is_empty());
    let e: EnrolRequest = round_trip(check("enrol-request.schema.json", "enrol-request.json"));
    assert!(e.device.problems().is_empty());
    let _: EnrolResponse = round_trip(check("enrol-response.schema.json", "enrol-response.json"));
    let _: ErrorBody = round_trip(check("error.schema.json", "error-revoked.json"));
    let b: Bundle = round_trip(check("provisioning.schema.json", "provisioning-bundle.json"));
    assert_eq!(b.problems(), Vec::<String>::new());
    assert_eq!(b.firmware, None);
}

/// The signed-update additions: optional members of the heartbeat and bundle, and the report.
/// Older messages, without them, still read the same (above).
#[test]
fn firmware_additions_match_their_schemas_and_types() {
    use hub_protocol::firmware::{FirmwareReport, ReportReason, ReportState};
    let h: Heartbeat = round_trip(check("heartbeat.schema.json", "heartbeat-with-firmware.json"));
    assert!(h.problems().is_empty());
    let fw = h.firmware.clone().unwrap();
    assert!(fw.problems().is_empty());
    assert_eq!(
        (fw.key_ids.len(), fw.security_floor, fw.attempts_left),
        (2, 3, Some(4))
    );
    // attempts_left is optional, and 0 to 5.
    let schema: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir().join("heartbeat.schema.json")).unwrap()).unwrap();
    let hv = jsonschema::validator_for(&schema).unwrap();
    let mut beat = serde_json::to_value(&h).unwrap();
    for (n, ok) in [(0, true), (5, true), (6, false), (-1, false)] {
        beat["firmware"]["attempts_left"] = serde_json::json!(n);
        assert_eq!(hv.is_valid(&beat), ok, "{n}");
    }
    beat["firmware"].as_object_mut().unwrap().remove("attempts_left");
    assert!(hv.is_valid(&beat));
    let without: Heartbeat = serde_json::from_value(beat).unwrap();
    assert_eq!(without.firmware.as_ref().unwrap().attempts_left, None);
    // An enrolment carries the same `device` object.
    let mut enrol: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir().join("examples/device/enrol-request.json")).unwrap())
            .unwrap();
    enrol["device"] = serde_json::to_value(Heartbeat {
        firmware: Some(fw),
        ..serde_json::from_value(enrol["device"].clone()).unwrap()
    })
    .unwrap();
    let schema: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir().join("enrol-request.schema.json")).unwrap()).unwrap();
    assert!(jsonschema::validator_for(&schema).unwrap().is_valid(&enrol));
    let _: EnrolRequest = round_trip(enrol);

    let b: Bundle = round_trip(check(
        "provisioning.schema.json",
        "provisioning-bundle-with-firmware.json",
    ));
    assert_eq!(b.problems(), Vec::<String>::new());
    assert_eq!(b.firmware.as_ref().unwrap().trusted_keys.len(), 2);
    assert!(
        format!("{b:?}").contains("trusted_keys"),
        "public keys are not secret"
    );

    for (file, state, reason) in [
        ("firmware-report-downloading.json", ReportState::Downloading, None),
        (
            "firmware-report-refused.json",
            ReportState::Refused,
            Some(ReportReason::Downgrade),
        ),
        ("firmware-report-installed.json", ReportState::Installed, None),
        ("firmware-report-rolled-back.json", ReportState::RolledBack, None),
    ] {
        let r: FirmwareReport = round_trip(check("firmware-report.schema.json", file));
        assert!(r.problems().is_empty(), "{file}");
        assert_eq!((r.state, r.reason), (state, reason), "{file}");
    }
    // The schema says what a display sends; the hub reads more leniently.
    let s: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir().join("firmware-report.schema.json")).unwrap()).unwrap();
    let v = jsonschema::validator_for(&s).unwrap();
    let offer = "5d41402abc4b2a76b9719d911017c592";
    for bad in [
        serde_json::json!({"offer_id": offer, "state": "refused"}),
        serde_json::json!({"offer_id": offer, "state": "refused", "reason": "flash"}),
        serde_json::json!({"offer_id": offer, "state": "installed", "reason": "flash"}),
        serde_json::json!({"offer_id": "x", "state": "installed"}),
        serde_json::json!({"offer_id": offer, "state": "exploded"}),
    ] {
        assert!(!v.is_valid(&bad), "{bad}");
    }
    assert!(v.is_valid(&serde_json::json!({"offer_id": offer, "state": "failed", "reason": "flash"})));
}

#[test]
fn secrets_never_show_in_debug_output() {
    let v: serde_json::Value = serde_json::from_slice(
        &std::fs::read(dir().join("examples/device/provisioning-bundle.json")).unwrap(),
    )
    .unwrap();
    let b: Bundle = serde_json::from_value(v).unwrap();
    let d = format!("{b:?}");
    assert!(
        !d.contains(&b.enrolment.secret) && !d.contains("example-passphrase"),
        "{d}"
    );
    let e: EnrolRequest =
        serde_json::from_slice(&std::fs::read(dir().join("examples/device/enrol-request.json")).unwrap())
            .unwrap();
    assert!(!format!("{e:?}").contains(&e.enrolment_secret));
}

#[test]
fn usb_messages_match_their_schema_and_types() {
    use hub_protocol::usb::{FromDisplay, ToDisplay};
    let read = |p: &str| -> serde_json::Value {
        serde_json::from_slice(&std::fs::read(dir().join(p)).unwrap()).unwrap()
    };
    // The provisioning schema, inlined where usb.schema.json refers to it by its $id.
    let mut schema = read("usb.schema.json");
    let mut bundle = read("provisioning.schema.json");
    bundle
        .as_object_mut()
        .unwrap()
        .retain(|k, _| k != "$schema" && k != "$id");
    schema["oneOf"][1]["properties"]["bundle"] = bundle;
    let validator = jsonschema::validator_for(&schema).unwrap();
    for (file, from_manager) in [
        ("manager-hello.json", true),
        ("manager-provision.json", true),
        ("display-hello.json", false),
        ("display-provisioned.json", false),
        ("display-error.json", false),
        ("display-hello-with-firmware.json", false),
        ("display-error-keys-locked.json", false),
    ] {
        let v = read(&format!("examples/usb/{file}"));
        let errors: Vec<_> = validator.iter_errors(&v).map(|e| e.to_string()).collect();
        assert!(errors.is_empty(), "{file}: {errors:?}");
        if from_manager {
            let _: ToDisplay = round_trip(v);
        } else {
            let _: FromDisplay = round_trip(v);
        }
    }
    // Each example is one line, as on the wire.
    for f in std::fs::read_dir(dir().join("examples/usb")).unwrap() {
        let text = std::fs::read_to_string(f.unwrap().path()).unwrap();
        assert_eq!(text.matches('\n').count(), 1);
        assert!(text.len() <= hub_protocol::usb::MAX_LINE_BYTES);
    }
    assert!(!validator.is_valid(&serde_json::json!({"type": "provision", "bundle": {}})));
    let locked: FromDisplay =
        serde_json::from_value(read("examples/usb/display-error-keys-locked.json")).unwrap();
    assert!(matches!(
        locked,
        FromDisplay::Error {
            code: hub_protocol::usb::UsbErrorCode::FirmwareKeysLocked,
            ..
        }
    ));
    let hello: FromDisplay =
        serde_json::from_value(read("examples/usb/display-hello-with-firmware.json")).unwrap();
    let FromDisplay::Hello { device, .. } = hello else {
        panic!("hello")
    };
    let fw = device.firmware.unwrap();
    assert_eq!((fw.slot_bytes, fw.attempts_left), (3_145_728, Some(5)));
}
