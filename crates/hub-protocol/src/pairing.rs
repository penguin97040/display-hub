//! The admin pairing string `hubd pair-admin` prints, and the desktop manager reads.
//!
//! `dh1.<payload>.<check>`: `payload` is base64url (no padding) of the port (2 bytes, big-endian),
//! the SHA-256 of the hub's public key (32 bytes), a one-time secret (16 bytes) and the host name
//! (UTF-8, the rest). `check` is the CRC-32 of the payload bytes, as 8 lower-case hex digits, so a
//! mistyped string is caught before anything is sent. Plain dots and base64url survive copying from
//! a terminal, and an IPv6 address needs no special quoting.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;

pub const PREFIX: &str = "dh1";
pub const SECRET_LEN: usize = 16;
pub const SPKI_HASH_LEN: usize = 32;
const MAX_HOST_LEN: usize = 253;

#[derive(Clone, PartialEq, Eq)]
pub struct PairingString {
    pub host: String,
    pub port: u16,
    /// SHA-256 of the hub certificate's SubjectPublicKeyInfo.
    pub spki_sha256: [u8; SPKI_HASH_LEN],
    pub secret: [u8; SECRET_LEN],
}

impl std::fmt::Debug for PairingString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PairingString")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("spki_sha256", &hex(&self.spki_sha256))
            .field("secret", &"[redacted]")
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PairingError {
    #[error("this isn't a Display Hub pairing string (it should start with \"dh1.\")")]
    NotPairing,
    #[error("the pairing string is incomplete or has a typing mistake; copy it again from the hub")]
    Damaged,
    #[error("the pairing string names no hub address")]
    NoHost,
}

impl PairingString {
    pub fn encode(&self) -> String {
        let mut bytes = Vec::with_capacity(2 + SPKI_HASH_LEN + SECRET_LEN + self.host.len());
        bytes.extend_from_slice(&self.port.to_be_bytes());
        bytes.extend_from_slice(&self.spki_sha256);
        bytes.extend_from_slice(&self.secret);
        bytes.extend_from_slice(self.host.as_bytes());
        format!(
            "{PREFIX}.{}.{:08x}",
            URL_SAFE_NO_PAD.encode(&bytes),
            crc32fast::hash(&bytes)
        )
    }

    /// Reads a pairing string, ignoring surrounding white space and line breaks a terminal added.
    pub fn decode(s: &str) -> Result<Self, PairingError> {
        let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
        let mut parts = s.split('.');
        if parts.next() != Some(PREFIX) {
            return Err(PairingError::NotPairing);
        }
        let (Some(payload), Some(check), None) = (parts.next(), parts.next(), parts.next()) else {
            return Err(PairingError::Damaged);
        };
        let bytes = URL_SAFE_NO_PAD
            .decode(payload)
            .map_err(|_| PairingError::Damaged)?;
        let check = u32::from_str_radix(check, 16).map_err(|_| PairingError::Damaged)?;
        if crc32fast::hash(&bytes) != check || bytes.len() < 2 + SPKI_HASH_LEN + SECRET_LEN {
            return Err(PairingError::Damaged);
        }
        let port = u16::from_be_bytes([bytes[0], bytes[1]]);
        let spki_sha256 = bytes[2..2 + SPKI_HASH_LEN].try_into().expect("length checked");
        let secret = bytes[2 + SPKI_HASH_LEN..2 + SPKI_HASH_LEN + SECRET_LEN]
            .try_into()
            .expect("length checked");
        let host = std::str::from_utf8(&bytes[2 + SPKI_HASH_LEN + SECRET_LEN..])
            .map_err(|_| PairingError::Damaged)?;
        if host.is_empty()
            || host.len() > MAX_HOST_LEN
            || host.chars().any(|c| c.is_control() || c.is_whitespace())
        {
            return Err(PairingError::NoHost);
        }
        if port == 0 {
            return Err(PairingError::Damaged);
        }
        Ok(Self {
            host: host.to_string(),
            port,
            spki_sha256,
            secret,
        })
    }

    /// The secret as sent to `POST /v1/admin/pair`.
    pub fn secret_text(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.secret)
    }
}

/// Lower-case hex.
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A hash grouped for reading aloud or comparing by eye: `ab12 cd34 ...`.
pub fn fingerprint(spki_sha256: &[u8; SPKI_HASH_LEN]) -> String {
    hex(spki_sha256)
        .as_bytes()
        .chunks(4)
        .map(|c| std::str::from_utf8(c).expect("hex"))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(host: &str) -> PairingString {
        PairingString {
            host: host.into(),
            port: 8443,
            spki_sha256: [7; 32],
            secret: [9; 16],
        }
    }

    #[test]
    fn round_trips() {
        for host in ["raspberrypi.local", "192.168.1.20", "fd00::1", "hub"] {
            let p = sample(host);
            assert_eq!(PairingString::decode(&p.encode()).unwrap(), p);
        }
    }

    #[test]
    fn ignores_white_space_from_a_terminal() {
        let s = sample("raspberrypi.local").encode();
        let wrapped = format!("  {}\n{}  \r\n", &s[..30], &s[30..]);
        assert_eq!(
            PairingString::decode(&wrapped).unwrap(),
            sample("raspberrypi.local")
        );
    }

    #[test]
    fn catches_typing_mistakes() {
        let s = sample("raspberrypi.local").encode();
        let mut b = s.clone().into_bytes();
        let i = 10;
        b[i] = if b[i] == b'A' { b'B' } else { b'A' };
        assert_eq!(
            PairingString::decode(std::str::from_utf8(&b).unwrap()),
            Err(PairingError::Damaged)
        );
        assert_eq!(
            PairingString::decode(&s[..s.len() - 1]),
            Err(PairingError::Damaged)
        );
        assert_eq!(
            PairingString::decode("https://example.com"),
            Err(PairingError::NotPairing)
        );
        assert_eq!(PairingString::decode("dh1.abc"), Err(PairingError::Damaged));
    }

    #[test]
    fn debug_hides_the_secret() {
        let d = format!("{:?}", sample("h"));
        assert!(d.contains("[redacted]") && !d.contains("9, 9"));
    }

    #[test]
    fn fingerprints_are_grouped() {
        assert_eq!(&fingerprint(&[0xab; 32])[..9], "abab abab");
    }
}
