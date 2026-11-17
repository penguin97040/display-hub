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
use std::sync::{Arc, LazyLock, Mutex};

use hub_core::{Settings, Source, StoredContent, StoredPage};
use hub_protocol::device::DeviceInfo;
use hub_protocol::limits::{MAX_METER_ITEMS, MAX_TITLE_LEN};
use hub_protocol::shapes::KIND_METER;
use hub_protocol::text::sanitise_text;
use hub_protocol::{DataQuality, Meter, MeterItem, Timestamp};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};

use crate::secret::Role;

pub mod firmware;

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
    // 4: PC helpers become helpers that send pages to named feeds. Each helper's services become
    // feeds of the same names, and the old name for helpers, in sources, roles and code purposes,
    // becomes `helper`. Tokens and pairings keep working: only names change. SQLite can't change a
    // CHECK constraint in place, so the two tables that have one are copied into new ones.
    r#"
    CREATE TABLE helper_feeds (
        subject TEXT NOT NULL,
        feed TEXT NOT NULL,
        PRIMARY KEY (subject, feed)
    );
    INSERT INTO helper_feeds (subject, feed) SELECT subject, service FROM collector_scopes;
    DROP TABLE collector_scopes;
    UPDATE pages SET source = 'helper' WHERE source = 'collector';

    CREATE TABLE credentials_new (
        token_hash BLOB PRIMARY KEY,
        role TEXT NOT NULL CHECK (role IN ('admin', 'device', 'helper')),
        subject TEXT NOT NULL,
        label TEXT NOT NULL,
        created_at INTEGER NOT NULL,
        revoked_at INTEGER
    );
    INSERT INTO credentials_new (token_hash, role, subject, label, created_at, revoked_at)
        SELECT token_hash, CASE role WHEN 'collector' THEN 'helper' ELSE role END,
               subject, label, created_at, revoked_at
        FROM credentials;
    DROP TABLE credentials;
    ALTER TABLE credentials_new RENAME TO credentials;
    CREATE INDEX credentials_subject ON credentials(role, subject);

    CREATE TABLE one_time_codes_new (
        code_hash BLOB PRIMARY KEY,
        purpose TEXT NOT NULL CHECK (purpose IN ('admin-pair', 'device-enrol', 'helper-pair')),
        label TEXT NOT NULL,
        created_at INTEGER NOT NULL,
        expires_at INTEGER NOT NULL,
        used_at INTEGER
    );
    INSERT INTO one_time_codes_new (code_hash, purpose, label, created_at, expires_at, used_at)
        SELECT code_hash, CASE purpose WHEN 'collector-pair' THEN 'helper-pair' ELSE purpose END,
               label, created_at, expires_at, used_at
        FROM one_time_codes;
    DROP TABLE one_time_codes;
    ALTER TABLE one_time_codes_new RENAME TO one_time_codes;
    "#,
    // 5: the template a page was made from (`name/version`) and the answers to its settings form,
    // as a JSON object. Both are null for pages made without a template.
    r#"
    ALTER TABLE pages ADD COLUMN template TEXT;
    ALTER TABLE pages ADD COLUMN settings TEXT;
    "#,
    // 6: signed firmware updates (`docs/firmware-updates.md`). Packages the hub holds (each file is
    // `firmware/<id>.dhfw` in the data folder), the owner's labels for signing keys, and offers, with
    // at most one open offer per display. Each display gains `auto_update`, always off for now.
    // Publishers, a new role with their own pairing codes, need the two CHECK constraints widened,
    // so those tables are copied as in version 4.
    r#"
    CREATE TABLE firmware_packages (
        id TEXT PRIMARY KEY,
        device_type TEXT NOT NULL,
        hardware_revisions TEXT NOT NULL,
        firmware_version TEXT NOT NULL,
        security_counter INTEGER NOT NULL,
        summary TEXT,
        signer_key_id TEXT NOT NULL,
        signer_public_key TEXT NOT NULL,
        image_size INTEGER NOT NULL,
        size INTEGER NOT NULL,
        update_size INTEGER NOT NULL,
        usb_install INTEGER NOT NULL,
        uploaded_at INTEGER NOT NULL,
        uploaded_by_role TEXT NOT NULL CHECK (uploaded_by_role IN ('admin', 'publisher')),
        uploaded_by TEXT NOT NULL
    );
    CREATE INDEX firmware_packages_device_type ON firmware_packages(device_type);
    CREATE TABLE firmware_signers (
        key_id TEXT PRIMARY KEY,
        label TEXT NOT NULL,
        updated_at INTEGER NOT NULL
    );
    CREATE TABLE firmware_offers (
        id TEXT PRIMARY KEY,
        device_id TEXT NOT NULL REFERENCES devices(id) ON DELETE CASCADE,
        package_id TEXT NOT NULL,
        firmware_version TEXT NOT NULL,
        from_version TEXT,
        made_at INTEGER NOT NULL,
        made_by TEXT NOT NULL,
        expires_at INTEGER NOT NULL,
        state TEXT NOT NULL,
        reason TEXT,
        reported_version TEXT,
        updated_at INTEGER NOT NULL,
        ended_at INTEGER
    );
    CREATE UNIQUE INDEX firmware_offers_one_open ON firmware_offers(device_id) WHERE ended_at IS NULL;
    CREATE INDEX firmware_offers_package ON firmware_offers(package_id);
    ALTER TABLE devices ADD COLUMN auto_update INTEGER NOT NULL DEFAULT 0;

    CREATE TABLE credentials_new (
        token_hash BLOB PRIMARY KEY,
        role TEXT NOT NULL CHECK (role IN ('admin', 'device', 'helper', 'publisher')),
        subject TEXT NOT NULL,
        label TEXT NOT NULL,
        created_at INTEGER NOT NULL,
        revoked_at INTEGER
    );
    INSERT INTO credentials_new (token_hash, role, subject, label, created_at, revoked_at)
        SELECT token_hash, role, subject, label, created_at, revoked_at FROM credentials;
    DROP TABLE credentials;
    ALTER TABLE credentials_new RENAME TO credentials;
    CREATE INDEX credentials_subject ON credentials(role, subject);

    CREATE TABLE one_time_codes_new (
        code_hash BLOB PRIMARY KEY,
        purpose TEXT NOT NULL CHECK (purpose IN ('admin-pair', 'device-enrol', 'helper-pair', 'publisher-pair')),
        label TEXT NOT NULL,
        created_at INTEGER NOT NULL,
        expires_at INTEGER NOT NULL,
        used_at INTEGER
    );
    INSERT INTO one_time_codes_new (code_hash, purpose, label, created_at, expires_at, used_at)
        SELECT code_hash, purpose, label, created_at, expires_at, used_at FROM one_time_codes;
    DROP TABLE one_time_codes;
    ALTER TABLE one_time_codes_new RENAME TO one_time_codes;
    "#,
];

/// Data an upgrade moves with code rather than SQL: run straight after that version's SQL, in the
/// same transaction. Never edit one that has shipped either.
const DATA_MOVES: &[(i64, DataMove)] = &[(3, move_to_pages), (4, store_demo_samples)];

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
    // Ids the admin routes use for themselves, as at version 3. A subscription that had one is
    // renamed to the first unused `<id>-N`.
    const RESERVED: [&str; 1] = ["order"];
    let mut taken: std::collections::HashSet<String> = rows.iter().map(|r| r.0.clone()).collect();
    let mut position = 0i64;
    for (id, service, title, source, created_at, quality, updated_at, metrics, note, stored_at) in rows {
        let id = if RESERVED.contains(&id.as_str()) {
            let new_id = (2..)
                .map(|n| format!("{id}-{n}"))
                .find(|c| !taken.contains(c))
                .expect("an unused id exists");
            tracing::warn!(old = %id, new = %new_id, "renamed a page whose id the admin API now reserves");
            taken.insert(new_id.clone());
            new_id
        } else {
            id
        };
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

/// Version 4: the hub no longer makes up demo figures for usage pages, so each demo usage page
/// that has nothing stored is given the sample figures it showed before, as its stored content.
/// The hub shows a demo page's content as fresh for as long as it keeps it, so the figures carry no
/// reset times, which would pass and clear them.
///
/// This keeps its own copy of the sample figures of that time, as [`move_to_pages`] does of names.
fn store_demo_samples(conn: &Connection) -> Result<(), StoreError> {
    const NOTE: &str = "Sample figures, not real usage";
    /// A sample meter item: its id, label and share.
    type Item = (&'static str, &'static str, f64);
    const SAMPLES: [(&str, &[Item]); 3] = [
        (
            "claude-max",
            &[
                ("five-hour", "Five-hour allowance used", 42.0),
                ("weekly", "Weekly allowance used", 24.0),
            ],
        ),
        (
            "chatgpt-plus",
            &[
                ("five-hour", "Five-hour allowance used", 63.0),
                ("weekly", "Weekly allowance used", 38.0),
            ],
        ),
        (
            "opencode-go-plus",
            &[
                ("five-hour", "Five-hour allowance used", 17.0),
                ("weekly", "Weekly allowance used", 31.0),
                ("monthly", "Monthly allowance used", 9.0),
            ],
        ),
    ];
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    for (feed, figures) in SAMPLES {
        let items = figures
            .iter()
            .map(|&(id, label, percent)| MeterItem::new(id, label, Some(percent), None))
            .collect();
        let data = serde_json::to_string(&Meter { items }).expect("serialises");
        conn.execute(
            "UPDATE pages SET quality = 'demo', updated_at = ?2, data = ?3, note = ?4, stored_at = ?2
             WHERE source = 'demo' AND kind = ?5 AND feed = ?1 AND quality IS NULL",
            params![feed, now, data, NOTE, KIND_METER],
        )?;
    }
    Ok(())
}

/// What [`Store::put_content`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContentWrite {
    Written,
    /// It was already stored, so nothing was written.
    Unchanged,
    /// The page's source is no longer the one the content is for. Nothing was written.
    SourceChanged,
    NoPage,
}

