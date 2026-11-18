//! What the hub keeps about signed firmware updates (`docs/firmware-updates.md`, "The hub's part"):
//! packages, the owner's labels for signing keys, and offers. Package files themselves live in the
//! data folder (see `crate::firmware`); a row here describes each one.
//!
//! An offer is open while `ended_at` is null and it hasn't expired. Expiry needs no write: readers
//! treat an open offer past `expires_at` as expired, and every write that touches offers first
//! records the expiry of any that have passed, so the one-open-offer index never blocks a new one.

use std::collections::{HashMap, HashSet};

use hub_protocol::firmware::{ReportReason, ReportState};
use hub_protocol::Timestamp;
use rusqlite::{params, Connection, OptionalExtension, Row, TransactionBehavior};

use super::{ts, Store, StoreError};
use crate::secret::Role;

/// Ended offers kept for each display, newest first, so the app can show what happened last.
pub const MAX_OFFER_HISTORY_PER_DISPLAY: usize = 10;
/// Signing keys the owner may label.
pub const MAX_SIGNER_LABELS: usize = 64;

/// An offer's state as the hub records it: the display's report states, plus `offered` (made, no
/// report yet), `cancelled` and `expired`.
pub const OFFERED: &str = "offered";
pub const CANCELLED: &str = "cancelled";
pub const EXPIRED: &str = "expired";

/// Why an offer was cancelled, in its `reason` column.
pub const CANCELLED_BY_ADMIN: &str = "by_admin";
pub const CANCELLED_REPLACED: &str = "replaced";
pub const CANCELLED_PACKAGE_DELETED: &str = "package_deleted";
pub const CANCELLED_DISPLAY_REMOVED: &str = "display_removed";
pub const CANCELLED_PACKAGE_MISSING: &str = "package_missing";
/// The reason recorded when a heartbeat, not a report, showed the new version running.
pub const INSTALLED_BY_HEARTBEAT: &str = "seen_in_heartbeat";

/// One stored package.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackageRow {
    pub id: String,
    pub device_type: String,
    pub hardware_revisions: Vec<String>,
    pub firmware_version: String,
    pub security_counter: u32,
    pub summary: Option<String>,
    pub signer_key_id: String,
    pub signer_public_key: String,
    pub image_size: u64,
    /// The whole file.
    pub size: u64,
    /// What a display receives: header, manifest and image.
    pub update_size: u64,
    pub usb_install: bool,
    /// The manifest's `key_set`, as the row holds it.
    pub key_set: KeySet,
    pub uploaded_at: Timestamp,
    pub uploaded_by_role: Role,
    pub uploaded_by: String,
    /// The uploader's label, from its credential.
    pub uploaded_by_label: Option<String>,
}

/// A package's `key_set`: the keys a display trusts once it has installed the package.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeySet {
    /// The manifest has none: a display keeps the keys it trusts.
    Absent,
    /// The key ids, 64 hex digits each.
    Keys(Vec<String>),
    /// The row's value can't be read. Never taken as `Absent`, which would let the courtesy check
    /// pass a package that drops a key: such a package is listed but never offered.
    Unreadable,
}

impl KeySet {
    /// The manifest's key ids, if it has a `key_set`.
    pub fn from_keys(keys: Option<Vec<String>>) -> Self {
        keys.map_or(Self::Absent, Self::Keys)
    }
}

/// One offer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OfferRow {
    pub id: String,
    pub device_id: String,
    pub package_id: String,
    pub firmware_version: String,
    /// What the display was running when the offer was made, by its last report.
    pub from_version: Option<String>,
    pub made_at: Timestamp,
    pub made_by: String,
    pub expires_at: Timestamp,
    pub state: String,
    pub reason: Option<String>,
    /// What the display said it was running in its last report about this offer.
    pub reported_version: Option<String>,
    pub updated_at: Timestamp,
    pub ended_at: Option<Timestamp>,
}

