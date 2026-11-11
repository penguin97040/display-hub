//! What the desktop manager gives a display over USB the first time: where the hub is, how to
//! recognise it, a one-time enrolment secret, and optionally Wi-Fi details.
//!
//! The bundle is a secret while it is valid: it carries the enrolment secret and may carry the Wi-Fi
//! password. A display stores the hub details and Wi-Fi settings, uses the secret once, and keeps
//! only the device token it gets back.

use serde::{Deserialize, Serialize};

use crate::timestamp::Timestamp;

pub const FORMAT: &str = "display-hub.provisioning";
pub const VERSION: u32 = 1;
/// The fixed name in every hub certificate. Firmware checks it (`common_name` in ESP-IDF), whatever
/// address it connects to.
pub const HUB_SERVER_NAME: &str = "hub.display-hub.internal";
pub const MAX_HOSTS: usize = 4;
pub const MAX_TRUST_ANCHORS: usize = 2;
pub const MAX_CERT_PEM_LEN: usize = 4096;
/// The largest serialised bundle, including Wi-Fi details.
pub const MAX_BUNDLE_BYTES: usize = 12 * 1024;

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct Bundle {
    pub format: String,
    pub version: u32,
    pub hub: HubLocation,
    pub enrolment: Enrolment,
    /// `null` when the display already has Wi-Fi settings, or for the simulator.
    pub wifi: Option<Wifi>,
}

impl std::fmt::Debug for Bundle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Bundle")
            .field("hub", &self.hub)
            .field("enrolment_expires_at", &self.enrolment.expires_at)
            .field("wifi", &self.wifi.as_ref().map(|w| &w.ssid))
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HubLocation {
    /// Addresses to try, in order: a `.local` name, then a fixed IP address, for example.
    pub hosts: Vec<String>,
    pub port: u16,
    /// Always [`HUB_SERVER_NAME`].
    pub server_name: String,
    /// One or two certificates to trust: the hub's current one, and its next during a key change.
    /// The display accepts a connection only if the hub presents one of these exactly.
    pub trust_anchors: Vec<TrustAnchor>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustAnchor {
    /// The hub's self-signed certificate, PEM.
    pub certificate_pem: String,
    /// SHA-256 of its SubjectPublicKeyInfo, lower-case hex, for firmware that pins the key.
    pub spki_sha256: String,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct Enrolment {
    pub secret: String,
    pub expires_at: Timestamp,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct Wifi {
    pub ssid: String,
    pub password: String,
}

impl Bundle {
    pub fn problems(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.format != FORMAT || self.version != VERSION {
            out.push(format!("not a {FORMAT} version {VERSION} bundle"));
        }
        if self.hub.hosts.is_empty() || self.hub.hosts.len() > MAX_HOSTS {
            out.push(format!("1-{MAX_HOSTS} hub hosts"));
        }
        if self
            .hub
            .hosts
            .iter()
            .any(|h| h.is_empty() || h.len() > 253 || h.chars().any(|c| c.is_whitespace() || c.is_control()))
        {
            out.push("hub hosts must be plain host names or addresses".into());
        }
        if self.hub.port == 0 {
            out.push("hub port must not be 0".into());
        }
        if self.hub.server_name != HUB_SERVER_NAME {
            out.push(format!("server_name must be {HUB_SERVER_NAME}"));
        }
        if self.hub.trust_anchors.is_empty() || self.hub.trust_anchors.len() > MAX_TRUST_ANCHORS {
            out.push(format!("1-{MAX_TRUST_ANCHORS} trust anchors"));
        }
        for a in &self.hub.trust_anchors {
            if !a.certificate_pem.starts_with("-----BEGIN CERTIFICATE-----")
                || a.certificate_pem.len() > MAX_CERT_PEM_LEN
            {
                out.push("trust anchors must be PEM certificates".into());
            }
            if a.spki_sha256.len() != 64
                || !a
                    .spki_sha256
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            {
                out.push("spki_sha256 must be 64 lower-case hex digits".into());
            }
        }
        if self.enrolment.secret.is_empty() || self.enrolment.secret.len() > 64 {
            out.push("enrolment secret missing or too long".into());
        }
        if let Some(w) = &self.wifi {
            // 802.11 limits: SSID up to 32 bytes; WPA2 passphrase 8-63 characters.
            if w.ssid.is_empty() || w.ssid.len() > 32 {
                out.push("Wi-Fi network name must be 1-32 bytes".into());
            }
            if !w.password.is_empty() && !(8..=63).contains(&w.password.len()) {
                out.push("Wi-Fi password must be empty (open network) or 8-63 characters".into());
            }
        }
        if serde_json::to_vec(self).map(|b| b.len()).unwrap_or(usize::MAX) > MAX_BUNDLE_BYTES {
            out.push(format!("bundle larger than {MAX_BUNDLE_BYTES} bytes"));
        }
        out
    }
}
