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
/// All packages one publisher uploaded, together, so one publisher token can't fill the shelf.
pub const MAX_PUBLISHER_STORE_BYTES: u64 = 64 * 1024 * 1024;
/// An upload is refused if it would leave less than this free on the disk.
pub const MIN_FREE_DISK_BYTES: u64 = 64 * 1024 * 1024;
/// The largest upload: the largest package there is.
pub const MAX_UPLOAD_BYTES: u64 = hub_protocol::limits::MAX_FIRMWARE_PACKAGE_BYTES;
/// Uploads handled at once. One at a time keeps the storage limits exact.
pub const MAX_CONCURRENT_UPLOADS: usize = 1;
/// The time limit of an upload or a download, in place of the hub's usual 10 seconds.
pub const FIRMWARE_REQUEST_TIMEOUT: Duration =
    Duration::from_secs(hub_protocol::limits::FIRMWARE_DOWNLOAD_TIMEOUT_SECONDS);
/// An upload that sends nothing for this long is dropped, well before the overall limit.
pub const UPLOAD_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
/// How long firmware transfers may take. Always [`FirmwareTimeLimits::default`] in service.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FirmwareTimeLimits {
    /// A download, from its answer to its last byte. A display that stops reading loses its
    /// connection at this point, and the slot and file it held.
    pub download: Duration,
    /// The longest an upload may send nothing.
    pub upload_idle: Duration,
}

impl Default for FirmwareTimeLimits {
    fn default() -> Self {
        Self {
            download: FIRMWARE_REQUEST_TIMEOUT,
            upload_idle: UPLOAD_IDLE_TIMEOUT,
        }
    }
}

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

/// Versions compare as the app compares them (`firmware_package::version`).
pub use firmware_package::version::compare as compare_versions;
use firmware_package::version::order as version_order;

// ----- the shelf -----

/// Why there is no room for a new package.
#[derive(Debug, PartialEq, Eq)]
pub enum NoRoom {
    /// Its device type has a full shelf, and every package that could make room is on offer.
    AllOnOffer,
    /// Its device type has a full shelf, and making room would remove a package someone other than
    /// this publisher uploaded.
    OthersPackages,
    /// This publisher's packages would pass [`MAX_PUBLISHER_STORE_BYTES`].
    PublisherFull,
    /// All packages together would pass [`MAX_FIRMWARE_STORE_BYTES`].
    StoreFull,
}

impl NoRoom {
    /// A plain sentence for the uploader.
    pub fn message(&self, device_type: &str) -> String {
        match self {
            NoRoom::AllOnOffer => format!(
                "The hub already holds {MAX_PACKAGES_PER_DEVICE_TYPE} packages for {device_type} displays, and \
                 every one is on offer to a display. Wait for those updates to finish, or cancel one, then \
                 upload again."
            ),
            NoRoom::OthersPackages => format!(
                "The hub already holds {MAX_PACKAGES_PER_DEVICE_TYPE} packages for {device_type} displays, and \
                 making room would remove one that someone else uploaded. Ask the hub's owner to remove a \
                 package first."
            ),
            NoRoom::PublisherFull => format!(
                "The hub keeps at most {} MiB of firmware from each publisher, and this package would take \
                 yours past that. Ask the hub's owner to remove one of your packages first.",
                MAX_PUBLISHER_STORE_BYTES / (1024 * 1024)
            ),
            NoRoom::StoreFull => format!(
                "The hub keeps at most {} MiB of firmware, and this package would take it past that. Remove a \
                 package first.",
                MAX_FIRMWARE_STORE_BYTES / (1024 * 1024)
            ),
        }
    }
}