impl OfferRow {
    /// Whether the offer is still open at `now`.
    pub fn is_open(&self, now: Timestamp) -> bool {
        self.ended_at.is_none() && self.expires_at > now
    }

    /// The state to show at `now`: an open offer past its time reads as expired.
    pub fn state_at(&self, now: Timestamp) -> &str {
        if self.ended_at.is_none() && self.expires_at <= now {
            EXPIRED
        } else {
            &self.state
        }
    }
}

/// What [`Store::record_report`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReportOutcome {
    /// No offer with that id for this display.
    NoOffer,
    /// Recorded. `settled` when it was a final result, newly recorded.
    Recorded { offer: Box<OfferRow>, settled: bool },
    /// Already settled, or a progress report on an offer that has ended: nothing changed.
    Unchanged,
}

/// What a new offer is for.
pub struct NewOffer<'a> {
    pub id: &'a str,
    pub device_id: &'a str,
    pub package_id: &'a str,
    pub from_version: Option<&'a str>,
    pub made_by: &'a str,
    pub now: Timestamp,
    pub expires_at: Timestamp,
}

const PACKAGE_COLUMNS: &str = "p.id, p.device_type, p.hardware_revisions, p.firmware_version, \
     p.security_counter, p.summary, p.signer_key_id, p.signer_public_key, p.image_size, p.size, \
     p.update_size, p.usb_install, p.uploaded_at, p.uploaded_by_role, p.uploaded_by, p.key_set, \
     (SELECT c.label FROM credentials c WHERE c.subject = p.uploaded_by AND c.role = p.uploaded_by_role \
      ORDER BY c.created_at DESC LIMIT 1)";

const OFFER_COLUMNS: &str = "id, device_id, package_id, firmware_version, from_version, made_at, \
     made_by, expires_at, state, reason, reported_version, updated_at, ended_at";

fn package_row(r: &Row<'_>) -> rusqlite::Result<PackageRow> {
    let revisions: String = r.get(2)?;
    let role: String = r.get(13)?;
    Ok(PackageRow {
        id: r.get(0)?,
        device_type: r.get(1)?,
        hardware_revisions: serde_json::from_str(&revisions).unwrap_or_default(),
        firmware_version: r.get(3)?,
        security_counter: u32::try_from(r.get::<_, i64>(4)?).unwrap_or(u32::MAX),
        summary: r.get(5)?,
        signer_key_id: r.get(6)?,
        signer_public_key: r.get(7)?,
        image_size: r.get::<_, i64>(8)? as u64,
        size: r.get::<_, i64>(9)? as u64,
        update_size: r.get::<_, i64>(10)? as u64,
        usb_install: r.get(11)?,
        uploaded_at: ts(r.get(12)?),
        uploaded_by_role: Role::parse(&role).unwrap_or(Role::Admin),
        uploaded_by: r.get(14)?,
        key_set: match r.get::<_, Option<String>>(15)? {
            None => KeySet::Absent,
            Some(ks) => serde_json::from_str(&ks).map_or(KeySet::Unreadable, KeySet::Keys),
        },
        uploaded_by_label: r.get(16)?,
    })
}

/// Says once in the log, for a query's rows, which packages' `key_set` can't be read.
fn log_unreadable<'a>(rows: impl IntoIterator<Item = &'a PackageRow>) {
    let ids: Vec<&str> = rows
        .into_iter()
        .filter(|p| p.key_set == KeySet::Unreadable)
        .map(|p| p.id.as_str())
        .collect();
    if !ids.is_empty() {
        tracing::error!(
            packages = %ids.join(", "),
            "a stored package's key_set can't be read, so it won't be offered; remove the package and add it again"
        );
    }
}

