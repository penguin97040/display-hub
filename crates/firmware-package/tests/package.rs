//! The whole-package checker ("Checking a package") and the builder, end to end.

mod common;

use std::io::{Cursor, Read};

use common::*;
use firmware_package::json::JsonProblem;
use firmware_package::limits::*;
use firmware_package::{
    build_and_sign, check, check_file, update_form, update_form_len, BuildError, HeaderError, LayoutError,
    ManifestError, PackageError,
};
use serde_json::json;

fn run(bytes: &[u8]) -> Result<firmware_package::CheckedPackage, PackageError> {
    check(Cursor::new(bytes), bytes.len() as u64)
}

#[test]
fn round_trip() {
    let k = key();
    let built = build(&k, &with_usb(release()));
    let c = run(&built.bytes).unwrap();
    assert_eq!(c, built.checked);
    assert_eq!(c.manifest.device_type, DEVICE_TYPE);
    assert_eq!(c.manifest.firmware_version, "1.2.0");
    assert_eq!(c.manifest.security_counter, 3);
    assert_eq!(c.manifest.image_size, image().len() as u64);
    assert_eq!(c.signer(), k.public_key());
    assert_eq!(c.signer_key_id(), k.public_key().key_id());
    assert_eq!(c.len, built.bytes.len() as u64);
    assert_eq!(c.package_id.to_hex(), sha256_hex(&built.bytes));
    let usb = c.manifest.usb_install.as_ref().unwrap();
    assert_eq!(usb.parts.len(), 2);
    assert_eq!(usb.parts[0].name, "bootloader");
    assert_eq!(c.manifest.parts_len(), 2100 + 3072);
    // The manifest is kept exactly as signed.
    let mlen = c.header.manifest_len as usize;
    assert_eq!(c.manifest_bytes, built.bytes[HEADER_LEN..HEADER_LEN + mlen]);

    // And from a file.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("llm-usage-display-1.2.0.dhfw");
    std::fs::write(&path, &built.bytes).unwrap();
    assert_eq!(check_file(&path).unwrap(), c);
}

#[test]
fn signing_twice_gives_two_valid_packages() {
    let k = key();
    let a = build(&k, &release());
    let b = build(&k, &release());
    assert_ne!(a.bytes, b.bytes);
    assert_ne!(a.checked.package_id, b.checked.package_id);
    assert_eq!(a.checked.manifest, b.checked.manifest);
}

#[test]
fn the_update_form_is_the_start_of_the_package_without_the_parts() {
    let k = key();
    let built = build(&k, &with_usb(release()));
    let c = &built.checked;
    let expected = HEADER_LEN as u64 + u64::from(c.header.manifest_len) + image().len() as u64;
    assert_eq!(c.update_form_len(), expected);
    assert_eq!(
        update_form_len(c.header.manifest_len, c.manifest.image_size),
        expected
    );
    assert_eq!(c.len, expected + 2100 + 3072);
    let mut form = Vec::new();
    update_form(Cursor::new(&built.bytes), c)
        .read_to_end(&mut form)
        .unwrap();
    assert_eq!(form, built.bytes[..expected as usize]);

    // Without USB parts the update form is the whole package.
    let plain = build(&k, &release());
    assert_eq!(plain.checked.update_form_len(), plain.bytes.len() as u64);
}

#[test]
fn tampering_with_any_signed_header_byte_is_caught() {
    let k = key();
    let built = build(&k, &release());
    for i in 0..SIGNED_HEADER_LEN {
        let mut b = built.bytes.clone();
        b[i] ^= 0x01;
        let err = run(&b).unwrap_err();
        match i {
            0..=3 => assert_eq!(err, PackageError::Header(HeaderError::BadMagic), "byte {i}"),
            4 => assert!(matches!(
                err,
                PackageError::Header(HeaderError::BadFormatVersion(_))
            )),
            5 => assert!(matches!(err, PackageError::Header(HeaderError::BadAlgorithm(_)))),
            // A different manifest length is refused, runs past the end, or breaks the signature.
            6 | 7 => assert!(
                matches!(
                    err,
                    PackageError::Header(HeaderError::BadManifestLength(_))
                        | PackageError::Truncated
                        | PackageError::BadSignature
                ),
                "byte {i}: {err:?}"
            ),
            // The prefix byte, then the key: no longer a valid point, or not the signer.
            _ => assert!(
                matches!(err, PackageError::BadPublicKey | PackageError::BadSignature),
                "byte {i}: {err:?}"
            ),
        }
    }
    // Every signature byte too.
    for i in OFFSET_SIGNATURE..HEADER_LEN {
        let mut b = built.bytes.clone();
        b[i] ^= 0x80;
        assert_eq!(run(&b).unwrap_err(), PackageError::BadSignature, "byte {i}");
    }
}

