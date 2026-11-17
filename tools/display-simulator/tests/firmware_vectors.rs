//! The committed firmware update vectors (`protocol/v1/firmware/`) are what this reference display
//! does, agree with a fresh generation in everything but their random keys and signatures, and
//! cover the document's display cases. They hold public keys only.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use display_simulator::firmware::vectors::{self, stable, Test, Vector};

fn dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../protocol/v1/firmware")
}

fn committed() -> Vec<Vector> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir()).unwrap() {
        let p = e.unwrap().path();
        if p.extension().is_some_and(|x| x == "json") {
            out.push(serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap());
        }
    }
    out.sort_by(|a: &Vector, b| a.name.cmp(&b.name));
    out
}

#[test]
fn committed_vectors_match_the_reference_and_a_fresh_generation() {
    let (n, failures) = vectors::check(&dir()).unwrap();
    assert!(failures.is_empty(), "{failures:#?}");
    let files = committed();
    assert_eq!(n, files.len());
    let fresh = vectors::generate().unwrap();
    let names = |vs: &[Vector]| vs.iter().map(|v| v.name.clone()).collect::<BTreeSet<_>>();
    assert_eq!(
        names(&files),
        names(&fresh),
        "Write the missing ones: display-simulator conform write"
    );
    for v in &fresh {
        let file = files.iter().find(|f| f.name == v.name).unwrap();
        assert_eq!(
            stable(file),
            stable(v),
            "{} is out of date: display-simulator conform write",
            v.name
        );
        // The fresh one has new keys and signatures, and passes just the same.
        assert_eq!(&vectors::reference(v).unwrap(), v);
        assert_ne!(file.keys, v.keys);
    }
}

#[test]
fn writing_again_keeps_every_vector_that_is_still_right() {
    let tmp = tempfile::tempdir().unwrap();
    let (written, kept) = vectors::write(tmp.path()).unwrap();
    assert_eq!(kept, 0);
    let before: Vec<_> = std::fs::read_dir(tmp.path())
        .unwrap()
        .map(|e| {
            let p = e.unwrap().path();
            (p.clone(), std::fs::read(p).unwrap())
        })
        .collect();
    assert_eq!(vectors::write(tmp.path()).unwrap(), (0, written));
    for (p, bytes) in &before {
        assert_eq!(&std::fs::read(p).unwrap(), bytes, "{} was rewritten", p.display());
    }

    // A damaged vector is written afresh, a stray one is removed, and check catches both first.
    let one = tmp.path().join("package-02-wrong-magic.json");
    let mut v: Vector = serde_json::from_slice(&std::fs::read(&one).unwrap()).unwrap();
    v.description.push_str(" Changed.");
    std::fs::write(&one, serde_json::to_vec(&v).unwrap()).unwrap();
    let mut wrong = v.clone();
    let Test::Package { expect, .. } = &mut wrong.test else {
        panic!("a package vector")
    };
    expect.state = "installed".into();
    wrong.name = "stray".into();
    std::fs::write(tmp.path().join("stray.json"), serde_json::to_vec(&wrong).unwrap()).unwrap();
    let (_, failures) = vectors::check(tmp.path()).unwrap();
    assert!(failures.iter().any(|f| f.contains("stray.json")), "{failures:?}");
    assert_eq!(vectors::write(tmp.path()).unwrap(), (1, written - 1));
    assert!(!tmp.path().join("stray.json").exists());
    let (_, failures) = vectors::check(tmp.path()).unwrap();
    assert!(failures.is_empty(), "{failures:?}");
}

