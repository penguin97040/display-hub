//! The hub's shelf of firmware packages (`docs/firmware-updates.md`, "The hub's part"): where the
//! files live, the storage limits, what to evict, and the courtesy checks made before an offer.
//!
//! Each package is `<data_dir>/firmware/<package_id>.dhfw`, owner-only, written to a temporary file
//! in the same folder and renamed into place once it has passed every check. The hub never signs,
//! alters or fetches a package. Checking one is `firmware_package::check`'s job, never repeated here.

use std::cmp::Ordering;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use firmware_package::{CheckedPackage, KeyId};
use hub_core::api::{FirmwareOfferView, FirmwarePackageView};
use hub_protocol::device::DeviceInfo;
use hub_protocol::Timestamp;

use crate::secret::Role;
use crate::store::firmware::{OfferRow, PackageRow};
use crate::store::{Store, StoreError};

/// The packages folder, inside the data folder.
pub const FIRMWARE_DIR: &str = "firmware";
/// A stored package's file name ends in this.
pub const PACKAGE_EXTENSION: &str = "dhfw";
/// Uploads in progress are `.upload-<random>.tmp` in the packages folder.
const UPLOAD_PREFIX: &str = ".upload-";
const UPLOAD_SUFFIX: &str = ".tmp";
/// Packages kept for each device type. A fourth makes room by removing that type's lowest version
/// that isn't on offer.
pub const MAX_PACKAGES_PER_DEVICE_TYPE: usize = 3;
/// All packages together.
pub const MAX_FIRMWARE_STORE_BYTES: u64 = 256 * 1024 * 1024;
/// An upload is refused if it would leave less than this free on the disk.
pub const MIN_FREE_DISK_BYTES: u64 = 64 * 1024 * 1024;
/// The largest upload: the largest package there is.
pub const MAX_UPLOAD_BYTES: u64 = hub_protocol::limits::MAX_FIRMWARE_PACKAGE_BYTES;
/// Uploads handled at once. One at a time keeps the storage limits exact.
pub const MAX_CONCURRENT_UPLOADS: usize = 1;
/// The time limit of an upload or a download, in place of the hub's usual 10 seconds.
pub const FIRMWARE_REQUEST_TIMEOUT: Duration =
    Duration::from_secs(hub_protocol::limits::FIRMWARE_DOWNLOAD_TIMEOUT_SECONDS);
/// The size of each piece a package is streamed in, to disk and to a display.
pub const STREAM_CHUNK_BYTES: usize = 64 * 1024;

/// The packages folder for a data folder.
pub fn dir(data_dir: &Path) -> PathBuf {
    data_dir.join(FIRMWARE_DIR)
}

/// A stored package's file.
pub fn package_path(dir: &Path, package_id: &str) -> PathBuf {
    dir.join(format!("{package_id}.{PACKAGE_EXTENSION}"))
}

/// A new temporary file name for an upload.
pub fn upload_path(dir: &Path) -> PathBuf {
    let r = crate::secret::random_bytes::<8>();
    let tag: String = r.iter().map(|b| format!("{b:02x}")).collect();
    dir.join(format!("{UPLOAD_PREFIX}{tag}{UPLOAD_SUFFIX}"))
}

/// Makes the packages folder if it is missing, owner-only.
pub fn ensure_dir(dir: &Path) -> std::io::Result<()> {
    let mut b = std::fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        b.mode(0o700);
    }
    b.create(dir)
}

/// Opens a new upload file, owner-only, refusing to replace anything.
pub fn create_upload(path: &Path) -> std::io::Result<std::fs::File> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)
}

/// Moves a checked upload into place as its package's file, and makes the rename durable.
pub fn keep_upload(upload: &Path, dest: &Path) -> std::io::Result<()> {
    std::fs::rename(upload, dest)?;
    #[cfg(unix)]
    if let Some(parent) = dest.parent() {
        std::fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
}

/// Whether `name` is one of ours: a package file named by its id, or an upload in progress.
fn package_id_of(name: &str) -> Option<&str> {
    let id = name.strip_suffix(&format!(".{PACKAGE_EXTENSION}"))?;
    hub_protocol::firmware::is_lower_hex(id, hub_protocol::firmware::PACKAGE_ID_HEX_LEN).then_some(id)
}

fn is_upload(name: &str) -> bool {
    name.starts_with(UPLOAD_PREFIX) && name.ends_with(UPLOAD_SUFFIX)
}

/// What [`tidy`] did at start-up.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Tidied {
    /// Rows dropped because their file is missing (after a restore, say).
    pub dropped_rows: Vec<String>,
    /// Files removed: unfinished uploads, and packages with no row (a crash between the rename and
    /// the row). They can be uploaded again.
    pub removed_files: usize,
}

