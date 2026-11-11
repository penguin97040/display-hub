//! Backup, restore, checks and the data-folder lock, for the command line on the Pi.
//!
//! A backup is a folder: `hub.db` (SQLite's online backup, safe while the hub runs), the identity
//! (certificate and **private key**), and a marker file. Restoring never deletes anything: what was
//! there is moved aside first.

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

use hub_core::Clock;
use hub_protocol::pairing::fingerprint;
use hub_protocol::Timestamp;

use crate::api::Hub;
use crate::config::Config;
use crate::identity::Identity;
use crate::store::Store;

/// Marks a folder as a Display Hub backup, and says what's in it.
pub const MARKER: &str = "DISPLAY-HUB-BACKUP.txt";
const MARKER_FIRST_LINE: &str = "Display Hub backup, format 1";
const LOCK_FILE: &str = "hubd.lock";

#[derive(Debug, thiserror::Error)]
pub enum MaintenanceError {
    #[error("{0}")]
    Refused(String),
    #[error("{what}: {source}")]
    Io { what: String, source: std::io::Error },
    #[error(transparent)]
    Store(#[from] crate::store::StoreError),
    #[error(transparent)]
    Identity(#[from] crate::identity::IdentityError),
}

fn io(what: impl Into<String>) -> impl FnOnce(std::io::Error) -> MaintenanceError {
    let what = what.into();
    move |source| MaintenanceError::Io { what, source }
}

/// Held while `hubd serve` runs, so a second hub or a restore can't use the same folder.
#[derive(Debug)]
pub struct DataLock {
    _file: File,
}

pub fn lock(data_dir: &Path) -> Result<DataLock, MaintenanceError> {
    std::fs::create_dir_all(data_dir).map_err(io(format!("creating {}", data_dir.display())))?;
    let path = data_dir.join(LOCK_FILE);
    let file = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(io(format!("opening {}", path.display())))?;
    match file.try_lock() {
        Ok(()) => Ok(DataLock { _file: file }),
        Err(std::fs::TryLockError::WouldBlock) => Err(MaintenanceError::Refused(format!(
            "the hub is running with {}. Stop it first: sudo systemctl stop display-hub",
            data_dir.display()
        ))),
        Err(std::fs::TryLockError::Error(e)) => Err(io(format!("locking {}", path.display()))(e)),
    }
}

fn private_dir(path: &Path) -> Result<(), MaintenanceError> {
    std::fs::create_dir_all(path).map_err(io(format!("creating {}", path.display())))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .map_err(io(format!("setting permissions on {}", path.display())))?;
    }
    Ok(())
}

fn private_copy(from: &Path, to: &Path) -> Result<(), MaintenanceError> {
    let bytes = std::fs::read(from).map_err(io(format!("reading {}", from.display())))?;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(to).map_err(io(format!("writing {}", to.display())))?;
    f.write_all(&bytes)
        .map_err(io(format!("writing {}", to.display())))?;
    f.sync_all().map_err(io(format!("writing {}", to.display())))
}

/// What a backup holds, checked by opening it.
#[derive(Debug)]
pub struct BackupSummary {
    pub fingerprint: String,
    pub schema_version: i64,
    pub pages: usize,
    pub displays: usize,
}

/// Opens a backup or data folder and checks the database and identity agree with each other.
fn inspect(dir: &Path) -> Result<BackupSummary, MaintenanceError> {
    let identity = Identity::load_or_create(&dir.join("identity"), false)?;
    let db = dir.join("hub.db");
    if !Store::exists(&db) {
        return Err(MaintenanceError::Refused(format!(
            "{} has no hub.db",
            dir.display()
        )));
    }
    // Read-only, with no migrations: checking a backup or a running hub must change nothing.
    let (schema_version, pages, displays) = Store::inspect(&db)?;
    Ok(BackupSummary {
        fingerprint: fingerprint(&identity.spki_sha256),
        schema_version,
        pages,
        displays,
    })
}

/// Writes a backup of a running or stopped hub into `dest`, which must not exist yet.
pub fn backup(hub: &Hub, config: &Config, dest: &Path) -> Result<BackupSummary, MaintenanceError> {
    if dest.exists() {
        return Err(MaintenanceError::Refused(format!(
            "{} already exists. Choose a new folder, such as one named after today's date.",
            dest.display()
        )));
    }
    private_dir(dest)?;
    private_dir(&dest.join("identity"))?;
    let db = dest.join("hub.db");
    hub.store.backup_to(&db)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&db, std::fs::Permissions::from_mode(0o600))
            .map_err(io(format!("setting permissions on {}", db.display())))?;
    }
    for name in [crate::identity::CERT_FILE, crate::identity::KEY_FILE] {
        private_copy(
            &config.identity_dir().join(name),
            &dest.join("identity").join(name),
        )?;
    }
    let summary = inspect(dest)?;
    let marker = format!(
        "{MARKER_FIRST_LINE}\n\
         Made: {}\n\
         Hub key fingerprint: {}\n\
         Database schema: {}\n\
         Pages: {}, displays: {}\n\n\
         This folder holds the hub's private key and every credential's hash. Keep it as safe as\n\
         the Pi itself: anyone with it can pretend to be your hub to your displays.\n\
         Restore with: sudo -u display-hub hubd restore <this folder>\n",
        hub.clock.now(),
        summary.fingerprint,
        summary.schema_version,
        summary.pages,
        summary.displays
    );
    let path = dest.join(MARKER);
    let mut f = File::create_new(&path).map_err(io(format!("writing {}", path.display())))?;
    f.write_all(marker.as_bytes())
        .map_err(io(format!("writing {}", path.display())))?;
    hub.store
        .audit(hub.clock.now(), "backup_written", &dest.display().to_string())?;
    Ok(summary)
}

