//! A maker's signing key file (`.dhkey`), always protected by a passphrase.
//!
//! # Layout, version 1
//!
//! A key file is one UTF-8 JSON object, written with a trailing newline. It is read under the same
//! strict JSON rules as a manifest (no byte-order mark, no repeated member names, whole numbers only)
//! and, unlike a manifest, refuses members it doesn't know: a change to the layout gets a new
//! `version`. Hex is lower-case.
//!
//! ```json
//! {
//!   "format": "display-hub.signing-key",
//!   "version": 1,
//!   "public_key": "04…",
//!   "kdf": "pbkdf2-hmac-sha256",
//!   "kdf_rounds": 600000,
//!   "salt": "…",
//!   "cipher": "aes-256-gcm",
//!   "nonce": "…",
//!   "encrypted_private_key": "…"
//! }
//! ```
//!
//! | Member | Value |
//! |---|---|
//! | `format` | Always `display-hub.signing-key`. |
//! | `version` | Always `1` for this layout. |
//! | `public_key` | The key's public half, 130 hex digits (65 bytes, uncompressed). In the clear, so the file can name its key without the passphrase. |
//! | `kdf`, `kdf_rounds` | Always `pbkdf2-hmac-sha256` and `600000`. Written out so the file says how it is protected; version 1 accepts no other values. |
//! | `salt` | 16 random bytes, 32 hex digits, new for every file. |
//! | `cipher` | Always `aes-256-gcm`. |
//! | `nonce` | 12 random bytes, 24 hex digits, new for every file. |
//! | `encrypted_private_key` | The private key as unencrypted PKCS#8 v1 (as `ring` makes it), encrypted with AES-256-GCM, then the 16-byte tag, in hex. |
//!
//! The AES key is PBKDF2-HMAC-SHA256 of the passphrase, with the salt and 600,000 rounds, 32 bytes
//! long. The passphrase is first put into Unicode NFC form, so the same passphrase typed on different
//! systems gives the same key; it must be at least 12 characters. The associated data, which the tag
//! covers but which isn't encrypted, is the ASCII text `display-hub.signing-key.v1` and a zero byte,
//! then the 65-byte public key, then `kdf_rounds` as 4 little-endian bytes, then the 16-byte salt. So
//! changing the public key, the rounds or the salt in the file makes it fail to open, just as a wrong
//! passphrase does. After opening, the private key's own public half must equal `public_key`.
//!
//! The file is created new (never over an existing file) and readable and writable by its owner
//! only: mode `0600` on Unix, and on Windows a protected access list (one that inherits nothing from
//! its folder) granting the current user alone.

use std::io::{Read, Write};
use std::num::NonZeroU32;
use std::path::Path;

use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM, NONCE_LEN as RING_NONCE_LEN};
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;
use zeroize::Zeroizing;

use crate::hex;
use crate::json::{parse_object, JsonProblem};
use crate::key::{PublicKey, SigningKey};
use crate::limits::PUBLIC_KEY_LEN;

pub const KEY_FILE_FORMAT: &str = "display-hub.signing-key";
pub const KEY_FILE_VERSION: u64 = 1;
pub const KDF_NAME: &str = "pbkdf2-hmac-sha256";
pub const PBKDF2_ROUNDS: u32 = 600_000;
pub const SALT_LEN: usize = 16;
pub const CIPHER_NAME: &str = "aes-256-gcm";
pub const NONCE_LEN: usize = RING_NONCE_LEN;
/// AES-256's key length.
pub const AES_KEY_LEN: usize = 32;
/// AES-GCM's tag length.
pub const TAG_LEN: usize = 16;
/// The shortest passphrase, in characters (Unicode scalar values) after NFC.
pub const MIN_PASSPHRASE_CHARS: usize = 12;
/// The longest passphrase, in bytes.
pub const MAX_PASSPHRASE_BYTES: usize = 1024;
/// The largest key file read. A version 1 file is about 650 bytes.
pub const MAX_KEY_FILE_BYTES: u64 = 4096;
/// The largest PKCS#8 document accepted inside a key file. `ring`'s P-256 keys are 138 bytes.
pub const MAX_PKCS8_LEN: usize = 512;
/// The text at the front of the associated data, with its zero byte.
pub const AAD_CONTEXT: &[u8] = b"display-hub.signing-key.v1\0";
/// A key file is one flat object.
const KEY_FILE_DEPTH: usize = 1;

