//! Signing key files: made, written, opened, and refused when wrong.

use firmware_package::keyfile::{self, KeyFileError, MIN_PASSPHRASE_CHARS};
use serde_json::Value;

const PASSPHRASE: &str = "correct horse battery staple";

#[test]
fn a_key_file_opens_with_its_passphrase_and_signs() {
    let (key, file) = keyfile::generate(PASSPHRASE).unwrap();
    assert_eq!(keyfile::public_key(&file).unwrap(), key.public_key());
    let opened = keyfile::decrypt(&file, PASSPHRASE).unwrap();
    assert_eq!(opened.public_key(), key.public_key());
    let sig = opened.sign(b"hello").unwrap();
    ring::signature::UnparsedPublicKey::new(&ring::signature::ECDSA_P256_SHA256_FIXED, key.public_key().0)
        .verify(b"hello", &sig)
        .unwrap();

    // The layout, as documented.
    let v: Value = serde_json::from_slice(&file).unwrap();
    assert_eq!(v["format"], "display-hub.signing-key");
    assert_eq!(v["version"], 1);
    assert_eq!(v["kdf"], "pbkdf2-hmac-sha256");
    assert_eq!(v["kdf_rounds"], 600_000);
    assert_eq!(v["cipher"], "aes-256-gcm");
    assert_eq!(v["public_key"], key.public_key().to_hex());
    assert_eq!(v["salt"].as_str().unwrap().len(), 32);
    assert_eq!(v["nonce"].as_str().unwrap().len(), 24);
    assert_eq!(v.as_object().unwrap().len(), 9);
    assert!(file.ends_with(b"\n"));
    // The private key is nowhere in the clear: the file never holds the PKCS#8 prefix.
    let text = String::from_utf8(file.clone()).unwrap();
    assert!(!text.contains("308187020100"));
}

#[test]
fn the_passphrase_is_normalised() {
    // "é" composed, and as "e" with a combining accent: the same passphrase to a person.
    let composed = "caf\u{e9} au lait, please";
    let decomposed = "cafe\u{301} au lait, please";
    let (_, file) = keyfile::generate(composed).unwrap();
    assert!(keyfile::decrypt(&file, decomposed).is_ok());
}

#[test]
fn a_wrong_passphrase_is_refused() {
    let (_, file) = keyfile::generate(PASSPHRASE).unwrap();
    assert!(matches!(
        keyfile::decrypt(&file, "correct horse battery stapler"),
        Err(KeyFileError::WrongPassphraseOrChanged)
    ));
}

#[test]
fn a_short_passphrase_is_refused() {
    let short = "a".repeat(MIN_PASSPHRASE_CHARS - 1);
    assert!(matches!(
        keyfile::generate(&short),
        Err(KeyFileError::ShortPassphrase)
    ));
    // Counted in characters, not bytes.
    assert!(matches!(
        keyfile::check_passphrase(&"ā".repeat(MIN_PASSPHRASE_CHARS - 1)),
        Err(KeyFileError::ShortPassphrase)
    ));
    assert!(keyfile::check_passphrase(&"ā".repeat(MIN_PASSPHRASE_CHARS)).is_ok());
    assert!(matches!(
        keyfile::check_passphrase(&"a".repeat(keyfile::MAX_PASSPHRASE_BYTES + 1)),
        Err(KeyFileError::LongPassphrase)
    ));
}

/// The file with one member replaced.
fn edit(file: &[u8], member: &str, value: Value) -> Vec<u8> {
    let mut v: Value = serde_json::from_slice(file).unwrap();
    v[member] = value;
    serde_json::to_vec_pretty(&v).unwrap()
}

fn flip_hex(s: &str, at: usize) -> String {
    let mut b = s.as_bytes().to_vec();
    b[at] = if b[at] == b'0' { b'1' } else { b'0' };
    String::from_utf8(b).unwrap()
}

