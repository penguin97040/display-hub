//! Public keys, key ids and fingerprints, and the signing key a maker holds.

use ring::rand::SystemRandom;
use ring::signature::{EcdsaKeyPair, KeyPair, ECDSA_P256_SHA256_FIXED_SIGNING};
use zeroize::Zeroizing;

use crate::hex;
use crate::limits::{
    FINGERPRINT_GROUP_LEN, FINGERPRINT_HEX_LEN, PUBLIC_KEY_LEN, PUBLIC_KEY_PREFIX, SHA256_LEN, SIGNATURE_LEN,
};

/// An ECDSA P-256 public key in its 65-byte uncompressed form. Holding one says nothing about
/// whether it is a valid point: see [`PublicKey::is_valid_point`].
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct PublicKey(pub [u8; PUBLIC_KEY_LEN]);

impl PublicKey {
    pub fn as_bytes(&self) -> &[u8; PUBLIC_KEY_LEN] {
        &self.0
    }

    /// 130 lower-case hex digits, as written in JSON.
    pub fn to_hex(&self) -> String {
        hex::encode(&self.0)
    }

    /// From 130 lower-case hex digits. The point is not checked here.
    pub fn from_hex(s: &str) -> Option<Self> {
        hex::decode_exact::<PUBLIC_KEY_LEN>(s).map(Self)
    }

    /// Whether this is a valid P-256 point: the `0x04` prefix, both coordinates in range and on the
    /// curve, and not the point at infinity. P-256 has a cofactor of one, so that is every check a
    /// public key needs.
    ///
    /// `ring` has no call that only validates a key, so this does a throwaway ECDH agreement with a
    /// fresh private key: `ring` validates the peer key first, with the same parser its ECDSA
    /// verification uses, and refuses the agreement if the key is invalid.
    pub fn is_valid_point(&self) -> bool {
        use ring::agreement::{agree_ephemeral, EphemeralPrivateKey, UnparsedPublicKey, ECDH_P256};
        if self.0[0] != PUBLIC_KEY_PREFIX {
            return false;
        }
        let Ok(mine) = EphemeralPrivateKey::generate(&ECDH_P256, &SystemRandom::new()) else {
            return false;
        };
        agree_ephemeral(mine, &UnparsedPublicKey::new(&ECDH_P256, &self.0), |_| ()).is_ok()
    }

    pub fn key_id(&self) -> KeyId {
        KeyId(sha256(&self.0))
    }

    pub fn fingerprint(&self) -> String {
        self.key_id().fingerprint()
    }
}

impl std::fmt::Debug for PublicKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "PublicKey({})", self.fingerprint())
    }
}

/// The SHA-256 of a public key's 65 bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct KeyId(pub [u8; SHA256_LEN]);

impl KeyId {
    /// 64 lower-case hex digits.
    pub fn to_hex(&self) -> String {
        hex::encode(&self.0)
    }

    /// The first 32 hex digits of the key id in groups of four, joined by `-`
    /// (`3f9a-01c2-…`): what people compare.
    pub fn fingerprint(&self) -> String {
        let digits = self.to_hex();
        digits.as_bytes()[..FINGERPRINT_HEX_LEN]
            .chunks(FINGERPRINT_GROUP_LEN)
            .map(|g| std::str::from_utf8(g).expect("hex is ASCII"))
            .collect::<Vec<_>>()
            .join("-")
    }
}

pub(crate) fn sha256(bytes: &[u8]) -> [u8; SHA256_LEN] {
    let d = ring::digest::digest(&ring::digest::SHA256, bytes);
    d.as_ref().try_into().expect("SHA-256 is 32 bytes")
}

/// A maker's private signing key, in memory. Made with [`SigningKey::generate`], or opened from a key
/// file with [`crate::keyfile::decrypt`].
pub struct SigningKey {
    pair: EcdsaKeyPair,
    public: PublicKey,
    rng: SystemRandom,
}