/// Brings the packages folder and its rows into agreement, when the hub starts serving. Only files
/// this module names are ever removed.
pub fn tidy(dir: &Path, store: &Store, now: Timestamp) -> Result<Tidied, StoreError> {
    if let Err(e) = ensure_dir(dir) {
        tracing::warn!(error = %e, path = %dir.display(), "couldn't make the firmware folder");
    }
    let mut out = Tidied::default();
    let known: std::collections::HashSet<String> =
        store.firmware_packages()?.into_iter().map(|p| p.id).collect();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let stray = is_upload(&name) || package_id_of(&name).is_some_and(|id| !known.contains(id));
            if stray && e.file_type().is_ok_and(|t| t.is_file()) && std::fs::remove_file(e.path()).is_ok() {
                out.removed_files += 1;
            }
        }
    }
    out.dropped_rows = store.drop_missing_packages(|id| package_path(dir, id).is_file(), now)?;
    Ok(out)
}

/// Bytes free on the disk holding `dir`, for an ordinary user. `None` where this build can't tell,
/// and then the free-space limit isn't checked (the hub's home is Linux on a Pi).
#[cfg(target_os = "linux")]
pub fn free_bytes(dir: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(dir.as_os_str().as_bytes()).ok()?;
    // SAFETY: statvfs reads a NUL-terminated path and writes only into the struct it is given.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(path.as_ptr(), &mut st) } != 0 {
        return None;
    }
    Some((st.f_bavail as u64).saturating_mul(st.f_frsize as u64))
}

#[cfg(not(target_os = "linux"))]
pub fn free_bytes(_dir: &Path) -> Option<u64> {
    None
}

/// Writes all of `bytes` to `file`. A small helper so the upload loop reads plainly.
pub fn write_chunk(file: &mut std::fs::File, bytes: &[u8]) -> std::io::Result<()> {
    file.write_all(bytes)
}

// ----- versions -----

/// A `MAJOR.MINOR.PATCH[-pre]` version, as the manifest rules allow.
#[derive(Debug, PartialEq, Eq)]
struct Version<'a> {
    numbers: [u64; 3],
    pre: Option<&'a str>,
}

fn parse_version(s: &str) -> Option<Version<'_>> {
    let (core, pre) = match s.split_once('-') {
        Some((c, p)) if !p.is_empty() => (c, Some(p)),
        Some(_) => return None,
        None => (s, None),
    };
    let mut parts = core.split('.');
    let mut numbers = [0u64; 3];
    for n in &mut numbers {
        let p = parts.next()?;
        if p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        *n = p.parse().ok()?;
    }
    parts.next().is_none().then_some(Version { numbers, pre })
}

/// Compares two versions as the app does: three numbers, and with those equal, a version with a
/// pre-release tag is older than one without. Two tags compare part by part (split at `.`), numbers
/// as numbers and anything else as text. `None` if either isn't a version.
pub fn compare_versions(a: &str, b: &str) -> Option<Ordering> {
    let (a, b) = (parse_version(a)?, parse_version(b)?);
    Some(a.numbers.cmp(&b.numbers).then_with(|| match (a.pre, b.pre) {
        (None, None) => Ordering::Equal,
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (Some(x), Some(y)) => {
            let mut xs = x.split('.');
            let mut ys = y.split('.');
            loop {
                match (xs.next(), ys.next()) {
                    (None, None) => break Ordering::Equal,
                    (None, Some(_)) => break Ordering::Less,
                    (Some(_), None) => break Ordering::Greater,
                    (Some(p), Some(q)) => {
                        let o = match (p.parse::<u64>(), q.parse::<u64>()) {
                            (Ok(m), Ok(n)) => m.cmp(&n),
                            (Ok(_), Err(_)) => Ordering::Less,
                            (Err(_), Ok(_)) => Ordering::Greater,
                            _ => p.cmp(q),
                        };
                        if o != Ordering::Equal {
                            break o;
                        }
                    }
                }
            }
        }
    }))
}

/// Orders versions for choosing the lowest: real versions by [`compare_versions`], anything
/// unreadable first.
fn version_order(a: &str, b: &str) -> Ordering {
    match (parse_version(a).is_some(), parse_version(b).is_some()) {
        (true, true) => compare_versions(a, b).unwrap_or(Ordering::Equal),
        (false, true) => Ordering::Less,
        (true, false) => Ordering::Greater,
        (false, false) => a.cmp(b),
    }
}

