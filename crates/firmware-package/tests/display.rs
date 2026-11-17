//! A display's checks before installing, steps 1 to 7, and each reason code.

mod common;

use common::*;
use firmware_package::limits::*;
use firmware_package::{Display, Manifest, PublicKey, Refusal, SigningKey};
use serde_json::json;

const SLOT: u64 = 1024 * 1024;

fn display(keys: &[PublicKey]) -> Display<'_> {
    Display {
        trusted_keys: keys,
        device_type: DEVICE_TYPE,
        hardware_revision: HARDWARE,
        security_floor: 3,
        slot_bytes: SLOT,
    }
}

/// Runs steps 3 to 7 on a whole update form, as a display reads it.
fn on_form(d: &Display<'_>, form: &[u8]) -> Result<Manifest, Refusal> {
    let header: [u8; HEADER_LEN] = form[..HEADER_LEN].try_into().unwrap();
    let h = d.check_header(&header)?;
    let mlen = h.header.manifest_len as usize;
    d.check_manifest(&h, &form[HEADER_LEN..HEADER_LEN + mlen], form.len() as u64)
}

fn form_for(k: &SigningKey, manifest: &serde_json::Value) -> Vec<u8> {
    package(k, &json_bytes(manifest), &image(), &[])
}

#[test]
fn a_valid_package_passes_and_the_second_key_works_too() {
    let (a, b) = (key(), key());
    let keys = [a.public_key(), b.public_key()];
    let d = display(&keys);
    let built = build(&a, &release());
    let m = on_form(&d, &built.bytes).unwrap();
    assert_eq!(m.firmware_version, "1.2.0");
    let header: [u8; HEADER_LEN] = built.bytes[..HEADER_LEN].try_into().unwrap();
    assert_eq!(d.check_header(&header).unwrap().stored_key, a.public_key());
    // The combined call agrees.
    let mlen = built.checked.header.manifest_len as usize;
    assert!(d
        .check(
            &header,
            &built.bytes[HEADER_LEN..HEADER_LEN + mlen],
            built.bytes.len() as u64
        )
        .is_ok());
    assert!(on_form(&d, &build(&b, &release()).bytes).is_ok());
}

#[test]
fn no_trusted_keys() {
    let d = display(&[]);
    assert_eq!(d.check_offer(100, 100), Err(Refusal::NoTrustedKeys));
    let built = build(&key(), &release());
    assert_eq!(on_form(&d, &built.bytes).unwrap_err(), Refusal::NoTrustedKeys);
}

#[test]
fn offer_size_comes_first() {
    let keys = [key().public_key()];
    let d = display(&keys);
    let limit = (HEADER_LEN + MAX_MANIFEST_LEN) as u64 + SLOT;
    assert_eq!(d.check_offer(limit, limit), Ok(()));
    assert_eq!(d.check_offer(limit + 1, limit + 1), Err(Refusal::TooLarge));
    assert_eq!(d.check_offer(1000, 1001), Err(Refusal::TooLarge));
    assert_eq!(Refusal::TooLarge.as_str(), "too_large");
}

#[test]
fn bad_format_headers() {
    let k = key();
    let keys = [k.public_key()];
    let d = display(&keys);
    let good = build(&k, &release()).bytes;
    let header = |f: &dyn Fn(&mut [u8])| {
        let mut h: [u8; HEADER_LEN] = good[..HEADER_LEN].try_into().unwrap();
        f(&mut h);
        d.check_header(&h).map(|_| ())
    };
    assert_eq!(header(&|h| h[0] = b'X'), Err(Refusal::BadFormat));
    assert_eq!(header(&|h| h[OFFSET_FORMAT_VERSION] = 2), Err(Refusal::BadFormat));
    assert_eq!(header(&|h| h[OFFSET_ALGORITHM] = 2), Err(Refusal::BadFormat));
    for len in [0u16, 1, 4097] {
        assert_eq!(
            header(&|h| h[OFFSET_MANIFEST_LEN..OFFSET_PUBLIC_KEY].copy_from_slice(&len.to_le_bytes())),
            Err(Refusal::BadFormat),
            "{len}"
        );
    }
    assert_eq!(Refusal::BadFormat.as_str(), "bad_format");
}

#[test]
fn untrusted_key() {
    let keys = [key().public_key()];
    let d = display(&keys);
    let other = build(&key(), &release()).bytes;
    assert_eq!(on_form(&d, &other).unwrap_err(), Refusal::UntrustedKey);
    assert_eq!(Refusal::UntrustedKey.as_str(), "untrusted_key");
}