impl SigningKey {
    /// A new key from the operating system's random source, and its PKCS#8 form for a key file.
    pub fn generate() -> Result<(Self, Zeroizing<Vec<u8>>), ring::error::Unspecified> {
        let rng = SystemRandom::new();
        let doc = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng)?;
        let pkcs8 = Zeroizing::new(doc.as_ref().to_vec());
        let key = Self::from_pkcs8(&pkcs8).map_err(|_| ring::error::Unspecified)?;
        Ok((key, pkcs8))
    }

    /// From an unencrypted PKCS#8 v1 P-256 key.
    pub fn from_pkcs8(pkcs8: &[u8]) -> Result<Self, ring::error::KeyRejected> {
        let rng = SystemRandom::new();
        let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8, &rng)?;
        let public = PublicKey(
            pair.public_key()
                .as_ref()
                .try_into()
                .expect("a P-256 public key is 65 bytes uncompressed"),
        );
        Ok(Self { pair, public, rng })
    }

    pub fn public_key(&self) -> PublicKey {
        self.public
    }

    /// The fixed 64-byte signature over the SHA-256 of `message`. `ring` uses a random nonce, so two
    /// signatures over the same message differ and both verify.
    pub fn sign(&self, message: &[u8]) -> Result<[u8; SIGNATURE_LEN], ring::error::Unspecified> {
        let sig = self.pair.sign(&self.rng, message)?;
        sig.as_ref().try_into().map_err(|_| ring::error::Unspecified)
    }
}

impl std::fmt::Debug for SigningKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SigningKey")
            .field("public", &self.public)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_generated_key_is_a_valid_point_and_others_are_not() {
        let (key, _) = SigningKey::generate().unwrap();
        let public = key.public_key();
        assert!(public.is_valid_point());
        let mut off_curve = public;
        off_curve.0[64] ^= 1;
        assert!(!off_curve.is_valid_point());
        let mut wrong_prefix = public;
        wrong_prefix.0[0] = 0x05;
        assert!(!wrong_prefix.is_valid_point());
        assert!(!PublicKey([0; PUBLIC_KEY_LEN]).is_valid_point());
        let mut zeros = [0u8; PUBLIC_KEY_LEN];
        zeros[0] = PUBLIC_KEY_PREFIX;
        assert!(!PublicKey(zeros).is_valid_point());
        // Coordinates at or above the field prime are refused too.
        let mut big = [0xffu8; PUBLIC_KEY_LEN];
        big[0] = PUBLIC_KEY_PREFIX;
        assert!(!PublicKey(big).is_valid_point());
    }

    #[test]
    fn key_ids_and_fingerprints() {
        let key = PublicKey([7; PUBLIC_KEY_LEN]);
        let id = key.key_id();
        assert_eq!(id.0, sha256(&[7; PUBLIC_KEY_LEN]));
        let hex = id.to_hex();
        assert_eq!(hex.len(), 64);
        let fp = key.fingerprint();
        assert_eq!(fp.len(), 32 + 7);
        assert_eq!(fp.replace('-', ""), hex[..32]);
        assert_eq!(fp.split('-').count(), 8);
        assert!(fp.split('-').all(|g| g.len() == 4));
    }

    #[test]
    fn hex_round_trip() {
        let (key, _) = SigningKey::generate().unwrap();
        let p = key.public_key();
        assert_eq!(PublicKey::from_hex(&p.to_hex()), Some(p));
        assert_eq!(PublicKey::from_hex(&p.to_hex().to_uppercase()), None);
    }

    #[test]
    fn signatures_differ_but_both_verify() {
        use ring::signature::{UnparsedPublicKey, ECDSA_P256_SHA256_FIXED};
        let (key, _) = SigningKey::generate().unwrap();
        let a = key.sign(b"message").unwrap();
        let b = key.sign(b"message").unwrap();
        assert_ne!(a, b);
        let v = UnparsedPublicKey::new(&ECDSA_P256_SHA256_FIXED, key.public_key().0);
        assert!(v.verify(b"message", &a).is_ok());
        assert!(v.verify(b"message", &b).is_ok());
    }
}