#[derive(Debug, thiserror::Error)]
pub enum KeyFileError {
    #[error("the passphrase must be at least {MIN_PASSPHRASE_CHARS} characters")]
    ShortPassphrase,
    #[error("the passphrase is longer than {MAX_PASSPHRASE_BYTES} bytes")]
    LongPassphrase,
    #[error("this isn't a Display Hub signing key file: {0}")]
    NotAKeyFile(String),
    #[error("this key file is version {0}; this tool knows version {KEY_FILE_VERSION}")]
    UnsupportedVersion(u64),
    #[error("couldn't open the key: the passphrase is wrong, or the key file has been changed")]
    WrongPassphraseOrChanged,
    #[error("the key file is damaged: {0}")]
    Damaged(String),
    #[error("{0} already exists; choose another name, so no key is overwritten")]
    Exists(String),
    #[error("the system's random source failed")]
    Random,
    #[error("{0}")]
    Io(String),
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct KeyFileJson {
    format: String,
    version: u64,
    public_key: String,
    kdf: String,
    kdf_rounds: u64,
    salt: String,
    cipher: String,
    nonce: String,
    encrypted_private_key: String,
}

/// Refuses a passphrase too short or too long for a key file.
pub fn check_passphrase(passphrase: &str) -> Result<(), KeyFileError> {
    if passphrase.len() > MAX_PASSPHRASE_BYTES {
        return Err(KeyFileError::LongPassphrase);
    }
    if passphrase.nfc().count() < MIN_PASSPHRASE_CHARS {
        return Err(KeyFileError::ShortPassphrase);
    }
    Ok(())
}

/// A new signing key, and the contents of a key file holding it under `passphrase`.
pub fn generate(passphrase: &str) -> Result<(SigningKey, Vec<u8>), KeyFileError> {
    check_passphrase(passphrase)?;
    let (key, pkcs8) = SigningKey::generate().map_err(|_| KeyFileError::Random)?;
    let file = encrypt(&pkcs8, &key.public_key(), passphrase)?;
    Ok((key, file))
}

/// The contents of a key file holding `pkcs8`, whose public half is `public`, under `passphrase`.
pub fn encrypt(pkcs8: &[u8], public: &PublicKey, passphrase: &str) -> Result<Vec<u8>, KeyFileError> {
    check_passphrase(passphrase)?;
    let rng = SystemRandom::new();
    let mut salt = [0u8; SALT_LEN];
    let mut nonce = [0u8; NONCE_LEN];
    rng.fill(&mut salt).map_err(|_| KeyFileError::Random)?;
    rng.fill(&mut nonce).map_err(|_| KeyFileError::Random)?;
    let cipher = aes_key(passphrase, &salt)?;
    let mut sealed = Zeroizing::new(pkcs8.to_vec());
    cipher
        .seal_in_place_append_tag(
            Nonce::assume_unique_for_key(nonce),
            Aad::from(associated_data(public, PBKDF2_ROUNDS, &salt)),
            &mut *sealed,
        )
        .map_err(|_| KeyFileError::Damaged("encryption failed".into()))?;
    let json = KeyFileJson {
        format: KEY_FILE_FORMAT.into(),
        version: KEY_FILE_VERSION,
        public_key: public.to_hex(),
        kdf: KDF_NAME.into(),
        kdf_rounds: u64::from(PBKDF2_ROUNDS),
        salt: hex::encode(&salt),
        cipher: CIPHER_NAME.into(),
        nonce: hex::encode(&nonce),
        encrypted_private_key: hex::encode(&sealed),
    };
    let mut out = serde_json::to_vec_pretty(&json).expect("a key file always serialises");
    out.push(b'\n');
    Ok(out)
}

/// The public key a key file holds. Needs no passphrase.
pub fn public_key(file: &[u8]) -> Result<PublicKey, KeyFileError> {
    Ok(parse(file)?.public_key)
}

/// Opens a key file with its passphrase.
pub fn decrypt(file: &[u8], passphrase: &str) -> Result<SigningKey, KeyFileError> {
    check_passphrase(passphrase)?;
    let parsed = parse(file)?;
    let cipher = aes_key(passphrase, &parsed.salt)?;
    let mut buf = Zeroizing::new(parsed.sealed);
    let pkcs8 = cipher
        .open_in_place(
            Nonce::assume_unique_for_key(parsed.nonce),
            Aad::from(associated_data(&parsed.public_key, PBKDF2_ROUNDS, &parsed.salt)),
            &mut buf,
        )
        .map_err(|_| KeyFileError::WrongPassphraseOrChanged)?;
    let key = SigningKey::from_pkcs8(pkcs8)
        .map_err(|e| KeyFileError::Damaged(format!("the private key isn't a P-256 key ({e})")))?;
    if key.public_key() != parsed.public_key {
        return Err(KeyFileError::Damaged(
            "the private key doesn't match the public key written in the file".into(),
        ));
    }
    Ok(key)
}

/// Writes a new key file: never over an existing file, and owner-only from the moment it exists.
pub fn write_new(path: &Path, contents: &[u8]) -> Result<(), KeyFileError> {
    let mut f = create_owner_only(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::AlreadyExists {
            KeyFileError::Exists(path.display().to_string())
        } else {
            KeyFileError::Io(format!("couldn't create {}: {e}", path.display()))
        }
    })?;
    let written = f.write_all(contents).and_then(|()| f.sync_all());
    if let Err(e) = written {
        drop(f);
        let _ = std::fs::remove_file(path);
        return Err(KeyFileError::Io(format!(
            "couldn't write {}: {e}",
            path.display()
        )));
    }
    Ok(())
}

