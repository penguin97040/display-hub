//! The helper's connection to the hub: a helper token in the OS credential store, and the hub's
//! address and key in `hubctl.json` (no secrets), in `%AppData%\display-hub\hubctl\` on Windows.

use std::io::Write;
use std::path::{Path, PathBuf};

use hub_client::{ClientError, HubClient};
use hub_core::api::FeedAccepted;
use hub_protocol::pairing::{hex, PairingString};
use hub_protocol::Timestamp;
use os_secrets::TokenStore;
use serde::{Deserialize, Serialize};
use usage::UsageReport;
use zeroize::Zeroizing;

/// The credential store's service name for the helper's token.
pub const SERVICE: &str = "display-hub-hubctl";
const CONFIG: &str = "hubctl.json";
const LOG: &str = "hubctl.log";
const LOG_MAX_BYTES: u64 = 64 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum HelperError {
    #[error(
        "this PC helper isn't set up. Run `hubctl login` with a helper string from the manager's Settings."
    )]
    NotSetUp,
    #[error(
        "the helper's credential is missing from the system's credential store. Run `hubctl login` again."
    )]
    NoToken,
    #[error("the system's credential store didn't work: {0}")]
    Store(String),
    #[error("{0}")]
    Files(String),
    #[error(transparent)]
    Hub(#[from] ClientError),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelperConfig {
    pub host: String,
    pub port: u16,
    /// SHA-256 of the hub's public key, lower-case hex: the only key the helper accepts.
    pub spki_sha256: String,
    /// Files written before helpers sent to feeds name the same things differently.
    #[serde(alias = "collector_id")]
    pub helper_id: String,
    /// The feeds it may send to: a service's feed has the service's slug as its name.
    #[serde(alias = "services")]
    pub feeds: Vec<String>,
    pub paired_at: Timestamp,
}

impl HelperConfig {
    fn pin(&self) -> Option<[u8; 32]> {
        let bytes: Vec<u8> = (0..self.spki_sha256.len())
            .step_by(2)
            .map(|i| {
                self.spki_sha256
                    .get(i..i + 2)
                    .and_then(|h| u8::from_str_radix(h, 16).ok())
            })
            .collect::<Option<_>>()?;
        bytes.try_into().ok()
    }

    /// The credential store's account for the token. Its name is older than helpers sending to
    /// feeds, and stays so a token already stored is still found.
    fn token_account(&self) -> String {
        format!(
            "collector-token-{}",
            &self.spki_sha256[..16.min(self.spki_sha256.len())]
        )
    }
}

/// The helper's folder: `dir` if given, else the user's configuration folder.
pub fn folder(dir: Option<&Path>) -> PathBuf {
    match dir {
        Some(d) => d.to_path_buf(),
        None => dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("display-hub")
            .join("hubctl"),
    }
}