/// Restores a backup into the data folder. The hub must be stopped. Anything already there is moved
/// to `replaced-<time>/` inside the data folder, never deleted, and only with `replace`.
pub fn restore(
    config: &Config,
    src: &Path,
    replace: bool,
    clock: &dyn Clock,
) -> Result<(BackupSummary, Option<PathBuf>), MaintenanceError> {
    let marker = std::fs::read_to_string(src.join(MARKER)).unwrap_or_default();
    if !marker.starts_with(MARKER_FIRST_LINE) {
        return Err(MaintenanceError::Refused(format!(
            "{} isn't a Display Hub backup (no {MARKER})",
            src.display()
        )));
    }
    let summary = inspect(src)?;
    if summary.schema_version > crate::store::SCHEMA_VERSION {
        return Err(MaintenanceError::Refused(
            "this backup is from a newer hubd. Install that version first.".into(),
        ));
    }
    let data = &config.data_dir;
    let _lock = lock(data)?;
    let existing: Vec<PathBuf> = ["hub.db", "hub.db-wal", "hub.db-shm", "identity"]
        .iter()
        .map(|n| data.join(n))
        .filter(|p| p.exists())
        .collect();
    let mut moved_to = None;
    if !existing.is_empty() {
        if !replace {
            return Err(MaintenanceError::Refused(format!(
                "{} already has a hub. To replace it with the backup, add --replace; what's there now is moved \
                 aside, not deleted.",
                data.display()
            )));
        }
        let aside = data.join(format!("replaced-{}", stamp(clock.now())));
        private_dir(&aside)?;
        for p in &existing {
            let to = aside.join(p.file_name().expect("named"));
            std::fs::rename(p, &to).map_err(io(format!("moving {} aside", p.display())))?;
        }
        moved_to = Some(aside);
    }
    private_dir(&data.join("identity"))?;
    for name in [crate::identity::CERT_FILE, crate::identity::KEY_FILE] {
        private_copy(
            &src.join("identity").join(name),
            &data.join("identity").join(name),
        )?;
    }
    private_copy(&src.join("hub.db"), &data.join("hub.db"))?;
    let restored = inspect(data)?;
    Store::open(&data.join("hub.db"))?.audit(clock.now(), "backup_restored", &src.display().to_string())?;
    Ok((restored, moved_to))
}

/// `2026-11-11T01-02-03Z`: a timestamp that is also a valid file name everywhere.
fn stamp(t: Timestamp) -> String {
    t.to_string().replace(':', "-")
}

/// One line of `hubd check`.
#[derive(Debug, PartialEq, Eq)]
pub struct Finding {
    pub ok: bool,
    pub text: String,
}

