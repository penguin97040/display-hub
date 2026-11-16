//! Sends page content to a Display Hub. A helper pairs with the hub once, using the helper string
//! the manager made for it, and then sends a whole page body to each feed it was invited for
//! (`docs/helpers.md`). This library is what the `hub-send` tool and other Rust helpers (such as
//! `usage-helper`) build on.
//!
//! Two things are kept per helper, in its own folder: the hub's address and key hash in
//! `connection.json` (no secrets), and the helper token, either in the operating system's credential
//! store or, for headless Unix services, in an owner-only file ([`FileStore`]).

mod file_store;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

pub use file_store::{FileStore, TOKEN_FILE};
pub use hub_client::ClientError;
use hub_client::HubClient;
pub use hub_core::api::{FeedAccepted, FeedContent};
use hub_protocol::pairing::{hex, PairingString};
use hub_protocol::{ErrorCode, Timestamp};
use os_secrets::{OsStore, TokenStore};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

const CONFIG: &str = "connection.json";
const LOG: &str = "helper.log";
const LOG_MAX_BYTES: u64 = 64 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(
        "this helper isn't set up. Run `{tool} login` with a helper string from the manager's Settings."
    )]
    NotSetUp { tool: &'static str },
    #[error("the helper's token is missing. Run `{tool} login` again.")]
    NoToken { tool: &'static str },
    #[error("the place the token is kept didn't work: {0}")]
    Store(String),
    #[error("{0}")]
    Files(String),
    #[error("that isn't a helper string: {0}")]
    BadHelperString(String),
    #[error("this helper string was already used. Make a new one in the manager's Settings.")]
    HelperStringUsed,
    #[error(
        "this helper string has expired (they last 10 minutes). Make a new one in the manager's Settings."
    )]
    HelperStringExpired,
    #[error(transparent)]
    Hub(#[from] ClientError),
}

/// Where the helper token is kept.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum StoreKind {
    /// The operating system's credential store (Windows Credential Manager, the Secret Service).
    #[default]
    Os,
    /// An owner-only file in the helper's folder, for headless Unix services with no keyring.
    File,
}

impl StoreKind {
    /// `os` or `file`.
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim() {
            "os" => Ok(Self::Os),
            "file" => Ok(Self::File),
            other => Err(format!("{other:?} isn't a token store: choose `os` or `file`")),
        }
    }
}

/// The token store of `kind` for the helper whose folder is `dir`. `service` names the helper in the
/// credential store, so two helpers never share an entry.
pub fn token_store(
    kind: StoreKind,
    service: &'static str,
    dir: &Path,
) -> Result<Box<dyn TokenStore>, String> {
    Ok(match kind {
        StoreKind::Os => Box::new(OsStore { service }),
        StoreKind::File => Box::new(FileStore::new(dir)?),
    })
}

/// The hub a helper is paired with. No secrets.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    pub host: String,
    pub port: u16,
    /// SHA-256 of the hub's public key, lower-case hex: the only key the helper accepts.
    pub spki_sha256: String,
    pub helper_id: String,
    /// The feeds it may send to.
    pub feeds: Vec<String>,
    pub paired_at: Timestamp,
}

impl Config {
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

    /// The credential store's account for the token: one per helper, so two profiles paired with the
    /// same hub never share an entry.
    fn token_account(&self) -> String {
        format!("token-{}", self.helper_id)
    }
}

/// The default folder for the helper called `app`: `<user config folder>/display-hub/<app>`.
pub fn default_dir(app: &str) -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("display-hub")
        .join(app)
}

/// Makes the folder if missing, owner-only on Unix. An existing folder is left as it is.
pub(crate) fn private_dir(d: &Path) -> std::io::Result<()> {
    if d.is_dir() {
        return Ok(());
    }
    let mut b = std::fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        b.mode(0o700);
    }
    b.create(d)
}

