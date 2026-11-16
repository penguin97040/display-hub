//! A token kept in a file, for a headless Linux service (a Raspberry Pi helper under systemd) that has
//! no keyring to use. Unix only. The file is `token` in the helper's folder: created owner-only,
//! atomically, and refused on reading if anyone else could read it.

use std::path::{Path, PathBuf};

use os_secrets::TokenStore;
use zeroize::Zeroizing;

/// The token file's name inside the helper's folder.
pub const TOKEN_FILE: &str = "token";

pub struct FileStore {
    dir: PathBuf,
}

impl FileStore {
    /// A store for the token file in `dir`. Refused on platforms without Unix file permissions.
    pub fn new(dir: &Path) -> Result<Self, String> {
        #[cfg(unix)]
        {
            Ok(Self {
                dir: dir.to_path_buf(),
            })
        }
        #[cfg(not(unix))]
        {
            let _ = dir;
            Err(
                "the file store is only for Linux and other Unix systems, where it can keep the token \
                 owner-only. On this computer, use the system's credential store (leave out --store file)."
                    .into(),
            )
        }
    }

    pub fn path(&self) -> PathBuf {
        self.dir.join(TOKEN_FILE)
    }
}

impl TokenStore for FileStore {
    fn check(&self) -> Result<(), String> {
        #[cfg(unix)]
        {
            crate::private_dir(&self.dir)
                .map_err(|e| format!("couldn't make the folder {}: {e}", self.dir.display()))?;
            if self.path().exists() {
                self.load("")?;
            }
            Ok(())
        }
        #[cfg(not(unix))]
        Err("the file store is only for Unix systems".into())
    }

    // The file holds the one token this folder's helper uses, so the account name is not needed.
    fn load(&self, _account: &str) -> Result<Option<Zeroizing<String>>, String> {
        #[cfg(unix)]
        {
            read_token(&self.path())
        }
        #[cfg(not(unix))]
        Err("the file store is only for Unix systems".into())
    }

    fn save(&self, _account: &str, token: &str) -> Result<(), String> {
        crate::write_private(&self.path(), token.as_bytes()).map_err(|e| e.to_string())
    }

    fn delete(&self, _account: &str) -> Result<(), String> {
        match std::fs::remove_file(self.path()) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("couldn't remove {}: {e}", self.path().display())),
        }
    }
}

#[cfg(unix)]
fn read_token(path: &Path) -> Result<Option<Zeroizing<String>>, String> {
    use std::io::Read;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

    // Opened without following a link, then checked through the open file, so it can't be swapped
    // between the check and the read.
    let f = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(format!(
                "couldn't open the token file {} (it must be a plain file, not a link): {e}",
                path.display()
            ))
        }
    };
    let meta = f
        .metadata()
        .map_err(|e| format!("couldn't check the token file {}: {e}", path.display()))?;
    // SAFETY: geteuid has no arguments and cannot fail.
    let me = unsafe { libc::geteuid() };
    if !meta.is_file() {
        return Err(format!("{} isn't a plain file", path.display()));
    }
    if meta.uid() != me {
        return Err(format!(
            "the token file {} belongs to another user, so it isn't trusted. Run the login again as the user that runs this helper.",
            path.display()
        ));
    }
    if meta.mode() & 0o077 != 0 {
        return Err(format!(
            "the token file {} can be read by other users (its permissions are {:o}). Run `chmod 600 {}`, or log in again.",
            path.display(),
            meta.mode() & 0o777,
            path.display()
        ));
    }
    let mut bytes = Zeroizing::new(Vec::new());
    f.take(4096)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("couldn't read {}: {e}", path.display()))?;
    let text = std::str::from_utf8(&bytes).map_err(|_| format!("{} doesn't hold a token", path.display()))?;
    let token = text.trim();
    if token.is_empty() {
        return Ok(None);
    }
    Ok(Some(Zeroizing::new(token.to_string())))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};

    fn mode(p: &Path) -> u32 {
        std::fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn saving_makes_an_owner_only_file_in_an_owner_only_folder() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("a").join("helper");
        let store = FileStore::new(&dir).unwrap();
        store.save("ignored", "dhc_secret").unwrap();
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&store.path()), 0o600);
        assert_eq!(store.load("x").unwrap().unwrap().as_str(), "dhc_secret");
        // Saving again replaces it, and leaves no temporary file behind.
        store.save("ignored", "dhc_second").unwrap();
        assert_eq!(store.load("x").unwrap().unwrap().as_str(), "dhc_second");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
    }

    #[test]
    fn a_token_file_others_can_read_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let store = FileStore::new(root.path()).unwrap();
        store.save("x", "dhc_secret").unwrap();
        for bad in [0o644, 0o640, 0o604] {
            std::fs::set_permissions(store.path(), std::fs::Permissions::from_mode(bad)).unwrap();
            let err = store.load("x").unwrap_err();
            assert!(err.contains("can be read by other users"), "{err}");
            assert!(!err.contains("dhc_secret"));
            assert!(store.check().is_err());
        }
        std::fs::set_permissions(store.path(), std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(store.load("x").unwrap().is_some());
    }

    #[test]
    fn a_linked_token_file_is_refused_and_missing_means_none() {
        let root = tempfile::tempdir().unwrap();
        let store = FileStore::new(root.path()).unwrap();
        assert!(store.load("x").unwrap().is_none());
        let elsewhere = root.path().join("elsewhere");
        std::fs::write(&elsewhere, "dhc_secret").unwrap();
        std::fs::set_permissions(&elsewhere, std::fs::Permissions::from_mode(0o600)).unwrap();
        symlink(&elsewhere, store.path()).unwrap();
        assert!(store.load("x").is_err());
    }

    #[test]
    fn deleting_removes_the_file_and_is_fine_twice() {
        let root = tempfile::tempdir().unwrap();
        let store = FileStore::new(root.path()).unwrap();
        store.save("x", "dhc_secret").unwrap();
        store.delete("x").unwrap();
        assert!(!store.path().exists());
        store.delete("x").unwrap();
    }
}