/// Which stored package to remove to make room for `new`, after the per-device-type, per-publisher
/// and overall limits: `Ok(None)` when there is room already. The new package itself is always
/// kept. `publisher` is the uploader's id when a publisher uploads: it may only make room by
/// removing its own packages. An admin may remove anyone's.
pub fn plan_room(
    stored: &[PackageRow],
    new: &PackageRow,
    on_offer: &std::collections::HashSet<String>,
    publisher: Option<&str>,
) -> Result<Option<String>, NoRoom> {
    let same: Vec<&PackageRow> = stored
        .iter()
        .filter(|p| p.device_type == new.device_type)
        .collect();
    let evict = if same.len() < MAX_PACKAGES_PER_DEVICE_TYPE {
        None
    } else {
        let free: Vec<&PackageRow> = same.into_iter().filter(|p| !on_offer.contains(&p.id)).collect();
        if free.is_empty() {
            return Err(NoRoom::AllOnOffer);
        }
        let lowest = |c: &[&PackageRow]| {
            c.iter()
                .min_by(|a, b| {
                    version_order(&a.firmware_version, &b.firmware_version)
                        .then(a.uploaded_at.cmp(&b.uploaded_at))
                })
                .map(|p| (*p).clone())
        };
        let pick = match publisher {
            None => lowest(&free),
            Some(me) => {
                let mine: Vec<&PackageRow> = free.into_iter().filter(|p| is_by(p, me)).collect();
                lowest(&mine)
            }
        };
        Some(pick.ok_or(NoRoom::OthersPackages)?)
    };
    let freed = evict.as_ref().map_or(0, |p| p.size);
    if let Some(me) = publisher {
        let freed_mine = evict.as_ref().filter(|p| is_by(p, me)).map_or(0, |p| p.size);
        let mine: u64 = stored.iter().filter(|p| is_by(p, me)).map(|p| p.size).sum();
        if mine - freed_mine + new.size > MAX_PUBLISHER_STORE_BYTES {
            return Err(NoRoom::PublisherFull);
        }
    }
    let total: u64 = stored.iter().map(|p| p.size).sum();
    if total - freed + new.size > MAX_FIRMWARE_STORE_BYTES {
        return Err(NoRoom::StoreFull);
    }
    Ok(evict.map(|p| p.id))
}

/// Whether publisher `id` uploaded `p`.
fn is_by(p: &PackageRow, id: &str) -> bool {
    p.uploaded_by_role == Role::Publisher && p.uploaded_by == id
}

/// The header and manifest at the start of a stored package's file, exactly as signed: the signer's
/// key and the manifest bytes. `None` if the file can't be read or doesn't start with a header.
pub fn signed_part(path: &Path) -> Option<(firmware_package::PublicKey, Vec<u8>)> {
    use firmware_package::limits::HEADER_LEN;
    use std::io::Read;
    let mut f = std::fs::File::open(path).ok()?;
    let mut header = [0u8; HEADER_LEN];
    f.read_exact(&mut header).ok()?;
    let header = firmware_package::Header::parse(&header).ok()?;
    let mut manifest = vec![0u8; usize::from(header.manifest_len)];
    f.read_exact(&mut manifest).ok()?;
    Some((header.public_key, manifest))
}