#[test]
fn a_tampered_file_is_refused() {
    let (_, file) = keyfile::generate(PASSPHRASE).unwrap();
    let v: Value = serde_json::from_slice(&file).unwrap();
    let s = |m: &str| v[m].as_str().unwrap().to_string();

    // Every encrypted, authenticated or bound member: refused as if the passphrase were wrong.
    let sealed = s("encrypted_private_key");
    for (member, value) in [
        ("encrypted_private_key", flip_hex(&sealed, 10)),
        ("encrypted_private_key", flip_hex(&sealed, sealed.len() - 1)),
        ("nonce", flip_hex(&s("nonce"), 0)),
        ("salt", flip_hex(&s("salt"), 5)),
    ] {
        let changed = edit(&file, member, Value::String(value));
        assert!(
            matches!(
                keyfile::decrypt(&changed, PASSPHRASE),
                Err(KeyFileError::WrongPassphraseOrChanged)
            ),
            "{member}"
        );
    }
    // Another valid public key in its place.
    let (other, _) = firmware_package::SigningKey::generate().unwrap();
    let swapped = edit(&file, "public_key", Value::String(other.public_key().to_hex()));
    assert_eq!(keyfile::public_key(&swapped).unwrap(), other.public_key());
    assert!(matches!(
        keyfile::decrypt(&swapped, PASSPHRASE),
        Err(KeyFileError::WrongPassphraseOrChanged)
    ));
    // Not a valid point.
    let broken = edit(
        &file,
        "public_key",
        Value::String(flip_hex(&s("public_key"), 129)),
    );
    assert!(matches!(
        keyfile::public_key(&broken),
        Err(KeyFileError::Damaged(_))
    ));
    // Other parameters.
    let rounds = edit(&file, "kdf_rounds", Value::from(1000));
    assert!(matches!(
        keyfile::decrypt(&rounds, PASSPHRASE),
        Err(KeyFileError::Damaged(_))
    ));
    let cipher = edit(&file, "cipher", Value::from("aes-128-gcm"));
    assert!(matches!(
        keyfile::decrypt(&cipher, PASSPHRASE),
        Err(KeyFileError::Damaged(_))
    ));
    let version = edit(&file, "version", Value::from(2));
    assert!(matches!(
        keyfile::decrypt(&version, PASSPHRASE),
        Err(KeyFileError::UnsupportedVersion(2))
    ));
    let format = edit(&file, "format", Value::from("something-else"));
    assert!(matches!(
        keyfile::public_key(&format),
        Err(KeyFileError::NotAKeyFile(_))
    ));
    let extra = edit(&file, "extra", Value::from(1));
    assert!(matches!(
        keyfile::public_key(&extra),
        Err(KeyFileError::Damaged(_))
    ));
    // A repeated member, and something that isn't a key file at all.
    let text = String::from_utf8(file.clone()).unwrap();
    let repeated = text.replacen('{', "{\n  \"version\": 1,", 1);
    assert!(matches!(
        keyfile::public_key(repeated.as_bytes()),
        Err(KeyFileError::NotAKeyFile(_))
    ));
    assert!(matches!(
        keyfile::public_key(b"DHFW"),
        Err(KeyFileError::NotAKeyFile(_))
    ));
    assert!(matches!(
        keyfile::public_key(&vec![b' '; 5000]),
        Err(KeyFileError::NotAKeyFile(_))
    ));
}

#[test]
fn write_new_never_overwrites() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("maker.dhkey");
    keyfile::write_new(&path, b"first").unwrap();
    assert!(matches!(
        keyfile::write_new(&path, b"second"),
        Err(KeyFileError::Exists(_))
    ));
    assert_eq!(std::fs::read(&path).unwrap(), b"first");
    assert_eq!(keyfile::read(&path).unwrap(), b"first");
}

#[cfg(unix)]
#[test]
fn a_key_file_is_owner_only_on_unix() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("maker.dhkey");
    let (_, file) = keyfile::generate(PASSPHRASE).unwrap();
    keyfile::write_new(&path, &file).unwrap();
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    assert!(keyfile::decrypt(&keyfile::read(&path).unwrap(), PASSPHRASE).is_ok());
}