/// Creates `path`, failing if it exists, readable and writable by its owner only (`0600`).
#[cfg(not(windows))]
fn create_owner_only(path: &Path) -> std::io::Result<std::fs::File> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)
}

/// Creates `path`, failing if it exists, with a protected access list that grants the current user
/// full access and nobody else any. The list is part of the call that creates the file, so there is
/// no moment when the file exists with its folder's inherited permissions.
#[cfg(windows)]
fn create_owner_only(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::FromRawHandle;
    use std::ptr::null_mut;

    use windows_sys::Win32::Foundation::{
        CloseHandle, LocalFree, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::{
        GetTokenInformation, TokenUser, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
    };
    use windows_sys::Win32::Storage::FileSystem::{CreateFileW, CREATE_NEW, FILE_ATTRIBUTE_NORMAL};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    let last = std::io::Error::last_os_error;
    // SAFETY: each call gets valid pointers to buffers of the sizes it is told, every handle and
    // allocation is released on every path, and the SID is read only while `buf` holds it.
    unsafe {
        // The current user's SID, as text.
        let mut token: HANDLE = null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return Err(last());
        }
        let mut len = 0u32;
        GetTokenInformation(token, TokenUser, null_mut(), 0, &mut len);
        // u64s, so the TOKEN_USER at the front is suitably aligned.
        let mut buf = vec![0u64; (len as usize).div_ceil(8).max(1)];
        let got = GetTokenInformation(token, TokenUser, buf.as_mut_ptr().cast(), len, &mut len);
        let err = last();
        CloseHandle(token);
        if got == 0 {
            return Err(err);
        }
        let user = &*(buf.as_ptr() as *const TOKEN_USER);
        let mut sid_text: *mut u16 = null_mut();
        if ConvertSidToStringSidW(user.User.Sid, &mut sid_text) == 0 {
            return Err(last());
        }
        let mut n = 0;
        while *sid_text.add(n) != 0 {
            n += 1;
        }
        let sid = String::from_utf16_lossy(std::slice::from_raw_parts(sid_text, n));
        LocalFree(sid_text.cast());

        // "P" protects the list from inheriting; the one entry gives this user full access.
        let sddl: Vec<u16> = format!("D:P(A;;FA;;;{sid})")
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let mut descriptor: PSECURITY_DESCRIPTOR = null_mut();
        if ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            null_mut(),
        ) == 0
        {
            return Err(last());
        }
        let attributes = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor,
            bInheritHandle: 0,
        };
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let handle = CreateFileW(
            wide.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            0,
            &attributes,
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL,
            null_mut(),
        );
        let err = last();
        LocalFree(descriptor);
        if handle == INVALID_HANDLE_VALUE {
            return Err(err);
        }
        Ok(std::fs::File::from_raw_handle(handle))
    }
}

/// Reads a key file's contents, refusing anything far larger than a key file.
pub fn read(path: &Path) -> Result<Vec<u8>, KeyFileError> {
    let f = std::fs::File::open(path)
        .map_err(|e| KeyFileError::Io(format!("couldn't open {}: {e}", path.display())))?;
    let mut out = Vec::new();
    f.take(MAX_KEY_FILE_BYTES + 1)
        .read_to_end(&mut out)
        .map_err(|e| KeyFileError::Io(format!("couldn't read {}: {e}", path.display())))?;
    if out.len() as u64 > MAX_KEY_FILE_BYTES {
        return Err(KeyFileError::NotAKeyFile("it is far too large".into()));
    }
    Ok(out)
}

