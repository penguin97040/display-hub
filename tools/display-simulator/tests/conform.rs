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

#[test]
fn key_behaviours_hold() {
    let all = conform::vectors(&protocol()).unwrap();
    let get = |n: &str| all.iter().find(|v| v.name == n).unwrap().expect.clone();
    // Tolerated leniencies are shown; the rest of the invalid examples are refused.
    assert_eq!(get("invalid-extra-field").outcome, "show");
    assert_eq!(
        get("invalid-unknown-quality").pages[0].quality_label,
        "UNAVAILABLE"
    );
    assert!(
        get("invalid-unknown-quality").pages[0].metrics.is_empty(),
        "unavailable never shows figures"
    );
    assert_eq!(get("invalid-missing-used-percent").outcome, "show");
    assert_eq!(get("invalid-percent-over-100").outcome, "unreadable");
    assert_eq!(get("invalid-wrong-version").outcome, "incompatible");
    // Unknown stays unknown.
    assert!(all
        .iter()
        .flat_map(|v| &v.expect.pages)
        .flat_map(|p| &p.metrics)
        .any(|m| m.used_percent.is_none()));
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
            .flat_map(|p| &p.metrics)
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
