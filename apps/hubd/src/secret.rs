//! Credentials and one-time codes: generated from the OS's secure random source, stored only as
//! SHA-256 hashes, never printed by `Debug`, and wiped from memory when dropped.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use ring::rand::{SecureRandom, SystemRandom};
use zeroize::Zeroize;

/// Who a token belongs to. The prefix makes a leaked token recognisable, and stops one kind being
/// used as another.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Role {
    Admin,
    Device,
    Collector,
}

impl Role {
    pub fn prefix(self) -> &'static str {
        match self {
            Role::Admin => "dha_",
            Role::Device => "dhd_",
            Role::Collector => "dhc_",
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Role::Admin => "admin",
            Role::Device => "device",
            Role::Collector => "collector",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        [Role::Admin, Role::Device, Role::Collector]
            .into_iter()
            .find(|r| r.as_str() == s)
    }

    /// The role a token's prefix claims. Says nothing about whether the token is real.
    pub fn of_token(token: &str) -> Option<Self> {
        [Role::Admin, Role::Device, Role::Collector]
            .into_iter()
            .find(|r| token.starts_with(r.prefix()))
    }
}

/// A secret value. Shows as `[redacted]`, and is zeroed on drop.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(s: String) -> Self {
        Self(s)
    }

    /// The value, for the one place that must send or display it.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[redacted]")
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    SystemRandom::new()
        .fill(&mut b)
        .expect("the operating system's random source works");
    b
}

/// A new long-lived token: prefix plus 256 random bits.
pub fn new_token(role: Role) -> Secret {
    Secret(format!(
        "{}{}",
        role.prefix(),
        URL_SAFE_NO_PAD.encode(random_bytes::<32>())
    ))
}

/// A new one-time code: 128 random bits. Short-lived, so this is plenty.
pub fn new_code() -> ([u8; 16], Secret) {
    let raw = random_bytes::<16>();
    (raw, Secret(URL_SAFE_NO_PAD.encode(raw)))
}

/// What the database stores in place of a secret.
pub fn hash(secret: &str) -> [u8; 32] {
    ring::digest::digest(&ring::digest::SHA256, secret.as_bytes())
        .as_ref()
        .try_into()
        .expect("SHA-256 is 32 bytes")
}

/// A short random identifier for a record, such as a display: `d-` and 12 hex digits.
pub fn new_id(prefix: &str) -> String {
    let b = random_bytes::<6>();
    format!(
        "{prefix}-{}",
        b.iter().map(|x| format!("{x:02x}")).collect::<String>()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_carry_their_role_and_are_unique() {
        let a = new_token(Role::Admin);
        let b = new_token(Role::Admin);
        assert_ne!(a, b);
        assert_eq!(Role::of_token(a.expose()), Some(Role::Admin));
        assert_eq!(
            Role::of_token(new_token(Role::Device).expose()),
            Some(Role::Device)
        );
        assert_eq!(a.expose().len(), 4 + 43);
        assert_eq!(format!("{a:?}"), "[redacted]");
    }

    #[test]
    fn hashes_are_stable_and_distinct() {
        assert_eq!(hash("x"), hash("x"));
        assert_ne!(hash("x"), hash("y"));
    }
}