/// Checks a hub's configuration and data without changing anything.
pub fn check(config: &Config) -> Vec<Finding> {
    let mut out = Vec::new();
    let mut say = |ok: bool, text: String| out.push(Finding { ok, text });
    say(
        true,
        format!(
            "listens on {}; displays look for {}",
            config.listen,
            config.hosts().join(", ")
        ),
    );
    let data = &config.data_dir;
    if !data.exists() {
        say(
            false,
            format!(
                "{} doesn't exist yet. Start the hub once to create it.",
                data.display()
            ),
        );
        return out;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        match std::fs::metadata(data) {
            Ok(m) if m.permissions().mode() & 0o077 == 0 => {
                say(true, format!("{} is private (0700)", data.display()))
            }
            Ok(m) => say(
                false,
                format!(
                    "{} is readable by others (mode {:o}); run chmod 700 on it",
                    data.display(),
                    m.permissions().mode() & 0o777
                ),
            ),
            Err(e) => say(false, format!("can't read {}: {e}", data.display())),
        }
    }
    match inspect(data) {
        Ok(s) => {
            say(
                true,
                format!("identity and database agree; key fingerprint {}", s.fingerprint),
            );
            say(
                true,
                format!("database schema {}, integrity check passed", s.schema_version),
            );
            say(true, format!("{} pages, {} displays", s.pages, s.displays));
        }
        Err(e) => say(false, e.to_string()),
    }
    match crate::api::clock_synchronised() {
        Some(true) => say(true, "the system clock is synchronised".into()),
        Some(false) => say(
            false,
            "the system clock isn't synchronised; check timedatectl status".into(),
        ),
        None => {}
    }
    out
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use hub_core::{FakeClock, Service, Source};

    fn config(dir: &Path) -> Config {
        Config {
            data_dir: dir.to_path_buf(),
            ..Config::default()
        }
    }

    #[test]
    fn backup_and_restore_keep_the_identity_and_never_delete() {
        let tmp = tempfile::tempdir().unwrap();
        let clock = FakeClock::parse("2026-11-11T01:00:00Z");
        let a = config(&tmp.path().join("a"));
        let hub = crate::open_hub(a.clone(), Arc::new(clock.clone())).unwrap();
        hub.store
            .add_subscription(
                &hub_core::Subscription {
                    id: "claude-max".into(),
                    service: Service::ClaudeMax,
                    title: "Claude Max".into(),
                    source: Source::Demo,
                },
                clock.now(),
            )
            .unwrap();
        let dest = tmp.path().join("backup");
        let made = backup(&hub, &a, &dest).unwrap();
        assert_eq!(made.pages, 1);
        // One self-contained file, and looking at it leaves nothing behind.
        let listed = |d: &Path| {
            let mut v: Vec<_> = std::fs::read_dir(d)
                .unwrap()
                .map(|e| e.unwrap().file_name().into_string().unwrap())
                .collect();
            v.sort();
            v
        };
        let before = listed(&dest);
        assert!(!before.iter().any(|n| n.ends_with("-wal")), "{before:?}");
        inspect(&dest).unwrap();
        assert_eq!(listed(&dest), before);
        assert!(backup(&hub, &a, &dest).is_err(), "never overwrites a backup");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&dest), 0o700);
            assert_eq!(mode(&dest.join("identity/hub-key.pem")), 0o600);
            assert_eq!(mode(&dest.join("hub.db")), 0o600);
        }

        // Into an empty folder: the same key comes back.
        let b = config(&tmp.path().join("b"));
        let (restored, aside) = restore(&b, &dest, false, &clock).unwrap();
        assert_eq!(
            (restored.fingerprint.as_str(), restored.pages, aside),
            (made.fingerprint.as_str(), 1, None)
        );
        let reopened = crate::open_hub(b.clone(), Arc::new(clock.clone())).unwrap();
        assert_eq!(reopened.identity.spki_sha256, hub.identity.spki_sha256);
        drop(reopened);

        // Over an existing hub: refused without --replace, and moved aside with it.
        let other = config(&tmp.path().join("c"));
        let c_hub = crate::open_hub(other.clone(), Arc::new(clock.clone())).unwrap();
        let old_key = c_hub.identity.spki_sha256;
        drop(c_hub);
        assert!(restore(&other, &dest, false, &clock)
            .unwrap_err()
            .to_string()
            .contains("--replace"));
        let (_, aside) = restore(&other, &dest, true, &clock).unwrap();
        let aside = aside.expect("moved aside");
        assert!(aside.join("hub.db").exists() && aside.join("identity/hub-key.pem").exists());
        let kept = Identity::load_or_create(&aside.join("identity"), false).unwrap();
        assert_eq!(
            kept.spki_sha256, old_key,
            "the replaced hub's key is kept, not deleted"
        );

        // Not a backup at all.
        assert!(restore(&other, tmp.path(), true, &clock)
            .unwrap_err()
            .to_string()
            .contains("isn't a Display Hub backup"));
    }

    #[test]
    fn the_lock_stops_a_second_hub_and_a_restore_while_running() {
        let tmp = tempfile::tempdir().unwrap();
        let held = lock(tmp.path()).unwrap();
        assert!(lock(tmp.path())
            .unwrap_err()
            .to_string()
            .contains("systemctl stop"));
        drop(held);
        assert!(lock(tmp.path()).is_ok());
    }

    #[test]
    fn check_never_upgrades_a_database() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = config(&tmp.path().join("hub"));
        crate::open_hub(cfg.clone(), Arc::new(FakeClock::parse("2026-11-11T01:00:00Z"))).unwrap();
        // Pretend the database is from the first release.
        let conn = rusqlite::Connection::open(cfg.db_path()).unwrap();
        conn.execute_batch("DROP TABLE collector_scopes; PRAGMA user_version = 1;")
            .unwrap();
        drop(conn);
        assert!(check(&cfg).iter().any(|f| f.text.contains("database schema 1")));
        let version: i64 = rusqlite::Connection::open(cfg.db_path())
            .unwrap()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 1, "check changed nothing");
    }

    #[test]
    fn check_reports_problems_without_changing_anything() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = config(&tmp.path().join("hub"));
        let f = check(&cfg);
        assert!(f.iter().any(|x| !x.ok && x.text.contains("doesn't exist yet")));
        assert!(!cfg.data_dir.exists(), "check creates nothing");
        crate::open_hub(cfg.clone(), Arc::new(FakeClock::parse("2026-11-11T01:00:00Z"))).unwrap();
        let f = check(&cfg);
        assert!(
            f.iter()
                .any(|x| x.ok && x.text.contains("integrity check passed")),
            "{f:?}"
        );
        std::fs::remove_file(cfg.identity_dir().join("hub-key.pem")).unwrap();
        assert!(check(&cfg).iter().any(|x| !x.ok));
    }
}
