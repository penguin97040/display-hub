//! The published schema, the example messages and this crate's validation must all agree.

use std::path::{Path, PathBuf};

use hub_protocol::limits::*;
use hub_protocol::{
    receive, CantShow, DataQuality, MeterItem, Page, PageContent, ReceiveError, Snapshot, Timestamp,
};

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
        if !SCHEMA_CANT_EXPRESS.contains(&name.as_str()) {
            assert!(!schema.is_valid(&json), "{name}: the schema accepted it");
        }
        // Parsing is deliberately lenient about these two (see the next test), so only the schema,
        // which describes exactly what a hub sends, catches them.
        if TOLERATED.contains(&name.as_str()) {
            continue;
        }
        let strict = serde_json::from_slice::<Snapshot>(&bytes).map(|s| s.validate());
        assert!(!matches!(strict, Ok(Ok(()))), "{name}: validation accepted it");
    }
}

/// Rules a hub checks that JSON Schema can't say: nesting depth, the page size cap and unique ids.
const SCHEMA_CANT_EXPRESS: [&str; 3] = ["custom-data-too-deep", "page-too-large", "meter-duplicate-ids"];

/// Unknown fields are ignored, so only the schema (which describes exactly what a hub sends)
/// catches a stray top-level field.
const TOLERATED: [&str; 1] = ["extra-field"];

