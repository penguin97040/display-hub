//! SQLite storage: pages and their order, what each display shows, displays, credentials, one-time
//! codes and the audit log.
//!
//! The Pi stores everything on an SD card that can lose power at any moment. So the database runs in
//! WAL mode with `synchronous=FULL` (a commit is on disk when it returns), checks itself at start-up,
//! and never deletes or recreates itself when something is wrong. Before an upgrade changes an
//! existing database, a copy of it is saved beside it, and the upgrade runs in one transaction.
//! Frequent, unimportant writes (display last-seen times) are kept in memory and flushed now and
//! then, to spare the card.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use hub_core::{MetricValue, PageData, Service, Source, StoredContent, StoredPage, Subscription, Window};
use hub_protocol::device::DeviceInfo;
use hub_protocol::limits::{MAX_METER_ITEMS, MAX_TITLE_LEN};
use hub_protocol::shapes::KIND_METER;
use hub_protocol::text::sanitise_text;
use hub_protocol::{DataQuality, Meter, MeterItem, Timestamp};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};

use crate::secret::Role;

/// Each entry upgrades the schema by one version. Never edit one that has shipped: add another.
/// The newest schema this build knows.
pub const SCHEMA_VERSION: i64 = MIGRATIONS.len() as i64;

const MIGRATIONS: &[&str] = &[
    // 1: the first schema.
    r#"
    CREATE TABLE subscriptions (
        id TEXT PRIMARY KEY,
        service TEXT NOT NULL,
        title TEXT NOT NULL,
        source TEXT NOT NULL,
        position INTEGER NOT NULL,
        created_at INTEGER NOT NULL
    );
    CREATE TABLE page_data (
        subscription_id TEXT PRIMARY KEY REFERENCES subscriptions(id) ON DELETE CASCADE,
        quality TEXT NOT NULL,
        updated_at INTEGER,
        metrics TEXT NOT NULL,
        note TEXT,
        stored_at INTEGER NOT NULL
    );
    CREATE TABLE devices (
        id TEXT PRIMARY KEY,
        name TEXT NOT NULL,
        enrolled_at INTEGER NOT NULL,
        last_seen_at INTEGER,
        info TEXT,
        revoked_at INTEGER
    );
    CREATE TABLE credentials (
        token_hash BLOB PRIMARY KEY,
        role TEXT NOT NULL CHECK (role IN ('admin', 'device', 'collector')),
        subject TEXT NOT NULL,
        label TEXT NOT NULL,
        created_at INTEGER NOT NULL,
        revoked_at INTEGER
    );
    CREATE INDEX credentials_subject ON credentials(role, subject);
    CREATE TABLE one_time_codes (
        code_hash BLOB PRIMARY KEY,
        purpose TEXT NOT NULL CHECK (purpose IN ('admin-pair', 'device-enrol', 'collector-pair')),
        label TEXT NOT NULL,
        created_at INTEGER NOT NULL,
        expires_at INTEGER NOT NULL,
        used_at INTEGER
    );
    CREATE TABLE audit_log (
        id INTEGER PRIMARY KEY,
        at INTEGER NOT NULL,
        event TEXT NOT NULL,
        detail TEXT NOT NULL
    );
    CREATE INDEX audit_log_at ON audit_log(at);
    "#,
    // 2: the services each collector (PC helper) may report figures for.
    r#"
    CREATE TABLE collector_scopes (
        subject TEXT NOT NULL,
        service TEXT NOT NULL,
        PRIMARY KEY (subject, service)
    );
    "#,
    // 3: pages of any kind, replacing subscriptions and their figures (moved by `move_to_pages`),
    // and the pages each display shows. A page's content is what was last stored for it: `quality`
    // is null until something is, and `data` is its JSON in the shape `kind` names.
    r#"
    CREATE TABLE pages (
        id TEXT PRIMARY KEY,
        title TEXT NOT NULL,
        kind TEXT NOT NULL,
        source TEXT NOT NULL,
        feed TEXT,
        position INTEGER NOT NULL,
        stale_after_seconds INTEGER NOT NULL,
        created_at INTEGER NOT NULL,
        quality TEXT,
        updated_at INTEGER,
        data TEXT,
        note TEXT,
        stored_at INTEGER
    );
    CREATE TABLE display_pages (
        device_id TEXT NOT NULL REFERENCES devices(id) ON DELETE CASCADE,
        page_id TEXT NOT NULL REFERENCES pages(id) ON DELETE CASCADE,
        PRIMARY KEY (device_id, page_id)
    );
    "#,
];

/// Data an upgrade moves with code rather than SQL: run straight after that version's SQL, in the
/// same transaction. Never edit one that has shipped either.
const DATA_MOVES: &[(i64, DataMove)] = &[(3, move_to_pages)];

type DataMove = fn(&Connection) -> Result<(), StoreError>;