#[test]
fn tampering_with_the_manifest_image_or_any_part_is_caught() {
    let k = key();
    let built = build(&k, &with_usb(release()));
    let mlen = built.checked.header.manifest_len as usize;
    for i in HEADER_LEN..HEADER_LEN + mlen {
        let mut b = built.bytes.clone();
        b[i] ^= 0x01;
        assert_eq!(
            run(&b).unwrap_err(),
            PackageError::BadSignature,
            "manifest byte {i}"
        );
    }
    let image_start = HEADER_LEN + mlen;
    let image_len = image().len();
    for i in [
        image_start,
        image_start + image_len / 2,
        image_start + image_len - 1,
    ] {
        let mut b = built.bytes.clone();
        b[i] ^= 0x01;
        assert_eq!(run(&b).unwrap_err(), PackageError::ImageHash);
    }
    let boot = image_start + image_len;
    let table = boot + 2100;
    for (i, name) in [
        (boot, "bootloader"),
        (table - 1, "bootloader"),
        (table, "partition-table"),
        (built.bytes.len() - 1, "partition-table"),
    ] {
        let mut b = built.bytes.clone();
        b[i] ^= 0x01;
        assert_eq!(run(&b).unwrap_err(), PackageError::PartHash(name.into()));
    }
}

#[test]
fn the_wrong_key_or_an_invalid_point_is_refused() {
    let k = key();
    let other = key();
    let built = build(&k, &release());
    // Another valid key named in the header.
    let mut b = built.bytes.clone();
    b[OFFSET_PUBLIC_KEY..OFFSET_SIGNATURE].copy_from_slice(other.public_key().as_bytes());
    assert_eq!(run(&b).unwrap_err(), PackageError::BadSignature);
    // Signed by one key, naming another, from the start.
    let m = json_bytes(&manifest_value());
    let b = package_signed_as(&k, &other.public_key(), &m, &image(), &[]);
    assert_eq!(run(&b).unwrap_err(), PackageError::BadSignature);
    // Not a point on the curve.
    let mut b = built.bytes.clone();
    b[OFFSET_SIGNATURE - 1] ^= 0x01;
    assert_eq!(run(&b).unwrap_err(), PackageError::BadPublicKey);
    let mut b = built.bytes.clone();
    b[OFFSET_PUBLIC_KEY..OFFSET_SIGNATURE].fill(0);
    assert_eq!(run(&b).unwrap_err(), PackageError::BadPublicKey);
    // A signature of all zeros.
    let mut b = built.bytes.clone();
    b[OFFSET_SIGNATURE..HEADER_LEN].fill(0);
    assert_eq!(run(&b).unwrap_err(), PackageError::BadSignature);
}

#[test]
fn a_signature_without_the_fixed_text_is_refused() {
    let k = key();
    let m = json_bytes(&manifest_value());
    let prefix = firmware_package::header::signed_prefix(m.len() as u16, &k.public_key());
    let mut message = prefix.to_vec();
    message.extend_from_slice(&m);
    let sig = k.sign(&message).unwrap();
    let mut b = prefix.to_vec();
    b.extend_from_slice(&sig);
    b.extend_from_slice(&m);
    b.extend_from_slice(&image());
    assert_eq!(run(&b).unwrap_err(), PackageError::BadSignature);
}

