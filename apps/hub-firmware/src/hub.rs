//! Talking to a hub as a publisher (`docs/firmware-updates.md`, "Who may upload"): `login` pairs with
//! the publisher string the owner made, and `add` uploads a package. A publisher's token can add
//! packages to the hub's shelf and list them, nothing else: installing one still takes the owner
//! pressing **Update**, and the display's own checks.
//!
//! Two things are kept, as `hub-send` keeps them for a helper: the hub's address and key hash in
//! `connection.json` in this tool's folder (no secrets), and the token, in the operating system's
//! credential store or, for a headless Linux build machine, an owner-only file beside it.

use std::path::{Path, PathBuf};

use firmware_package::limits::MAX_PACKAGE_LEN;
use hub_client::{ClientError, HubClient};
use hub_core::api::FirmwarePackageView;
use hub_protocol::pairing::{hex, PairingString};
use hub_protocol::{ErrorCode, Timestamp};
use os_secrets::TokenStore;
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

const CONFIG: &str = "connection.json";
/// The credential store's service name for this tool's token.
pub const SERVICE: &str = "display-hub-hub-firmware";

/// The hub this tool is paired with. No secrets.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Connection {
    pub host: String,
    pub port: u16,
    /// SHA-256 of the hub's public key, lower-case hex: the only key the tool accepts.
    pub spki_sha256: String,
    pub publisher_id: String,
    pub paired_at: Timestamp,
}

impl Connection {
    fn pin(&self) -> Option<[u8; 32]> {
        firmware_package::hex::decode_exact::<32>(&self.spki_sha256)
    }

    fn token_account(&self) -> String {
        format!("token-{}", self.publisher_id)
    }
}

/// This tool's folder and token store.
pub struct Profile {
    dir: PathBuf,
    store: Box<dyn TokenStore>,
}

fn hub_error(e: ClientError) -> String {
    match e.code() {
        Some(ErrorCode::Forbidden) => "the hub refused: this publisher may only add and list packages".into(),
        Some(ErrorCode::TokenRevoked) => "the hub's owner removed this publisher. Ask them for a new \
             publisher string, then run `hub-firmware login` again."
            .into(),
        Some(ErrorCode::Unauthorised) => {
            "the hub doesn't recognise this publisher. Run `hub-firmware login` again.".into()
        }
        _ => e.to_string(),
    }
}

impl Profile {
    pub fn new(dir: PathBuf, store: Box<dyn TokenStore>) -> Self {
        Self { dir, store }
    }

    pub fn connection(&self) -> Result<Connection, String> {
        let bytes = std::fs::read(self.dir.join(CONFIG)).map_err(|_| {
            "this computer isn't paired with a hub. Run `hub-firmware login` with a publisher string from \
             the hub's owner."
                .to_string()
        })?;
        serde_json::from_slice(&bytes)
            .map_err(|e| format!("{CONFIG} is damaged ({e}); run `hub-firmware login` again"))
    }

    /// Pairs with the hub named in a publisher string, and keeps the token and the hub's details.
    pub async fn login(&self, publisher_string: &str, now: Timestamp) -> Result<Connection, String> {
        let pairing = PairingString::decode(publisher_string.trim())
            .map_err(|e| format!("that isn't a publisher string: {e}"))?;
        // Check the store first, so the hub never issues a token that can't be kept.
        self.store
            .check()
            .map_err(|e| format!("the place the token is kept didn't work: {e}"))?;
        let earlier = self.connection().ok();
        let client = HubClient::pinned(&pairing.host, pairing.port, vec![pairing.spki_sha256])
            .map_err(|e| e.to_string())?;
        let resp = client
            .publisher_pair(&pairing.secret_text())
            .await
            .map_err(|e| match e.code() {
                Some(ErrorCode::EnrolmentUsed) => {
                    "this publisher string was already used. Ask the hub's owner for a new one.".to_string()
                }
                Some(ErrorCode::EnrolmentExpired) => "this publisher string has expired (they last 10 \
                     minutes). Ask the hub's owner for a new one."
                    .to_string(),
                _ => e.to_string(),
            })?;
        let conn = Connection {
            host: pairing.host.clone(),
            port: pairing.port,
            spki_sha256: hex(&pairing.spki_sha256),
            publisher_id: resp.publisher_id,
            paired_at: now,
        };
        self.store
            .save(&conn.token_account(), &resp.publisher_token)
            .map_err(|e| format!("the place the token is kept didn't work: {e}"))?;
        hub_send::write_private(
            &self.dir.join(CONFIG),
            &serde_json::to_vec_pretty(&conn).expect("serialises"),
        )
        .map_err(|e| e.to_string())?;
        if let Some(old) = earlier.filter(|old| old.token_account() != conn.token_account()) {
            // Best effort: the new pairing works either way.
            let _ = self.store.delete(&old.token_account());
        }
        Ok(conn)
    }

    /// A ready connection to the hub, with the token.
    pub fn open(&self) -> Result<(Connection, HubClient, Zeroizing<String>), String> {
        let conn = self.connection()?;
        let token = self
            .store
            .load(&conn.token_account())
            .map_err(|e| format!("the place the token is kept didn't work: {e}"))?
            .ok_or("the publisher's token is missing. Run `hub-firmware login` again.")?;
        let pin = conn
            .pin()
            .ok_or_else(|| format!("{CONFIG} holds an unreadable key"))?;
        let client = HubClient::pinned(&conn.host, conn.port, vec![pin]).map_err(|e| e.to_string())?;
        Ok((conn, client, token))
    }
}

/// Uploads a package file after checking it here first, so a damaged one is caught before it is
/// sent. `true` when the hub didn't have it already.
pub async fn add(profile: &Profile, path: &Path) -> Result<(FirmwarePackageView, bool), String> {
    let (_, client, token) = profile.open()?;
    firmware_package::check_file(path)
        .map_err(|e| format!("{} isn't a valid package: {e}", path.display()))?;
    let bytes = crate::read_bounded(path, "package", MAX_PACKAGE_LEN)?;
    client.publisher_upload(&token, bytes).await.map_err(hub_error)
}

/// What `add` and `publish` print about a package the hub now holds.
pub fn added(view: &FirmwarePackageView, created: bool, conn: &Connection) -> String {
    format!(
        "{} {} {} for {} on the hub at {}:{}.\n\
         Package id: {}\n\
         Signer fingerprint: {}\n\
         It is on the hub's shelf. Nothing installs until the owner presses Update for a display, and the \
         display checks it again itself.",
        if created { "Added" } else { "The hub already had" },
        view.device_type,
        view.firmware_version,
        view.hardware_revisions.join(", "),
        conn.host,
        conn.port,
        view.package_id,
        view.signer_fingerprint
    )
}