/// Version 3: each usage subscription becomes a `meter` page with the same id, title, order and
/// source, its service as the page's feed, and its stored figures (typed-in ones included) as meter
/// data, so displays show what they did before. Every active display shows every page.
///
/// This reads what version 2 wrote, so it keeps its own copy of the names and rules of that time
/// rather than using today's usage types. A row version 2 couldn't read either (an unknown service
/// or source) was never shown, and is left behind in the pre-upgrade copy.
fn move_to_pages(conn: &Connection) -> Result<(), StoreError> {
    const SERVICES: [(&str, &str); 3] = [
        ("claude-max", "Claude Max"),
        ("chatgpt-plus", "ChatGPT Plus"),
        ("opencode-go-plus", "OpenCode Go Plus"),
    ];
    // Each source and how long its figures stayed fresh.
    const SOURCES: [(&str, i64); 4] = [
        ("demo", 3600),
        ("manual", 86_400),
        ("collector", 1800),
        ("none", 3600),
    ];
    const WINDOWS: [(&str, &str); 3] = [
        ("five-hour", "Five-hour allowance used"),
        ("weekly", "Weekly allowance used"),
        ("monthly", "Monthly allowance used"),
    ];
    #[derive(serde::Deserialize)]
    struct Figure {
        window: String,
        used_percent: Option<f64>,
        resets_at: Option<Timestamp>,
    }
    type Row = (
        String,
        String,
        String,
        String,
        i64,
        Option<String>,
        Option<i64>,
        Option<String>,
        Option<String>,
        Option<i64>,
    );
    let rows: Vec<Row> = {
        let mut st = conn.prepare(
            "SELECT s.id, s.service, s.title, s.source, s.created_at,
                    d.quality, d.updated_at, d.metrics, d.note, d.stored_at
             FROM subscriptions s LEFT JOIN page_data d ON d.subscription_id = s.id
             ORDER BY s.position, s.created_at",
        )?;
        let rows = st.query_map([], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
                r.get(7)?,
                r.get(8)?,
                r.get(9)?,
            ))
        })?;
        rows.collect::<Result<_, _>>()?
    };
    let mut position = 0i64;
    for (id, service, title, source, created_at, quality, updated_at, metrics, note, stored_at) in rows {
        let (Some(&(_, default_title)), Some(&(_, stale_after))) = (
            SERVICES.iter().find(|(s, _)| *s == service),
            SOURCES.iter().find(|(s, _)| *s == source),
        ) else {
            tracing::warn!(%id, "not moving a subscription with an unknown service or source");
            continue;
        };
        let title = sanitise_text(&title, MAX_TITLE_LEN).unwrap_or_else(|| default_title.into());
        // Version 2 read figures it couldn't parse, or with a window it didn't know, as none at all,
        // and showed shares rounded to one decimal place, or unknown when out of range.
        let data = metrics.map(|m| {
            let figures: Vec<Figure> = serde_json::from_str(&m).unwrap_or_default();
            let known = figures
                .iter()
                .all(|f| WINDOWS.iter().any(|(w, _)| *w == f.window));
            let items = figures
                .iter()
                .filter(|_| known)
                .take(MAX_METER_ITEMS)
                .map(|f| {
                    let label = WINDOWS.iter().find(|(w, _)| *w == f.window).expect("known").1;
                    let percent = f
                        .used_percent
                        .filter(|p| p.is_finite() && (0.0..=100.0).contains(p))
                        .map(|p| (p * 10.0).round() / 10.0);
                    MeterItem::new(f.window.clone(), label, percent, f.resets_at)
                })
                .collect();
            serde_json::to_string(&Meter { items }).expect("serialises")
        });
        conn.execute(
            "INSERT INTO pages (id, title, kind, source, feed, position, stale_after_seconds, created_at,
                                quality, updated_at, data, note, stored_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                id,
                title,
                KIND_METER,
                source,
                service,
                position,
                stale_after,
                created_at,
                quality,
                updated_at,
                data,
                note,
                stored_at
            ],
        )?;
        position += 1;
    }
    conn.execute(
        "INSERT INTO display_pages (device_id, page_id)
         SELECT d.id, p.id FROM devices d CROSS JOIN pages p WHERE d.revoked_at IS NULL",
        [],
    )?;
    conn.execute_batch("DROP TABLE page_data; DROP TABLE subscriptions;")?;
    Ok(())
}

/// Audit entries older than this are pruned.
pub const AUDIT_RETENTION_SECONDS: i64 = 90 * 86_400;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error(
        "the database at {path} failed its integrity check ({detail}). It has not been changed. Stop \
         the hub, then restore the latest backup with `hubd restore` (see docs/deploy-raspberry-pi.md)."
    )]
    Corrupt { path: PathBuf, detail: String },
    #[error(
        "the database at {path} was written by a newer hubd (schema {found}; this build knows up to \
         {known}). Install the newer hubd again, or restore a backup made by this version."
    )]
    TooNew { path: PathBuf, found: i64, known: i64 },
    #[error(
        "couldn't save a copy of the database at {path} before upgrading it ({detail}), so it has not \
         been upgraded or changed. Check the data folder has free space and that the hub may write to it."
    )]
    UpgradeCopy { path: PathBuf, detail: String },
    #[error(
        "upgrading the database at {path} from schema {from} failed ({detail}). It has not been \
         changed, and the copy saved beside it before the upgrade is kept. Install the previous hubd \
         again to keep using it."
    )]
    UpgradeFailed {
        path: PathBuf,
        from: i64,
        detail: String,
    },
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("{0}")]
    Invalid(String),
}

/// The outcome of using a one-time code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodeUse {
    Accepted,
    /// It existed but had already been used: possibly someone else saw it.
    AlreadyUsed,
    /// It never existed, or has expired.
    Unknown,
}

/// A credential found by its token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Credential {
    pub role: Role,
    pub subject: String,
    pub revoked: bool,
}

/// One admin, for `hubd admin list`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdminEntry {
    pub subject: String,
    pub label: String,
    pub created_at: Timestamp,
    pub revoked: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DeviceRow {
    pub id: String,
    pub name: String,
    pub enrolled_at: Timestamp,
    pub last_seen_at: Option<Timestamp>,
    pub info: Option<DeviceInfo>,
}

#[derive(Clone)]
pub struct Store {
    conn: Arc<Mutex<Connection>>,
    pub path: PathBuf,
}

fn ts(t: i64) -> Timestamp {
    Timestamp::from_unix(t).unwrap_or_else(|| Timestamp::from_unix(0).expect("epoch"))
}