// ----- the shelf -----

/// Every package that could make room is on offer to a display.
#[derive(Debug, PartialEq, Eq)]
pub struct AllOnOffer;

/// Which stored package to remove to make room for a new one of `device_type`: `Ok(None)` when there
/// is room already. The new package itself is always kept.
pub fn pick_eviction(
    stored: &[PackageRow],
    device_type: &str,
    on_offer: &std::collections::HashSet<String>,
) -> Result<Option<String>, AllOnOffer> {
    let same: Vec<&PackageRow> = stored.iter().filter(|p| p.device_type == device_type).collect();
    if same.len() < MAX_PACKAGES_PER_DEVICE_TYPE {
        return Ok(None);
    }
    same.into_iter()
        .filter(|p| !on_offer.contains(&p.id))
        .min_by(|a, b| {
            version_order(&a.firmware_version, &b.firmware_version).then(a.uploaded_at.cmp(&b.uploaded_at))
        })
        .map(|p| Some(p.id.clone()))
        .ok_or(AllOnOffer)
}

/// A row for a package that passed `firmware_package::check`.
pub fn row_from(checked: &CheckedPackage, role: Role, by: &str, now: Timestamp) -> PackageRow {
    let m = &checked.manifest;
    PackageRow {
        id: checked.package_id.to_hex(),
        device_type: m.device_type.clone(),
        hardware_revisions: m.hardware_revisions.clone(),
        firmware_version: m.firmware_version.clone(),
        security_counter: m.security_counter,
        summary: m.summary.clone(),
        signer_key_id: checked.signer_key_id().to_hex(),
        signer_public_key: checked.signer().to_hex(),
        image_size: m.image_size,
        size: checked.len,
        update_size: checked.update_form_len(),
        usb_install: m.usb_install.is_some(),
        uploaded_at: now,
        uploaded_by_role: role,
        uploaded_by: by.to_string(),
        uploaded_by_label: None,
    }
}

/// The fingerprint of a key id given as hex, or the first digits if it isn't one.
pub fn fingerprint(key_id: &str) -> String {
    match firmware_package::hex::decode_exact::<32>(key_id) {
        Some(b) => KeyId(b).fingerprint(),
        None => key_id.chars().take(32).collect(),
    }
}

pub fn package_view(
    p: &PackageRow,
    labels: &std::collections::HashMap<String, String>,
    offered_to: Vec<String>,
) -> FirmwarePackageView {
    FirmwarePackageView {
        package_id: p.id.clone(),
        device_type: p.device_type.clone(),
        hardware_revisions: p.hardware_revisions.clone(),
        firmware_version: p.firmware_version.clone(),
        security_counter: p.security_counter,
        summary: p.summary.clone(),
        signer_key_id: p.signer_key_id.clone(),
        signer_fingerprint: fingerprint(&p.signer_key_id),
        signer_label: labels.get(&p.signer_key_id).cloned(),
        size: p.size,
        update_size: p.update_size,
        usb_install: p.usb_install,
        uploaded_at: p.uploaded_at,
        uploaded_by_role: p.uploaded_by_role.as_str().to_string(),
        uploaded_by: p.uploaded_by.clone(),
        uploaded_by_label: p.uploaded_by_label.clone(),
        offered_to,
    }
}

pub fn offer_view(o: &OfferRow, now: Timestamp) -> FirmwareOfferView {
    FirmwareOfferView {
        offer_id: o.id.clone(),
        package_id: o.package_id.clone(),
        firmware_version: o.firmware_version.clone(),
        state: o.state_at(now).to_string(),
        reason: o.reason.clone(),
        reported_version: o.reported_version.clone(),
        made_at: o.made_at,
        expires_at: o.expires_at,
        updated_at: o.updated_at,
        ended_at: o
            .ended_at
            .or_else(|| (o.state_at(now) != o.state).then_some(o.expires_at)),
    }
}