fn offer_row(r: &Row<'_>) -> rusqlite::Result<OfferRow> {
    Ok(OfferRow {
        id: r.get(0)?,
        device_id: r.get(1)?,
        package_id: r.get(2)?,
        firmware_version: r.get(3)?,
        from_version: r.get(4)?,
        made_at: ts(r.get(5)?),
        made_by: r.get(6)?,
        expires_at: ts(r.get(7)?),
        state: r.get(8)?,
        reason: r.get(9)?,
        reported_version: r.get(10)?,
        updated_at: ts(r.get(11)?),
        ended_at: r.get::<_, Option<i64>>(12)?.map(ts),
    })
}

/// Records that every open offer past its time has expired.
fn expire(conn: &Connection, now: Timestamp) -> rusqlite::Result<usize> {
    conn.execute(
        "UPDATE firmware_offers SET state = ?2, ended_at = expires_at, updated_at = expires_at \
         WHERE ended_at IS NULL AND expires_at <= ?1",
        params![now.unix(), EXPIRED],
    )
}

/// Cancels the open offers that `filter` (an SQL condition on `?3`) picks.
fn cancel_where(
    conn: &Connection,
    filter: &str,
    arg: &str,
    reason: &str,
    now: Timestamp,
) -> rusqlite::Result<usize> {
    conn.execute(
        &format!(
            "UPDATE firmware_offers SET state = '{CANCELLED}', reason = ?2, ended_at = ?1, updated_at = ?1 \
             WHERE ended_at IS NULL AND {filter}"
        ),
        params![now.unix(), reason, arg],
    )
}

/// Keeps the newest ended offers of a display, up to [`MAX_OFFER_HISTORY_PER_DISPLAY`].
fn prune_history(conn: &Connection, device: &str) -> rusqlite::Result<usize> {
    conn.execute(
        "DELETE FROM firmware_offers WHERE device_id = ?1 AND ended_at IS NOT NULL AND id NOT IN \
         (SELECT id FROM firmware_offers WHERE device_id = ?1 AND ended_at IS NOT NULL \
          ORDER BY ended_at DESC, made_at DESC, rowid DESC LIMIT ?2)",
        params![device, MAX_OFFER_HISTORY_PER_DISPLAY as i64],
    )
}

impl Store {
    // ----- packages -----

    /// Every stored package, by device type and then newest first.
    pub fn firmware_packages(&self) -> Result<Vec<PackageRow>, StoreError> {
        let conn = self.conn();
        let mut st = conn.prepare(&format!(
            "SELECT {PACKAGE_COLUMNS} FROM firmware_packages p ORDER BY p.device_type, p.uploaded_at DESC, p.id"
        ))?;
        let rows = st.query_map([], package_row)?.collect::<Result<Vec<_>, _>>()?;
        log_unreadable(&rows);
        Ok(rows)
    }

    pub fn firmware_package(&self, id: &str) -> Result<Option<PackageRow>, StoreError> {
        let conn = self.conn();
        let row = conn
            .query_row(
                &format!("SELECT {PACKAGE_COLUMNS} FROM firmware_packages p WHERE p.id = ?1"),
                params![id],
                package_row,
            )
            .optional()?;
        log_unreadable(&row);
        Ok(row)
    }

