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