/// The courtesy checks before an offer: by the display's own last report, would it refuse this
/// package? A plain sentence if so. The display repeats every check; these only save a pointless
/// download and say why.
pub fn courtesy(info: Option<&DeviceInfo>, p: &PackageRow) -> Result<(), String> {
    let Some(info) = info else {
        return Err(
            "This display hasn't reported itself to the hub yet. Wait for it to check in, then try again."
                .into(),
        );
    };
    let Some(fw) = &info.firmware else {
        return Err(
            "This display hasn't said which firmware keys it trusts, so it can't take updates over Wi-Fi. \
             Its firmware may be too old, or it was set up without keys: use Install firmware over USB."
                .into(),
        );
    };
    if p.device_type != info.device_type {
        return Err(format!(
            "This package is for {} displays, and this display is a {}.",
            p.device_type, info.device_type
        ));
    }
    if !p.hardware_revisions.contains(&info.hardware_revision) {
        return Err(format!(
            "This package isn't built for this display's hardware ({}). It is for: {}.",
            info.hardware_revision,
            p.hardware_revisions.join(", ")
        ));
    }
    if !fw.key_ids.contains(&p.signer_key_id) {
        return Err(if fw.key_ids.is_empty() {
            "This display trusts no firmware keys, so it refuses every update. Use Install firmware over USB."
                .into()
        } else {
            format!(
                "This display doesn't trust the key that signed this package ({}). Only its maker's keys can update it.",
                fingerprint(&p.signer_key_id)
            )
        });
    }
    if p.security_counter < fw.security_floor {
        return Err(format!(
            "This display has already installed a release with a security fix this one lacks (its security \
             floor is {}, and this package's counter is {}), so it would refuse it.",
            fw.security_floor, p.security_counter
        ));
    }
    if p.image_size > fw.slot_bytes {
        return Err(format!(
            "This firmware is {} bytes, too big for the display's spare space of {} bytes.",
            p.image_size, fw.slot_bytes
        ));
    }
    Ok(())
}

/// The newest package that passes the courtesy checks for a display and is newer than what it runs.
pub fn best_package<'a>(info: Option<&DeviceInfo>, packages: &'a [PackageRow]) -> Option<&'a PackageRow> {
    let current = info.map(|i| i.firmware_version.as_str());
    packages
        .iter()
        .filter(|p| courtesy(info, p).is_ok())
        .filter(
            |p| match current.and_then(|c| compare_versions(&p.firmware_version, c)) {
                Some(o) => o == Ordering::Greater,
                // A version that can't be compared isn't called newer.
                None => false,
            },
        )
        .max_by(|a, b| version_order(&a.firmware_version, &b.firmware_version))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_as_three_numbers_with_pre_releases_older() {
        use Ordering::*;
        for (a, b, o) in [
            ("1.2.0", "1.10.0", Less),
            ("1.2.0", "1.2.0", Equal),
            ("2.0.0", "1.99.99", Greater),
            ("1.3.0-beta.1", "1.3.0", Less),
            ("1.3.0-beta.2", "1.3.0-beta.10", Less),
            ("1.3.0-beta", "1.3.0-beta.1", Less),
            ("1.3.0-1", "1.3.0-alpha", Less),
            ("0.0.0-simulator", "0.0.1", Less),
        ] {
            assert_eq!(compare_versions(a, b), Some(o), "{a} {b}");
        }
        for bad in ["1.2", "1.2.3.4", "v1.2.3", "1.2.3-", "", "1..2"] {
            assert_eq!(compare_versions(bad, "1.0.0"), None, "{bad}");
        }
    }

    #[test]
    fn only_our_own_file_names_are_recognised() {
        let id = "a".repeat(64);
        assert_eq!(package_id_of(&format!("{id}.dhfw")), Some(id.as_str()));
        assert_eq!(package_id_of("notes.dhfw"), None);
        assert_eq!(package_id_of(&format!("{}.dhfw", "A".repeat(64))), None);
        assert!(is_upload(".upload-0011.tmp"));
        assert!(!is_upload("hub.db"));
        let p = upload_path(Path::new("x"));
        assert!(is_upload(p.file_name().unwrap().to_str().unwrap()));
    }

    #[test]
    fn the_hubs_limits_match_the_package_format() {
        use firmware_package::limits::*;
        assert_eq!(hub_protocol::limits::MAX_FIRMWARE_PACKAGE_BYTES, MAX_PACKAGE_LEN);
        assert_eq!(
            hub_protocol::limits::MAX_FIRMWARE_UPDATE_BYTES,
            HEADER_LEN as u64 + MAX_MANIFEST_LEN as u64 + MAX_IMAGE_SIZE
        );
        assert_eq!(hub_protocol::limits::MAX_TRUSTED_KEYS, MAX_TRUSTED_KEYS);
        assert_eq!(hub_protocol::firmware::PUBLIC_KEY_HEX_LEN, PUBLIC_KEY_HEX_LEN);
        assert_eq!(hub_protocol::firmware::KEY_ID_HEX_LEN, SHA256_HEX_LEN);
    }
}
