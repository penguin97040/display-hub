//! The committed conformance vectors are exactly what this reference display does, and cover
//! every example and error group.

use std::path::{Path, PathBuf};

use display_simulator::conform;

fn protocol() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../protocol/v1")
}

#[test]
fn committed_vectors_match_the_reference_and_are_complete() {
    let dir = protocol().join("vectors");
    let (n, failures) = conform::check(&dir).unwrap();
    assert!(
        failures.is_empty(),
        "{failures:#?}\nRegenerate: display-simulator conform write"
    );
    let generated = conform::vectors(&protocol()).unwrap();
    assert_eq!(n, generated.len(), "Regenerate: display-simulator conform write");
    for v in &generated {
        let file: conform::Vector =
            serde_json::from_slice(&std::fs::read(dir.join(format!("{}.json", v.name))).unwrap()).unwrap();
        assert_eq!(&file, v, "{} is out of date; regenerate", v.name);
    }
    let outcomes: std::collections::BTreeSet<_> =
        generated.iter().map(|v| v.expect.outcome.as_str()).collect();
    for o in [
        "show",
        "unreadable",
        "reprovision",
        "incompatible",
        "retry",
        "fault",
    ] {
        assert!(outcomes.contains(o), "no vector for {o}");
    }
}

fn meter_items(p: &conform::PageExpect) -> Vec<conform::MeterExpect> {
    match &p.content {
        Some(conform::ContentExpect::Meter { items }) => items.clone(),
        _ => Vec::new(),
    }
}

#[test]
fn key_behaviours_hold() {
    let all = conform::vectors(&protocol()).unwrap();
    let get = |n: &str| all.iter().find(|v| v.name == n).unwrap().expect.clone();
    // Tolerated leniencies are shown; the rest of the invalid examples are refused.
    assert_eq!(get("invalid-extra-field").outcome, "show");
    assert_eq!(get("invalid-shape-extra-field").pages[0].show, "show");
    assert_eq!(
        get("invalid-unknown-quality").pages[0].quality_label,
        "UNAVAILABLE"
    );
    assert!(
        get("invalid-unknown-quality").pages[0].content.is_none(),
        "unavailable never shows figures"
    );
    assert_eq!(get("invalid-missing-used-percent").outcome, "show");
    assert_eq!(
        meter_items(&get("invalid-missing-used-percent").pages[0])[0].percent,
        None
    );
    assert_eq!(get("invalid-wrong-version").outcome, "incompatible");
    assert_eq!(get("invalid-wrong-message-type").outcome, "unreadable");
    // A page the display can't show is a card, and costs only that page.
    for (name, reason, lines) in [
        ("invalid-unknown-kind", "unknown_kind", 2),
        ("invalid-custom-kind-bad-version", "unknown_kind", 2),
        ("valid-custom-kind", "unknown_kind", 2),
        ("invalid-shape-bad-data", "bad_data", 1),
        ("invalid-percent-over-100", "bad_data", 1),
        ("invalid-percent-as-string", "bad_data", 1),
        ("invalid-unavailable-with-data", "bad_data", 1),
        ("invalid-data-missing", "bad_data", 1),
        ("invalid-custom-data-too-deep", "bad_data", 1),
        ("invalid-page-too-large", "too_large", 1),
    ] {
        let e = get(name);
        assert_eq!(e.outcome, "show", "{name}");
        let cards: Vec<_> = e.pages.iter().filter(|p| p.show == "cant_show").collect();
        assert_eq!(cards.len(), 1, "{name}");
        assert_eq!(cards[0].cant_show_reason.as_deref(), Some(reason), "{name}");
        assert_eq!(cards[0].card_lines.len(), lines, "{name}");
        assert_eq!(cards[0].card_lines[0], hub_protocol::CANT_SHOW_TEXT);
        assert!(cards[0].content.is_none(), "{name}");
        assert!(
            e.pages.iter().any(|p| p.show == "show"),
            "{name}: the other pages still show"
        );
    }
    // Every shape is drawn, and unknown stays unknown.
    let shapes = get("valid-all-shapes");
    assert!(shapes.pages.iter().all(|p| p.show == "show"));
    let tags: Vec<_> = shapes
        .pages
        .iter()
        .map(|p| match p.content.as_ref().unwrap() {
            conform::ContentExpect::Meter { .. } => "meter",
            conform::ContentExpect::BigNumber { .. } => "big_number",
            conform::ContentExpect::Text { .. } => "text",
            conform::ContentExpect::List { .. } => "list",
        })
        .collect();
    for t in ["meter", "big_number", "text", "list"] {
        assert!(tags.contains(&t), "{t}");
    }
    assert!(shapes
        .pages
        .iter()
        .flat_map(meter_items)
        .any(|m| m.percent.is_none()));
    assert!(shapes.pages.iter().any(|p| matches!(
        &p.content,
        Some(conform::ContentExpect::BigNumber { value: None, text, .. }) if text == "-"
    )));
    // Holding a snapshot ages it, and past resets are due.
    let held = get("held-until-stale");
    assert!(
        held.pages.iter().all(|p| p.stale == p.age_seconds.is_some()),
        "pages with figures go stale; never-updated ones have no age"
    );
    assert!(
        get("held-past-reset")
            .pages
            .iter()
            .flat_map(meter_items)
            .filter(|m| m.reset_due)
            .count()
            > 0
    );
    // Errors: the group decides, not the status.
    assert_eq!(get("error-token-revoked").next_poll, "hourly");
    assert_eq!(get("error-unauthorised").outcome, "fault");
    assert_eq!(get("error-unknown-code").outcome, "retry");
    assert_eq!(get("error-bad-gateway-html").outcome, "retry");
    assert_eq!(get("oversize").outcome, "unreadable");
}