impl Store {
    /// Opens (creating if needed) the database, checks it and brings its schema up to date.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        let conn = Connection::open(path)?;
        Self::prepare(conn, path)
    }

    /// What a read-only look at a database finds: its schema version and how many pages and displays.
    /// Opens it read-only and runs no migrations, so checking a backup or a live hub changes nothing.
    pub fn inspect(path: &Path) -> Result<(i64, usize, usize), StoreError> {
        let flags = rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let conn = Connection::open_with_flags(path, flags)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let check: String = conn
            .query_row("PRAGMA quick_check", [], |r| r.get(0))
            .map_err(|e| StoreError::Corrupt {
                path: path.into(),
                detail: e.to_string(),
            })?;
        if check != "ok" {
            return Err(StoreError::Corrupt {
                path: path.into(),
                detail: check,
            });
        }
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        let count = |sql: &str| -> Result<usize, StoreError> {
            Ok(conn.query_row(sql, [], |r| r.get::<_, i64>(0))? as usize)
        };
        // Before version 3, pages were subscriptions.
        let has_pages: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'pages')",
            [],
            |r| r.get(0),
        )?;
        let pages = if has_pages {
            "SELECT COUNT(*) FROM pages"
        } else {
            "SELECT COUNT(*) FROM subscriptions"
        };
        Ok((version, count(pages)?, count("SELECT COUNT(*) FROM devices")?))
    }

    /// An in-memory database, for tests.
    pub fn in_memory() -> Result<Self, StoreError> {
        Self::prepare(Connection::open_in_memory()?, Path::new(":memory:"))
    }

    fn prepare(conn: Connection, path: &Path) -> Result<Self, StoreError> {
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let check: Result<String, _> = conn.query_row("PRAGMA quick_check", [], |r| r.get(0));
        match check {
            Ok(s) if s == "ok" => {}
            Ok(detail) => {
                return Err(StoreError::Corrupt {
                    path: path.into(),
                    detail,
                })
            }
            Err(e) => {
                return Err(StoreError::Corrupt {
                    path: path.into(),
                    detail: e.to_string(),
                })
            }
        }
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let store = Self {
            conn: Arc::new(Mutex::new(conn)),
            path: path.into(),
        };
        store.migrate()?;
        Ok(store)
    }

    pub fn schema_version(&self) -> Result<i64, StoreError> {
        Ok(self.conn().query_row("PRAGMA user_version", [], |r| r.get(0))?)
    }

    fn migrate(&self) -> Result<(), StoreError> {
        let mut conn = self.conn();
        let current: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        let known = MIGRATIONS.len() as i64;
        if current > known {
            return Err(StoreError::TooNew {
                path: self.path.clone(),
                found: current,
                known,
            });
        }
        if current == known {
            return Ok(());
        }
        // A new database (version 0) has nothing to lose. An existing one is copied first.
        if current > 0 {
            let copy = self.copy_before_upgrade(&conn, current)?;
            tracing::info!(path = %copy.display(), from = current, "saved a copy of the database before upgrading it");
        }
        // Every step in one transaction: a failure part-way leaves the database as it was.
        let upgrade = |conn: &mut Connection| -> Result<(), StoreError> {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Exclusive)?;
            for (i, sql) in MIGRATIONS.iter().enumerate().skip(current as usize) {
                let version = (i + 1) as i64;
                tx.execute_batch(sql)?;
                if let Some((_, data_move)) = DATA_MOVES.iter().find(|(v, _)| *v == version) {
                    data_move(&tx)?;
                }
            }
            tx.pragma_update(None, "user_version", known)?;
            tx.commit()?;
            Ok(())
        };
        match upgrade(&mut conn) {
            Ok(()) => {}
            Err(e) if current > 0 => {
                return Err(StoreError::UpgradeFailed {
                    path: self.path.clone(),
                    from: current,
                    detail: e.to_string(),
                })
            }
            Err(e) => return Err(e),
        }
        tracing::info!(from = current, to = known, "database schema upgraded");
        Ok(())
    }

    /// Saves an online copy of the database beside it, owner-only, named after its schema version
    /// and the time: `hub-before-upgrade-schema-2-2026-11-16T05-39-17Z.db`. Never replaces a file.
    fn copy_before_upgrade(&self, conn: &Connection, version: i64) -> Result<PathBuf, StoreError> {
        let fail = |path: &Path, detail: String| StoreError::UpgradeCopy {
            path: path.into(),
            detail,
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64);
        let stamp = ts(now).to_string().replace(':', "-");
        let stem = self.path.file_stem().and_then(|s| s.to_str()).unwrap_or("hub");
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut attempt = 1;
        let dest = loop {
            let suffix = if attempt == 1 {
                String::new()
            } else {
                format!("-{attempt}")
            };
            let dest = self.path.with_file_name(format!(
                "{stem}-before-upgrade-schema-{version}-{stamp}{suffix}.db"
            ));
            match opts.open(&dest) {
                Ok(_) => break dest,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && attempt < 100 => attempt += 1,
                Err(e) => return Err(fail(&dest, e.to_string())),
            }
        };
        let copied = online_copy(conn, &dest).and_then(|()| {
            Connection::open(&dest)?.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
        });
        match copied {
            Ok(v) if v == version => Ok(dest),
            outcome => {
                // Only the incomplete copy, which this call just created, is removed.
                let _ = std::fs::remove_file(&dest);
                let detail = match outcome {
                    Ok(v) => format!("the copy reads as schema {v}"),
                    Err(e) => e.to_string(),
                };
                Err(fail(&dest, detail))
            }
        }
    }

    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        // A panic while holding the lock leaves SQLite consistent (transactions roll back), so a
        // poisoned lock is safe to keep using.
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Whether the database file already exists and holds a schema, before opening it.
    pub fn exists(path: &Path) -> bool {
        std::fs::metadata(path).map(|m| m.len() > 0).unwrap_or(false)
    }

    // ----- audit log -----

    pub fn audit(&self, now: Timestamp, event: &str, detail: &str) -> Result<(), StoreError> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO audit_log (at, event, detail) VALUES (?1, ?2, ?3)",
            params![now.unix(), event, detail],
        )?;
        conn.execute(
            "DELETE FROM audit_log WHERE at < ?1",
            params![now.unix() - AUDIT_RETENTION_SECONDS],
        )?;
        Ok(())
    }

    pub fn audit_events(&self, limit: usize) -> Result<Vec<(Timestamp, String, String)>, StoreError> {
        let conn = self.conn();
        let mut st = conn.prepare("SELECT at, event, detail FROM audit_log ORDER BY id DESC LIMIT ?1")?;
        let rows = st.query_map(params![limit as i64], |r| {
            Ok((ts(r.get(0)?), r.get(1)?, r.get(2)?))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    // ----- one-time codes -----

    pub fn add_code(
        &self,
        hash: &[u8; 32],
        purpose: &str,
        label: &str,
        now: Timestamp,
        expires: Timestamp,
    ) -> Result<(), StoreError> {
        let conn = self.conn();
        conn.execute(
            "DELETE FROM one_time_codes WHERE expires_at < ?1",
            params![now.unix() - 86_400],
        )?;
        conn.execute(
            "INSERT INTO one_time_codes (code_hash, purpose, label, created_at, expires_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![&hash[..], purpose, label, now.unix(), expires.unix()],
        )?;
        Ok(())
    }

    /// Marks a code used, if it exists, is for `purpose`, is unexpired and unused. Atomic, so two
    /// requests with the same code can't both succeed. Returns the code's label when accepted.
    pub fn use_code(
        &self,
        hash: &[u8; 32],
        purpose: &str,
        now: Timestamp,
    ) -> Result<(CodeUse, Option<String>), StoreError> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let row: Option<(i64, Option<i64>, String)> = tx
            .query_row(
                "SELECT expires_at, used_at, label FROM one_time_codes WHERE code_hash = ?1 AND purpose = ?2",
                params![&hash[..], purpose],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let outcome = match row {
            None => (CodeUse::Unknown, None),
            Some((_, Some(_), _)) => (CodeUse::AlreadyUsed, None),
            Some((expires, None, _)) if expires <= now.unix() => (CodeUse::Unknown, None),
            Some((_, None, label)) => {
                tx.execute(
                    "UPDATE one_time_codes SET used_at = ?1 WHERE code_hash = ?2",
                    params![now.unix(), &hash[..]],
                )?;
                (CodeUse::Accepted, Some(label))
            }
        };
        tx.commit()?;
        Ok(outcome)
    }

    // ----- credentials -----

    pub fn add_credential(
        &self,
        hash: &[u8; 32],
        role: Role,
        subject: &str,
        label: &str,
        now: Timestamp,
    ) -> Result<(), StoreError> {
        self.conn().execute(
            "INSERT INTO credentials (token_hash, role, subject, label, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![&hash[..], role.as_str(), subject, label, now.unix()],
        )?;
        Ok(())
    }

    pub fn credential(&self, hash: &[u8; 32]) -> Result<Option<Credential>, StoreError> {
        let conn = self.conn();
        let row: Option<(String, String, Option<i64>)> = conn
            .query_row(
                "SELECT role, subject, revoked_at FROM credentials WHERE token_hash = ?1",
                params![&hash[..]],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        Ok(row.and_then(|(role, subject, revoked)| {
            Some(Credential {
                role: Role::parse(&role)?,
                subject,
                revoked: revoked.is_some(),
            })
        }))
    }

    /// Revokes every credential of `role` for `subject`. Returns how many were active.
    pub fn revoke(&self, role: Role, subject: &str, now: Timestamp) -> Result<usize, StoreError> {
        Ok(self.conn().execute(
            "UPDATE credentials SET revoked_at = ?1 WHERE role = ?2 AND subject = ?3 AND revoked_at IS NULL",
            params![now.unix(), role.as_str(), subject],
        )?)
    }

    pub fn revoke_all(&self, role: Role, now: Timestamp) -> Result<usize, StoreError> {
        Ok(self.conn().execute(
            "UPDATE credentials SET revoked_at = ?1 WHERE role = ?2 AND revoked_at IS NULL",
            params![now.unix(), role.as_str()],
        )?)
    }

    pub fn admins(&self) -> Result<Vec<AdminEntry>, StoreError> {
        let conn = self.conn();
        let mut st = conn
            .prepare("SELECT subject, label, created_at, revoked_at FROM credentials WHERE role = 'admin' ORDER BY created_at")?;
        let rows = st.query_map([], |r| {
            Ok(AdminEntry {
                subject: r.get(0)?,
                label: r.get(1)?,
                created_at: ts(r.get(2)?),
                revoked: r.get::<_, Option<i64>>(3)?.is_some(),
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn count_active(&self, role: Role) -> Result<usize, StoreError> {
        let n: i64 = self.conn().query_row(
            "SELECT COUNT(*) FROM credentials WHERE role = ?1 AND revoked_at IS NULL",
            params![role.as_str()],
            |r| r.get(0),
        )?;
        Ok(n as usize)
    }

    // ----- devices -----

    pub fn add_device(
        &self,
        id: &str,
        name: &str,
        info: &DeviceInfo,
        now: Timestamp,
    ) -> Result<(), StoreError> {
        self.conn().execute(
            "INSERT INTO devices (id, name, enrolled_at, last_seen_at, info) VALUES (?1, ?2, ?3, ?3, ?4)",
            params![
                id,
                name,
                now.unix(),
                serde_json::to_string(info).expect("serialises")
            ],
        )?;
        Ok(())
    }

    /// Displays that haven't been revoked.
    pub fn devices(&self) -> Result<Vec<DeviceRow>, StoreError> {
        let conn = self.conn();
        let mut st = conn.prepare(
            "SELECT id, name, enrolled_at, last_seen_at, info FROM devices WHERE revoked_at IS NULL ORDER BY enrolled_at",
        )?;
        let rows = st.query_map([], |r| {
            Ok(DeviceRow {
                id: r.get(0)?,
                name: r.get(1)?,
                enrolled_at: ts(r.get(2)?),
                last_seen_at: r.get::<_, Option<i64>>(3)?.map(ts),
                info: r
                    .get::<_, Option<String>>(4)?
                    .and_then(|s| serde_json::from_str(&s).ok()),
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Revokes a display and its credentials. False if there was no such active display.
    pub fn revoke_device(&self, id: &str, now: Timestamp) -> Result<bool, StoreError> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let n = tx.execute(
            "UPDATE devices SET revoked_at = ?1 WHERE id = ?2 AND revoked_at IS NULL",
            params![now.unix(), id],
        )?;
        tx.execute(
            "UPDATE credentials SET revoked_at = ?1 WHERE role = 'device' AND subject = ?2 AND revoked_at IS NULL",
            params![now.unix(), id],
        )?;
        tx.commit()?;
        Ok(n > 0)
    }

    /// Writes buffered last-seen times and heartbeat details in one transaction.
    pub fn flush_seen(&self, seen: &[(String, Timestamp, Option<DeviceInfo>)]) -> Result<(), StoreError> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        for (id, at, info) in seen {
            tx.execute(
                "UPDATE devices SET last_seen_at = MAX(COALESCE(last_seen_at, 0), ?1), info = COALESCE(?2, info) WHERE id = ?3",
                params![at.unix(), info.as_ref().map(|i| serde_json::to_string(i).expect("serialises")), id],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    // ----- pages -----

    /// Every page, in display order, with what was last stored for it.
    pub fn pages(&self) -> Result<Vec<StoredPage>, StoreError> {
        let conn = self.conn();
        let mut st = conn.prepare(
            "SELECT id, title, kind, source, feed, stale_after_seconds, quality, updated_at, data, note
             FROM pages ORDER BY position, created_at",
        )?;
        let rows = st.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, i64>(5)?,
                r.get::<_, Option<String>>(6)?,
                r.get::<_, Option<i64>>(7)?,
                r.get::<_, Option<String>>(8)?,
                r.get::<_, Option<String>>(9)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id, title, kind, source, feed, stale, quality, updated, data, note) = row?;
            // A row this build can't interpret is skipped and logged, rather than breaking every snapshot.
            let Some(source) = Source::from_slug(&source) else {
                tracing::warn!(%id, "skipping a page with an unknown source");
                continue;
            };
            let content = quality.map(|q| StoredContent {
                quality: serde_json::from_value(serde_json::Value::String(q))
                    .unwrap_or(DataQuality::Unavailable),
                updated_at: updated.map(ts),
                data: data.and_then(|d| match serde_json::from_str(&d) {
                    Ok(v) => Some(v),
                    Err(_) => {
                        tracing::warn!(%id, "ignoring a page's stored data, which isn't valid JSON");
                        None
                    }
                }),
                note,
            });
            out.push(StoredPage {
                id,
                title,
                kind,
                source,
                feed,
                // Out of range is left for snapshot checks to catch, which leave the page off.
                stale_after_seconds: u32::try_from(stale).unwrap_or(0),
                content,
            });
        }
        Ok(out)
    }

    pub fn page(&self, id: &str) -> Result<Option<StoredPage>, StoreError> {
        Ok(self.pages()?.into_iter().find(|p| p.id == id))
    }

    pub fn page_ids(&self) -> Result<Vec<String>, StoreError> {
        let conn = self.conn();
        let mut st = conn.prepare("SELECT id FROM pages ORDER BY position, created_at")?;
        let rows = st.query_map([], |r| r.get(0))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Adds a page at the end of the order, with its content if it has any.
    pub fn add_page(&self, page: &StoredPage, now: Timestamp) -> Result<(), StoreError> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let count: i64 = tx.query_row("SELECT COUNT(*) FROM pages", [], |r| r.get(0))?;
        if count as usize >= hub_protocol::limits::MAX_PAGES {
            return Err(StoreError::Invalid(format!(
                "a display shows at most {} pages; remove one first",
                hub_protocol::limits::MAX_PAGES
            )));
        }
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM pages WHERE id = ?1)",
            params![page.id],
            |r| r.get(0),
        )?;
        if exists {
            return Err(StoreError::Invalid(format!(
                "a page with id {:?} already exists",
                page.id
            )));
        }
        let next: i64 = tx.query_row("SELECT COALESCE(MAX(position), -1) + 1 FROM pages", [], |r| {
            r.get(0)
        })?;
        tx.execute(
            "INSERT INTO pages (id, title, kind, source, feed, position, stale_after_seconds, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                page.id,
                page.title,
                page.kind,
                page.source.slug(),
                page.feed,
                next,
                page.stale_after_seconds,
                now.unix()
            ],
        )?;
        if let Some(content) = &page.content {
            write_content(&tx, &page.id, content, now)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Changes a page's title or source. Changing the source drops the stored content, which came
    /// from the old source, and resets how long the page stays fresh to the new source's default.
    pub fn update_page(
        &self,
        id: &str,
        title: Option<&str>,
        source: Option<Source>,
    ) -> Result<bool, StoreError> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let found: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM pages WHERE id = ?1)",
            params![id],
            |r| r.get(0),
        )?;
        if let Some(t) = title {
            tx.execute("UPDATE pages SET title = ?1 WHERE id = ?2", params![t, id])?;
        }
        if let Some(s) = source {
            tx.execute(
                "UPDATE pages SET source = ?1, stale_after_seconds = ?2, quality = NULL, updated_at = NULL,
                                  data = NULL, note = NULL, stored_at = NULL
                 WHERE id = ?3 AND source != ?1",
                params![s.slug(), s.stale_after_seconds(), id],
            )?;
        }
        tx.commit()?;
        Ok(found)
    }

    pub fn remove_page(&self, id: &str) -> Result<bool, StoreError> {
        Ok(self
            .conn()
            .execute("DELETE FROM pages WHERE id = ?1", params![id])?
            > 0)
    }

    /// Sets the display order. `ids` must name every page exactly once.
    pub fn reorder(&self, ids: &[String]) -> Result<(), StoreError> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let mut current: Vec<String> = {
            let mut st = tx.prepare("SELECT id FROM pages")?;
            let rows = st.query_map([], |r| r.get(0))?;
            rows.collect::<Result<_, _>>()?
        };
        let mut wanted = ids.to_vec();
        current.sort();
        wanted.sort();
        if current != wanted {
            return Err(StoreError::Invalid(
                "the new order must list every page exactly once".into(),
            ));
        }
        for (pos, id) in ids.iter().enumerate() {
            tx.execute(
                "UPDATE pages SET position = ?1 WHERE id = ?2",
                params![pos as i64, id],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Stores a page's content, replacing what was there. A write that changes nothing is skipped,
    /// to spare the SD card.
    pub fn put_content(&self, id: &str, content: &StoredContent, now: Timestamp) -> Result<bool, StoreError> {
        let conn = self.conn();
        let row: Option<ContentColumns> = conn
            .query_row(
                "SELECT quality, updated_at, data, note FROM pages WHERE id = ?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        let Some(row) = row else {
            return Err(StoreError::Invalid(format!("no page with id {id:?}")));
        };
        if row == content_columns(content) {
            return Ok(false);
        }
        write_content(&conn, id, content, now)?;
        Ok(true)
    }

    /// The pages a collector's report for `feed` goes to: that feed, with source `collector`.
    pub fn collector_feed_pages(&self, feed: &str) -> Result<Vec<String>, StoreError> {
        let conn = self.conn();
        let mut st = conn.prepare(
            "SELECT id FROM pages WHERE feed = ?1 AND source = 'collector' ORDER BY position, created_at",
        )?;
        let rows = st.query_map(params![feed], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    // ----- usage pages, as subscriptions -----
    //
    // The admin API still speaks of usage subscriptions: each is a meter page whose feed is the
    // service, and whose meter items are the allowance windows.

    pub fn subscriptions(&self) -> Result<Vec<(Subscription, Option<PageData>)>, StoreError> {
        Ok(self.pages()?.into_iter().filter_map(as_subscription).collect())
    }

    pub fn add_subscription(&self, sub: &Subscription, now: Timestamp) -> Result<(), StoreError> {
        self.add_page(
            &StoredPage {
                id: sub.id.clone(),
                title: sub.title.clone(),
                kind: KIND_METER.into(),
                source: sub.source,
                feed: Some(sub.service.slug().into()),
                stale_after_seconds: sub.source.stale_after_seconds(),
                content: None,
            },
            now,
        )
    }

    pub fn subscription_ids(&self) -> Result<Vec<String>, StoreError> {
        self.page_ids()
    }

    pub fn update_subscription(
        &self,
        id: &str,
        title: Option<&str>,
        source: Option<Source>,
    ) -> Result<bool, StoreError> {
        self.update_page(id, title, source)
    }

    pub fn remove_subscription(&self, id: &str) -> Result<bool, StoreError> {
        self.remove_page(id)
    }

    /// Stores figures for a usage page as meter data.
    pub fn put_page_data(&self, id: &str, data: &PageData, now: Timestamp) -> Result<bool, StoreError> {
        let items = data
            .metrics
            .iter()
            .map(|m| MeterItem::new(m.window.id(), m.window.label(), m.used_percent, m.resets_at))
            .collect();
        let content = StoredContent {
            quality: data.quality,
            updated_at: data.updated_at,
            data: Some(serde_json::to_value(Meter { items }).expect("serialises")),
            note: data.note.clone(),
        };
        self.put_content(id, &content, now)
    }

    pub fn subscription(&self, id: &str) -> Result<Option<Subscription>, StoreError> {
        Ok(self.page(id)?.and_then(as_subscription).map(|(s, _)| s))
    }

    /// The pages a collector's report for `service` goes to.
    pub fn collector_pages(&self, service: Service) -> Result<Vec<String>, StoreError> {
        self.collector_feed_pages(service.slug())
    }

    // ----- collectors -----

    pub fn add_collector_scope(&self, subject: &str, services: &[Service]) -> Result<(), StoreError> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        for s in services {
            tx.execute(
                "INSERT OR IGNORE INTO collector_scopes (subject, service) VALUES (?1, ?2)",
                params![subject, s.slug()],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn collector_services(&self, subject: &str) -> Result<Vec<Service>, StoreError> {
        let conn = self.conn();
        let mut st =
            conn.prepare("SELECT service FROM collector_scopes WHERE subject = ?1 ORDER BY service")?;
        let rows = st.query_map(params![subject], |r| r.get::<_, String>(0))?;
        Ok(rows
            .filter_map(|r| r.ok().and_then(|s| Service::from_slug(&s)))
            .collect())
    }

    /// Every collector that has paired, newest first.
    pub fn collectors(&self) -> Result<Vec<(String, String, Timestamp, bool)>, StoreError> {
        let conn = self.conn();
        let mut st = conn.prepare(
            "SELECT subject, label, created_at, revoked_at FROM credentials WHERE role = 'collector' ORDER BY created_at DESC",
        )?;
        let rows = st.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                ts(r.get(2)?),
                r.get::<_, Option<i64>>(3)?.is_some(),
            ))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    // ----- backup -----

    /// Copies the live database to `dest` with SQLite's online backup, safe while the hub runs.
    pub fn backup_to(&self, dest: &Path) -> Result<(), StoreError> {
        Ok(online_copy(&self.conn(), dest)?)
    }
}

/// A page's content as its database columns: quality, updated_at, data and note.
type ContentColumns = (Option<String>, Option<i64>, Option<String>, Option<String>);

fn content_columns(c: &StoredContent) -> ContentColumns {
    (
        Some(c.quality.as_str().to_string()),
        c.updated_at.map(|t| t.unix()),
        c.data
            .as_ref()
            .map(|d| serde_json::to_string(d).expect("serialises")),
        c.note.clone(),
    )
}

fn write_content(conn: &Connection, id: &str, c: &StoredContent, now: Timestamp) -> rusqlite::Result<usize> {
    let (quality, updated_at, data, note) = content_columns(c);
    conn.execute(
        "UPDATE pages SET quality = ?2, updated_at = ?3, data = ?4, note = ?5, stored_at = ?6 WHERE id = ?1",
        params![id, quality, updated_at, data, note, now.unix()],
    )
}

/// A meter page of a known usage service, as a subscription with its figures.
fn as_subscription(p: StoredPage) -> Option<(Subscription, Option<PageData>)> {
    let service = Service::from_slug(p.feed.as_deref()?)?;
    if p.kind != KIND_METER {
        return None;
    }
    let data = p.content.map(|c| PageData {
        quality: c.quality,
        updated_at: c.updated_at,
        metrics: c
            .data
            .and_then(|d| serde_json::from_value::<Meter>(d).ok())
            .map(|m| {
                m.items
                    .into_iter()
                    .filter_map(|i| {
                        Some(MetricValue {
                            window: Window::from_id(&i.id)?,
                            used_percent: i.percent,
                            resets_at: i.resets_at,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default(),
        note: c.note,
    });
    Some((
        Subscription {
            id: p.id,
            service,
            title: p.title,
            source: p.source,
        },
        data,
    ))
}

/// Copies `conn`'s database to `dest` with SQLite's online backup, as one self-contained file.
fn online_copy(conn: &Connection, dest: &Path) -> rusqlite::Result<()> {
    let mut out = Connection::open(dest)?;
    let backup = rusqlite::backup::Backup::new(conn, &mut out)?;
    backup.run_to_completion(256, std::time::Duration::from_millis(10), None)?;
    drop(backup);
    // No write-ahead log beside it.
    out.pragma_update(None, "journal_mode", "DELETE")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The time the parity fixture was built at: 2026-11-10T20:00:00Z.
    const NOW: i64 = 1_794_340_800;

    /// Representative data in the layout schema 2 used.
    pub const SCHEMA_2_DATA: &str = r#"
    INSERT INTO subscriptions (id, service, title, source, position, created_at) VALUES
        ('claude-max', 'claude-max', 'Claude Max', 'collector', 2, 100),
        ('chatgpt-plus', 'chatgpt-plus', 'ChatGPT Plus', 'none', 4, 101),
        ('opencode-go-plus', 'opencode-go-plus', 'OpenCode Go Plus', 'demo', 0, 102),
        ('work-claude', 'claude-max', 'Work Claude', 'manual', 1, 103),
        ('chatgpt-plus-2', 'chatgpt-plus', 'Spare ChatGPT', 'manual', 5, 104),
        ('opencode-go-plus-2', 'opencode-go-plus', 'Team OpenCode', 'collector', 3, 105),
        ('chatgpt-plus-3', 'chatgpt-plus', 'Old ChatGPT', 'collector', 6, 106),
        ('claude-max-2', 'claude-max', 'Empty manual', 'manual', 7, 107),
        ('claude-max-3', 'claude-max', 'Demo Claude', 'demo', 8, 108);
    INSERT INTO page_data (subscription_id, quality, updated_at, metrics, note, stored_at) VALUES
        ('claude-max', 'provider_reported', 1794340200,
         '[{"window":"five-hour","used_percent":80.0,"resets_at":"2026-11-10T19:00:00Z"},{"window":"weekly","used_percent":30.5,"resets_at":"2026-11-11T20:00:00Z"}]',
         NULL, 1794340200),
        ('work-claude', 'manual', 1794333600,
         '[{"window":"five-hour","used_percent":12.3,"resets_at":"2026-11-10T21:00:00Z"},{"window":"weekly","used_percent":null,"resets_at":null}]',
         'Typed in at lunch', 1794333600),
        ('chatgpt-plus-3', 'provider_reported', 1794250800,
         '[{"window":"five-hour","used_percent":99.9,"resets_at":"2026-11-10T20:01:40Z"},{"window":"weekly","used_percent":45.0,"resets_at":"2026-11-16T00:00:00Z"}]',
         NULL, 1794250800),
        ('claude-max-2', 'manual', 1794340740, '[]', NULL, 1794340740);
    INSERT INTO devices (id, name, enrolled_at, last_seen_at, info, revoked_at) VALUES
        ('d-desk', 'Desk', 200, 1794340000, NULL, NULL),
        ('d-kitchen', 'Kitchen', 201, NULL, NULL, NULL),
        ('d-old', 'Old', 150, 160, NULL, 170);
    INSERT INTO credentials (token_hash, role, subject, label, created_at, revoked_at) VALUES
        (x'01', 'admin', 'a-1', 'Desktop manager', 90, NULL),
        (x'02', 'device', 'd-desk', 'Desk', 200, NULL),
        (x'03', 'device', 'd-kitchen', 'Kitchen', 201, NULL),
        (x'04', 'device', 'd-old', 'Old', 150, 170),
        (x'05', 'collector', 'c-pc', 'PC helper', 210, NULL);
    INSERT INTO one_time_codes (code_hash, purpose, label, created_at, expires_at, used_at) VALUES
        (x'10', 'device-enrol', 'Desk', 190, 790, 200);
    INSERT INTO collector_scopes (subject, service) VALUES ('c-pc', 'claude-max');
    INSERT INTO audit_log (at, event, detail) VALUES (90, 'admin_paired', 'a-1 (Desktop manager)');
    "#;

    /// A database exactly as version 2 left it, with [`SCHEMA_2_DATA`] in it.
    fn schema_2_database(path: &Path) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(MIGRATIONS[0]).unwrap();
        conn.execute_batch(MIGRATIONS[1]).unwrap();
        conn.pragma_update(None, "user_version", 2).unwrap();
        conn.execute_batch(SCHEMA_2_DATA).unwrap();
    }

    /// Every row of the tables an upgrade must not touch, as text.
    fn untouched_tables(conn: &Connection) -> Vec<String> {
        let mut out = Vec::new();
        for (table, order) in [
            ("devices", "id"),
            ("credentials", "token_hash"),
            ("one_time_codes", "code_hash"),
            ("collector_scopes", "subject, service"),
            ("audit_log", "id"),
        ] {
            let mut st = conn
                .prepare(&format!("SELECT * FROM {table} ORDER BY {order}"))
                .unwrap();
            let n = st.column_count();
            let rows = st
                .query_map([], |r| {
                    let cols: Vec<String> = (0..n).map(|i| format!("{:?}", r.get_ref(i).unwrap())).collect();
                    Ok(format!("{table}: {}", cols.join(", ")))
                })
                .unwrap();
            out.extend(rows.map(Result::unwrap));
        }
        out
    }

    /// Files beside `path` that an upgrade saved.
    fn copies(dir: &Path) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("hub-before-upgrade-"))
            })
            .collect();
        out.sort();
        out
    }

    #[test]
    fn a_version_1_database_upgrades_and_keeps_its_data() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hub.db");
        {
            // Exactly what the first release wrote.
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(MIGRATIONS[0]).unwrap();
            conn.pragma_update(None, "user_version", 1).unwrap();
            conn.execute(
                "INSERT INTO subscriptions (id, service, title, source, position, created_at) \
                 VALUES ('claude-max', 'claude-max', 'Claude Max', 'collector', 0, 0)",
                [],
            )
            .unwrap();
        }
        let store = Store::open(&path).unwrap();
        assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
        assert_eq!(store.subscription_ids().unwrap(), vec!["claude-max".to_string()]);
        assert_eq!(
            store.collector_pages(Service::ClaudeMax).unwrap(),
            vec!["claude-max".to_string()]
        );
        store
            .add_collector_scope("c-1", &[Service::ClaudeMax, Service::ClaudeMax])
            .unwrap();
        assert_eq!(store.collector_services("c-1").unwrap(), vec![Service::ClaudeMax]);
        let saved = copies(dir.path());
        assert_eq!(saved.len(), 1);
        let name = saved[0].file_name().unwrap().to_str().unwrap();
        assert!(name.starts_with("hub-before-upgrade-schema-1-"), "{name}");
    }

    #[test]
    fn upgrading_schema_2_shows_every_display_the_same_pages_as_before() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hub.db");
        schema_2_database(&path);
        let before = untouched_tables(&Connection::open(&path).unwrap());

        let store = Store::open(&path).unwrap();
        assert_eq!(store.schema_version().unwrap(), 3);
        // What the schema 2 code built from the same data, captured before it was replaced.
        let expected: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/schema-2-pages.json")).unwrap();
        let snap = hub_core::build_snapshot(ts(NOW), 300, &store.subscriptions().unwrap());
        snap.validate().unwrap();
        assert_eq!(serde_json::to_value(&snap.pages).unwrap(), expected);

        let conn = store.conn();
        assert_eq!(untouched_tables(&conn), before);
        let ticks: Vec<(String, i64)> = conn
            .prepare("SELECT device_id, COUNT(*) FROM display_pages GROUP BY device_id ORDER BY device_id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        // Both active displays show all nine pages; the revoked one shows none.
        assert_eq!(ticks, [("d-desk".into(), 9), ("d-kitchen".into(), 9)]);
        let old_tables: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name IN ('subscriptions', 'page_data')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(old_tables, 0);
        let manual: (String, String, i64) = conn
            .query_row(
                "SELECT kind, feed, stale_after_seconds FROM pages WHERE id = 'work-claude'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(manual, ("meter".into(), "claude-max".into(), 86_400));
    }

    #[test]
    fn a_copy_at_the_old_schema_is_saved_before_upgrading() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hub.db");
        schema_2_database(&path);
        drop(Store::open(&path).unwrap());
        let saved = copies(dir.path());
        assert_eq!(saved.len(), 1, "{saved:?}");
        let name = saved[0].file_name().unwrap().to_str().unwrap();
        assert!(name.starts_with("hub-before-upgrade-schema-2-20"), "{name}");
        assert!(name.ends_with("Z.db") && !name.contains(':'), "{name}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&saved[0]).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        // The copy is a whole database at schema 2, with nothing beside it.
        let (version, pages, displays) = Store::inspect(&saved[0]).unwrap();
        assert_eq!((version, pages, displays), (2, 9, 3));
        assert!(!dir.path().join(format!("{name}-wal")).exists());

        // Opening it again upgrades nothing, so saves nothing; nor does a new database.
        drop(Store::open(&path).unwrap());
        drop(Store::open(&dir.path().join("new.db")).unwrap());
        assert_eq!(copies(dir.path()).len(), 1);
    }

    #[test]
    fn a_failed_upgrade_leaves_the_database_at_schema_2() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hub.db");
        schema_2_database(&path);
        // Something already called `pages` makes version 3's first statement fail.
        Connection::open(&path)
            .unwrap()
            .execute_batch("CREATE TABLE pages (x TEXT);")
            .unwrap();
        let Err(e) = Store::open(&path) else {
            panic!("upgraded anyway")
        };
        assert!(matches!(e, StoreError::UpgradeFailed { from: 2, .. }), "{e}");
        let conn = Connection::open(&path).unwrap();
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(version, 2);
        let count = |sql: &str| conn.query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap();
        assert_eq!(count("SELECT COUNT(*) FROM subscriptions"), 9);
        assert_eq!(count("SELECT COUNT(*) FROM page_data"), 4);
        assert_eq!(
            count("SELECT COUNT(*) FROM sqlite_master WHERE name = 'display_pages'"),
            0
        );
        // The copy was made first, and is kept.
        assert_eq!(copies(dir.path()).len(), 1);
    }

    #[test]
    fn a_failed_data_move_rolls_back_the_new_tables_too() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hub.db");
        schema_2_database(&path);
        // A NOT NULL title gone null can only come from damage, and stops the move part-way.
        Connection::open(&path)
            .unwrap()
            .execute_batch(
                "PRAGMA writable_schema = ON;
                 UPDATE sqlite_master SET sql = replace(sql, 'title TEXT NOT NULL', 'title TEXT')
                     WHERE name = 'subscriptions';",
            )
            .unwrap();
        Connection::open(&path)
            .unwrap()
            .execute(
                "UPDATE subscriptions SET title = NULL WHERE id = 'chatgpt-plus-3'",
                [],
            )
            .unwrap();
        let Err(e) = Store::open(&path) else {
            panic!("upgraded anyway")
        };
        assert!(e.to_string().contains("Invalid column type"), "{e}");
        let conn = Connection::open(&path).unwrap();
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(version, 2);
        let new_tables: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name IN ('pages', 'display_pages')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(new_tables, 0);
    }

    #[test]
    fn custom_kind_pages_round_trip() {
        let store = Store::in_memory().unwrap();
        let now = ts(NOW);
        let page = StoredPage {
            id: "flights".into(),
            title: "Overhead".into(),
            kind: "nearby-flights/1".into(),
            source: Source::Manual,
            feed: None,
            stale_after_seconds: 600,
            content: Some(StoredContent {
                quality: DataQuality::Manual,
                updated_at: Some(now),
                data: Some(
                    json!({"flights": [{"callsign": "ANZ1", "altitude_m": 10_000}], "n": {"deep": [1, 2.5, null]}}),
                ),
                note: Some("Hand-made".into()),
            }),
        };
        store.add_page(&page, now).unwrap();
        assert_eq!(store.pages().unwrap(), vec![page.clone()]);
        // A custom page is not a usage page.
        assert!(store.subscriptions().unwrap().is_empty());
        let content = page.content.clone().unwrap();
        assert!(!store.put_content("flights", &content, now).unwrap());
        let changed = StoredContent {
            data: Some(json!({"flights": []})),
            ..content
        };
        assert!(store.put_content("flights", &changed, now).unwrap());
        assert_eq!(store.page("flights").unwrap().unwrap().content, Some(changed));
        assert!(matches!(
            store.put_content("nope", &page.content.unwrap(), now),
            Err(StoreError::Invalid(_))
        ));
    }
}