    /// The ids of packages on offer to some display at `now`.
    pub fn packages_on_offer(&self, now: Timestamp) -> Result<HashSet<String>, StoreError> {
        let conn = self.conn();
        let mut st = conn.prepare(
            "SELECT DISTINCT package_id FROM firmware_offers WHERE ended_at IS NULL AND expires_at > ?1",
        )?;
        let rows = st.query_map(params![now.unix()], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Each package's open offers at `now`: package id to display ids.
    pub fn offered_to(&self, now: Timestamp) -> Result<HashMap<String, Vec<String>>, StoreError> {
        let conn = self.conn();
        let mut st = conn.prepare(
            "SELECT package_id, device_id FROM firmware_offers WHERE ended_at IS NULL AND expires_at > ?1 \
             ORDER BY made_at",
        )?;
        let rows = st.query_map(params![now.unix()], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        let mut out: HashMap<String, Vec<String>> = HashMap::new();
        for row in rows {
            let (p, d) = row?;
            out.entry(p).or_default().push(d);
        }
        Ok(out)
    }

    /// Adds a package's row and removes the rows in `evict`, in one transaction. An evicted package
    /// that has come on offer since it was chosen makes the whole thing fail, changing nothing.
    pub fn add_firmware_package(
        &self,
        row: &PackageRow,
        evict: &[String],
        now: Timestamp,
    ) -> Result<(), StoreError> {
        let key_set = match &row.key_set {
            KeySet::Absent => None,
            KeySet::Keys(ks) => Some(serde_json::to_string(ks).expect("serialises")),
            KeySet::Unreadable => {
                return Err(StoreError::Invalid(
                    "A package's key_set must be known to store it.".into(),
                ))
            }
        };
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        expire(&tx, now)?;
        for id in evict {
            let on_offer: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM firmware_offers WHERE package_id = ?1 AND ended_at IS NULL)",
                params![id],
                |r| r.get(0),
            )?;
            if on_offer {
                return Err(StoreError::Invalid(
                    "A package chosen to make room came on offer meanwhile. Try the upload again.".into(),
                ));
            }
            tx.execute("DELETE FROM firmware_packages WHERE id = ?1", params![id])?;
        }
        tx.execute(
            "INSERT INTO firmware_packages (id, device_type, hardware_revisions, firmware_version, \
             security_counter, summary, signer_key_id, signer_public_key, image_size, size, update_size, \
             usb_install, uploaded_at, uploaded_by_role, uploaded_by, key_set) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
            params![
                row.id,
                row.device_type,
                serde_json::to_string(&row.hardware_revisions).expect("serialises"),
                row.firmware_version,
                i64::from(row.security_counter),
                row.summary,
                row.signer_key_id,
                row.signer_public_key,
                row.image_size as i64,
                row.size as i64,
                row.update_size as i64,
                row.usb_install,
                row.uploaded_at.unix(),
                row.uploaded_by_role.as_str(),
                row.uploaded_by,
                key_set,
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Removes a package's row and cancels its open offers. False if there was no such package.
    pub fn remove_firmware_package(
        &self,
        id: &str,
        reason: &str,
        now: Timestamp,
    ) -> Result<bool, StoreError> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        expire(&tx, now)?;
        cancel_where(&tx, "package_id = ?3", id, reason, now)?;
        let n = tx.execute("DELETE FROM firmware_packages WHERE id = ?1", params![id])?;
        tx.commit()?;
        Ok(n > 0)
    }

    // ----- offers -----

    /// The open offer of a display at `now`, if it has one. Read only, since every poll asks.
    pub fn open_offer(&self, device: &str, now: Timestamp) -> Result<Option<OfferRow>, StoreError> {
        let conn = self.conn();
        Ok(conn
            .query_row(
                &format!(
                    "SELECT {OFFER_COLUMNS} FROM firmware_offers \
                     WHERE device_id = ?1 AND ended_at IS NULL AND expires_at > ?2"
                ),
                params![device, now.unix()],
                offer_row,
            )
            .optional()?)
    }

    /// Each display's open offer, or else its most recent one, as of `now`.
    pub fn latest_offers(&self, now: Timestamp) -> Result<HashMap<String, OfferRow>, StoreError> {
        let conn = self.conn();
        let mut st = conn.prepare(&format!(
            "SELECT {OFFER_COLUMNS} FROM firmware_offers ORDER BY made_at, rowid"
        ))?;
        let rows = st.query_map([], offer_row)?;
        let mut out: HashMap<String, OfferRow> = HashMap::new();
        for row in rows {
            let row = row?;
            match out.get(&row.device_id) {
                // An open offer always wins; otherwise the later one does (rows come oldest first).
                Some(kept) if kept.is_open(now) && !row.is_open(now) => {}
                _ => {
                    out.insert(row.device_id.clone(), row);
                }
            }
        }
        Ok(out)
    }

    /// Makes an offer, ending any open one the display had (it is replaced, as when the owner presses
    /// Update again). The display must be active and the package stored.
    pub fn make_offer(&self, offer: &NewOffer<'_>) -> Result<OfferRow, StoreError> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        expire(&tx, offer.now)?;
        let active: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM devices WHERE id = ?1 AND revoked_at IS NULL)",
            params![offer.device_id],
            |r| r.get(0),
        )?;
        let version: Option<String> = tx
            .query_row(
                "SELECT firmware_version FROM firmware_packages WHERE id = ?1",
                params![offer.package_id],
                |r| r.get(0),
            )
            .optional()?;
        let (true, Some(version)) = (active, version) else {
            return Err(StoreError::Invalid(
                "That display or package is no longer on the hub.".into(),
            ));
        };
        cancel_where(
            &tx,
            "device_id = ?3",
            offer.device_id,
            CANCELLED_REPLACED,
            offer.now,
        )?;
        tx.execute(
            "INSERT INTO firmware_offers (id, device_id, package_id, firmware_version, from_version, \
             made_at, made_by, expires_at, state, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?6)",
            params![
                offer.id,
                offer.device_id,
                offer.package_id,
                version,
                offer.from_version,
                offer.now.unix(),
                offer.made_by,
                offer.expires_at.unix(),
                OFFERED,
            ],
        )?;
        prune_history(&tx, offer.device_id)?;
        let row = tx.query_row(
            &format!("SELECT {OFFER_COLUMNS} FROM firmware_offers WHERE id = ?1"),
            params![offer.id],
            offer_row,
        )?;
        tx.commit()?;
        Ok(row)
    }

    /// Cancels a display's open offer. False if it had none.
    pub fn cancel_offer(&self, device: &str, now: Timestamp) -> Result<bool, StoreError> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        expire(&tx, now)?;
        let n = cancel_where(&tx, "device_id = ?3", device, CANCELLED_BY_ADMIN, now)?;
        tx.commit()?;
        Ok(n > 0)
    }

