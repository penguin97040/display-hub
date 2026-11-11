//! SQLite storage: subscriptions and their order, stored figures, displays, credentials, one-time
//! codes and the audit log.
//!
//! The Pi stores everything on an SD card that can lose power at any moment. So the database runs in
//! WAL mode with `synchronous=FULL` (a commit is on disk when it returns), checks itself at start-up,
//! and never deletes or recreates itself when something is wrong. Frequent, unimportant writes
//! (display last-seen times) are kept in memory and flushed now and then, to spare the card.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use hub_core::{MetricValue, PageData, Service, Source, Subscription};
use hub_protocol::device::DeviceInfo;
use hub_protocol::{DataQuality, Timestamp};
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
];

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
        let count = |table: &str| -> Result<usize, StoreError> {
            let sql = match table {
                "subscriptions" => "SELECT COUNT(*) FROM subscriptions",
                _ => "SELECT COUNT(*) FROM devices",
            };
            Ok(conn.query_row(sql, [], |r| r.get::<_, i64>(0))? as usize)
        };
        Ok((version, count("subscriptions")?, count("devices")?))
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
        for (i, sql) in MIGRATIONS.iter().enumerate().skip(current as usize) {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Exclusive)?;
            tx.execute_batch(sql)?;
            tx.pragma_update(None, "user_version", (i + 1) as i64)?;
            tx.commit()?;
            tracing::info!(version = i + 1, "database schema upgraded");
        }
        Ok(())
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

    // ----- subscriptions -----

    pub fn subscriptions(&self) -> Result<Vec<(Subscription, Option<PageData>)>, StoreError> {
        let conn = self.conn();
        let mut st = conn.prepare(
            "SELECT s.id, s.service, s.title, s.source, d.quality, d.updated_at, d.metrics, d.note
             FROM subscriptions s LEFT JOIN page_data d ON d.subscription_id = s.id
             ORDER BY s.position, s.created_at",
        )?;
        let rows = st.query_map([], |r| {
            let id: String = r.get(0)?;
            let service: String = r.get(1)?;
            let source: String = r.get(3)?;
            let quality: Option<String> = r.get(4)?;
            let metrics: Option<String> = r.get(6)?;
            Ok((
                id,
                service,
                r.get::<_, String>(2)?,
                source,
                quality,
                r.get::<_, Option<i64>>(5)?,
                metrics,
                r.get::<_, Option<String>>(7)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id, service, title, source, quality, updated, metrics, note) = row?;
            // A row this build can't interpret is skipped and logged, rather than breaking every snapshot.
            let (Some(service), Some(source)) = (Service::from_slug(&service), Source::from_slug(&source))
            else {
                tracing::warn!(%id, "skipping a subscription with an unknown service or source");
                continue;
            };
            let data = match (quality, metrics) {
                (Some(q), Some(m)) => {
                    let quality = serde_json::from_value::<DataQuality>(serde_json::Value::String(q))
                        .unwrap_or(DataQuality::Unavailable);
                    let metrics: Vec<MetricValue> = serde_json::from_str(&m).unwrap_or_default();
                    Some(PageData {
                        quality,
                        updated_at: updated.map(ts),
                        metrics,
                        note,
                    })
                }
                _ => None,
            };
            out.push((
                Subscription {
                    id,
                    service,
                    title,
                    source,
                },
                data,
            ));
        }
        Ok(out)
    }

    pub fn add_subscription(&self, sub: &Subscription, now: Timestamp) -> Result<(), StoreError> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let count: i64 = tx.query_row("SELECT COUNT(*) FROM subscriptions", [], |r| r.get(0))?;
        if count as usize >= hub_protocol::limits::MAX_PAGES {
            return Err(StoreError::Invalid(format!(
                "a display shows at most {} pages; remove one first",
                hub_protocol::limits::MAX_PAGES
            )));
        }
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM subscriptions WHERE id = ?1)",
            params![sub.id],
            |r| r.get(0),
        )?;
        if exists {
            return Err(StoreError::Invalid(format!(
                "a page with id {:?} already exists",
                sub.id
            )));
        }
        let next: i64 = tx.query_row(
            "SELECT COALESCE(MAX(position), -1) + 1 FROM subscriptions",
            [],
            |r| r.get(0),
        )?;
        tx.execute(
            "INSERT INTO subscriptions (id, service, title, source, position, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![sub.id, sub.service.slug(), sub.title, sub.source.slug(), next, now.unix()],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn subscription_ids(&self) -> Result<Vec<String>, StoreError> {
        let conn = self.conn();
        let mut st = conn.prepare("SELECT id FROM subscriptions ORDER BY position, created_at")?;
        let rows = st.query_map([], |r| r.get(0))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Changes a subscription's title or source. Changing the source drops stored figures, which
    /// came from the old source.
    pub fn update_subscription(
        &self,
        id: &str,
        title: Option<&str>,
        source: Option<Source>,
    ) -> Result<bool, StoreError> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let mut found = false;
        if let Some(t) = title {
            found |= tx.execute(
                "UPDATE subscriptions SET title = ?1 WHERE id = ?2",
                params![t, id],
            )? > 0;
        }
        if let Some(s) = source {
            let changed = tx.execute(
                "UPDATE subscriptions SET source = ?1 WHERE id = ?2 AND source != ?1",
                params![s.slug(), id],
            )?;
            if changed > 0 {
                tx.execute("DELETE FROM page_data WHERE subscription_id = ?1", params![id])?;
            }
            found |= tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM subscriptions WHERE id = ?1)",
                params![id],
                |r| r.get::<_, bool>(0),
            )?;
        }
        tx.commit()?;
        Ok(found)
    }

    pub fn remove_subscription(&self, id: &str) -> Result<bool, StoreError> {
        Ok(self
            .conn()
            .execute("DELETE FROM subscriptions WHERE id = ?1", params![id])?
            > 0)
    }

    /// Sets the display order. `ids` must name every subscription exactly once.
    pub fn reorder(&self, ids: &[String]) -> Result<(), StoreError> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let mut current: Vec<String> = {
            let mut st = tx.prepare("SELECT id FROM subscriptions")?;
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
                "UPDATE subscriptions SET position = ?1 WHERE id = ?2",
                params![pos as i64, id],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Stores figures for a subscription, replacing what was there. A write that changes nothing is
    /// skipped, to spare the SD card.
    pub fn put_page_data(&self, id: &str, data: &PageData, now: Timestamp) -> Result<bool, StoreError> {
        let metrics = serde_json::to_string(&data.metrics).expect("serialises");
        let quality = data.quality.as_str();
        let conn = self.conn();
        let unchanged: bool = conn
            .query_row(
                "SELECT quality = ?2 AND updated_at IS ?3 AND metrics = ?4 AND note IS ?5 FROM page_data WHERE subscription_id = ?1",
                params![id, quality, data.updated_at.map(|t| t.unix()), metrics, data.note],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(false);
        if unchanged {
            return Ok(false);
        }
        let n = conn.execute(
            "INSERT INTO page_data (subscription_id, quality, updated_at, metrics, note, stored_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(subscription_id) DO UPDATE SET quality = ?2, updated_at = ?3, metrics = ?4, note = ?5, stored_at = ?6",
            params![id, quality, data.updated_at.map(|t| t.unix()), metrics, data.note, now.unix()],
        );
        match n {
            Ok(_) => Ok(true),
            Err(rusqlite::Error::SqliteFailure(e, _))
                if e.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                Err(StoreError::Invalid(format!("no page with id {id:?}")))
            }
            Err(e) => Err(e.into()),
        }
    }

    pub fn subscription(&self, id: &str) -> Result<Option<Subscription>, StoreError> {
        Ok(self
            .subscriptions()?
            .into_iter()
            .map(|(s, _)| s)
            .find(|s| s.id == id))
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

    /// The pages a collector's report for `service` goes to: that service, with source `collector`.
    pub fn collector_pages(&self, service: Service) -> Result<Vec<String>, StoreError> {
        let conn = self.conn();
        let mut st = conn.prepare(
            "SELECT id FROM subscriptions WHERE service = ?1 AND source = 'collector' ORDER BY position",
        )?;
        let rows = st.query_map(params![service.slug()], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    // ----- backup -----

    /// Copies the live database to `dest` with SQLite's online backup, safe while the hub runs.
    pub fn backup_to(&self, dest: &Path) -> Result<(), StoreError> {
        let conn = self.conn();
        let mut out = Connection::open(dest)?;
        let backup = rusqlite::backup::Backup::new(&conn, &mut out)?;
        backup.run_to_completion(256, std::time::Duration::from_millis(10), None)?;
        drop(backup);
        // One self-contained file: no write-ahead log beside it.
        out.pragma_update(None, "journal_mode", "DELETE")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    }
}