#[test]
fn bad_signatures() {
    let k = key();
    let keys = [k.public_key()];
    let d = display(&keys);
    let good = build(&k, &release());
    let mlen = good.checked.header.manifest_len as usize;

    let mut b = good.bytes.clone();
    b[OFFSET_SIGNATURE + 10] ^= 0x04;
    assert_eq!(on_form(&d, &b).unwrap_err(), Refusal::BadSignature);

    let mut b = good.bytes.clone();
    b[HEADER_LEN + mlen / 2] ^= 0x01;
    assert_eq!(on_form(&d, &b).unwrap_err(), Refusal::BadSignature);

    let mut b = good.bytes.clone();
    b[OFFSET_SIGNATURE..HEADER_LEN].fill(0);
    assert_eq!(on_form(&d, &b).unwrap_err(), Refusal::BadSignature);

    // Signed with the right key over the same bytes, but without the fixed text in front.
    let m = &good.bytes[HEADER_LEN..HEADER_LEN + mlen];
    let mut message = good.bytes[..SIGNED_HEADER_LEN].to_vec();
    message.extend_from_slice(m);
    let mut b = good.bytes.clone();
    b[OFFSET_SIGNATURE..HEADER_LEN].copy_from_slice(&k.sign(&message).unwrap());
    assert_eq!(on_form(&d, &b).unwrap_err(), Refusal::BadSignature);
    assert_eq!(Refusal::BadSignature.as_str(), "bad_signature");
}

#[test]
fn bad_format_manifests() {
    let k = key();
    let keys = [k.public_key()];
    let d = display(&keys);
    let refused = |bytes: &[u8]| on_form(&d, &package(&k, bytes, &image(), &[])).unwrap_err();
    assert_eq!(refused(b"not json at all"), Refusal::BadFormat);
    for field in [
        "device_type",
        "hardware_revisions",
        "firmware_version",
        "security_counter",
        "image_size",
        "image_sha256",
    ] {
        let mut v = manifest_value();
        v.as_object_mut().unwrap().remove(field);
        assert_eq!(refused(&json_bytes(&v)), Refusal::BadFormat, "{field}");
    }
    let mut v = manifest_value();
    v["security_counter"] = json!("3");
    assert_eq!(refused(&json_bytes(&v)), Refusal::BadFormat);
    let text = String::from_utf8(json_bytes(&manifest_value())).unwrap();
    let n = image().len();
    for bad in [format!("-{n}"), format!("{n}.5")] {
        let m = text.replace(&format!("\"image_size\":{n}"), &format!("\"image_size\":{bad}"));
        assert_eq!(refused(m.as_bytes()), Refusal::BadFormat, "{bad}");
    }
    // The manifest passed must be the header's length.
    let built = build(&k, &release());
    let header: [u8; HEADER_LEN] = built.bytes[..HEADER_LEN].try_into().unwrap();
    let h = d.check_header(&header).unwrap();
    assert_eq!(
        d.check_manifest(&h, b"{}", built.bytes.len() as u64),
        Err(Refusal::BadFormat)
    );
}

#[test]
fn unknown_fields_and_usb_install_are_ignored() {
    let k = key();
    let keys = [k.public_key()];
    let d = display(&keys);
    let mut v = manifest_value();
    v["future"] = json!({"x": [1, 2]});
    // A usb_install the strict checker would refuse.
    v["usb_install"] = json!({"chip": "esp32s3"});
    assert!(on_form(&d, &form_for(&k, &v)).is_ok());
}

#[test]
fn wrong_device_type() {
    let k = key();
    let keys = [k.public_key()];
    let d = display(&keys);
    for other in ["llm-usage-displaz", "LLM-usage-display", "llm-usage-display2"] {
        let mut v = manifest_value();
        v["device_type"] = json!(other);
        assert_eq!(
            on_form(&d, &form_for(&k, &v)).unwrap_err(),
            Refusal::WrongDeviceType,
            "{other}"
        );
    }
    assert_eq!(Refusal::WrongDeviceType.as_str(), "wrong_device_type");
}

#[test]
fn wrong_hardware() {
    let k = key();
    let keys = [k.public_key()];
    let d = display(&keys);
    let mut v = manifest_value();
    v["hardware_revisions"] = json!(["waveshare-amoled-1.8-v1"]);
    assert_eq!(
        on_form(&d, &form_for(&k, &v)).unwrap_err(),
        Refusal::WrongHardware
    );
    // The display's revision as the prefix of a longer one doesn't count.
    v["hardware_revisions"] = json!([format!("{HARDWARE}-b")]);
    assert_eq!(
        on_form(&d, &form_for(&k, &v)).unwrap_err(),
        Refusal::WrongHardware
    );
    assert_eq!(Refusal::WrongHardware.as_str(), "wrong_hardware");
}