fn private_dir(d: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(d)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Writes a file only its owner can read, atomically.
pub fn write_private(path: &Path, bytes: &[u8]) -> Result<(), HelperError> {
    let fail = |e: std::io::Error| HelperError::Files(format!("couldn't write {}: {e}", path.display()));
    if let Some(d) = path.parent() {
        private_dir(d).map_err(fail)?;
    }
    let tmp = path.with_extension("tmp");
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(&tmp).map_err(fail)?;
    f.write_all(bytes).map_err(fail)?;
    f.sync_all().map_err(fail)?;
    std::fs::rename(&tmp, path).map_err(fail)
}

pub fn load_config(dir: &Path) -> Result<HelperConfig, HelperError> {
    let bytes = std::fs::read(dir.join(CONFIG)).map_err(|_| HelperError::NotSetUp)?;
    serde_json::from_slice(&bytes)
        .map_err(|e| HelperError::Files(format!("{CONFIG} is damaged ({e}); run `hubctl login` again")))
}

/// Pairs with the hub named in a helper string, and keeps the token and the hub's details.
pub async fn login(
    dir: &Path,
    store: &dyn TokenStore,
    pairing: &PairingString,
    now: Timestamp,
) -> Result<HelperConfig, HelperError> {
    // Check the store first, so the hub never issues a token this PC can't keep.
    store.check().map_err(HelperError::Store)?;
    let client = HubClient::pinned(&pairing.host, pairing.port, vec![pairing.spki_sha256])?;
    let resp = client.helper_pair(&pairing.secret_text()).await?;
    let cfg = HelperConfig {
        host: pairing.host.clone(),
        port: pairing.port,
        spki_sha256: hex(&pairing.spki_sha256),
        helper_id: resp.helper_id,
        feeds: resp.feeds,
        paired_at: now,
    };
    store
        .save(&cfg.token_account(), &resp.helper_token)
        .map_err(HelperError::Store)?;
    write_private(
        &dir.join(CONFIG),
        &serde_json::to_vec_pretty(&cfg).expect("serialises"),
    )?;
    Ok(cfg)
}

/// Forgets the hub on this PC. The helper also needs removing in the manager to cut it off.
pub fn logout(dir: &Path, store: &dyn TokenStore) -> Result<Option<HelperConfig>, HelperError> {
    let Ok(cfg) = load_config(dir) else {
        return Ok(None);
    };
    store.delete(&cfg.token_account()).map_err(HelperError::Store)?;
    std::fs::remove_file(dir.join(CONFIG)).map_err(|e| HelperError::Files(e.to_string()))?;
    Ok(Some(cfg))
}

/// A ready connection to the hub.
pub struct Helper {
    pub config: HelperConfig,
    client: HubClient,
    token: Zeroizing<String>,
}

impl Helper {
    pub fn open(dir: &Path, store: &dyn TokenStore) -> Result<Self, HelperError> {
        let config = load_config(dir)?;
        let token = store
            .load(&config.token_account())
            .map_err(HelperError::Store)?
            .ok_or(HelperError::NoToken)?;
        let pin = config
            .pin()
            .ok_or_else(|| HelperError::Files(format!("{CONFIG} holds an unreadable key")))?;
        let client = HubClient::pinned(&config.host, config.port, vec![pin])?;
        Ok(Self {
            config,
            client,
            token,
        })
    }

    /// Sends a report as a meter page to its service's feed, once its figures pass the usage checks.
    pub async fn send(&self, report: &UsageReport) -> Result<FeedAccepted, HelperError> {
        let (feed, body) = report.to_feed_content().map_err(ClientError::Invalid)?;
        Ok(self.client.put_feed(&self.token, feed, &body).await?)
    }

    pub async fn reachable(&self) -> Result<(), HelperError> {
        self.client.health().await?;
        Ok(())
    }
}

/// Appends a line to the helper's log (kept small). The status line has nowhere else to say why a
/// send failed; `hubctl status` shows the end of it. Never a credential.
pub fn log(dir: &Path, now: Timestamp, line: &str) {
    let path = dir.join(LOG);
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > LOG_MAX_BYTES) {
        let _ = std::fs::rename(&path, dir.join(format!("{LOG}.old")));
    }
    let _ = private_dir(dir);
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    if let Ok(mut f) = opts.open(&path) {
        let clean: String = line.chars().filter(|c| !c.is_control()).take(400).collect();
        let _ = writeln!(f, "{now} {clean}");
    }
}

pub fn log_tail(dir: &Path, lines: usize) -> Vec<String> {
    let text = std::fs::read_to_string(dir.join(LOG)).unwrap_or_default();
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_settings_file_from_before_feeds_still_loads() {
        let dir = tempfile::tempdir().unwrap();
        let old = r#"{"host": "raspberrypi.local", "port": 8443, "spki_sha256": "ab", "collector_id": "c-1",
                      "services": ["claude-max", "chatgpt-plus"], "paired_at": "2026-11-10T20:00:00Z"}"#;
        std::fs::write(dir.path().join(CONFIG), old).unwrap();
        let cfg = load_config(dir.path()).unwrap();
        assert_eq!(cfg.helper_id, "c-1");
        assert_eq!(cfg.feeds, ["claude-max", "chatgpt-plus"]);
        assert_eq!(cfg.token_account(), "collector-token-ab");
    }
}