#[test]
fn lengths_trailing_bytes_and_truncation() {
    let k = key();
    let built = build(&k, &with_usb(release()));
    let n = built.bytes.len() as u64;

    // A stated length that doesn't add up.
    let mut longer = built.bytes.clone();
    longer.push(0);
    assert_eq!(
        run(&longer).unwrap_err(),
        PackageError::LengthMismatch {
            expected: n,
            actual: n + 1
        }
    );
    let shorter = &built.bytes[..built.bytes.len() - 1];
    assert_eq!(
        run(shorter).unwrap_err(),
        PackageError::LengthMismatch {
            expected: n,
            actual: n - 1
        }
    );
    // The right length stated, but the stream carries on, or stops early.
    assert_eq!(
        check(Cursor::new(&longer), n).unwrap_err(),
        PackageError::TrailingBytes
    );
    assert_eq!(
        check(Cursor::new(shorter), n).unwrap_err(),
        PackageError::Truncated
    );
    assert_eq!(
        check(Cursor::new(&built.bytes[..100]), n).unwrap_err(),
        PackageError::Truncated
    );
    // The update form alone isn't a whole package when it has parts.
    let form = &built.bytes[..built.checked.update_form_len() as usize];
    assert!(matches!(
        run(form).unwrap_err(),
        PackageError::LengthMismatch { .. }
    ));

    // Too short and too large, refused on the length alone.
    assert_eq!(run(&built.bytes[..138]).unwrap_err(), PackageError::TooShort(138));
    assert_eq!(run(&[]).unwrap_err(), PackageError::TooShort(0));
    let empty = std::io::empty();
    assert_eq!(
        check(empty, MAX_PACKAGE_LEN + 1).unwrap_err(),
        PackageError::TooLarge(MAX_PACKAGE_LEN + 1)
    );
    // At exactly the minimum, the header is read.
    let mut min = built.bytes[..MIN_PACKAGE_LEN as usize].to_vec();
    min[OFFSET_MANIFEST_LEN..OFFSET_PUBLIC_KEY].copy_from_slice(&2u16.to_le_bytes());
    assert!(matches!(
        run(&min).unwrap_err(),
        PackageError::BadSignature | PackageError::BadPublicKey
    ));
    // A manifest that would run past the stated length is never read past it.
    let mut b = built.bytes[..200].to_vec();
    b[OFFSET_MANIFEST_LEN..OFFSET_PUBLIC_KEY].copy_from_slice(&4096u16.to_le_bytes());
    assert_eq!(run(&b).unwrap_err(), PackageError::Truncated);
}

#[test]
fn a_package_too_large_is_refused_by_length_before_anything_is_read() {
    struct Panics;
    impl Read for Panics {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            panic!("nothing should be read");
        }
    }
    assert_eq!(
        check(Panics, u64::MAX).unwrap_err(),
        PackageError::TooLarge(u64::MAX)
    );
    assert_eq!(check(Panics, 10).unwrap_err(), PackageError::TooShort(10));
}

#[test]
fn manifest_len_bounds() {
    let k = key();
    let img = image();
    for len in [0u16, 1, 4097, u16::MAX] {
        let mut b = package(&k, &json_bytes(&manifest_value()), &img, &[]);
        b[OFFSET_MANIFEST_LEN..OFFSET_PUBLIC_KEY].copy_from_slice(&len.to_le_bytes());
        assert_eq!(
            run(&b).unwrap_err(),
            PackageError::Header(HeaderError::BadManifestLength(len))
        );
    }
    // 2 bytes: a signed `{}` gets as far as the manifest's rules.
    let b = package(&k, b"{}", &img, &[]);
    assert!(matches!(
        run(&b).unwrap_err(),
        PackageError::Manifest(ManifestError::Shape(_))
    ));
    // 4096 bytes: a full-size manifest, padded with an unknown member, is fine.
    let mut v = manifest_value();
    v["padding"] = json!("");
    let base = json_bytes(&v).len();
    v["padding"] = json!("x".repeat(MAX_MANIFEST_LEN - base));
    let m = json_bytes(&v);
    assert_eq!(m.len(), MAX_MANIFEST_LEN);
    let c = run(&package(&k, &m, &img, &[])).unwrap();
    assert_eq!(c.header.manifest_len as usize, MAX_MANIFEST_LEN);
}

fn manifest_err(k: &firmware_package::SigningKey, m: &[u8]) -> ManifestError {
    match run(&package(k, m, &image(), &[])).unwrap_err() {
        PackageError::Manifest(e) => e,
        other => panic!("expected a manifest error, got {other:?}"),
    }
}