/// What [`Store::update_page`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PageUpdate {
    Updated,
    NoPage,
    /// The change would make it a helper page, which needs a feed for helpers to send to. Nothing
    /// was changed.
    HelperNeedsFeed,
}

/// What [`Store::put_feed_content`] did: the pages it wrote to, and the pages of the feed that take
/// content from a helper but are of another kind, which it left alone.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FeedWrite {
    pub updated: Vec<String>,
    pub skipped: Vec<String>,
}

/// The most pages the hub holds. Each display shows at most its first
/// [`MAX_PAGES`](hub_protocol::limits::MAX_PAGES) ticked ones.
pub const MAX_STORED_PAGES: usize = 50;

/// Lays down schema `version` as this build's migrations wrote it, in an empty database, for tests
/// that need a database from an older release. It moves no data.
#[doc(hidden)]
pub fn create_at_schema(conn: &Connection, version: i64) -> Result<(), StoreError> {
    for sql in MIGRATIONS.iter().take(version as usize) {
        conn.execute_batch(sql)?;
    }
    conn.pragma_update(None, "user_version", version)?;
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
    /// The ids of the pages the display shows (its ticks), in the shared page order.
    pub pages: Vec<String>,
    /// Whether the hub may offer it updates by itself. Always off for now.
    pub auto_update: bool,
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
        let stamp = file_stamp(ts(now));
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
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO devices (id, name, enrolled_at, last_seen_at, info) VALUES (?1, ?2, ?3, ?3, ?4)",
            params![
                id,
                name,
                now.unix(),
                serde_json::to_string(info).expect("serialises")
            ],
        )?;
        // A new display shows every page.
        tx.execute(
            "INSERT INTO display_pages (device_id, page_id) SELECT ?1, id FROM pages",
            params![id],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Displays that haven't been revoked.
    pub fn devices(&self) -> Result<Vec<DeviceRow>, StoreError> {
        let conn = self.conn();
        let mut st = conn.prepare(
            "SELECT id, name, enrolled_at, last_seen_at, info, auto_update FROM devices WHERE revoked_at IS NULL ORDER BY enrolled_at",
        )?;
        let rows = st.query_map([], device_row)?;
        let mut devices: Vec<DeviceRow> = rows.collect::<Result<_, _>>()?;
        // Every display's ticks in a second query (not one per display), in the shared page order.
        let at: std::collections::HashMap<String, usize> = devices
            .iter()
            .enumerate()
            .map(|(i, d)| (d.id.clone(), i))
            .collect();
        let mut st = conn.prepare(
            "SELECT t.device_id, p.id FROM display_pages t JOIN pages p ON p.id = t.page_id
             ORDER BY p.position, p.created_at",
        )?;
        let ticks = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        for tick in ticks {
            let (device, page) = tick?;
            if let Some(&i) = at.get(&device) {
                devices[i].pages.push(page);
            }
        }
        Ok(devices)
    }

    /// One active display, without its ticks.
    pub fn device(&self, id: &str) -> Result<Option<DeviceRow>, StoreError> {
        Ok(self
            .conn()
            .query_row(
                "SELECT id, name, enrolled_at, last_seen_at, info, auto_update FROM devices WHERE id = ?1 AND revoked_at IS NULL",
                params![id],
                device_row,
            )
            .optional()?)
    }

    /// How many displays are active, without reading their ticks.
    pub fn device_count(&self) -> Result<usize, StoreError> {
        let n: i64 =
            self.conn()
                .query_row("SELECT COUNT(*) FROM devices WHERE revoked_at IS NULL", [], |r| {
                    r.get(0)
                })?;
        Ok(n as usize)
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
        tx.execute("DELETE FROM display_pages WHERE device_id = ?1", params![id])?;
        firmware::cancel_for_removed_display(&tx, id, now)?;
        tx.commit()?;
        Ok(n > 0)
    }

    /// Whether `id` is a display that hasn't been revoked.
    pub fn device_active(&self, id: &str) -> Result<bool, StoreError> {
        Ok(self.conn().query_row(
            "SELECT EXISTS(SELECT 1 FROM devices WHERE id = ?1 AND revoked_at IS NULL)",
            params![id],
            |r| r.get(0),
        )?)
    }

    /// The ids of the pages a display shows (its ticks), in the shared page order. `None` if there
    /// is no such active display.
    pub fn display_page_ids(&self, device: &str) -> Result<Option<Vec<String>>, StoreError> {
        if !self.device_active(device)? {
            return Ok(None);
        }
        let conn = self.conn();
        let mut st = conn.prepare(
            "SELECT p.id FROM display_pages t JOIN pages p ON p.id = t.page_id
             WHERE t.device_id = ?1 ORDER BY p.position, p.created_at",
        )?;
        let rows = st.query_map(params![device], |r| r.get(0))?;
        Ok(Some(rows.collect::<Result<_, _>>()?))
    }

    /// Sets which pages a display shows. Each id must be a page; order and repeats don't matter,
    /// since displays share one page order. False if there is no such active display.
    pub fn set_display_pages(&self, device: &str, ids: &[String]) -> Result<bool, StoreError> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let active: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM devices WHERE id = ?1 AND revoked_at IS NULL)",
            params![device],
            |r| r.get(0),
        )?;
        if !active {
            return Ok(false);
        }
        for id in ids {
            let exists: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM pages WHERE id = ?1)",
                params![id],
                |r| r.get(0),
            )?;
            if !exists {
                return Err(StoreError::Invalid(format!("no page with id {id:?}")));
            }
        }
        tx.execute("DELETE FROM display_pages WHERE device_id = ?1", params![device])?;
        for id in ids {
            tx.execute(
                "INSERT OR IGNORE INTO display_pages (device_id, page_id) VALUES (?1, ?2)",
                params![device, id],
            )?;
        }
        tx.commit()?;
        Ok(true)
    }

    /// The pages a display's snapshot is built from: its first
    /// [`MAX_PAGES`](hub_protocol::limits::MAX_PAGES) ticked pages, in order. `None` if there is
    /// no such active display. One query, since every poll asks.
    pub fn display_pages(&self, device: &str) -> Result<Option<Vec<StoredPage>>, StoreError> {
        let conn = self.conn();
        let pages = read_pages(
            &conn,
            &DISPLAY_PAGES_SQL,
            params![device, hub_protocol::limits::MAX_PAGES as i64],
        )?;
        if pages.is_empty() {
            // No ticked pages, or no such display: only the second needs another look.
            let active: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM devices WHERE id = ?1 AND revoked_at IS NULL)",
                params![device],
                |r| r.get(0),
            )?;
            if !active {
                return Ok(None);
            }
        }
        Ok(Some(pages))
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
        read_pages(&conn, &ALL_PAGES_SQL, [])
    }

    pub fn page(&self, id: &str) -> Result<Option<StoredPage>, StoreError> {
        let conn = self.conn();
        Ok(read_pages(&conn, &ONE_PAGE_SQL, params![id])?.into_iter().next())
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
        if count as usize >= MAX_STORED_PAGES {
            return Err(StoreError::Invalid(format!(
                "the hub holds at most {MAX_STORED_PAGES} pages; remove one first"
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
            "INSERT INTO pages (id, title, kind, source, feed, position, stale_after_seconds, created_at,
                                template, settings)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                page.id,
                page.title,
                page.kind,
                page.source.slug(),
                page.feed,
                next,
                page.stale_after_seconds,
                now.unix(),
                page.template,
                settings_column(page.settings.as_ref()),
            ],
        )?;
        if let Some(content) = &page.content {
            write_content(&tx, &page.id, content, now)?;
        }
        // A new page shows on every display; each display's ticks can then take it off.
        tx.execute(
            "INSERT INTO display_pages (device_id, page_id) SELECT id, ?1 FROM devices WHERE revoked_at IS NULL",
            params![page.id],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Changes a page's title, source, stale time or settings, all or nothing. Changing the source
    /// drops the stored content, which came from the old source, and resets the stale time to the
    /// new source's default unless `stale_after_seconds` is given too. New settings replace the old;
    /// empty ones remove them.
    pub fn update_page(
        &self,
        id: &str,
        title: Option<&str>,
        source: Option<Source>,
        stale_after_seconds: Option<u32>,
        settings: Option<&Settings>,
    ) -> Result<PageUpdate, StoreError> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let row: Option<(String, Option<String>)> = tx
            .query_row("SELECT source, feed FROM pages WHERE id = ?1", params![id], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .optional()?;
        let Some((current, feed)) = row else {
            return Ok(PageUpdate::NoPage);
        };
        let new_source = source.filter(|s| s.slug() != current);
        if new_source == Some(Source::Helper) && feed.is_none() {
            return Ok(PageUpdate::HelperNeedsFeed);
        }
        if let Some(t) = title {
            tx.execute("UPDATE pages SET title = ?1 WHERE id = ?2", params![t, id])?;
        }
        if let Some(s) = new_source {
            tx.execute(
                "UPDATE pages SET source = ?1, stale_after_seconds = ?2, quality = NULL, updated_at = NULL,
                                  data = NULL, note = NULL, stored_at = NULL
                 WHERE id = ?3",
                params![s.slug(), s.stale_after_seconds(), id],
            )?;
        }
        if let Some(stale) = stale_after_seconds {
            tx.execute(
                "UPDATE pages SET stale_after_seconds = ?1 WHERE id = ?2",
                params![stale, id],
            )?;
        }
        if let Some(settings) = settings {
            tx.execute(
                "UPDATE pages SET settings = ?1 WHERE id = ?2",
                params![settings_column(Some(settings)), id],
            )?;
        }
        tx.commit()?;
        Ok(PageUpdate::Updated)
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

    /// Stores a page's content, replacing what was there, but only while its source is `source`:
    /// the check and the write are one transaction. A write that changes nothing is skipped, to
    /// spare the SD card.
    pub fn put_content(
        &self,
        id: &str,
        source: Source,
        content: &StoredContent,
        now: Timestamp,
    ) -> Result<ContentWrite, StoreError> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let row: Option<(String, ContentColumns)> = tx
            .query_row(
                "SELECT source, quality, updated_at, data, note FROM pages WHERE id = ?1",
                params![id],
                |r| Ok((r.get(0)?, (r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))),
            )
            .optional()?;
        let Some((stored_source, stored)) = row else {
            return Ok(ContentWrite::NoPage);
        };
        if stored_source != source.slug() {
            return Ok(ContentWrite::SourceChanged);
        }
        if stored == content_columns(content) {
            return Ok(ContentWrite::Unchanged);
        }
        write_content(&tx, id, content, now)?;
        tx.commit()?;
        Ok(ContentWrite::Written)
    }

    /// Writes a helper's content to every page of `feed` whose source is `helper` and whose kind is
    /// `kind`, in one transaction. Pages it wouldn't change are not rewritten. `check` runs on each page
    /// that would be written, inside the transaction; if it fails for any of them nothing is written and
    /// its error is returned.
    pub fn put_feed_content<E: From<StoreError>>(
        &self,
        feed: &str,
        kind: &str,
        content: &StoredContent,
        now: Timestamp,
        mut check: impl FnMut(&StoredPage) -> Result<(), E>,
    ) -> Result<FeedWrite, E> {
        let mut conn = self.conn();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(StoreError::from)?;
        let mut out = FeedWrite::default();
        let pages = read_pages(&tx, &FEED_PAGES_SQL, params![feed])?;
        for page in pages {
            if page.kind == kind {
                check(&page)?;
                out.updated.push(page.id);
            } else {
                out.skipped.push(page.id);
            }
        }
        let (quality, updated_at, data, note) = content_columns(content);
        for id in &out.updated {
            tx.execute(
                "UPDATE pages SET quality = ?2, updated_at = ?3, data = ?4, note = ?5, stored_at = ?6
                 WHERE id = ?1 AND NOT (quality IS ?2 AND updated_at IS ?3 AND data IS ?4 AND note IS ?5)",
                params![id, quality, updated_at, data, note, now.unix()],
            )
            .map_err(StoreError::from)?;
        }
        tx.commit().map_err(StoreError::from)?;
        Ok(out)
    }

    /// The pages of `feed` whose source is `helper`, in order: the pages a helper of the feed fills.
    pub fn feed_pages(&self, feed: &str) -> Result<Vec<StoredPage>, StoreError> {
        let conn = self.conn();
        read_pages(&conn, &FEED_PAGES_SQL, params![feed])
    }

    // ----- helpers -----

    pub fn add_helper_feeds(&self, subject: &str, feeds: &[String]) -> Result<(), StoreError> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        for f in feeds {
            tx.execute(
                "INSERT OR IGNORE INTO helper_feeds (subject, feed) VALUES (?1, ?2)",
                params![subject, f],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// The feeds a helper may send to, in name order.
    pub fn helper_feeds(&self, subject: &str) -> Result<Vec<String>, StoreError> {
        let conn = self.conn();
        let mut st = conn.prepare("SELECT feed FROM helper_feeds WHERE subject = ?1 ORDER BY feed")?;
        let rows = st.query_map(params![subject], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Every helper that has paired, newest first.
    pub fn helpers(&self) -> Result<Vec<(String, String, Timestamp, bool)>, StoreError> {
        let conn = self.conn();
        let mut st = conn.prepare(
            "SELECT subject, label, created_at, revoked_at FROM credentials WHERE role = 'helper' ORDER BY created_at DESC",
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

/// A display's row: id, name, enrolled_at, last_seen_at, info and auto_update, in that order.
fn device_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<DeviceRow> {
    Ok(DeviceRow {
        id: r.get(0)?,
        name: r.get(1)?,
        enrolled_at: ts(r.get(2)?),
        last_seen_at: r.get::<_, Option<i64>>(3)?.map(ts),
        info: r
            .get::<_, Option<String>>(4)?
            .and_then(|s| serde_json::from_str(&s).ok()),
        pages: Vec::new(),
        auto_update: r.get(5)?,
    })
}

/// The columns [`read_pages`] reads, from `pages` aliased as `p`.
const PAGE_COLUMNS: &str = "p.id, p.title, p.kind, p.source, p.feed, p.stale_after_seconds, p.quality, \
     p.updated_at, p.data, p.note, p.template, p.settings";

static ALL_PAGES_SQL: LazyLock<String> =
    LazyLock::new(|| format!("SELECT {PAGE_COLUMNS} FROM pages p ORDER BY p.position, p.created_at"));

static ONE_PAGE_SQL: LazyLock<String> =
    LazyLock::new(|| format!("SELECT {PAGE_COLUMNS} FROM pages p WHERE p.id = ?1"));

/// The helper pages of the feed `?1`, in order.
static FEED_PAGES_SQL: LazyLock<String> = LazyLock::new(|| {
    format!(
        "SELECT {PAGE_COLUMNS} FROM pages p WHERE p.feed = ?1 AND p.source = 'helper' \
         ORDER BY p.position, p.created_at"
    )
});

/// A display's ticked pages whose source this build knows, in order, when the display is active.
/// `?1` is the display, `?2` the most pages to return.
static DISPLAY_PAGES_SQL: LazyLock<String> = LazyLock::new(|| {
    let known = Source::ALL.map(|s| format!("'{}'", s.slug())).join(", ");
    format!(
        "SELECT {PAGE_COLUMNS} FROM display_pages t
         JOIN devices d ON d.id = t.device_id AND d.revoked_at IS NULL
         JOIN pages p ON p.id = t.page_id AND p.source IN ({known})
         WHERE t.device_id = ?1
         ORDER BY p.position, p.created_at LIMIT ?2"
    )
});

/// Runs a query selecting [`PAGE_COLUMNS`] and reads each row as a page.
fn read_pages(
    conn: &Connection,
    sql: &str,
    args: impl rusqlite::Params,
) -> Result<Vec<StoredPage>, StoreError> {
    let mut st = conn.prepare_cached(sql)?;
    let rows = st.query_map(args, |r| {
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
            (r.get::<_, Option<String>>(10)?, r.get::<_, Option<String>>(11)?),
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (id, title, kind, source, feed, stale, quality, updated, data, note, (template, settings)) = row?;
        // A row this build can't interpret is skipped and logged, rather than breaking every snapshot.
        let Some(source) = Source::from_slug(&source) else {
            tracing::warn!(%id, "skipping a page with an unknown source");
            continue;
        };
        let content = quality.map(|q| StoredContent {
            quality: serde_json::from_value(serde_json::Value::String(q)).unwrap_or(DataQuality::Unavailable),
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
        let settings = settings.and_then(|s| match serde_json::from_str::<Settings>(&s) {
            Ok(v) => Some(v),
            Err(_) => {
                tracing::warn!(%id, "ignoring a page's stored settings, which aren't a JSON object");
                None
            }
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
            template,
            settings,
        });
    }
    Ok(out)
}

/// Settings as their column: JSON text, or null when there are none.
fn settings_column(settings: Option<&Settings>) -> Option<String> {
    settings
        .filter(|s| !s.is_empty())
        .map(|s| serde_json::to_string(s).expect("settings serialise"))
}

/// `2026-11-11T01-02-03Z`: a timestamp that is also a valid file name everywhere.
pub(crate) fn file_stamp(t: Timestamp) -> String {
    t.to_string().replace(':', "-")
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
    const SCHEMA_2_DATA: &str = include_str!("../tests/fixtures/schema-2-data.sql");

    /// A database exactly as version 2 left it, with [`SCHEMA_2_DATA`] in it.
    fn schema_2_database(path: &Path) {
        let conn = Connection::open(path).unwrap();
        create_at_schema(&conn, 2).unwrap();
        conn.execute_batch(SCHEMA_2_DATA).unwrap();
    }

    /// What the schema 2 code built from [`SCHEMA_2_DATA`], captured before it was replaced, as an
    /// upgraded hub serves it. Since version 4, demo figures are stored rather than made up by the
    /// hub, and have no reset times.
    fn schema_2_pages() -> serde_json::Value {
        let mut pages: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/schema-2-pages.json")).unwrap();
        for p in pages.as_array_mut().unwrap() {
            if p["data_quality"] == "demo" {
                for item in p["data"]["items"].as_array_mut().unwrap() {
                    item.as_object_mut().unwrap().remove("resets_at");
                }
            }
        }
        pages
    }

    /// Every row of the tables an upgrade must keep, as text. Version 4 renamed helpers' role, code
    /// purpose and table of scopes, so those are read under today's names.
    fn untouched_tables(conn: &Connection) -> Vec<String> {
        let upgraded: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name = 'helper_feeds')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let scopes = if upgraded {
            "SELECT subject, feed FROM helper_feeds"
        } else {
            "SELECT subject, service FROM collector_scopes"
        };
        let role = "CASE role WHEN 'collector' THEN 'helper' ELSE role END";
        let purpose = "CASE purpose WHEN 'collector-pair' THEN 'helper-pair' ELSE purpose END";
        let mut out = Vec::new();
        for (table, sql) in [
            (
                "devices",
                "SELECT id, name, enrolled_at, last_seen_at, info, revoked_at FROM devices ORDER BY id".to_string(),
            ),
            (
                "credentials",
                format!("SELECT token_hash, {role}, subject, label, created_at, revoked_at FROM credentials ORDER BY token_hash"),
            ),
            (
                "one_time_codes",
                format!("SELECT code_hash, {purpose}, label, created_at, expires_at, used_at FROM one_time_codes ORDER BY code_hash"),
            ),
            ("helper_feeds", format!("{scopes} ORDER BY 1, 2")),
            ("audit_log", "SELECT * FROM audit_log ORDER BY id".to_string()),
        ] {
            let mut st = conn.prepare(&sql).unwrap();
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
        assert_eq!(store.page_ids().unwrap(), vec!["claude-max".to_string()]);
        let report = StoredContent {
            quality: DataQuality::ProviderReported,
            updated_at: Some(ts(NOW)),
            data: Some(json!({"items": [{"id": "weekly", "label": "Weekly", "percent": 1.0}]})),
            note: None,
        };
        assert_eq!(
            store
                .put_feed_content("claude-max", "meter", &report, ts(NOW), |_| Ok::<(), StoreError>(
                    ()
                ))
                .unwrap()
                .updated,
            vec!["claude-max".to_string()]
        );
        store
            .add_helper_feeds("h-1", &["claude-max".into(), "claude-max".into()])
            .unwrap();
        assert_eq!(store.helper_feeds("h-1").unwrap(), vec!["claude-max".to_string()]);
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
        assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
        let expected = schema_2_pages();
        // Each display, with every page ticked, gets what every display got before; so does the
        // all-pages preview.
        for display in ["d-desk", "d-kitchen"] {
            let pages = store.display_pages(display).unwrap().unwrap();
            let snap = hub_core::build_snapshot(ts(NOW), 300, &pages);
            snap.validate().unwrap();
            assert_eq!(serde_json::to_value(&snap.pages).unwrap(), expected, "{display}");
        }
        assert_eq!(store.display_pages("d-old").unwrap(), None);
        let all = hub_core::build_snapshot(ts(NOW), 300, &store.pages().unwrap());
        assert_eq!(serde_json::to_value(&all.pages).unwrap(), expected);

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
            template: None,
            settings: None,
        };
        store.add_page(&page, now).unwrap();
        assert_eq!(store.pages().unwrap(), vec![page.clone()]);
        let content = page.content.clone().unwrap();
        assert_eq!(
            store
                .put_content("flights", Source::Manual, &content, now)
                .unwrap(),
            ContentWrite::Unchanged
        );
        let changed = StoredContent {
            data: Some(json!({"flights": []})),
            ..content
        };
        assert_eq!(
            store
                .put_content("flights", Source::Manual, &changed, now)
                .unwrap(),
            ContentWrite::Written
        );
        assert_eq!(store.page("flights").unwrap().unwrap().content, Some(changed));
        assert_eq!(
            store
                .put_content("nope", Source::Manual, &page.content.clone().unwrap(), now)
                .unwrap(),
            ContentWrite::NoPage
        );
    }

    fn text_page(id: &str) -> StoredPage {
        StoredPage {
            id: id.into(),
            title: id.into(),
            kind: "text".into(),
            source: Source::Manual,
            feed: None,
            stale_after_seconds: 600,
            content: None,
            template: None,
            settings: None,
        }
    }

    fn device(store: &Store, id: &str) {
        let info = DeviceInfo {
            device_type: "test-display".into(),
            firmware_version: "0.0.0-test".into(),
            hardware_revision: "simulated".into(),
            protocol_version: 1,
            firmware: None,
        };
        store.add_device(id, id, &info, ts(NOW)).unwrap();
    }

    #[test]
    fn each_display_shows_its_ticked_pages_in_the_shared_order() {
        let store = Store::in_memory().unwrap();
        store.add_page(&text_page("a"), ts(NOW)).unwrap();
        device(&store, "d-1");
        store.add_page(&text_page("b"), ts(NOW)).unwrap();
        device(&store, "d-2");
        let ids = |d: &str| store.display_page_ids(d).unwrap().unwrap();
        // A new page shows on every display, and a new display shows every page.
        assert_eq!(ids("d-1"), ["a", "b"]);
        assert_eq!(ids("d-2"), ["a", "b"]);
        store.add_page(&text_page("c"), ts(NOW)).unwrap();

        // Ticks are a set: the shared order decides how they are shown.
        assert!(store
            .set_display_pages("d-1", &["c".into(), "a".into(), "c".into()])
            .unwrap());
        assert_eq!(ids("d-1"), ["a", "c"]);
        assert_eq!(ids("d-2"), ["a", "b", "c"]);
        store.reorder(&["c".into(), "b".into(), "a".into()]).unwrap();
        assert_eq!(ids("d-1"), ["c", "a"]);
        let shown: Vec<String> = store
            .display_pages("d-1")
            .unwrap()
            .unwrap()
            .into_iter()
            .map(|p| p.id)
            .collect();
        assert_eq!(shown, ["c", "a"]);

        assert!(matches!(
            store.set_display_pages("d-1", &["nope".into()]),
            Err(StoreError::Invalid(_))
        ));
        assert_eq!(ids("d-1"), ["c", "a"], "a refused change changes nothing");
        // The device list carries every display's ticks, in the same order.
        assert_eq!(store.device_count().unwrap(), 2);
        let listed: Vec<(String, Vec<String>)> = store
            .devices()
            .unwrap()
            .into_iter()
            .map(|d| (d.id, d.pages))
            .collect();
        assert_eq!(
            listed,
            [
                ("d-1".to_string(), vec!["c".to_string(), "a".to_string()]),
                (
                    "d-2".to_string(),
                    vec!["c".to_string(), "b".to_string(), "a".to_string()]
                ),
            ]
        );
        assert!(!store.set_display_pages("d-unknown", &[]).unwrap());
        assert!(store.set_display_pages("d-2", &[]).unwrap());
        assert!(ids("d-2").is_empty());

        // Removing a page or revoking a display takes its ticks with it.
        assert!(store.remove_page("c").unwrap());
        assert_eq!(ids("d-1"), ["a"]);
        assert!(store.revoke_device("d-1", ts(NOW)).unwrap());
        assert_eq!(store.display_page_ids("d-1").unwrap(), None);
        let left: i64 = store
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM display_pages WHERE device_id = 'd-1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(left, 0);
        assert!(!store.set_display_pages("d-1", &["a".into()]).unwrap());
    }

    #[test]
    fn the_hub_holds_50_pages_and_each_display_gets_its_first_10_ticked() {
        let store = Store::in_memory().unwrap();
        device(&store, "d-1");
        for i in 0..MAX_STORED_PAGES {
            store.add_page(&text_page(&format!("p{i}")), ts(NOW)).unwrap();
        }
        assert!(matches!(
            store.add_page(&text_page("one-more"), ts(NOW)),
            Err(StoreError::Invalid(_))
        ));
        assert_eq!(store.pages().unwrap().len(), MAX_STORED_PAGES);
        assert_eq!(
            store.display_page_ids("d-1").unwrap().unwrap().len(),
            MAX_STORED_PAGES
        );
        let shown = |store: &Store| -> Vec<String> {
            store
                .display_pages("d-1")
                .unwrap()
                .unwrap()
                .into_iter()
                .map(|p| p.id)
                .collect()
        };
        let first: Vec<String> = (0..10).map(|i| format!("p{i}")).collect();
        assert_eq!(shown(&store), first);
        // Unticking pages brings later ticked ones forward.
        let odd: Vec<String> = (0..MAX_STORED_PAGES)
            .filter(|i| i % 2 == 1)
            .map(|i| format!("p{i}"))
            .collect();
        store.set_display_pages("d-1", &odd).unwrap();
        assert_eq!(shown(&store), odd[..10]);
        let snap = hub_core::build_snapshot(ts(NOW), 300, &store.display_pages("d-1").unwrap().unwrap());
        assert_eq!(snap.pages.len(), hub_protocol::limits::MAX_PAGES);
    }

    #[test]
    fn content_is_written_only_while_the_page_keeps_its_source() {
        let store = Store::in_memory().unwrap();
        let now = ts(NOW);
        store.add_page(&text_page("a"), now).unwrap();
        let content = StoredContent {
            quality: DataQuality::Manual,
            updated_at: Some(now),
            data: Some(json!({"text": "Hello"})),
            note: None,
        };
        assert_eq!(
            store.put_content("a", Source::Manual, &content, now).unwrap(),
            ContentWrite::Written
        );
        // Its source changed since the caller looked: the write is refused, and nothing changes.
        store
            .update_page("a", None, Some(Source::Demo), None, None)
            .unwrap();
        assert_eq!(
            store.put_content("a", Source::Manual, &content, now).unwrap(),
            ContentWrite::SourceChanged
        );
        assert_eq!(store.page("a").unwrap().unwrap().content, None);
        assert_eq!(store.page("nope").unwrap(), None);
    }

    #[test]
    fn helper_content_reaches_only_helper_pages_of_its_feed_and_kind() {
        let store = Store::in_memory().unwrap();
        let now = ts(NOW);
        let page = |id: &str, kind: &str, source: Source, feed: Option<&str>| StoredPage {
            kind: kind.into(),
            source,
            feed: feed.map(String::from),
            ..text_page(id)
        };
        for p in [
            page("meter", "meter", Source::Helper, Some("garden")),
            page("text", "text", Source::Helper, Some("garden")),
            page("manual", "meter", Source::Manual, Some("garden")),
            page("other", "meter", Source::Helper, Some("kitchen")),
            page("later", "meter", Source::Helper, Some("garden")),
        ] {
            store.add_page(&p, now).unwrap();
        }
        let content = StoredContent {
            quality: DataQuality::ProviderReported,
            updated_at: Some(now),
            data: Some(json!({"items": [{"id": "soil", "label": "Soil moisture", "percent": 5.0}]})),
            note: None,
        };
        let ok = |_: &StoredPage| Ok::<(), StoreError>(());
        // A check that fails for the last page it sees stops the whole write: nothing is changed.
        let mut seen = Vec::new();
        #[derive(Debug)]
        struct Refused;
        impl From<StoreError> for Refused {
            fn from(_: StoreError) -> Self {
                Refused
            }
        }
        let refused = store.put_feed_content("garden", "meter", &content, now, |p| {
            seen.push(p.id.clone());
            if p.id == "later" {
                Err(Refused)
            } else {
                Ok(())
            }
        });
        assert!(refused.is_err());
        assert_eq!(
            seen,
            ["meter", "later"],
            "only helper pages of the feed and kind are checked"
        );
        assert!(
            store.page("meter").unwrap().unwrap().content.is_none(),
            "rolled back"
        );
        assert_eq!(
            store
                .put_feed_content("garden", "meter", &content, now, ok)
                .unwrap(),
            FeedWrite {
                updated: vec!["meter".into(), "later".into()],
                skipped: vec!["text".into()],
            }
        );
        let stored: Vec<(String, bool)> = store
            .pages()
            .unwrap()
            .into_iter()
            .map(|p| (p.id, p.content.is_some()))
            .collect();
        assert_eq!(
            stored,
            [
                ("meter".into(), true),
                ("text".into(), false),
                ("manual".into(), false),
                ("other".into(), false),
                ("later".into(), true),
            ]
        );
        assert_eq!(
            store
                .put_feed_content("nobody", "meter", &content, now, ok)
                .unwrap(),
            FeedWrite::default()
        );
    }

    #[test]
    fn switching_to_helper_needs_a_feed_and_changes_nothing_otherwise() {
        let store = Store::in_memory().unwrap();
        let now = ts(NOW);
        let fed = StoredPage {
            feed: Some("garden".into()),
            ..text_page("fed")
        };
        store.add_page(&text_page("bare"), now).unwrap();
        store.add_page(&fed, now).unwrap();
        let to_helper = |id: &str| {
            store
                .update_page(id, Some("Changed"), Some(Source::Helper), Some(900), None)
                .unwrap()
        };
        assert_eq!(to_helper("bare"), PageUpdate::HelperNeedsFeed);
        let p = store.page("bare").unwrap().unwrap();
        assert_eq!(
            (p.title.as_str(), p.source, p.stale_after_seconds),
            ("bare", Source::Manual, 600)
        );
        // Any kind may take content from a helper.
        assert_eq!(to_helper("fed"), PageUpdate::Updated);
        assert_eq!(store.page("fed").unwrap().unwrap().source, Source::Helper);
        // Not a change of source, so the rule doesn't apply; nor does it to a missing page.
        assert_eq!(to_helper("fed"), PageUpdate::Updated);
        assert_eq!(to_helper("nope"), PageUpdate::NoPage);
    }

    /// Representative data in the layout schema 3 used.
    const SCHEMA_3_DATA: &str = include_str!("../tests/fixtures/schema-3-data.sql");

    #[test]
    fn upgrading_schema_3_makes_scopes_feeds_and_keeps_helpers_working() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hub.db");
        {
            let conn = Connection::open(&path).unwrap();
            create_at_schema(&conn, 3).unwrap();
            conn.execute_batch(SCHEMA_3_DATA).unwrap();
        }
        let before = untouched_tables(&Connection::open(&path).unwrap());
        let store = Store::open(&path).unwrap();
        assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
        let saved = copies(dir.path());
        assert_eq!(saved.len(), 1);
        let name = saved[0].file_name().unwrap().to_str().unwrap();
        assert!(name.starts_with("hub-before-upgrade-schema-3-"), "{name}");
        assert_eq!(Store::inspect(&saved[0]).unwrap(), (3, 4, 1));

        // Every row is kept, under the new names.
        assert_eq!(untouched_tables(&store.conn()), before);
        assert_eq!(
            store.helper_feeds("c-pc").unwrap(),
            ["chatgpt-plus", "claude-max"]
        );
        let token = crate::secret::hash("dhc_test_helper");
        assert_eq!(
            store.credential(&token).unwrap(),
            Some(Credential {
                role: Role::Helper,
                subject: "c-pc".into(),
                revoked: false,
            })
        );
        let helpers = store.helpers().unwrap();
        assert_eq!(
            helpers.iter().map(|h| (h.0.as_str(), h.3)).collect::<Vec<_>>(),
            [("c-old", true), ("c-pc", false)]
        );
        // An invite made before the upgrade can still be used once.
        let invite = crate::secret::hash("invite-before-upgrade");
        assert_eq!(
            store.use_code(&invite, "helper-pair", ts(NOW)).unwrap().0,
            CodeUse::Accepted
        );

        // The helper page's source is renamed and its figures kept; a demo usage page with nothing
        // stored gets the sample figures it showed before, with no reset times.
        let pages = store.pages().unwrap();
        let sources: Vec<(&str, Source)> = pages.iter().map(|p| (p.id.as_str(), p.source)).collect();
        assert_eq!(
            sources,
            [
                ("claude-max", Source::Helper),
                ("words", Source::Manual),
                ("chatgpt-plus", Source::Demo),
                ("flights", Source::Demo),
            ]
        );
        let reported = pages[0].content.as_ref().unwrap();
        assert_eq!(reported.quality, DataQuality::ProviderReported);
        assert_eq!(
            reported.data.as_ref().unwrap()["items"][1]["percent"],
            json!(30.5)
        );
        let demo = pages[2].content.as_ref().unwrap();
        assert_eq!(demo.quality, DataQuality::Demo);
        assert_eq!(
            demo.data,
            Some(json!({"items": [
                {"id": "five-hour", "label": "Five-hour allowance used", "percent": 63.0},
                {"id": "weekly", "label": "Weekly allowance used", "percent": 38.0}
            ]}))
        );
        assert_eq!(demo.note.as_deref(), Some("Sample figures, not real usage"));
        assert_eq!(pages[3].content, None, "only demo usage pages get sample figures");
        let snap = hub_core::build_snapshot(ts(NOW), 300, &store.display_pages("d-desk").unwrap().unwrap());
        snap.validate().unwrap();
        assert_eq!(snap.pages.len(), 4);
        assert_eq!(snap.pages[2].data_quality, DataQuality::Demo);

        // The helper's figures still reach its page through its feed.
        let content = StoredContent {
            quality: DataQuality::ProviderReported,
            updated_at: Some(ts(NOW)),
            data: Some(json!({"items": []})),
            note: None,
        };
        assert_eq!(
            store
                .put_feed_content(
                    "claude-max",
                    "meter",
                    &content,
                    ts(NOW),
                    |_| Ok::<(), StoreError>(())
                )
                .unwrap()
                .updated,
            ["claude-max"]
        );
        let leftovers: i64 = store
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name IN ('collector_scopes', 'credentials_new', 'one_time_codes_new')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(leftovers, 0);
    }

    #[test]
    fn a_subscription_with_a_reserved_id_is_renamed_by_the_upgrade() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hub.db");
        {
            let conn = Connection::open(&path).unwrap();
            create_at_schema(&conn, 2).unwrap();
            conn.execute_batch(
                "INSERT INTO subscriptions (id, service, title, source, position, created_at) VALUES
                     ('order', 'claude-max', 'Order', 'manual', 0, 1),
                     ('order-2', 'claude-max', 'Taken', 'manual', 1, 2);
                 INSERT INTO page_data (subscription_id, quality, updated_at, metrics, note, stored_at) VALUES
                     ('order', 'manual', 1794340200, '[{\"window\":\"weekly\",\"used_percent\":5.0,\"resets_at\":null}]', 'Kept', 1);
                 INSERT INTO devices (id, name, enrolled_at) VALUES ('d-1', 'Desk', 1);",
            )
            .unwrap();
        }
        let store = Store::open(&path).unwrap();
        assert_eq!(store.page_ids().unwrap(), ["order-3", "order-2"]);
        let renamed = store.page("order-3").unwrap().unwrap();
        assert_eq!(renamed.title, "Order");
        let content = renamed.content.unwrap();
        assert_eq!(content.note.as_deref(), Some("Kept"));
        assert_eq!(content.data.unwrap()["items"][0]["percent"], json!(5.0));
        assert_eq!(
            store.display_page_ids("d-1").unwrap().unwrap(),
            ["order-3", "order-2"]
        );
    }

    /// Representative data in the layout schema 4 used.
    const SCHEMA_4_DATA: &str = include_str!("../tests/fixtures/schema-4-data.sql");

    /// Every column version 4 had in `pages` and `display_pages`, as text, in a fixed order.
    fn page_rows(conn: &Connection) -> Vec<String> {
        let mut out = Vec::new();
        for sql in [
            "SELECT id, title, kind, source, feed, position, stale_after_seconds, created_at, quality, \
             updated_at, data, note, stored_at FROM pages ORDER BY id",
            "SELECT device_id, page_id FROM display_pages ORDER BY 1, 2",
        ] {
            let mut st = conn.prepare(sql).unwrap();
            let n = st.column_count();
            let rows = st
                .query_map([], |r| {
                    Ok((0..n)
                        .map(|i| format!("{:?}", r.get_ref(i).unwrap()))
                        .collect::<Vec<_>>()
                        .join(", "))
                })
                .unwrap();
            out.extend(rows.map(Result::unwrap));
        }
        out
    }

    #[test]
    fn upgrading_schema_4_keeps_every_row_and_gives_pages_room_for_settings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hub.db");
        {
            let conn = Connection::open(&path).unwrap();
            create_at_schema(&conn, 4).unwrap();
            conn.execute_batch(SCHEMA_4_DATA).unwrap();
        }
        let (before, pages_before) = {
            let conn = Connection::open(&path).unwrap();
            (untouched_tables(&conn), page_rows(&conn))
        };
        let store = Store::open(&path).unwrap();
        assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
        let saved = copies(dir.path());
        assert_eq!(saved.len(), 1);
        let name = saved[0].file_name().unwrap().to_str().unwrap();
        assert!(name.starts_with("hub-before-upgrade-schema-4-"), "{name}");
        assert_eq!(Store::inspect(&saved[0]).unwrap(), (4, 4, 2));
        assert_eq!(page_rows(&Connection::open(&saved[0]).unwrap()), pages_before);

        // Every row is kept as it was, and no page has a template or settings yet.
        assert_eq!(untouched_tables(&store.conn()), before);
        assert_eq!(page_rows(&store.conn()), pages_before);
        let pages = store.pages().unwrap();
        assert_eq!(
            pages.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(),
            ["pi-status", "words", "chatgpt-plus", "flights"]
        );
        assert!(pages.iter().all(|p| p.template.is_none() && p.settings.is_none()));
        let snap = hub_core::build_snapshot(ts(NOW), 300, &store.display_pages("d-desk").unwrap().unwrap());
        snap.validate().unwrap();
        assert_eq!(snap.pages.len(), 3);
        assert_eq!(snap.pages[0].data.as_ref().unwrap()["value"], json!(48.2));

        // The helper and the invite keep working, and a page can now keep settings.
        let token = crate::secret::hash("dhc_test_helper");
        assert_eq!(store.credential(&token).unwrap().unwrap().subject, "h-pi");
        assert_eq!(store.helper_feeds("h-pi").unwrap(), ["claude-max", "pi-status"]);
        let invite = crate::secret::hash("invite-before-upgrade");
        assert_eq!(
            store.use_code(&invite, "helper-pair", ts(NOW)).unwrap().0,
            CodeUse::Accepted
        );
        let settings = json!({"title": "Pi status"}).as_object().unwrap().clone();
        assert_eq!(
            store
                .update_page("pi-status", None, None, None, Some(&settings))
                .unwrap(),
            PageUpdate::Updated
        );
        assert_eq!(store.page("pi-status").unwrap().unwrap().settings, Some(settings));
    }

    #[test]
    fn a_page_keeps_its_template_and_settings_and_its_helper_can_find_them() {
        let store = Store::in_memory().unwrap();
        let now = ts(NOW);
        let settings = json!({"service": "claude-max", "shows": ["weekly"]})
            .as_object()
            .unwrap()
            .clone();
        let page = StoredPage {
            id: "claude-max".into(),
            title: "Claude Max".into(),
            kind: "meter".into(),
            source: Source::Helper,
            feed: Some("claude-max".into()),
            stale_after_seconds: 1800,
            content: None,
            template: Some("usage-limits/1".into()),
            settings: Some(settings.clone()),
        };
        store.add_page(&page, now).unwrap();
        store
            .add_page(
                &StoredPage {
                    id: "typed".into(),
                    source: Source::Manual,
                    ..page.clone()
                },
                now,
            )
            .unwrap();
        assert_eq!(store.page("claude-max").unwrap(), Some(page.clone()));
        // Only the feed's helper pages are found.
        let found = store.feed_pages("claude-max").unwrap();
        assert_eq!(
            found.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(),
            ["claude-max"]
        );
        assert!(store.feed_pages("other").unwrap().is_empty());

        // New settings replace the old; empty ones remove them; none leaves them be.
        let other = json!({"service": "chatgpt-plus"}).as_object().unwrap().clone();
        store
            .update_page("claude-max", Some("Renamed"), None, None, Some(&other))
            .unwrap();
        let changed = store.page("claude-max").unwrap().unwrap();
        assert_eq!(
            (changed.title.as_str(), changed.settings),
            ("Renamed", Some(other.clone()))
        );
        store.update_page("claude-max", None, None, None, None).unwrap();
        assert_eq!(store.page("claude-max").unwrap().unwrap().settings, Some(other));
        store
            .update_page("claude-max", None, None, None, Some(&Settings::new()))
            .unwrap();
        let cleared = store.page("claude-max").unwrap().unwrap();
        assert_eq!(cleared.settings, None);
        assert_eq!(cleared.template.as_deref(), Some("usage-limits/1"));

        // Damaged settings are left out rather than breaking the page.
        store
            .conn()
            .execute("UPDATE pages SET settings = '[1]' WHERE id = 'typed'", [])
            .unwrap();
        assert_eq!(store.page("typed").unwrap().unwrap().settings, None);
    }

    /// Representative data in the layout schema 5 used.
    const SCHEMA_5_DATA: &str = include_str!("../tests/fixtures/schema-5-data.sql");

    #[test]
    fn upgrading_schema_5_keeps_every_row_and_makes_room_for_firmware_and_publishers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hub.db");
        {
            let conn = Connection::open(&path).unwrap();
            create_at_schema(&conn, 5).unwrap();
            conn.execute_batch(SCHEMA_5_DATA).unwrap();
        }
        let (before, pages_before) = {
            let conn = Connection::open(&path).unwrap();
            (untouched_tables(&conn), page_rows(&conn))
        };
        let store = Store::open(&path).unwrap();
        assert_eq!(store.schema_version().unwrap(), 6);
        let saved = copies(dir.path());
        assert_eq!(saved.len(), 1);
        let name = saved[0].file_name().unwrap().to_str().unwrap();
        assert!(name.starts_with("hub-before-upgrade-schema-5-"), "{name}");
        assert_eq!(Store::inspect(&saved[0]).unwrap(), (5, 4, 2));

        // Every row is kept as it was, settings included.
        assert_eq!(untouched_tables(&store.conn()), before);
        assert_eq!(page_rows(&store.conn()), pages_before);
        let pi = store.page("pi-status").unwrap().unwrap();
        assert_eq!(pi.template.as_deref(), Some("pi-status/1"));
        assert_eq!(pi.settings.unwrap()["sensor"], json!("cpu"));

        // Displays keep their report and start with automatic updates off.
        let desk = store.device("d-desk").unwrap().unwrap();
        assert!(!desk.auto_update);
        assert_eq!(desk.info.unwrap().firmware_version, "1.0.0");
        assert!(store.device("d-old").unwrap().is_none());

        // Tokens and codes keep working, and the new role and purpose are allowed.
        let token = crate::secret::hash("dhc_test_helper");
        assert_eq!(store.credential(&token).unwrap().unwrap().subject, "h-pi");
        let admin = crate::secret::hash("dha_test_admin");
        assert_eq!(store.credential(&admin).unwrap().unwrap().role, Role::Admin);
        let pair = crate::secret::hash("pair-before-upgrade");
        assert_eq!(
            store.use_code(&pair, "admin-pair", ts(NOW)).unwrap().0,
            CodeUse::Accepted
        );
        let p = crate::secret::hash("dhp_new");
        store
            .add_credential(&p, Role::Publisher, "p-1", "Maker", ts(NOW))
            .unwrap();
        assert_eq!(store.credential(&p).unwrap().unwrap().role, Role::Publisher);
        store
            .add_code(
                &crate::secret::hash("c"),
                "publisher-pair",
                "Maker",
                ts(NOW),
                ts(NOW + 600),
            )
            .unwrap();
        assert!(store
            .add_code(&crate::secret::hash("d"), "nonsense", "x", ts(NOW), ts(NOW + 600))
            .is_err());
        assert!(store.firmware_packages().unwrap().is_empty());
        assert!(store.latest_offers(ts(NOW)).unwrap().is_empty());
    }

    fn package(id: &str, device_type: &str, version: &str) -> firmware::PackageRow {
        firmware::PackageRow {
            id: id.into(),
            device_type: device_type.into(),
            hardware_revisions: vec!["simulated".into()],
            firmware_version: version.into(),
            security_counter: 1,
            summary: None,
            signer_key_id: "k".repeat(64),
            signer_public_key: format!("04{}", "0".repeat(128)),
            image_size: 100,
            size: 300,
            update_size: 280,
            usb_install: false,
            uploaded_at: ts(NOW),
            uploaded_by_role: Role::Admin,
            uploaded_by: "a-1".into(),
            uploaded_by_label: None,
        }
    }

    fn offer<'a>(id: &'a str, device: &'a str, pkg: &'a str, now: i64) -> firmware::NewOffer<'a> {
        firmware::NewOffer {
            id,
            device_id: device,
            package_id: pkg,
            from_version: Some("0.0.0-test"),
            made_by: "a-1",
            now: ts(now),
            expires_at: ts(now + hub_protocol::limits::FIRMWARE_OFFER_LIFETIME_SECONDS),
        }
    }

    #[test]
    fn a_display_has_one_open_offer_which_ends_when_replaced_cancelled_expired_or_removed() {
        use firmware::*;
        let store = Store::in_memory().unwrap();
        device(&store, "d-a");
        device(&store, "d-b");
        store
            .add_firmware_package(&package("p1", "test-display", "1.0.0"), &[], ts(NOW))
            .unwrap();
        store
            .add_firmware_package(&package("p2", "test-display", "1.1.0"), &[], ts(NOW))
            .unwrap();

        let first = store.make_offer(&offer("o1", "d-a", "p1", NOW)).unwrap();
        assert_eq!(
            (first.state.as_str(), first.firmware_version.as_str()),
            (OFFERED, "1.0.0")
        );
        assert_eq!(store.open_offer("d-a", ts(NOW)).unwrap().unwrap().id, "o1");
        // Pressing Update again replaces it.
        store.make_offer(&offer("o2", "d-a", "p2", NOW + 1)).unwrap();
        assert_eq!(store.open_offer("d-a", ts(NOW + 1)).unwrap().unwrap().id, "o2");
        let latest = store.latest_offers(ts(NOW + 1)).unwrap();
        assert_eq!(latest["d-a"].id, "o2");
        let replaced = store
            .record_report(
                "d-a",
                "o1",
                hub_protocol::firmware::ReportState::Downloading,
                None,
                None,
                ts(NOW + 2),
            )
            .unwrap();
        assert_eq!(
            replaced,
            ReportOutcome::Unchanged,
            "progress on an ended offer is ignored"
        );
        assert_eq!(
            store.packages_on_offer(ts(NOW + 2)).unwrap(),
            std::collections::HashSet::from(["p2".to_string()])
        );

        // Cancelled.
        assert!(store.cancel_offer("d-a", ts(NOW + 3)).unwrap());
        assert!(!store.cancel_offer("d-a", ts(NOW + 3)).unwrap());
        assert!(store.open_offer("d-a", ts(NOW + 3)).unwrap().is_none());
        assert_eq!(store.latest_offers(ts(NOW + 3)).unwrap()["d-a"].state, CANCELLED);

        // Expired: gone from reads at once, and recorded by the next write.
        store.make_offer(&offer("o3", "d-a", "p1", NOW + 4)).unwrap();
        let later = NOW + 4 + hub_protocol::limits::FIRMWARE_OFFER_LIFETIME_SECONDS;
        assert!(store.open_offer("d-a", ts(later)).unwrap().is_none());
        assert_eq!(
            store.latest_offers(ts(later)).unwrap()["d-a"].state_at(ts(later)),
            EXPIRED
        );
        store.make_offer(&offer("o4", "d-a", "p1", later)).unwrap();
        assert_eq!(store.open_offer("d-a", ts(later)).unwrap().unwrap().id, "o4");

        // Deleting the package cancels its offers; removing the display cancels its own.
        store.make_offer(&offer("o5", "d-b", "p2", later)).unwrap();
        assert!(store
            .remove_firmware_package("p1", CANCELLED_PACKAGE_DELETED, ts(later))
            .unwrap());
        assert!(store.open_offer("d-a", ts(later)).unwrap().is_none());
        assert_eq!(
            store.latest_offers(ts(later)).unwrap()["d-a"].reason.as_deref(),
            Some(CANCELLED_PACKAGE_DELETED)
        );
        assert!(store.revoke_device("d-b", ts(later)).unwrap());
        assert!(store.open_offer("d-b", ts(later)).unwrap().is_none());
        assert!(store.make_offer(&offer("o6", "d-b", "p2", later)).is_err());
        // The index itself refuses a second open offer.
        let dup = store.conn().execute(
            "INSERT INTO firmware_offers (id, device_id, package_id, firmware_version, made_at, made_by, \
             expires_at, state, updated_at) VALUES ('x', 'd-a', 'p2', '1', 0, 'a', 9999999999, 'offered', 0), \
             ('y', 'd-a', 'p2', '1', 0, 'a', 9999999999, 'offered', 0)",
            [],
        );
        assert!(dup.is_err());
    }

    #[test]
    fn reports_settle_offers_once_and_a_heartbeat_can_stand_in_for_a_lost_one() {
        use firmware::*;
        use hub_protocol::firmware::{ReportReason, ReportState};
        let store = Store::in_memory().unwrap();
        device(&store, "d-a");
        device(&store, "d-b");
        store
            .add_firmware_package(&package("p1", "test-display", "1.0.0"), &[], ts(NOW))
            .unwrap();
        store.make_offer(&offer("o1", "d-a", "p1", NOW)).unwrap();

        assert_eq!(
            store
                .record_report("d-b", "o1", ReportState::Installed, None, None, ts(NOW))
                .unwrap(),
            ReportOutcome::NoOffer,
            "another display's offer"
        );
        let ReportOutcome::Recorded {
            offer: o,
            settled: false,
        } = store
            .record_report(
                "d-a",
                "o1",
                ReportState::Downloading,
                None,
                Some("0.0.0-test"),
                ts(NOW + 1),
            )
            .unwrap()
        else {
            panic!("progress recorded")
        };
        assert!(o.is_open(ts(NOW + 1)));
        let ReportOutcome::Recorded {
            offer: o,
            settled: true,
        } = store
            .record_report(
                "d-a",
                "o1",
                ReportState::Refused,
                Some(ReportReason::Downgrade),
                Some("0.0.0-test"),
                ts(NOW + 2),
            )
            .unwrap()
        else {
            panic!("result recorded")
        };
        assert_eq!(
            (o.state.as_str(), o.reason.as_deref()),
            ("refused", Some("downgrade"))
        );
        assert!(!o.is_open(ts(NOW + 2)));
        // Repeated, or contradicted, it changes nothing.
        assert_eq!(
            store
                .record_report("d-a", "o1", ReportState::Installed, None, None, ts(NOW + 3))
                .unwrap(),
            ReportOutcome::Unchanged
        );

        // Unknown states and reasons are failures.
        store.make_offer(&offer("o2", "d-a", "p1", NOW + 4)).unwrap();
        let ReportOutcome::Recorded { offer: o, .. } = store
            .record_report(
                "d-a",
                "o2",
                ReportState::Refused,
                Some(ReportReason::Unrecognised),
                None,
                ts(NOW + 5),
            )
            .unwrap()
        else {
            panic!()
        };
        assert_eq!(
            (o.state.as_str(), o.reason.as_deref()),
            ("failed", Some("unrecognised"))
        );
        store.make_offer(&offer("o3", "d-a", "p1", NOW + 6)).unwrap();
        let ReportOutcome::Recorded { offer: o, .. } = store
            .record_report("d-a", "o3", ReportState::Unrecognised, None, None, ts(NOW + 7))
            .unwrap()
        else {
            panic!()
        };
        assert_eq!(o.state, "failed");

        // A result arriving after the owner cancelled is still recorded: the display acted first.
        store.make_offer(&offer("o4", "d-a", "p1", NOW + 8)).unwrap();
        store.cancel_offer("d-a", ts(NOW + 9)).unwrap();
        let ReportOutcome::Recorded {
            offer: o,
            settled: true,
        } = store
            .record_report(
                "d-a",
                "o4",
                ReportState::Installed,
                None,
                Some("1.0.0"),
                ts(NOW + 10),
            )
            .unwrap()
        else {
            panic!()
        };
        assert_eq!((o.state.as_str(), o.ended_at), ("installed", Some(ts(NOW + 9))));

        // A heartbeat naming the offered version counts as installed, unless the display was
        // already running it when the offer was made.
        store.make_offer(&offer("o5", "d-a", "p1", NOW + 11)).unwrap();
        assert!(store
            .installed_by_heartbeat("d-a", "0.0.0-test", ts(NOW + 12))
            .unwrap()
            .is_none());
        assert!(store
            .open_offer_for_version("d-a", "1.0.0", ts(NOW + 12))
            .unwrap());
        let o = store
            .installed_by_heartbeat("d-a", "1.0.0", ts(NOW + 12))
            .unwrap()
            .unwrap();
        assert_eq!(
            (o.id.as_str(), o.reason.as_deref()),
            ("o5", Some(INSTALLED_BY_HEARTBEAT))
        );
        assert!(store.open_offer("d-a", ts(NOW + 12)).unwrap().is_none());
        let same = firmware::NewOffer {
            from_version: Some("1.0.0"),
            ..offer("o6", "d-a", "p1", NOW + 13)
        };
        store.make_offer(&same).unwrap();
        assert!(store
            .installed_by_heartbeat("d-a", "1.0.0", ts(NOW + 14))
            .unwrap()
            .is_none());
        assert!(store.open_offer("d-a", ts(NOW + 14)).unwrap().is_some());

        // Only the newest ended offers are kept.
        for i in 0..(MAX_OFFER_HISTORY_PER_DISPLAY + 5) {
            store
                .make_offer(&offer(&format!("h{i}"), "d-a", "p1", NOW + 20 + i as i64))
                .unwrap();
        }
        let n: i64 = store
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM firmware_offers WHERE device_id = 'd-a'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n as usize, MAX_OFFER_HISTORY_PER_DISPLAY + 1);
    }

    #[test]
    fn eviction_refuses_a_package_that_came_on_offer_and_rows_without_files_are_dropped() {
        use firmware::*;
        let store = Store::in_memory().unwrap();
        device(&store, "d-a");
        store
            .add_firmware_package(&package("p1", "test-display", "1.0.0"), &[], ts(NOW))
            .unwrap();
        store
            .add_firmware_package(&package("p2", "test-display", "1.1.0"), &[], ts(NOW))
            .unwrap();
        store.make_offer(&offer("o1", "d-a", "p1", NOW)).unwrap();
        assert!(store
            .add_firmware_package(&package("p3", "test-display", "1.2.0"), &["p1".into()], ts(NOW))
            .is_err());
        assert!(store.firmware_package("p3").unwrap().is_none(), "nothing changed");
        store
            .add_firmware_package(&package("p3", "test-display", "1.2.0"), &["p2".into()], ts(NOW))
            .unwrap();
        let ids: Vec<String> = store
            .firmware_packages()
            .unwrap()
            .into_iter()
            .map(|p| p.id)
            .collect();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&"p1".to_string()) && ids.contains(&"p3".to_string()));

        let dropped = store.drop_missing_packages(|id| id != "p1", ts(NOW + 1)).unwrap();
        assert_eq!(dropped, ["p1"]);
        assert!(store.open_offer("d-a", ts(NOW + 1)).unwrap().is_none());
        assert_eq!(
            store.latest_offers(ts(NOW + 1)).unwrap()["d-a"].reason.as_deref(),
            Some(CANCELLED_PACKAGE_MISSING)
        );

        // Signer labels.
        store.set_signer_label(&"a".repeat(64), "Maker", ts(NOW)).unwrap();
        store
            .set_signer_label(&"a".repeat(64), "Maker 2", ts(NOW))
            .unwrap();
        assert_eq!(store.firmware_signers().unwrap()[0].1, "Maker 2");
        for i in 1..MAX_SIGNER_LABELS {
            store.set_signer_label(&format!("{i:064}"), "x", ts(NOW)).unwrap();
        }
        assert!(store
            .set_signer_label(&"b".repeat(64), "one too many", ts(NOW))
            .is_err());
        assert!(store.remove_signer_label(&"a".repeat(64)).unwrap());
        assert!(!store.remove_signer_label(&"a".repeat(64)).unwrap());
    }
}
