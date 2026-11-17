//! The paired hub's details, which aren't secret: where it is and which key it has. They live in
//! `manager.json` in the user's configuration folder (`%AppData%\display-hub\` on Windows). The admin
//! token is kept separately, in the OS credential store (`secrets`).

use std::io::Write;
use std::path::{Path, PathBuf};

use hub_protocol::pairing::{fingerprint, hex};
use hub_protocol::Timestamp;
use serde::{Deserialize, Serialize};

const FILE: &str = "manager.json";
const VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedHub {
    pub host: String,
    pub port: u16,
    /// SHA-256 of the hub's public key, lower-case hex. The only key this app accepts from it.
    pub spki_sha256: String,
    /// The name this PC gave itself on the hub.
    pub label: String,
    pub paired_at: Timestamp,
}

impl SavedHub {
    pub fn pin(&self) -> Option<[u8; 32]> {
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

    pub fn fingerprint(&self) -> String {
        self.pin().map(|p| fingerprint(&p)).unwrap_or_default()
    }

    pub fn new(host: &str, port: u16, pin: &[u8; 32], label: &str, paired_at: Timestamp) -> Self {
        Self {
            host: host.into(),
            port,
            spki_sha256: hex(pin),
            label: label.into(),
            paired_at,
        }
    }

    /// The credential store's account name for this hub's token.
    pub fn token_account(&self) -> String {
        format!(
            "admin-token-{}",
            &self.spki_sha256[..16.min(self.spki_sha256.len())]
        )
    }
}

#[derive(Serialize, Deserialize)]
struct FileV1 {
    version: u32,
    hub: Option<SavedHub>,
    /// The person closed the setup checklist on the Overview. Older files don't have this.
    #[serde(default)]
    guide_dismissed: bool,
}

/// Where settings live: `dir` if given (for testing), else the user's configuration folder.
pub fn folder(dir: Option<&Path>) -> Option<PathBuf> {
    match dir {
        Some(d) => Some(d.to_path_buf()),
        None => dirs::config_dir().map(|d| d.join("display-hub")),
    }
}

fn read(dir: &Path) -> Result<Option<FileV1>, String> {
    let path = dir.join(FILE);
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("couldn't read {}: {e}", path.display())),
    };
    let file: FileV1 = serde_json::from_slice(&bytes).map_err(|e| {
        format!(
            "{} is damaged ({e}). Forget the hub and pair again.",
            path.display()
        )
    })?;
    if file.version != VERSION {
        return Err(format!("{} is from a newer version of this app", path.display()));
    }
    if file.hub.as_ref().is_some_and(|h| h.pin().is_none()) {
        return Err(format!(
            "{} holds an unreadable key. Forget the hub and pair again.",
            path.display()
        ));
    }
    Ok(Some(file))
}

pub fn load(dir: &Path) -> Result<Option<SavedHub>, String> {
    Ok(read(dir)?.and_then(|f| f.hub))
}

/// Whether the setup checklist was closed. A missing or unreadable file means not closed.
pub fn load_guide_dismissed(dir: &Path) -> bool {
    read(dir).ok().flatten().is_some_and(|f| f.guide_dismissed)
}

/// Saves the paired hub. Forgetting it (`None`) also brings the setup guide back, for the next hub.
pub fn save(dir: &Path, hub: Option<&SavedHub>) -> Result<(), String> {
    let guide_dismissed = hub.is_some() && load_guide_dismissed(dir);
    write(dir, hub, guide_dismissed)
}

/// Remembers whether the setup checklist was closed, keeping the paired hub as it is.
pub fn save_guide_dismissed(dir: &Path, dismissed: bool) -> Result<(), String> {
    let hub = read(dir)?.and_then(|f| f.hub);
    write(dir, hub.as_ref(), dismissed)
}

/// Writes atomically, so a crash leaves the old file or the new one.
fn write(dir: &Path, hub: Option<&SavedHub>, guide_dismissed: bool) -> Result<(), String> {
    let fail = |e: std::io::Error| format!("couldn't save settings in {}: {e}", dir.display());
    std::fs::create_dir_all(dir).map_err(fail)?;
    let path = dir.join(FILE);
    let tmp = dir.join(format!("{FILE}.tmp"));
    let body = serde_json::to_vec_pretty(&FileV1 {
        version: VERSION,
        hub: hub.cloned(),
        guide_dismissed,
    })
    .expect("serialises");
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(&tmp).map_err(fail)?;
    f.write_all(&body).map_err(fail)?;
    f.sync_all().map_err(fail)?;
    std::fs::rename(&tmp, &path).map_err(fail)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_damage_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load(dir.path()).unwrap(), None);
        let hub = SavedHub::new(
            "raspberrypi.local",
            8443,
            &[0xab; 32],
            "Desk PC",
            Timestamp::from_unix(0).unwrap(),
        );
        save(dir.path(), Some(&hub)).unwrap();
        assert_eq!(load(dir.path()).unwrap(), Some(hub.clone()));
        assert_eq!(hub.pin(), Some([0xab; 32]));
        assert!(hub.fingerprint().starts_with("abab abab"));
        assert_eq!(hub.token_account(), "admin-token-abababababababab");
        save(dir.path(), None).unwrap();
        assert_eq!(load(dir.path()).unwrap(), None);
        std::fs::write(dir.path().join(FILE), "{").unwrap();
        assert!(load(dir.path()).unwrap_err().contains("damaged"));
    }

    #[test]
    fn closing_the_guide_is_remembered_and_forgetting_the_hub_brings_it_back() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!load_guide_dismissed(dir.path()));
        let hub = SavedHub::new("pi.local", 8443, &[1; 32], "PC", Timestamp::from_unix(0).unwrap());
        save(dir.path(), Some(&hub)).unwrap();
        save_guide_dismissed(dir.path(), true).unwrap();
        assert!(load_guide_dismissed(dir.path()));
        assert_eq!(load(dir.path()).unwrap(), Some(hub.clone()));
        // Saving the hub again (a refresh of its details) doesn't undo it.
        save(dir.path(), Some(&hub)).unwrap();
        assert!(load_guide_dismissed(dir.path()));
        save_guide_dismissed(dir.path(), false).unwrap();
        assert!(!load_guide_dismissed(dir.path()));
        save_guide_dismissed(dir.path(), true).unwrap();
        save(dir.path(), None).unwrap();
        assert!(!load_guide_dismissed(dir.path()));
        // A file from before this setting existed still loads.
        std::fs::write(dir.path().join(FILE), r#"{"version":1,"hub":null}"#).unwrap();
        assert_eq!(load(dir.path()).unwrap(), None);
        assert!(!load_guide_dismissed(dir.path()));
    }
}
