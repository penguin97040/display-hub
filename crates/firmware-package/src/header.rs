//! The fixed 137-byte header, and the bytes the signature covers.

use ring::signature::{UnparsedPublicKey, ECDSA_P256_SHA256_FIXED};

use crate::key::PublicKey;
use crate::limits::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub manifest_len: u16,
    /// The key the package says it was signed with. A display never trusts it because it is here.
    pub public_key: PublicKey,
    pub signature: [u8; SIGNATURE_LEN],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum HeaderError {
    #[error("it doesn't start with DHFW, so it isn't a firmware package")]
    BadMagic,
    #[error("its format version is {0}; only version {FORMAT_VERSION} is known")]
    BadFormatVersion(u8),
    #[error("its signature algorithm is {0}; only {ALGORITHM_ECDSA_P256_SHA256} (ECDSA P-256 with SHA-256) is known")]
    BadAlgorithm(u8),
    #[error("its manifest length is {0}; it must be from {MIN_MANIFEST_LEN} to {MAX_MANIFEST_LEN}")]
    BadManifestLength(u16),
}

impl Header {
    /// Checks the magic, format version, algorithm and `manifest_len`. The public key is not checked
    /// here: a checker then checks it is a valid point, and a display compares it with its own keys.
    pub fn parse(bytes: &[u8; HEADER_LEN]) -> Result<Self, HeaderError> {
        if bytes[OFFSET_MAGIC..OFFSET_FORMAT_VERSION] != MAGIC {
            return Err(HeaderError::BadMagic);
        }
        if bytes[OFFSET_FORMAT_VERSION] != FORMAT_VERSION {
            return Err(HeaderError::BadFormatVersion(bytes[OFFSET_FORMAT_VERSION]));
        }
        if bytes[OFFSET_ALGORITHM] != ALGORITHM_ECDSA_P256_SHA256 {
            return Err(HeaderError::BadAlgorithm(bytes[OFFSET_ALGORITHM]));
        }
        let manifest_len = u16::from_le_bytes([bytes[OFFSET_MANIFEST_LEN], bytes[OFFSET_MANIFEST_LEN + 1]]);
        if !(MIN_MANIFEST_LEN..=MAX_MANIFEST_LEN).contains(&(manifest_len as usize)) {
            return Err(HeaderError::BadManifestLength(manifest_len));
        }
        Ok(Self {
            manifest_len,
            public_key: PublicKey(
                bytes[OFFSET_PUBLIC_KEY..OFFSET_SIGNATURE]
                    .try_into()
                    .expect("the key field is 65 bytes"),
            ),
            signature: bytes[OFFSET_SIGNATURE..HEADER_LEN]
                .try_into()
                .expect("the signature field is 64 bytes"),
        })
    }

    pub fn to_bytes(&self) -> [u8; HEADER_LEN] {
        let mut out = [0u8; HEADER_LEN];
        out[..SIGNED_HEADER_LEN].copy_from_slice(&self.signed_prefix());
        out[OFFSET_SIGNATURE..].copy_from_slice(&self.signature);
        out
    }

    /// Header bytes 0 to 72: everything before the signature.
    pub fn signed_prefix(&self) -> [u8; SIGNED_HEADER_LEN] {
        signed_prefix(self.manifest_len, &self.public_key)
    }
}

/// Header bytes 0 to 72 for a manifest of `manifest_len` bytes signed by `key`.
pub fn signed_prefix(manifest_len: u16, key: &PublicKey) -> [u8; SIGNED_HEADER_LEN] {
    let mut out = [0u8; SIGNED_HEADER_LEN];
    out[OFFSET_MAGIC..OFFSET_FORMAT_VERSION].copy_from_slice(&MAGIC);
    out[OFFSET_FORMAT_VERSION] = FORMAT_VERSION;
    out[OFFSET_ALGORITHM] = ALGORITHM_ECDSA_P256_SHA256;
    out[OFFSET_MANIFEST_LEN..OFFSET_PUBLIC_KEY].copy_from_slice(&manifest_len.to_le_bytes());
    out[OFFSET_PUBLIC_KEY..OFFSET_SIGNATURE].copy_from_slice(key.as_bytes());
    out
}

/// The bytes the signature is made over (it signs their SHA-256): the fixed text and its zero byte,
/// header bytes 0 to 72, then the manifest exactly as it is in the file.
pub fn signed_bytes(signed_prefix: &[u8; SIGNED_HEADER_LEN], manifest: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(SIGNATURE_CONTEXT.len() + SIGNED_HEADER_LEN + manifest.len());
    out.extend_from_slice(SIGNATURE_CONTEXT);
    out.extend_from_slice(signed_prefix);
    out.extend_from_slice(manifest);
    out
}

/// Whether `header`'s signature over its own signed bytes and `manifest` verifies with `key`.
///
/// A checker passes the header's own key. A display passes **its stored copy** of the key, which it
/// has already found to be equal to the header's: the header's bytes are signed over, so the two are
/// the same 65 bytes, but the key used to verify is never taken from the package.
pub fn verify_signature(key: &PublicKey, header: &Header, manifest: &[u8]) -> bool {
    let message = signed_bytes(&header.signed_prefix(), manifest);
    UnparsedPublicKey::new(&ECDSA_P256_SHA256_FIXED, key.as_bytes())
        .verify(&message, &header.signature)
        .is_ok()
}