/// A stored package that is the same release as `checked`, signed again: the same signer and the
/// same manifest bytes, so the same image and parts, with only the signature different (an ECDSA
/// signature can be re-made, or have its `s` replaced by `n - s`, and still verify). Such a copy is
/// the same release, and must not sit beside the original or push it off the shelf.
pub fn same_release<'a>(
    dir: &Path,
    stored: &'a [PackageRow],
    checked: &CheckedPackage,
) -> Option<&'a PackageRow> {
    let signer = checked.signer().to_hex();
    let m = &checked.manifest;
    stored
        .iter()
        .filter(|p| {
            p.signer_public_key == signer
                && p.device_type == m.device_type
                && p.firmware_version == m.firmware_version
                && p.security_counter == m.security_counter
                && p.image_size == m.image_size
        })
        .find(|p| {
            signed_part(&package_path(dir, &p.id))
                .is_some_and(|(key, manifest)| key == checked.signer() && manifest == checked.manifest_bytes)
        })
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
        key_set: m
            .key_set
            .as_ref()
            .map(|ks| ks.iter().map(|k| k.key_id().to_hex()).collect()),
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
        image_size: p.image_size,
        usb_install: p.usb_install,
        key_set: p.key_set.clone(),
        uploaded_at: p.uploaded_at,
        uploaded_by_role: p.uploaded_by_role.as_str().to_string(),
        uploaded_by: p.uploaded_by.clone(),
        uploaded_by_label: p.uploaded_by_label.clone(),
        offered_to,
        evicted: None,
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
    if firmware_package::drops_key_unraised(
        p.key_set.as_deref(),
        &fw.key_ids,
        p.security_counter,
        fw.security_floor,
    ) {
        return Err(format!(
            "This package would stop the display trusting one of its firmware keys without a higher \
             security counter (this package's counter is {}, and the display's floor is {}), so it would \
             refuse it. Offer a newer release from the same maker.",
            p.security_counter, fw.security_floor
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

    fn row(id: &str, device_type: &str, version: &str, size: u64, by: Option<&str>) -> PackageRow {
        PackageRow {
            id: id.into(),
            device_type: device_type.into(),
            hardware_revisions: vec!["rev".into()],
            firmware_version: version.into(),
            security_counter: 0,
            summary: None,
            signer_key_id: "a".repeat(64),
            signer_public_key: format!("04{}", "b".repeat(128)),
            image_size: size,
            size,
            update_size: size,
            usb_install: false,
            key_set: None,
            uploaded_at: Timestamp::from_unix(1_800_000_000).unwrap(),
            uploaded_by_role: if by.is_some() {
                Role::Publisher
            } else {
                Role::Admin
            },
            uploaded_by: by.unwrap_or("admin").into(),
            uploaded_by_label: None,
        }
    }

    const MIB: u64 = 1024 * 1024;

    #[test]
    fn a_full_shelf_makes_room_by_the_lowest_version_and_a_publisher_only_with_its_own() {
        let none = std::collections::HashSet::new();
        let stored = vec![
            row("a", "t", "1.0.0", MIB, None),
            row("b", "t", "1.1.0", MIB, Some("p-me")),
            row("c", "t", "1.2.0", MIB, Some("p-other")),
        ];
        let new = row("n", "t", "2.0.0", MIB, Some("p-me"));
        // An admin's upload removes the lowest version, whoever uploaded it.
        let admin_new = row("n", "t", "2.0.0", MIB, None);
        assert_eq!(plan_room(&stored, &admin_new, &none, None), Ok(Some("a".into())));
        // A publisher's removes its own lowest, even when someone else's is lower.
        assert_eq!(
            plan_room(&stored, &new, &none, Some("p-me")),
            Ok(Some("b".into()))
        );
        // And is refused when only others' packages could go.
        let on_offer: std::collections::HashSet<String> = ["b".to_string()].into();
        assert_eq!(
            plan_room(&stored, &new, &on_offer, Some("p-me")),
            Err(NoRoom::OthersPackages)
        );
        assert_eq!(
            plan_room(&stored, &new, &on_offer, Some("p-nobody")),
            Err(NoRoom::OthersPackages)
        );
        let all: std::collections::HashSet<String> = ["a", "b", "c"].map(String::from).into();
        assert_eq!(
            plan_room(&stored, &new, &all, Some("p-me")),
            Err(NoRoom::AllOnOffer)
        );
        assert_eq!(
            plan_room(&stored, &admin_new, &all, None),
            Err(NoRoom::AllOnOffer)
        );
        // Another device type has room.
        let other = row("n", "u", "1.0.0", MIB, Some("p-other"));
        assert_eq!(plan_room(&stored, &other, &none, Some("p-other")), Ok(None));
        assert!(NoRoom::OthersPackages
            .message("t")
            .contains("someone else uploaded"));
    }

    #[test]
    fn each_publisher_has_its_own_cap_and_the_shelf_one_for_all() {
        let none = std::collections::HashSet::new();
        // 60 MiB from this publisher, across four device types.
        let stored: Vec<PackageRow> = (0..4)
            .map(|i| {
                row(
                    &format!("m{i}"),
                    &format!("t{i}"),
                    "1.0.0",
                    15 * MIB,
                    Some("p-me"),
                )
            })
            .collect();
        let new = row("n", "t9", "1.0.0", 5 * MIB, Some("p-me"));
        assert_eq!(
            plan_room(&stored, &new, &none, Some("p-me")),
            Err(NoRoom::PublisherFull)
        );
        let small = row("n", "t9", "1.0.0", 4 * MIB, Some("p-me"));
        assert_eq!(plan_room(&stored, &small, &none, Some("p-me")), Ok(None));
        // Another publisher, or an admin, isn't held to this one's cap.
        let theirs = row("n", "t9", "1.0.0", 5 * MIB, Some("p-other"));
        assert_eq!(plan_room(&stored, &theirs, &none, Some("p-other")), Ok(None));
        let admin = row("n", "t9", "1.0.0", 5 * MIB, None);
        assert_eq!(plan_room(&stored, &admin, &none, None), Ok(None));
        // Removing one of its own makes room under its cap.
        let mut full: Vec<PackageRow> = (0..3)
            .map(|i| {
                row(
                    &format!("s{i}"),
                    "t0",
                    &format!("1.{i}.0"),
                    15 * MIB,
                    Some("p-me"),
                )
            })
            .collect();
        full.push(row("x", "t1", "1.0.0", 15 * MIB, Some("p-me")));
        let next = row("n", "t0", "2.0.0", 15 * MIB, Some("p-me"));
        assert_eq!(
            plan_room(&full, &next, &none, Some("p-me")),
            Ok(Some("s0".into()))
        );
        // The whole shelf: 256 MiB for everyone together.
        let big: Vec<PackageRow> = (0..16)
            .map(|i| row(&format!("b{i}"), &format!("t{i}"), "1.0.0", 16 * MIB, None))
            .collect();
        let one_more = row("n", "t99", "1.0.0", 1, None);
        assert_eq!(plan_room(&big, &one_more, &none, None), Err(NoRoom::StoreFull));
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