/// What a display does with each invalid example. Most are refused; a few are forward-compatible,
/// and a page that can't be shown is a card, not a refusal.
#[test]
fn displays_refuse_tolerate_or_card_invalid_examples_as_documented() {
    for (name, bytes) in examples("invalid") {
        let got = receive(&bytes);
        let card = |why: CantShow| {
            let snap = got.as_ref().unwrap_or_else(|e| panic!("{name}: {e}"));
            let reasons: Vec<_> = snap.pages.iter().filter_map(|p| p.content().err()).collect();
            assert_eq!(reasons.len(), 1, "{name}: exactly one page is a card");
            assert_eq!(
                std::mem::discriminant(&reasons[0]),
                std::mem::discriminant(&why),
                "{name}: {:?}",
                reasons[0]
            );
            assert!(
                snap.pages.iter().any(|p| p.content().is_ok()),
                "{name}: the others show"
            );
        };
        match name.as_str() {
            // Additions from a newer v1 revision: shown, with the unknown quality as unavailable.
            "extra-field" | "shape-extra-field" => {
                assert!(got.unwrap().pages.iter().all(|p| p.content().is_ok()), "{name}")
            }
            "missing-used-percent" => {
                let snap = got.unwrap();
                let Some(PageContent::Meter(m)) = snap.pages[0].content().unwrap() else {
                    panic!("not a meter")
                };
                assert_eq!(m.items[0].percent, None);
            }
            "unknown-quality" => {
                let snap = got.unwrap();
                assert_eq!(snap.pages[0].data_quality, DataQuality::Unrecognised);
                assert!(!snap.pages[0].data_quality.shows_figures());
                assert_eq!(snap.pages[0].content(), Ok(None));
            }
            "unknown-kind" | "custom-kind-bad-version" => card(CantShow::UnknownKind),
            "page-too-large" => card(CantShow::TooLarge),
            "custom-kind-not-object"
            | "custom-data-too-deep"
            | "shape-bad-data"
            | "big-number-decimals"
            | "big-number-too-big"
            | "list-too-many-items"
            | "meter-duplicate-ids"
            | "data-missing"
            | "unavailable-with-data"
            | "percent-as-string"
            | "percent-negative"
            | "percent-over-100"
            | "too-many-metrics" => card(CantShow::BadData(String::new())),
            "wrong-version" => assert!(matches!(got, Err(ReceiveError::IncompatibleVersion(2))), "{name}"),
            "wrong-message-type" => assert!(matches!(got, Err(ReceiveError::WrongMessageType)), "{name}"),
            "bad-timestamp-offset" | "bad-timestamp-fraction" => {
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
    let Some(PageContent::Meter(m)) = snap.pages[0].content().unwrap() else {
        panic!("not a meter")
    };
    assert_eq!(m.items[1].percent, None);
    assert_eq!(m.items[1].resets_at, None);
    let out = serde_json::to_value(&snap).unwrap();
    assert!(out["pages"][0]["data"]["items"][1]["percent"].is_null());
}

#[test]
fn every_shape_and_a_custom_kind_are_covered_by_valid_examples() {
    let mut kinds = std::collections::BTreeSet::new();
    for (_, bytes) in examples("valid") {
        for p in receive(&bytes).unwrap().pages {
            kinds.insert(p.kind);
        }
    }
    for k in ["meter", "big_number", "text", "list", "nearby-flights/1"] {
        assert!(kinds.contains(k), "no valid example of {k}");
    }
}

/// Wraps one `data` object in a snapshot with a second, plain page.
fn snapshot_with(kind: &str, data: serde_json::Value) -> serde_json::Value {
    let page = |id: &str, kind: &str, data: serde_json::Value| {
        serde_json::json!({"id": id, "title": "T", "kind": kind, "data_quality": "manual",
            "updated_at": "2026-11-10T20:00:00Z", "stale_after_seconds": 1800, "note": null, "data": data})
    };
    serde_json::json!({"protocol_version": 1, "message_type": "dashboard.snapshot",
        "generated_at": "2026-11-10T20:00:00Z", "refresh_after_seconds": 300,
        "pages": [page("a", kind, data)]})
}

/// Optional fields inside a shape can be omitted or null, and the schema and the code agree on
/// that, and on `decimals` written as a whole-number float.
#[test]
fn the_schema_and_the_code_agree_on_null_optionals_and_whole_decimals() {
    use serde_json::json;
    let schema = schema();
    let cases = [
        (
            true,
            "meter",
            json!({"items":[{"id":"a","label":"A","percent":1.0,"resets_at":null}]}),
        ),
        (
            true,
            "big_number",
            json!({"value":1,"decimals":null,"unit":null,"caption":null}),
        ),
        (true, "big_number", json!({"value":1,"decimals":2.0})),
        (true, "list", json!({"items":[{"text":"x","detail":null}]})),
        (true, "big_number", json!({"value":1e15})),
        (false, "big_number", json!({"value":1.5e15})),
        (false, "big_number", json!({"value":1,"decimals":2.5})),
        (false, "big_number", json!({"value":1,"decimals":-1})),
        (
            false,
            "meter",
            json!({"items":[{"id":"a","label":"A","percent":1.0,"resets_at":5}]}),
        ),
        (false, "list", json!({"items":[{"text":"x","detail":5}]})),
    ];
    for (ok, kind, data) in cases {
        let body = snapshot_with(kind, data.clone());
        assert_eq!(schema.is_valid(&body), ok, "schema: {kind} {data}");
        let snap: Snapshot = serde_json::from_value(body.clone()).unwrap();
        assert_eq!(snap.validate().is_ok(), ok, "hub: {kind} {data}");
        let shown = receive(&serde_json::to_vec(&body).unwrap()).unwrap().pages[0]
            .content()
            .is_ok();
        assert_eq!(shown, ok, "display: {kind} {data}");
    }
}

/// The JSON parser has limits of its own. A display may refuse, as malformed, a body nested more
/// than about 100 levels or with a number outside the double range; a hub never sends either.
#[test]
fn a_display_may_refuse_absurdly_deep_or_huge_input_as_malformed() {
    let mut deep = String::from("1");
    for _ in 0..200 {
        deep = format!("{{\"n\":{deep}}}");
    }
    let body = format!(
        r#"{{"protocol_version":1,"message_type":"dashboard.snapshot","generated_at":"2026-11-10T20:00:00Z","refresh_after_seconds":300,"pages":[{{"id":"a","title":"T","kind":"deep/1","data_quality":"manual","updated_at":"2026-11-10T20:00:00Z","stale_after_seconds":1800,"note":null,"data":{deep}}}]}}"#
    );
    assert!(matches!(
        receive(body.as_bytes()),
        Err(ReceiveError::Malformed(_))
    ));
    let huge = serde_json::to_string(&snapshot_with("big_number", serde_json::json!({"value": 1}))).unwrap();
    let huge = huge.replace("\"value\":1", "\"value\":1e400");
    assert!(matches!(
        receive(huge.as_bytes()),
        Err(ReceiveError::Malformed(_))
    ));
    // Within the limits, deep custom data is a card, not a refusal.
    let mut ok = serde_json::json!({"x": 1});
    for _ in 0..20 {
        ok = serde_json::json!({ "n": ok });
    }
    let snap = receive(&serde_json::to_vec(&snapshot_with("deep/1", ok)).unwrap()).unwrap();
    assert!(snap.pages[0].content().is_err());
}

#[test]
fn oversized_bodies_are_refused_before_parsing() {
    let mut body = b"{\"protocol_version\":1,\"pad\":\"".to_vec();
    body.resize(MAX_SNAPSHOT_BYTES + 1, b'x');
    assert!(matches!(receive(&body), Err(ReceiveError::TooLarge)));
}

fn envelope(pages: Vec<Page>) -> Snapshot {
    Snapshot {
        protocol_version: 1,
        message_type: "dashboard.snapshot".into(),
        generated_at: Timestamp::parse("2026-11-10T20:00:00Z").unwrap(),
        refresh_after_seconds: 3600,
        pages,
    }
}

/// A page of exactly `MAX_PAGE_BYTES`, with the longest id, title and note, padded inside a custom
/// kind's `data`.
fn full_page(i: usize, t: Timestamp) -> Page {
    let wide = |n: usize| "😀".repeat(n);
    let mut page = Page::with_content(
        format!("{i}{}", "p".repeat(MAX_ID_LEN - 1)),
        wide(MAX_TITLE_LEN),
        DataQuality::ProviderReported,
        Some(t),
        604_800,
        Some(wide(MAX_NOTE_LEN)),
        PageContent::custom(
            format!("{}/9999", "k".repeat(MAX_KIND_LEN - 5)),
            serde_json::json!({ "pad": "" }),
        ),
    );
    let short = page.size();
    page.data = Some(serde_json::json!({ "pad": "x".repeat(MAX_PAGE_BYTES - short) }));
    assert_eq!(page.size(), MAX_PAGE_BYTES);
    page
}

/// The per-page cap must guarantee the size cap, with the longest envelope there is.
#[test]
fn ten_largest_pages_fit_the_size_cap() {
    let t = Timestamp::parse("2026-11-10T20:00:00Z").unwrap();
    let snap = envelope((0..MAX_PAGES).map(|i| full_page(i, t)).collect());
    let size = serde_json::to_vec(&snap).unwrap().len();
    assert!(size <= MAX_SNAPSHOT_BYTES, "worst case is {size} bytes");
    snap.validate().unwrap();
    // One byte more in a page is refused, not squeezed in.
    let mut over = snap.clone();
    over.pages[0].data = Some(serde_json::json!({ "pad": "x".repeat(MAX_PAGE_BYTES) }));
    assert!(over.validate().is_err());
    assert_eq!(over.pages[0].content(), Err(CantShow::TooLarge));
}

/// Every shape's own bounds, at their worst (four-byte characters everywhere), must fit in a page,
/// except a full list, which the page cap trims. The hub checks the cap, so it never overshoots.
#[test]
fn the_largest_meter_big_number_and_text_pages_fit_a_page() {
    use hub_protocol::{BigNumber, Meter, TextBlock};
    let t = Timestamp::parse("2026-11-10T20:00:00Z").unwrap();
    let wide = |n: usize| "😀".repeat(n);
    let head = |c: PageContent| {
        let mut p = Page::with_content(
            "p".repeat(MAX_ID_LEN),
            wide(MAX_TITLE_LEN),
            DataQuality::ProviderReported,
            Some(t),
            604_800,
            Some(wide(MAX_NOTE_LEN)),
            c,
        );
        p.id = "p".repeat(MAX_ID_LEN);
        p
    };
    let meter = Meter {
        items: (0..MAX_METER_ITEMS)
            .map(|j| {
                MeterItem::new(
                    format!("{j}{}", "m".repeat(MAX_ID_LEN - 1)),
                    wide(MAX_LABEL_LEN),
                    Some(33.333333333333336),
                    Some(t),
                )
            })
            .collect(),
    };
    let big = BigNumber {
        value: Some(-MAX_BIG_NUMBER_ABS),
        decimals: Some(MAX_DECIMALS),
        unit: Some(wide(MAX_UNIT_LEN)),
        caption: Some(wide(MAX_CAPTION_LEN)),
    };
    let text = TextBlock {
        text: wide(MAX_TEXT_LEN),
    };
    for page in [head(meter.into()), head(big.into()), head(text.into())] {
        assert!(
            page.size() <= MAX_PAGE_BYTES,
            "{} is {} bytes",
            page.kind,
            page.size()
        );
        envelope(vec![page]).validate().unwrap();
    }
}

#[test]
fn a_saved_copy_reads_back_even_when_rewriting_made_it_longer() {
    let (_, bytes) = examples("valid")
        .into_iter()
        .find(|(name, _)| name.starts_with("demo-three-pages"))
        .expect("the demo example");
    // Whitespace stands in for the few bytes a rewrite can add (`null` notes, `1E5` as `100000.0`).
    let mut long = bytes.clone();
    long.extend(std::iter::repeat_n(b' ', MAX_SNAPSHOT_BYTES));
    assert!(matches!(
        hub_protocol::receive(&long),
        Err(hub_protocol::ReceiveError::TooLarge)
    ));
    assert_eq!(
        hub_protocol::snapshot::reread(&long).unwrap(),
        hub_protocol::receive(&bytes).unwrap()
    );
}
