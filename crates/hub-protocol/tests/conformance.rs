//! The published schema, the example messages and this crate's validation must all agree.

use std::path::{Path, PathBuf};

use hub_protocol::limits::*;
use hub_protocol::{receive, DataQuality, Metric, Page, ReceiveError, Snapshot, Timestamp};

fn protocol_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../protocol/v1")
}

fn examples(kind: &str) -> Vec<(String, Vec<u8>)> {
    let mut out: Vec<_> = std::fs::read_dir(protocol_dir().join("examples").join(kind))
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .map(|p| {
            (
                p.file_stem().unwrap().to_string_lossy().into_owned(),
                std::fs::read(&p).unwrap(),
            )
        })
        .collect();
    out.sort();
    assert!(!out.is_empty(), "no {kind} examples");
    out
}

fn schema() -> jsonschema::Validator {
    let s: serde_json::Value =
        serde_json::from_slice(&std::fs::read(protocol_dir().join("snapshot.schema.json")).unwrap()).unwrap();
    jsonschema::validator_for(&s).unwrap()
}

#[test]
fn valid_examples_pass_the_schema_and_validation_and_round_trip() {
    let schema = schema();
    for (name, bytes) in examples("valid") {
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let errors: Vec<_> = schema.iter_errors(&json).map(|e| e.to_string()).collect();
        assert!(errors.is_empty(), "{name}: schema errors {errors:?}");
        let snap = receive(&bytes).unwrap_or_else(|e| panic!("{name}: {e}"));
        snap.validate().unwrap_or_else(|e| panic!("{name}: {e:?}"));
        assert_eq!(
            serde_json::to_value(&snap).unwrap(),
            json,
            "{name}: did not round-trip"
        );
    }
}

#[test]
fn invalid_examples_fail_both_the_schema_and_validation() {
    let schema = schema();
    for (name, bytes) in examples("invalid") {
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(!schema.is_valid(&json), "{name}: the schema accepted it");
        // Parsing is deliberately lenient about these two (see the next test), so only the schema,
        // which describes exactly what a hub sends, catches them.
        if TOLERATED.contains(&name.as_str()) {
            continue;
        }
        let strict = serde_json::from_slice::<Snapshot>(&bytes).map(|s| s.validate());
        assert!(!matches!(strict, Ok(Ok(()))), "{name}: validation accepted it");
    }
}

/// Unknown fields are ignored, and a missing value reads as unknown (`null`), never as zero.
const TOLERATED: [&str; 2] = ["extra-field", "missing-used-percent"];

/// What a display does with each invalid example. Most are refused; a few are forward-compatible.
#[test]
fn displays_refuse_or_tolerate_invalid_examples_as_documented() {
    for (name, bytes) in examples("invalid") {
        let got = receive(&bytes);
        match name.as_str() {
            // Additions from a newer v1 revision: shown, with the unknown quality as unavailable.
            "extra-field" => assert!(got.is_ok(), "{name}"),
            "missing-used-percent" => assert_eq!(got.unwrap().pages[0].metrics[0].used_percent, None),
            "unknown-quality" => {
                let snap = got.unwrap();
                assert_eq!(snap.pages[0].data_quality, DataQuality::Unrecognised);
                assert!(!snap.pages[0].data_quality.shows_figures());
            }
            "wrong-version" => assert!(matches!(got, Err(ReceiveError::IncompatibleVersion(2))), "{name}"),
            "wrong-message-type" => assert!(matches!(got, Err(ReceiveError::WrongMessageType)), "{name}"),
            "percent-as-string" | "bad-timestamp-offset" | "bad-timestamp-fraction" => {
                assert!(matches!(got, Err(ReceiveError::Malformed(_))), "{name}: {got:?}")
            }
            _ => assert!(matches!(got, Err(ReceiveError::Invalid(_))), "{name}: {got:?}"),
        }
    }
}

#[test]
fn missing_values_stay_null() {
    let bytes = std::fs::read(protocol_dir().join("examples/valid/stale-provider-reported.json")).unwrap();
    let snap = receive(&bytes).unwrap();
    let weekly = &snap.pages[0].metrics[1];
    assert_eq!(weekly.used_percent, None);
    assert_eq!(weekly.resets_at, None);
    let out = serde_json::to_value(&snap).unwrap();
    assert!(out["pages"][0]["metrics"][1]["used_percent"].is_null());
}

#[test]
fn oversized_bodies_are_refused_before_parsing() {
    let mut body = b"{\"protocol_version\":1,\"pad\":\"".to_vec();
    body.resize(MAX_SNAPSHOT_BYTES + 1, b'x');
    assert!(matches!(receive(&body), Err(ReceiveError::TooLarge)));
}

/// The per-field bounds must guarantee the size cap, even with four-byte characters everywhere.
#[test]
fn the_largest_possible_snapshot_fits_the_size_cap() {
    let t = Timestamp::parse("2026-11-10T20:00:00Z").unwrap();
    let wide = |n: usize| "😀".repeat(n);
    let snap = Snapshot {
        protocol_version: 1,
        message_type: "dashboard.snapshot".into(),
        dashboard_id: "llm-usage".into(),
        generated_at: t,
        refresh_after_seconds: 3600,
        pages: (0..MAX_PAGES)
            .map(|i| Page {
                id: format!("{i}{}", "p".repeat(MAX_ID_LEN - 1)),
                title: wide(MAX_TITLE_LEN),
                data_quality: DataQuality::ProviderReported,
                updated_at: Some(t),
                stale_after_seconds: 604_800,
                note: Some(wide(MAX_NOTE_LEN)),
                metrics: (0..MAX_METRICS)
                    .map(|j| Metric {
                        id: format!("{j}{}", "m".repeat(MAX_ID_LEN - 1)),
                        label: wide(MAX_LABEL_LEN),
                        used_percent: Some(33.333333333333336),
                        resets_at: Some(t),
                    })
                    .collect(),
            })
            .collect(),
    };
    let size = serde_json::to_vec(&snap).unwrap().len();
    assert!(size <= MAX_SNAPSHOT_BYTES, "worst case is {size} bytes");
    snap.validate().unwrap();
}