#[test]
fn the_vectors_cover_the_display_cases_and_every_reason() {
    let files = committed();
    let covered: BTreeSet<u32> = files.iter().flat_map(|v| v.cases.iter().copied()).collect();
    // The rest are run end to end against a real hub (tests/firmware.rs) or are the hub's own.
    let display_cases: BTreeSet<u32> = (1..=26).chain(29..=32).chain(34..=40).collect();
    assert!(
        display_cases.is_subset(&covered),
        "missing {:?}",
        display_cases.difference(&covered)
    );
    let mut reasons = BTreeSet::new();
    let mut states = BTreeSet::new();
    for v in &files {
        if let Test::Package { expect, .. } = &v.test {
            states.insert(expect.state.clone());
            reasons.extend(expect.reason.clone());
        }
    }
    assert_eq!(
        states,
        ["failed", "installed", "refused"].map(String::from).into()
    );
    // `flash` needs a write to fail, which only the simulator's test switch can make happen.
    for r in vectors::reasons().filter(|r| *r != "flash") {
        assert!(reasons.contains(r), "no vector reports {r}");
    }
}

#[test]
fn the_vectors_hold_public_keys_only() {
    for v in committed() {
        for (name, key) in &v.keys {
            let k = firmware_package::PublicKey::from_hex(key)
                .unwrap_or_else(|| panic!("{}: {name} isn't 130 hex digits", v.name));
            assert!(k.is_valid_point(), "{}: {name}", v.name);
        }
    }
    // Nothing that looks like a key file or PKCS#8 anywhere in the folder.
    for e in std::fs::read_dir(dir()).unwrap() {
        let text = std::fs::read_to_string(e.unwrap().path()).unwrap();
        for marker in ["PRIVATE", "pkcs8", "passphrase", "display-hub.signing-key"] {
            assert!(!text.contains(marker), "{marker}");
        }
    }
}

#[test]
fn key_behaviours_hold() {
    let files = committed();
    let get = |n: &str| files.iter().find(|v| v.name == n).unwrap().test.clone();
    let Test::Package { expect, .. } = get("package-01-valid") else {
        panic!()
    };
    assert_eq!(expect.reports, ["downloading", "installed"]);
    assert_eq!(expect.spare_slot, "set_to_boot");
    let after = expect.after_marked_good.unwrap();
    assert_eq!(
        (after.firmware_version.as_str(), after.security_floor),
        ("1.2.0", 4)
    );
    // Nothing is requested, or nothing read, for the refusals before the body.
    for (name, requests) in [
        ("package-12-no-trusted-keys", false),
        ("package-19-offer-over-limit", false),
        ("package-20-length-not-offer-size", true),
    ] {
        let Test::Package { expect, .. } = get(name) else {
            panic!()
        };
        assert_eq!(
            (expect.requests_download, expect.body_bytes_read),
            (requests, 0),
            "{name}"
        );
    }
    // Every refusal leaves the spare slot untouched.
    for v in &files {
        if let Test::Package { expect, .. } = &v.test {
            if expect.state == "refused" {
                assert_eq!(expect.spare_slot, "untouched", "{}", v.name);
                assert_eq!(expect.reports, ["refused"], "{}", v.name);
            }
        }
    }
    let Test::Package { expect, .. } = get("package-21-image-byte-changed") else {
        panic!()
    };
    assert_eq!(expect.spare_slot, "abandoned");
    assert_eq!(expect.reports, ["downloading", "failed"]);
    let Test::Package { expect, response, .. } = get("package-23-extra-bytes-after-image") else {
        panic!()
    };
    assert_eq!(Some(expect.body_bytes_read), response.content_length);
    assert!(response.body_hex.len() / 2 > expect.body_bytes_read as usize);
    let Test::Package { expect, .. } = get("package-35-key-set-retires-its-signer") else {
        panic!()
    };
    assert_eq!(
        expect.after_marked_good.unwrap().trusted_keys,
        ["recovery", "new"]
    );
    let Test::Trial { expect, .. } = get("trial-36-key-set-rolled-back") else {
        panic!()
    };
    assert_eq!(
        (expect.trusted_keys, expect.security_floor),
        (vec!["maker".to_string(), "recovery".into()], 3)
    );
    let Test::Offer { expect, .. } = get("offer-26-third-failure-waits-six-hours") else {
        panic!()
    };
    assert_eq!(expect.wait_seconds, Some(5 * 3600));
}
