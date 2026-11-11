//! Building the provisioning bundle a display receives over USB, from an enrolment grant.

use hub_core::api::EnrolmentGrant;
use hub_protocol::provisioning::{
    Bundle, Enrolment, HubLocation, TrustAnchor, Wifi, FORMAT, HUB_SERVER_NAME, VERSION,
};

/// A bundle for one display. `hosts` are the addresses to try, in order.
pub fn bundle(
    grant: &EnrolmentGrant,
    hosts: Vec<String>,
    port: u16,
    wifi: Option<Wifi>,
) -> Result<Bundle, String> {
    let b = Bundle {
        format: FORMAT.into(),
        version: VERSION,
        hub: HubLocation {
            hosts,
            port,
            server_name: HUB_SERVER_NAME.into(),
            trust_anchors: vec![TrustAnchor {
                certificate_pem: grant.identity.certificate_pem.clone(),
                spki_sha256: grant.identity.spki_sha256.clone(),
            }],
        },
        enrolment: Enrolment {
            secret: grant.secret.clone(),
            expires_at: grant.expires_at,
        },
        wifi,
    };
    let problems = b.problems();
    if problems.is_empty() {
        Ok(b)
    } else {
        Err(problems.join("; "))
    }
}

/// The key pins in a bundle's trust anchors.
pub fn pins(hub: &HubLocation) -> Vec<[u8; 32]> {
    hub.trust_anchors
        .iter()
        .filter_map(|a| {
            let bytes: Vec<u8> = (0..a.spki_sha256.len())
                .step_by(2)
                .filter_map(|i| {
                    a.spki_sha256
                        .get(i..i + 2)
                        .and_then(|h| u8::from_str_radix(h, 16).ok())
                })
                .collect();
            bytes.try_into().ok()
        })
        .collect()
}