#[test]
fn json_rules_through_the_checker() {
    let k = key();
    let good = String::from_utf8(json_bytes(&manifest_value())).unwrap();
    let body = &good[1..good.len() - 1];

    let repeated = format!(r#"{{{body},"device_type":"{DEVICE_TYPE}"}}"#);
    assert_eq!(
        manifest_err(&k, repeated.as_bytes()),
        ManifestError::Json(JsonProblem::RepeatedMember("device_type".into()))
    );
    let repeated_unknown = format!(r#"{{{body},"extra":{{"a":[{{"b":1,"b":1}}]}}}}"#);
    assert_eq!(
        manifest_err(&k, repeated_unknown.as_bytes()),
        ManifestError::Json(JsonProblem::RepeatedMember("b".into()))
    );
    let repeated_top_unknown = format!(r#"{{{body},"extra":1,"extra":2}}"#);
    assert_eq!(
        manifest_err(&k, repeated_top_unknown.as_bytes()),
        ManifestError::Json(JsonProblem::RepeatedMember("extra".into()))
    );

    let n = image().len();
    for number in [
        format!("-{n}"),
        format!("{n}.0"),
        format!("{n}e0"),
        "5e3".into(),
        "-0".into(),
    ] {
        let m = good.replace(
            &format!(r#""image_size":{n}"#),
            &format!(r#""image_size":{number}"#),
        );
        assert_ne!(m, good);
        assert_eq!(
            manifest_err(&k, m.as_bytes()),
            ManifestError::Json(JsonProblem::NotAWholeNumber),
            "{number}"
        );
    }
    let fraction_unknown = format!(r#"{{{body},"extra":[1.5]}}"#);
    assert_eq!(
        manifest_err(&k, fraction_unknown.as_bytes()),
        ManifestError::Json(JsonProblem::NotAWholeNumber)
    );

    let deep_ok = format!(r#"{{{body},"extra":{{"a":[{{"b":1}}]}}}}"#);
    assert!(run(&package(&k, deep_ok.as_bytes(), &image(), &[])).is_ok());
    let too_deep = format!(r#"{{{body},"extra":{{"a":[{{"b":[]}}]}}}}"#);
    assert_eq!(
        manifest_err(&k, too_deep.as_bytes()),
        ManifestError::Json(JsonProblem::TooDeep)
    );

    let mut bom = b"\xef\xbb\xbf".to_vec();
    bom.extend_from_slice(good.as_bytes());
    assert_eq!(
        manifest_err(&k, &bom),
        ManifestError::Json(JsonProblem::ByteOrderMark)
    );

    assert!(matches!(
        manifest_err(&k, b"not json"),
        ManifestError::Json(JsonProblem::Syntax(_))
    ));
    assert_eq!(
        manifest_err(&k, b"[1, 2]"),
        ManifestError::Json(JsonProblem::NotAnObject)
    );

    let unknown = format!(r#"{{{body},"future_field":{{"x":"y"}},"another":[true,null]}}"#);
    let c = run(&package(&k, unknown.as_bytes(), &image(), &[])).unwrap();
    assert_eq!(c.manifest.device_type, DEVICE_TYPE);
}

#[test]
fn usb_layout_is_checked() {
    let k = key();
    let img = image();
    let boot = vec![1u8; 100];
    let table = vec![2u8; 3072];
    let ok = usb_manifest_value(&[("bootloader", 0, &boot), ("partition-table", 32768, &table)]);
    let bytes = package(&k, &json_bytes(&ok), &img, &[&boot, &table]);
    assert!(run(&bytes).is_ok());

    let layout_err =
        |v: &serde_json::Value, parts: &[&[u8]]| run(&package(&k, &json_bytes(v), &img, parts)).unwrap_err();

    // Two parts overlapping.
    let v = usb_manifest_value(&[("bootloader", 0, &table), ("partition-table", 0, &table)]);
    assert!(matches!(
        layout_err(&v, &[&table, &table]),
        PackageError::Layout(LayoutError::Overlap(_, _))
    ));
    // A part running into the app image.
    let big = vec![3u8; 65536 + 1];
    let v = usb_manifest_value(&[("bootloader", 0, &big)]);
    assert_eq!(
        layout_err(&v, &[&big]),
        PackageError::Layout(LayoutError::Overlap(
            "part \"bootloader\"".into(),
            "app image".into()
        ))
    );
    // Touching but not overlapping is fine.
    let exact = vec![3u8; 65536];
    let v = usb_manifest_value(&[("bootloader", 0, &exact)]);
    assert!(run(&package(&k, &json_bytes(&v), &img, &[&exact])).is_ok());
    // A part placed inside the app image.
    let v = usb_manifest_value(&[("bootloader", 65536, &boot)]);
    assert!(matches!(
        layout_err(&v, &[&boot]),
        PackageError::Layout(LayoutError::Overlap(_, _))
    ));

    // Unaligned offsets.
    let v = usb_manifest_value(&[("bootloader", 4095, &boot)]);
    assert!(matches!(
        layout_err(&v, &[&boot]),
        PackageError::Manifest(ManifestError::Invalid { .. })
    ));
    let mut v = ok.clone();
    v["usb_install"]["app_offset"] = json!(65536 + 512);
    assert!(matches!(
        layout_err(&v, &[&boot, &table]),
        PackageError::Manifest(ManifestError::Invalid { .. })
    ));

    // Outside min_flash_bytes: the app image, and a part.
    let mut v = ok.clone();
    v["usb_install"]["min_flash_bytes"] = json!(65536 + img.len() - 1);
    assert_eq!(
        layout_err(&v, &[&boot, &table]),
        PackageError::Layout(LayoutError::OutsideFlash("app image".into()))
    );
    let mut v = ok.clone();
    v["usb_install"]["min_flash_bytes"] = json!(65536 + img.len());
    assert!(run(&package(&k, &json_bytes(&v), &img, &[&boot, &table])).is_ok());
    let mut v = usb_manifest_value(&[("bootloader", 0, &boot), ("partition-table", 1 << 20, &table)]);
    v["usb_install"]["min_flash_bytes"] = json!((1 << 20) + 3071);
    assert_eq!(
        layout_err(&v, &[&boot, &table]),
        PackageError::Layout(LayoutError::OutsideFlash("part \"partition-table\"".into()))
    );
    // An offset near the top of u64 doesn't wrap round.
    let v = usb_manifest_value(&[("bootloader", u64::MAX - 4095, &boot)]);
    assert!(matches!(
        layout_err(&v, &[&boot]),
        PackageError::Layout(LayoutError::OutsideFlash(_))
    ));
}

#[test]
fn build_refuses_what_the_checker_would() {
    let k = key();
    let mut r = release();
    r.device_type = String::new();
    assert!(matches!(
        build_and_sign(&k, &r, &image()),
        Err(BuildError::Manifest(_))
    ));
    assert!(matches!(
        build_and_sign(&k, &release(), &[]),
        Err(BuildError::ImageSize(0))
    ));
    let mut r = release();
    r.summary = Some("x".repeat(MAX_SUMMARY_LEN + 1));
    assert!(matches!(
        build_and_sign(&k, &r, &image()),
        Err(BuildError::Manifest(_))
    ));
    let mut r = with_usb(release());
    if let Some(u) = r.usb_install.as_mut() {
        u.parts[1].offset = 0;
    }
    assert!(matches!(
        build_and_sign(&k, &r, &image()),
        Err(BuildError::Layout(_))
    ));
    let big = vec![0u8; MAX_IMAGE_SIZE as usize + 1];
    assert!(matches!(
        build_and_sign(&k, &release(), &big),
        Err(BuildError::ImageSize(_))
    ));
    // The full-size image is fine, and so is a package near the limit.
    let full = vec![0u8; MAX_IMAGE_SIZE as usize];
    assert!(build_and_sign(&k, &release(), &full).is_ok());
}

#[test]
fn a_key_set_is_carried() {
    let k = key();
    let recovery = key();
    let mut r = release();
    r.key_set = Some(vec![k.public_key(), recovery.public_key()]);
    let c = build(&k, &r).checked;
    assert_eq!(
        c.manifest.key_set,
        Some(vec![k.public_key(), recovery.public_key()])
    );
}
