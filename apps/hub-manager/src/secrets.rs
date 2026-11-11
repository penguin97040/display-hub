//! Where the admin token lives: the operating system's credential store (Windows Credential Manager,
//! or the Secret Service on Linux). Never a plain file. If no store is available the app says so and
//! refuses to pair, rather than falling back to one.

use std::sync::OnceLock;
#[cfg(test)]
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use zeroize::Zeroizing;

/// The credential store's service name for every entry this app makes.
pub const SERVICE: &str = "display-hub-manager";

pub trait TokenStore: Send + Sync {
    /// Checks the store works before anything is entrusted to it.
    fn check(&self) -> Result<(), String>;
    fn load(&self, account: &str) -> Result<Option<Zeroizing<String>>, String>;
    fn save(&self, account: &str, token: &str) -> Result<(), String>;
    fn delete(&self, account: &str) -> Result<(), String>;
}

/// The operating system's store, through `keyring-core`.
pub struct OsStore;

fn init() -> &'static Result<(), String> {
    static INIT: OnceLock<Result<(), String>> = OnceLock::new();
    INIT.get_or_init(|| {
        #[cfg(windows)]
        let store = windows_native_keyring_store::Store::new();
        #[cfg(all(unix, not(target_os = "macos")))]
        let store = zbus_secret_service_keyring_store::Store::new();
        #[cfg(any(windows, all(unix, not(target_os = "macos"))))]
        {
            store
                .map(|s| keyring_core::set_default_store(s))
                .map_err(|e| e.to_string())
        }
        #[cfg(not(any(windows, all(unix, not(target_os = "macos")))))]
        Err("this platform's credential store isn't supported yet".to_string())
    })
}

fn entry(account: &str) -> Result<keyring_core::Entry, String> {
    init().clone()?;
    keyring_core::Entry::new(SERVICE, account).map_err(|e| e.to_string())
}

impl TokenStore for OsStore {
    fn check(&self) -> Result<(), String> {
        let probe = entry("probe")?;
        probe.set_password("probe").map_err(|e| e.to_string())?;
        let back = probe.get_password().map_err(|e| e.to_string())?;
        probe.delete_credential().map_err(|e| e.to_string())?;
        if back == "probe" {
            Ok(())
        } else {
            Err("the credential store gave back something else".into())
        }
    }

    fn load(&self, account: &str) -> Result<Option<Zeroizing<String>>, String> {
        match entry(account)?.get_password() {
            Ok(t) => Ok(Some(Zeroizing::new(t))),
            Err(keyring_core::Error::NoEntry) => Ok(None),
            Err(e) => Err(e.to_string()),
        }
    }

    fn save(&self, account: &str, token: &str) -> Result<(), String> {
        entry(account)?.set_password(token).map_err(|e| e.to_string())
    }

    fn delete(&self, account: &str) -> Result<(), String> {
        match entry(account)?.delete_credential() {
            Ok(()) | Err(keyring_core::Error::NoEntry) => Ok(()),
            Err(e) => Err(e.to_string()),
        }
    }
}

/// An in-memory store, for tests. `broken` behaves like a machine with no store.
#[cfg(test)]
#[derive(Default, Clone)]
pub struct MemoryStore {
    pub items: Arc<Mutex<HashMap<String, String>>>,
    pub broken: bool,
}

#[cfg(test)]
impl TokenStore for MemoryStore {
    fn check(&self) -> Result<(), String> {
        if self.broken {
            Err("no credential store is running".into())
        } else {
            Ok(())
        }
    }

    fn load(&self, account: &str) -> Result<Option<Zeroizing<String>>, String> {
        self.check()?;
        Ok(self
            .items
            .lock()
            .unwrap()
            .get(account)
            .cloned()
            .map(Zeroizing::new))
    }

    fn save(&self, account: &str, token: &str) -> Result<(), String> {
        self.check()?;
        self.items.lock().unwrap().insert(account.into(), token.into());
        Ok(())
    }

    fn delete(&self, account: &str) -> Result<(), String> {
        self.check()?;
        self.items.lock().unwrap().remove(account);
        Ok(())
    }
}