struct Parsed {
    public_key: PublicKey,
    salt: [u8; SALT_LEN],
    nonce: [u8; NONCE_LEN],
    sealed: Vec<u8>,
}

fn parse(file: &[u8]) -> Result<Parsed, KeyFileError> {
    if file.len() as u64 > MAX_KEY_FILE_BYTES {
        return Err(KeyFileError::NotAKeyFile("it is far too large".into()));
    }
    let object = parse_object(file, KEY_FILE_DEPTH)
        .map_err(|p: JsonProblem| KeyFileError::NotAKeyFile(p.to_string()))?;
    if object.get("format").and_then(|v| v.as_str()) != Some(KEY_FILE_FORMAT) {
        return Err(KeyFileError::NotAKeyFile(format!(
            "its format isn't {KEY_FILE_FORMAT:?}"
        )));
    }
    match object.get("version").and_then(|v| v.as_u64()) {
        Some(KEY_FILE_VERSION) => {}
        Some(other) => return Err(KeyFileError::UnsupportedVersion(other)),
        None => return Err(KeyFileError::NotAKeyFile("it has no version".into())),
    }
    let json: KeyFileJson = serde_json::from_value(serde_json::Value::Object(object))
        .map_err(|e| KeyFileError::Damaged(e.to_string()))?;
    let damaged = |what: &str| KeyFileError::Damaged(what.to_string());
    if json.kdf != KDF_NAME || json.kdf_rounds != u64::from(PBKDF2_ROUNDS) {
        return Err(damaged(
            "its key derivation isn't pbkdf2-hmac-sha256 with 600000 rounds",
        ));
    }
    if json.cipher != CIPHER_NAME {
        return Err(damaged("its cipher isn't aes-256-gcm"));
    }
    let public_key = PublicKey::from_hex(&json.public_key)
        .filter(PublicKey::is_valid_point)
        .ok_or_else(|| damaged("its public_key isn't a valid P-256 key in hex"))?;
    debug_assert_eq!(public_key.as_bytes().len(), PUBLIC_KEY_LEN);
    let salt =
        hex::decode_exact::<SALT_LEN>(&json.salt).ok_or_else(|| damaged("its salt isn't 32 hex digits"))?;
    let nonce = hex::decode_exact::<NONCE_LEN>(&json.nonce)
        .ok_or_else(|| damaged("its nonce isn't 24 hex digits"))?;
    let sealed = hex::decode(&json.encrypted_private_key)
        .filter(|b| b.len() > TAG_LEN && b.len() <= MAX_PKCS8_LEN + TAG_LEN)
        .ok_or_else(|| damaged("its encrypted_private_key isn't the right length of hex"))?;
    Ok(Parsed {
        public_key,
        salt,
        nonce,
        sealed,
    })
}

fn aes_key(passphrase: &str, salt: &[u8; SALT_LEN]) -> Result<LessSafeKey, KeyFileError> {
    let normalised = Zeroizing::new(passphrase.nfc().collect::<String>());
    let mut derived = Zeroizing::new([0u8; AES_KEY_LEN]);
    ring::pbkdf2::derive(
        ring::pbkdf2::PBKDF2_HMAC_SHA256,
        NonZeroU32::new(PBKDF2_ROUNDS).expect("the round count isn't zero"),
        salt,
        normalised.as_bytes(),
        &mut *derived,
    );
    let unbound = UnboundKey::new(&AES_256_GCM, &*derived)
        .map_err(|_| KeyFileError::Damaged("the derived key is the wrong length".into()))?;
    Ok(LessSafeKey::new(unbound))
}

fn associated_data(public: &PublicKey, rounds: u32, salt: &[u8; SALT_LEN]) -> Vec<u8> {
    let mut out = Vec::with_capacity(AAD_CONTEXT.len() + PUBLIC_KEY_LEN + 4 + SALT_LEN);
    out.extend_from_slice(AAD_CONTEXT);
    out.extend_from_slice(public.as_bytes());
    out.extend_from_slice(&rounds.to_le_bytes());
    out.extend_from_slice(salt);
    out
}