#[test]
fn downgrade() {
    let k = key();
    let keys = [k.public_key()];
    let d = display(&keys);
    let mut v = manifest_value();
    v["security_counter"] = json!(2);
    assert_eq!(on_form(&d, &form_for(&k, &v)).unwrap_err(), Refusal::Downgrade);
    // Equal to the floor, with an older version, is allowed.
    v["security_counter"] = json!(3);
    v["firmware_version"] = json!("1.0.0");
    assert!(on_form(&d, &form_for(&k, &v)).is_ok());
    assert_eq!(Refusal::Downgrade.as_str(), "downgrade");
}

#[test]
fn a_key_set_that_drops_a_trusted_key_needs_a_higher_counter() {
    let (maker, recovery, new) = (key(), key(), key());
    let hex = |k: &SigningKey| json!(k.public_key().to_hex());
    // After a release that retired `new` with counter 3, the display trusts maker and recovery.
    let keys = [maker.public_key(), recovery.public_key()];
    let d = display(&keys);

    // A same-counter replay of an older release whose key_set names `new` again (and so leaves
    // out recovery) is refused.
    let mut replay = manifest_value();
    replay["security_counter"] = json!(3);
    replay["firmware_version"] = json!("1.1.0");
    replay["key_set"] = json!([hex(&maker), hex(&new)]);
    assert_eq!(
        on_form(&d, &form_for(&maker, &replay)).unwrap_err(),
        Refusal::Downgrade
    );

    // A release that removes a key, with the counter raised, is accepted.
    let mut retire = manifest_value();
    retire["security_counter"] = json!(4);
    retire["key_set"] = json!([hex(&recovery), hex(&new)]);
    assert!(on_form(&d, &form_for(&maker, &retire)).is_ok());

    // A key_set that only adds keys, or names the same ones in another order, at the same counter.
    let mut adds = manifest_value();
    adds["security_counter"] = json!(3);
    adds["key_set"] = json!([hex(&recovery), hex(&maker), hex(&new)]);
    assert!(on_form(&d, &form_for(&maker, &adds)).is_ok());
    adds["key_set"] = json!([hex(&recovery), hex(&maker)]);
    assert!(on_form(&d, &form_for(&maker, &adds)).is_ok());
    // And no key_set at all changes nothing.
    let mut plain = manifest_value();
    plain["security_counter"] = json!(3);
    assert!(on_form(&d, &form_for(&maker, &plain)).is_ok());
}

#[test]
fn too_large_and_length_mismatch() {
    let k = key();
    let keys = [k.public_key()];
    let small = Display {
        slot_bytes: image().len() as u64 - 1,
        ..display(&keys)
    };
    let form = build(&k, &release()).bytes;
    assert_eq!(on_form(&small, &form).unwrap_err(), Refusal::TooLarge);
    let exact = Display {
        slot_bytes: image().len() as u64,
        ..display(&keys)
    };
    assert!(on_form(&exact, &form).is_ok());

    // A Content-Length that disagrees with the manifest's lengths.
    let d = display(&keys);
    let header: [u8; HEADER_LEN] = form[..HEADER_LEN].try_into().unwrap();
    let h = d.check_header(&header).unwrap();
    let mlen = h.header.manifest_len as usize;
    let manifest = &form[HEADER_LEN..HEADER_LEN + mlen];
    for wrong in [form.len() as u64 - 1, form.len() as u64 + 1] {
        assert_eq!(d.check_manifest(&h, manifest, wrong), Err(Refusal::BadFormat));
    }
    assert!(d.check_manifest(&h, manifest, form.len() as u64).is_ok());
}

#[test]
fn every_reason_code_is_the_documents() {
    let all = [
        (Refusal::NoTrustedKeys, "no_trusted_keys"),
        (Refusal::TooLarge, "too_large"),
        (Refusal::BadFormat, "bad_format"),
        (Refusal::UntrustedKey, "untrusted_key"),
        (Refusal::BadSignature, "bad_signature"),
        (Refusal::WrongDeviceType, "wrong_device_type"),
        (Refusal::WrongHardware, "wrong_hardware"),
        (Refusal::Downgrade, "downgrade"),
    ];
    for (r, s) in all {
        assert_eq!(r.as_str(), s);
        assert_eq!(r.to_string(), s);
    }
}