/// Writes a file only its owner can read, atomically: a new temporary file in the same folder, opened
/// owner-only, written and synced, then renamed over the target.
pub fn write_private(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let fail = |e: std::io::Error| Error::Files(format!("couldn't write {}: {e}", path.display()));
    let dir = match path.parent() {
        Some(d) if !d.as_os_str().is_empty() => d,
        _ => Path::new("."),
    };
    private_dir(dir).map_err(fail)?;
    let name = path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default();
    let tmp = dir.join(format!(
        ".{name}.{}.{}.tmp",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let written = (|| {
        let mut f = opts.open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written.map_err(fail)
}

/// One helper's place on this computer: its folder, where its token is kept, and the name its
/// messages use for its command (`hub-send`, `usage-helper`).
pub struct Profile {
    tool: &'static str,
    dir: PathBuf,
    store: Box<dyn TokenStore>,
}

impl Profile {
    pub fn new(tool: &'static str, dir: PathBuf, store: Box<dyn TokenStore>) -> Self {
        Self { tool, dir, store }
    }

    pub fn tool(&self) -> &'static str {
        self.tool
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The hub this helper is paired with.
    pub fn config(&self) -> Result<Config, Error> {
        let bytes = std::fs::read(self.dir.join(CONFIG)).map_err(|_| Error::NotSetUp { tool: self.tool })?;
        serde_json::from_slice(&bytes).map_err(|e| {
            Error::Files(format!(
                "{CONFIG} is damaged ({e}); run `{} login` again",
                self.tool
            ))
        })
    }

    /// Pairs with the hub named in a helper string, and keeps the token and the hub's details.
    pub async fn login(&self, helper_string: &str, now: Timestamp) -> Result<Config, Error> {
        let pairing =
            PairingString::decode(helper_string.trim()).map_err(|e| Error::BadHelperString(e.to_string()))?;
        // Check the store first, so the hub never issues a token that can't be kept.
        self.store.check().map_err(Error::Store)?;
        // A login over an earlier one: its token is kept under its own helper id, so it's removed
        // once the new one is saved, rather than left behind in the store.
        let earlier = self.config().ok();
        let client = HubClient::pinned(&pairing.host, pairing.port, vec![pairing.spki_sha256])?;
        let resp = client
            .helper_pair(&pairing.secret_text())
            .await
            .map_err(|e| match e.code() {
                Some(ErrorCode::EnrolmentUsed) => Error::HelperStringUsed,
                Some(ErrorCode::EnrolmentExpired) => Error::HelperStringExpired,
                _ => Error::Hub(e),
            })?;
        let cfg = Config {
            host: pairing.host.clone(),
            port: pairing.port,
            spki_sha256: hex(&pairing.spki_sha256),
            helper_id: resp.helper_id,
            feeds: resp.feeds,
            paired_at: now,
        };
        self.store
            .save(&cfg.token_account(), &resp.helper_token)
            .map_err(Error::Store)?;
        write_private(
            &self.dir.join(CONFIG),
            &serde_json::to_vec_pretty(&cfg).expect("serialises"),
        )?;
        if let Some(old) = earlier.filter(|old| old.token_account() != cfg.token_account()) {
            // Best effort: the new pairing works either way.
            let _ = self.store.delete(&old.token_account());
        }
        Ok(cfg)
    }

    /// Forgets the hub on this computer. The helper also needs removing in the manager to cut it off.
    pub fn logout(&self) -> Result<Option<Config>, Error> {
        let Ok(cfg) = self.config() else {
            return Ok(None);
        };
        self.store.delete(&cfg.token_account()).map_err(Error::Store)?;
        std::fs::remove_file(self.dir.join(CONFIG)).map_err(|e| Error::Files(e.to_string()))?;
        Ok(Some(cfg))
    }

    /// A ready connection to the hub.
    pub fn open(&self) -> Result<Hub, Error> {
        let config = self.config()?;
        let token = self
            .store
            .load(&config.token_account())
            .map_err(Error::Store)?
            .ok_or(Error::NoToken { tool: self.tool })?;
        let pin = config
            .pin()
            .ok_or_else(|| Error::Files(format!("{CONFIG} holds an unreadable key")))?;
        let client = HubClient::pinned(&config.host, config.port, vec![pin])?;
        Ok(Hub {
            config,
            client,
            token,
        })
    }

    /// Appends a line to the helper's log (kept small). A helper running in the background has nowhere
    /// else to say why a send failed. Never put a credential in it.
    pub fn log(&self, now: Timestamp, line: &str) {
        let path = self.dir.join(LOG);
        if std::fs::metadata(&path).is_ok_and(|m| m.len() > LOG_MAX_BYTES) {
            let _ = std::fs::rename(&path, self.dir.join(format!("{LOG}.old")));
        }
        let _ = private_dir(&self.dir);
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

    /// The last few lines of the log, oldest first.
    pub fn log_tail(&self, lines: usize) -> Vec<String> {
        let text = std::fs::read_to_string(self.dir.join(LOG)).unwrap_or_default();
        let all: Vec<&str> = text.lines().collect();
        all[all.len().saturating_sub(lines)..]
            .iter()
            .map(|s| s.to_string())
            .collect()
    }
}

/// A connection to the hub, with the helper's token.
pub struct Hub {
    pub config: Config,
    client: HubClient,
    token: Zeroizing<String>,
}

impl Hub {
    /// Sends a page body to one of the feeds this helper was invited for.
    pub async fn send_page(&self, feed: &str, content: &FeedContent) -> Result<FeedAccepted, Error> {
        Ok(self.client.put_feed(&self.token, feed, content).await?)
    }

    pub async fn reachable(&self) -> Result<(), Error> {
        self.client.health().await?;
        Ok(())
    }
}