    /// Records a display's report about one of its offers. A final result settles an open offer;
    /// it is also recorded on one that was cancelled or expired meanwhile, since the display may
    /// have acted on it first. A result for an offer already settled, or progress on one that has
    /// ended, changes nothing, with one exception: an offer counted as installed only because a
    /// heartbeat named its version is overruled by a later `rolled_back` or `failed` report (the
    /// heartbeat may have come from the trial boot, before it rolled back), and confirmed by a later
    /// `installed` one. A reported `installed` is never overruled.
    pub fn record_report(
        &self,
        device: &str,
        offer_id: &str,
        state: ReportState,
        reason: Option<ReportReason>,
        version: Option<&str>,
        now: Timestamp,
    ) -> Result<ReportOutcome, StoreError> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        expire(&tx, now)?;
        let Some(row) = tx
            .query_row(
                &format!("SELECT {OFFER_COLUMNS} FROM firmware_offers WHERE id = ?1 AND device_id = ?2"),
                params![offer_id, device],
                offer_row,
            )
            .optional()?
        else {
            return Ok(ReportOutcome::NoOffer);
        };
        let settled = !matches!(row.state.as_str(), OFFERED | CANCELLED | EXPIRED | "downloading");
        let by_heartbeat = row.state == "installed" && row.reason.as_deref() == Some(INSTALLED_BY_HEARTBEAT);
        // A state or reason this build doesn't know is recorded as a failure.
        let (state, reason) = match (state, reason) {
            (ReportState::Unrecognised, _) => ("failed", Some(ReportReason::Unrecognised.as_str())),
            (ReportState::Refused, Some(ReportReason::Unrecognised)) => {
                ("failed", Some(ReportReason::Unrecognised.as_str()))
            }
            (s @ (ReportState::Refused | ReportState::Failed), r) => {
                (s.as_str(), Some(r.unwrap_or(ReportReason::Unrecognised).as_str()))
            }
            (s, _) => (s.as_str(), None),
        };
        let overrules = by_heartbeat && matches!(state, "rolled_back" | "failed" | "installed");
        let outcome = if (settled && !overrules) || (state == "downloading" && row.ended_at.is_some()) {
            ReportOutcome::Unchanged
        } else if state == "downloading" {
            tx.execute(
                "UPDATE firmware_offers SET state = ?2, reported_version = COALESCE(?3, reported_version), \
                 updated_at = ?4 WHERE id = ?1",
                params![offer_id, state, version, now.unix()],
            )?;
            ReportOutcome::Recorded {
                offer: Box::new(read_offer(&tx, offer_id)?),
                settled: false,
            }
        } else {
            tx.execute(
                "UPDATE firmware_offers SET state = ?2, reason = ?3, reported_version = COALESCE(?4, reported_version), \
                 updated_at = ?5, ended_at = COALESCE(ended_at, ?5) WHERE id = ?1",
                params![offer_id, state, reason, version, now.unix()],
            )?;
            ReportOutcome::Recorded {
                offer: Box::new(read_offer(&tx, offer_id)?),
                settled: true,
            }
        };
        tx.commit()?;
        Ok(outcome)
    }

    /// A heartbeat naming `version`: if the display's open offer is for that version, and the
    /// display wasn't already running it when the offer was made, the update evidently installed and
    /// its report was lost. Settles the offer as installed and returns it.
    pub fn installed_by_heartbeat(
        &self,
        device: &str,
        version: &str,
        now: Timestamp,
    ) -> Result<Option<OfferRow>, StoreError> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let n = tx.execute(
            "UPDATE firmware_offers SET state = 'installed', reason = ?4, reported_version = ?2, \
             updated_at = ?3, ended_at = ?3 \
             WHERE device_id = ?1 AND ended_at IS NULL AND expires_at > ?3 AND firmware_version = ?2 \
             AND (from_version IS NULL OR from_version <> ?2)",
            params![device, version, now.unix(), INSTALLED_BY_HEARTBEAT],
        )?;
        let row = if n > 0 {
            tx.query_row(
                &format!(
                    "SELECT {OFFER_COLUMNS} FROM firmware_offers WHERE device_id = ?1 AND ended_at = ?2 \
                     AND state = 'installed' ORDER BY made_at DESC, rowid DESC LIMIT 1"
                ),
                params![device, now.unix()],
                offer_row,
            )
            .optional()?
        } else {
            None
        };
        tx.commit()?;
        Ok(row)
    }

    /// Records the version a display reported running in a final report, in its stored details, so
    /// the app shows it before the next heartbeat. False if the display has no stored
    /// details (it never enrolled with them) or is gone.
    pub fn set_reported_firmware_version(&self, device: &str, version: &str) -> Result<bool, StoreError> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let info: Option<Option<String>> = tx
            .query_row(
                "SELECT info FROM devices WHERE id = ?1 AND revoked_at IS NULL",
                params![device],
                |r| r.get(0),
            )
            .optional()?;
        let Some(Ok(mut info)) = info
            .flatten()
            .map(|i| serde_json::from_str::<hub_protocol::device::DeviceInfo>(&i))
        else {
            return Ok(false);
        };
        if info.firmware_version != version {
            info.firmware_version = version.to_string();
            tx.execute(
                "UPDATE devices SET info = ?2 WHERE id = ?1",
                params![device, serde_json::to_string(&info).expect("serialises")],
            )?;
        }
        tx.commit()?;
        Ok(true)
    }

    /// Whether a display's open offer is worth a look at its heartbeat: one exists for `version`.
    /// Read only, so most heartbeats cost no write.
    pub fn open_offer_for_version(
        &self,
        device: &str,
        version: &str,
        now: Timestamp,
    ) -> Result<bool, StoreError> {
        Ok(self.conn().query_row(
            "SELECT EXISTS(SELECT 1 FROM firmware_offers WHERE device_id = ?1 AND ended_at IS NULL \
             AND expires_at > ?3 AND firmware_version = ?2)",
            params![device, version, now.unix()],
            |r| r.get(0),
        )?)
    }

    /// Drops the rows of packages whose file is gone (after a restore, say), cancelling their open
    /// offers. Returns the ids dropped.
    pub fn drop_missing_packages(
        &self,
        exists: impl Fn(&str) -> bool,
        now: Timestamp,
    ) -> Result<Vec<String>, StoreError> {
        let ids: Vec<String> = {
            let conn = self.conn();
            let mut st = conn.prepare("SELECT id FROM firmware_packages ORDER BY id")?;
            let rows = st.query_map([], |r| r.get::<_, String>(0))?;
            rows.collect::<Result<_, _>>()?
        };
        let mut dropped = Vec::new();
        for id in ids.into_iter().filter(|id| !exists(id)) {
            self.remove_firmware_package(&id, CANCELLED_PACKAGE_MISSING, now)?;
            dropped.push(id);
        }
        Ok(dropped)
    }

    // ----- signers -----

    /// The owner's labels for signing keys, by label.
    pub fn firmware_signers(&self) -> Result<Vec<(String, String, Timestamp)>, StoreError> {
        let conn = self.conn();
        let mut st =
            conn.prepare("SELECT key_id, label, updated_at FROM firmware_signers ORDER BY label, key_id")?;
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, ts(r.get(2)?))))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Sets the label for a key, adding it if new (up to [`MAX_SIGNER_LABELS`]).
    pub fn set_signer_label(&self, key_id: &str, label: &str, now: Timestamp) -> Result<(), StoreError> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM firmware_signers WHERE key_id = ?1)",
            params![key_id],
            |r| r.get(0),
        )?;
        let count: i64 = tx.query_row("SELECT COUNT(*) FROM firmware_signers", [], |r| r.get(0))?;
        if !exists && count as usize >= MAX_SIGNER_LABELS {
            return Err(StoreError::Invalid(format!(
                "The hub keeps labels for at most {MAX_SIGNER_LABELS} signing keys. Remove one first."
            )));
        }
        tx.execute(
            "INSERT INTO firmware_signers (key_id, label, updated_at) VALUES (?1, ?2, ?3) \
             ON CONFLICT(key_id) DO UPDATE SET label = excluded.label, updated_at = excluded.updated_at",
            params![key_id, label, now.unix()],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn remove_signer_label(&self, key_id: &str) -> Result<bool, StoreError> {
        Ok(self
            .conn()
            .execute("DELETE FROM firmware_signers WHERE key_id = ?1", params![key_id])?
            > 0)
    }

    // ----- publishers -----

    /// Every publisher that has paired, newest first: id, label, when, and whether revoked.
    pub fn publishers(&self) -> Result<Vec<(String, String, Timestamp, bool)>, StoreError> {
        let conn = self.conn();
        let mut st = conn.prepare(
            "SELECT subject, label, created_at, revoked_at FROM credentials WHERE role = 'publisher' \
             ORDER BY created_at DESC",
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
}

fn read_offer(conn: &Connection, id: &str) -> rusqlite::Result<OfferRow> {
    conn.query_row(
        &format!("SELECT {OFFER_COLUMNS} FROM firmware_offers WHERE id = ?1"),
        params![id],
        offer_row,
    )
}

/// Cancels a display's open offers as it is removed, inside the caller's transaction.
pub(super) fn cancel_for_removed_display(
    conn: &Connection,
    device: &str,
    now: Timestamp,
) -> rusqlite::Result<usize> {
    expire(conn, now)?;
    cancel_where(conn, "device_id = ?3", device, CANCELLED_DISPLAY_REMOVED, now)
}
