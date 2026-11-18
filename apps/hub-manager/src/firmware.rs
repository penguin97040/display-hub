//! Firmware in the manager, without any GPUI (`docs/firmware-updates.md`, "The app's part"): who
//! signed a package, which update a display can take, what an offer's state means, and the first
//! install over USB as a plan of raw writes run through a small [`Board`] trait, so all of it is
//! tested without hardware. The view's wording for these is in `guidance.rs`.

use std::cmp::Ordering;
use std::sync::{Arc, Mutex};

use firmware_package::{CheckedPackage, PackageError};
use hub_core::api::{DisplayView, FirmwareOfferView, FirmwarePackageView, SignerView};
use hub_protocol::firmware::{BundleFirmware, ReportReason};
use hub_protocol::Timestamp;

/// The key ids (SHA-256 of the public key, 64 lower-case hex digits) of the official Display Hub
/// firmware keys, which the app names *Official Display Hub firmware*. Empty for now: no official
/// key exists yet. The official keys will be added in a later step, once they have been made and
/// their fingerprints published.
pub const OFFICIAL_KEYS: &[&str] = &[];

/// How long a download normally takes. After this, an offer still reported as downloading is shown
/// as restarting: the display has the image and is starting the new firmware (a trial boot doesn't
/// report until the new firmware has reached the hub).
pub const DOWNLOAD_EXPECTED_SECONDS: i64 = 120;

// ----- who signed it -----

/// Where a signer's name comes from: the app, the owner's label on the hub, or nowhere. A package
/// can't name itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SignerKind {
    /// One of [`OFFICIAL_KEYS`].
    Official,
    /// The owner's label for the key, kept on the hub, so only as trustworthy as the hub.
    Labelled(String),
    Unknown,
}

/// A signing key, as people see it: a name and a fingerprint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Signer {
    pub key_id: String,
    pub fingerprint: String,
    pub kind: SignerKind,
}

pub const OFFICIAL_NAME: &str = "Official Display Hub firmware";
pub const UNKNOWN_NAME: &str = "Unknown maker";

impl Signer {
    pub fn name(&self) -> String {
        match &self.kind {
            SignerKind::Official => OFFICIAL_NAME.into(),
            SignerKind::Labelled(l) => l.clone(),
            SignerKind::Unknown => UNKNOWN_NAME.into(),
        }
    }
}

/// The fingerprint of a key id, as the package library makes it: its first 32 hex digits in groups
/// of four. An id that isn't 64 hex digits is shown as it is, cut to that length and grouped the same
/// way.
pub fn fingerprint(key_id: &str) -> String {
    if let Some(b) = firmware_package::hex::decode_exact::<32>(key_id) {
        return firmware_package::KeyId(b).fingerprint();
    }
    let digits: Vec<char> = key_id.chars().take(32).collect();
    digits
        .chunks(4)
        .map(|g| g.iter().collect::<String>())
        .collect::<Vec<_>>()
        .join("-")
}

/// Whether the owner may not call a key this, as the hub compares it (the hub refuses it too).
pub use firmware_package::label::reserved_label;

/// Names a key: official keys first, then the owner's label, then "Unknown maker".
pub fn signer(key_id: &str, labels: &[SignerView]) -> Signer {
    signer_with(key_id, labels, OFFICIAL_KEYS)
}

fn signer_with(key_id: &str, labels: &[SignerView], official: &[&str]) -> Signer {
    let kind = if official.contains(&key_id) {
        SignerKind::Official
    } else if let Some(l) = labels.iter().find(|l| l.key_id == key_id) {
        SignerKind::Labelled(l.label.clone())
    } else {
        SignerKind::Unknown
    };
    Signer {
        key_id: key_id.to_string(),
        fingerprint: fingerprint(key_id),
        kind,
    }
}

// ----- versions -----

pub use firmware_package::version::{compare as compare_versions, is_pre_release, order as order_versions};

// ----- a display's firmware -----

/// What a display trusts, by its own last report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Trust {
    /// It hasn't reported its keys: firmware made before signed updates, or not checked in yet.
    NotReported,
    /// It trusts no keys, so it refuses every update until Install firmware gives it some.
    NoKeys,
    Keys(Vec<Signer>),
}

/// A package a display could be offered, as the Update button names it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Choice {
    pub package_id: String,
    pub version: String,
    pub signer: Signer,
    /// The maker's own words about the release. Never part of the app's sentences.
    pub summary: Option<String>,
    /// Older than what the display runs, or a pre-release: Update asks twice.
    pub older: bool,
    pub pre_release: bool,
    /// The display's version can't be compared with this one, so the app can't tell whether it is
    /// newer or older: Update asks twice for this too.
    pub unsure: bool,
}

impl Choice {
    fn of(p: &FirmwarePackageView, running: Option<&str>, labels: &[SignerView]) -> Self {
        let order = running.map(|r| compare_versions(&p.firmware_version, r));
        Choice {
            package_id: p.package_id.clone(),
            version: p.firmware_version.clone(),
            signer: signer(&p.signer_key_id, labels),
            summary: p.summary.clone(),
            older: order == Some(Some(Ordering::Less)),
            pre_release: is_pre_release(&p.firmware_version),
            unsure: matches!(order, Some(None)),
        }
    }

    /// Whether Update asks a second time before offering it.
    pub fn asks_twice(&self) -> bool {
        self.older || self.pre_release || self.unsure
    }
}

/// Where an offer has got to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Phase {
    /// Made; the display acts on it at its next check.
    Waiting,
    Downloading,
    /// Downloaded a while ago, and the new firmware hasn't reported yet: it is starting.
    Restarting,
    Installed,
    Refused(ReportReason),
    Failed(ReportReason),
    RolledBack,
    /// Why, as the hub says (`by_admin`, `replaced`, `package_deleted`, `display_removed`,
    /// `package_missing`).
    Cancelled(String),
    Expired,
    /// A state this app doesn't know, which counts as a failure.
    Unknown(String),
}

impl Phase {
    /// Whether the offer is still open, so it can be cancelled and the app keeps checking.
    pub fn is_open(&self) -> bool {
        matches!(self, Phase::Waiting | Phase::Downloading | Phase::Restarting)
    }

    pub fn is_problem(&self) -> bool {
        matches!(
            self,
            Phase::Refused(_) | Phase::Failed(_) | Phase::RolledBack | Phase::Expired | Phase::Unknown(_)
        )
    }
}

fn reason(r: &Option<String>) -> ReportReason {
    r.as_deref()
        .and_then(|r| serde_json::from_value(serde_json::Value::String(r.to_string())).ok())
        .unwrap_or(ReportReason::Unrecognised)
}

/// What an offer's state means, at `now` by the hub's clock.
pub fn phase(offer: &FirmwareOfferView, now: Option<Timestamp>) -> Phase {
    match offer.state.as_str() {
        "offered" => Phase::Waiting,
        "downloading" => {
            let long = now.is_some_and(|n| offer.updated_at.seconds_until(n) >= DOWNLOAD_EXPECTED_SECONDS);
            if long {
                Phase::Restarting
            } else {
                Phase::Downloading
            }
        }
        "installed" => Phase::Installed,
        "refused" => Phase::Refused(reason(&offer.reason)),
        "failed" => Phase::Failed(reason(&offer.reason)),
        "rolled_back" => Phase::RolledBack,
        "cancelled" => Phase::Cancelled(offer.reason.clone().unwrap_or_default()),
        "expired" => Phase::Expired,
        other => Phase::Unknown(other.to_string()),
    }
}

/// Everything a display card says about firmware.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DisplayFirmware {
    /// What it runs, if it has reported.
    pub version: Option<String>,
    /// Who signed what it runs, when a package on the hub matches its version and a key it trusts.
    pub signed_by: Option<Signer>,
    pub trust: Trust,
    /// The update to show first: the newest full release newer than what it runs, or a newer
    /// pre-release when it already runs one or there is no newer full release. Signed by a key it
    /// trusts.
    pub available: Option<Choice>,
    /// Other packages it would accept, newest first, for going back or trying a pre-release.
    pub others: Vec<Choice>,
    /// Its open or most recent offer.
    pub offer: Option<(FirmwareOfferView, Phase)>,
    /// The hub doesn't do signed updates (an older hub).
    pub hub_unaware: bool,
    /// The display has used up its update attempts for now (`attempts_left` is 0). It tries again
    /// by itself after a few hours switched on. Offers are still allowed; they wait.
    pub paused: bool,
}

/// Which button on a display card would try an offer's package again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Retry {
    /// The "Update to …" button.
    Update(String),
    /// One of the "Install …" buttons under the other versions.
    Install(String),
}

impl DisplayFirmware {
    /// The button that offers `package_id` again, if the card shows one.
    pub fn retry(&self, package_id: &str) -> Option<Retry> {
        if let Some(a) = self.available.as_ref().filter(|a| a.package_id == package_id) {
            return Some(Retry::Update(a.version.clone()));
        }
        self.others
            .iter()
            .find(|o| o.package_id == package_id)
            .map(|o| Retry::Install(o.version.clone()))
    }
}

/// Whether a package passes the checks the app can make for a display, by its last report: the
/// same device type and hardware revision, a key it trusts, at or above its floor, not dropping a
/// key it trusts without a higher counter, and small enough for its spare space. Never a package
/// the hub calls damaged, since what the app knows of it can't be relied on and the hub won't offer
/// it. The hub and the display check again.
pub fn accepts(d: &DisplayView, p: &FirmwarePackageView) -> bool {
    let Some(info) = &d.device else { return false };
    let Some(fw) = &info.firmware else { return false };
    !p.damaged
        && p.device_type == info.device_type
        && p.hardware_revisions.contains(&info.hardware_revision)
        && fw.key_ids.contains(&p.signer_key_id)
        && p.security_counter >= fw.security_floor
        && !firmware_package::drops_key_unraised(
            p.key_set.as_deref(),
            &fw.key_ids,
            p.security_counter,
            fw.security_floor,
        )
        && p.image_size <= fw.slot_bytes
}

/// The update a card shows first, from the packages newer than what the display runs: the newest
/// full release, unless the display already runs a pre-release or there is no newer full release,
/// when the newest of them all.
fn headline<'a>(newer: &[&'a FirmwarePackageView], running: &str) -> Option<&'a FirmwarePackageView> {
    let newest = |stable_only: bool| {
        newer
            .iter()
            .copied()
            .filter(|p| !stable_only || !is_pre_release(&p.firmware_version))
            .max_by(|a, b| order_versions(&a.firmware_version, &b.firmware_version))
    };
    if is_pre_release(running) {
        newest(false)
    } else {
        newest(true).or_else(|| newest(false))
    }
}

/// A display's firmware, from the hub's lists. `packages` is `None` when the hub didn't list them.
pub fn display_firmware(
    d: &DisplayView,
    packages: Option<&[FirmwarePackageView]>,
    labels: &[SignerView],
    now: Option<Timestamp>,
) -> DisplayFirmware {
    let info = d.device.as_ref();
    let version = info.map(|i| i.firmware_version.clone());
    let reported = info.and_then(|i| i.firmware.as_ref());
    let trust = match reported {
        None => Trust::NotReported,
        Some(f) if f.key_ids.is_empty() => Trust::NoKeys,
        Some(f) => Trust::Keys(f.key_ids.iter().map(|k| signer(k, labels)).collect()),
    };
    let packages = packages.unwrap_or_default();
    let running = version.as_deref();
    let same = |p: &FirmwarePackageView| {
        running.is_some_and(|r| compare_versions(&p.firmware_version, r) == Some(Ordering::Equal))
    };
    let signed_by = packages
        .iter()
        .find(|p| accepts(d, p) && same(p))
        .map(|p| signer(&p.signer_key_id, labels));
    let usable: Vec<&FirmwarePackageView> = packages.iter().filter(|p| accepts(d, p) && !same(p)).collect();
    // Chosen here rather than taken from the hub, so a pre-release isn't put first over a newer
    // full release.
    let available = running.and_then(|r| {
        let newer: Vec<&FirmwarePackageView> = usable
            .iter()
            .copied()
            .filter(|p| compare_versions(&p.firmware_version, r) == Some(Ordering::Greater))
            .collect();
        headline(&newer, r)
    });
    let mut others: Vec<&FirmwarePackageView> = usable
        .into_iter()
        .filter(|p| Some(&p.package_id) != available.map(|a| &a.package_id))
        .collect();
    others.sort_by(|a, b| order_versions(&b.firmware_version, &a.firmware_version));
    let fw = d.firmware.as_ref();
    DisplayFirmware {
        version: version.clone(),
        signed_by,
        trust,
        available: available.map(|p| Choice::of(p, running, labels)),
        others: others
            .into_iter()
            .map(|p| Choice::of(p, running, labels))
            .collect(),
        offer: fw.and_then(|f| f.offer.clone()).map(|o| {
            let ph = phase(&o, now);
            (o, ph)
        }),
        hub_unaware: fw.is_none(),
        paused: reported.is_some_and(|f| f.attempts_left == Some(0)),
    }
}

/// Whether any display has an offer still open, so the app checks the hub more often.
pub fn any_open_offer(displays: &[DisplayView], now: Option<Timestamp>) -> bool {
    displays.iter().any(|d| {
        d.firmware
            .as_ref()
            .and_then(|f| f.offer.as_ref())
            .is_some_and(|o| phase(o, now).is_open())
    })
}

// ----- the first install over USB -----

/// A package read and checked by this app, with its bytes.
#[derive(Clone)]
pub struct Loaded {
    pub bytes: Arc<Vec<u8>>,
    pub checked: CheckedPackage,
}

impl std::fmt::Debug for Loaded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Loaded")
            .field("package_id", &self.checked.package_id.to_hex())
            .field("len", &self.bytes.len())
            .finish()
    }
}

impl Loaded {
    /// Checks the bytes as the hub and the signing tool do. Nothing here says the key is trusted.
    pub fn check(bytes: Vec<u8>) -> Result<Self, PackageError> {
        let checked = firmware_package::check(std::io::Cursor::new(&bytes), bytes.len() as u64)?;
        Ok(Loaded {
            bytes: Arc::new(bytes),
            checked,
        })
    }

    pub fn signer(&self, labels: &[SignerView]) -> Signer {
        signer(&self.checked.signer_key_id().to_hex(), labels)
    }

    /// `"llm-usage-display" 1.2.0`: the device type is the maker's text, so it is quoted
    /// wherever the app's own sentences name it.
    pub fn title(&self) -> String {
        let m = &self.checked.manifest;
        package_title(&m.device_type, &m.firmware_version)
    }

    /// The keys a display trusts after a first install from this package: its `key_set` if it has
    /// one, otherwise the key that signed it.
    pub fn trusted_keys(&self, labels: &[SignerView]) -> Vec<Signer> {
        match &self.checked.manifest.key_set {
            Some(ks) => ks.iter().map(|k| signer(&k.key_id().to_hex(), labels)).collect(),
            None => vec![self.signer(labels)],
        }
    }
}

/// How the app's sentences name a package: `"llm-usage-display" 1.2.0`. The device type is quoted
/// because it is the maker's text; the version can only be numbers, letters, `.` and `-`.
pub fn package_title(device_type: &str, version: &str) -> String {
    format!("\"{device_type}\" {version}")
}

/// The bundle's `firmware` section after a first install from this package: its `key_set` if it
/// has one, otherwise the key that signed it, and its security counter as the starting floor.
pub fn bundle_firmware(p: &CheckedPackage) -> BundleFirmware {
    let trusted_keys = match &p.manifest.key_set {
        Some(ks) => ks.iter().map(|k| k.to_hex()).collect(),
        None => vec![p.signer().to_hex()],
    };
    BundleFirmware {
        trusted_keys,
        security_counter: p.manifest.security_counter,
    }
}

/// One raw write: bytes from the package, at an offset in flash.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Write {
    /// The part's name, or `app` for the app image.
    pub name: String,
    pub offset: u32,
    /// Where the bytes are in the package file.
    pub start: usize,
    pub len: usize,
}

/// What a first install writes, and the board it needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    pub chip: String,
    pub min_flash_bytes: u64,
    /// Each USB-install part in the manifest's order, then the app image.
    pub writes: Vec<Write>,
}

/// Why a package can't be installed over USB.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlanError {
    /// It has no `usb_install`.
    NotForUsb,
    /// An offset past 4 GiB, which no ESP32 flash has.
    OutOfRange,
}

impl Plan {
    /// The writes for a checked package. The checks already proved the regions lie inside
    /// `min_flash_bytes` without overlapping, and that the file is exactly as long as its parts.
    pub fn of(p: &CheckedPackage) -> Result<Plan, PlanError> {
        let usb = p.manifest.usb_install.as_ref().ok_or(PlanError::NotForUsb)?;
        let off = |o: u64| u32::try_from(o).map_err(|_| PlanError::OutOfRange);
        let image_start = firmware_package::limits::HEADER_LEN + p.header.manifest_len as usize;
        let image_len = p.manifest.image_size as usize;
        let mut at = image_start + image_len;
        let mut writes = Vec::new();
        for part in &usb.parts {
            writes.push(Write {
                name: part.name.clone(),
                offset: off(part.offset)?,
                start: at,
                len: part.size as usize,
            });
            at += part.size as usize;
        }
        writes.push(Write {
            name: "app".into(),
            offset: off(usb.app_offset)?,
            start: image_start,
            len: image_len,
        });
        Ok(Plan {
            chip: usb.chip.clone(),
            min_flash_bytes: usb.min_flash_bytes,
            writes,
        })
    }

    /// The total bytes written, for progress.
    pub fn total(&self) -> u64 {
        self.writes.iter().map(|w| w.len as u64).sum()
    }
}

/// What the board's loader said about itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BoardInfo {
    /// Lower case, as in `usb_install.chip`: `esp32s3`.
    pub chip: String,
    pub flash_bytes: u64,
    /// The chip only accepts its maker's signed loader commands (Secure Download Mode).
    pub secure_download: bool,
}

/// A board went wrong, in kinds a person can act on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BoardError {
    /// Nothing on that port, or the port doesn't exist.
    NoBoard(String),
    /// The port is open in another program.
    Busy(String),
    /// Something answered, but not the chip's loader: the board isn't in download mode.
    NotInDownloadMode(String),
    /// The connection broke or a write failed part way.
    Lost(String),
    Other(String),
}

/// A board in its built-in loader (download mode), reached over a serial port. The real one is
/// `flasher::EspBoard`; tests use a fake.
pub trait Board: Send {
    fn connect(&mut self) -> Result<BoardInfo, BoardError>;
    /// Erases the whole flash.
    fn erase_all(&mut self) -> Result<(), BoardError>;
    /// Writes `bytes` at `offset` as they are. `progress` gets the bytes written so far.
    fn write(&mut self, offset: u32, bytes: &[u8], progress: &mut dyn FnMut(u64)) -> Result<(), BoardError>;
    /// Restarts the board into its firmware and lets go of the serial port.
    fn restart(self: Box<Self>) -> Result<(), BoardError>;
}

/// Where an install has got to, for the progress line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    Connecting,
    Erasing,
    Writing {
        name: String,
        /// 1-based.
        index: usize,
        count: usize,
    },
    Restarting,
    /// Asking the hub for setup details and sending them to the display.
    SettingUp,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Progress {
    pub step: Step,
    pub written: u64,
    pub total: u64,
}

/// Shared between the worker, which updates it, and the view, which shows it.
#[derive(Clone)]
pub struct ProgressCell(Arc<Mutex<Progress>>);

impl std::fmt::Debug for ProgressCell {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("ProgressCell").field(&self.get()).finish()
    }
}

impl Default for ProgressCell {
    fn default() -> Self {
        Self(Arc::new(Mutex::new(Progress {
            step: Step::Connecting,
            written: 0,
            total: 0,
        })))
    }
}

impl ProgressCell {
    pub fn get(&self) -> Progress {
        self.0
            .lock()
            .map(|p| p.clone())
            .unwrap_or_else(|p| p.into_inner().clone())
    }

    pub fn set(&self, step: Step) {
        if let Ok(mut p) = self.0.lock() {
            p.step = step;
        }
    }

    fn written(&self, written: u64, total: u64) {
        if let Ok(mut p) = self.0.lock() {
            p.written = written;
            p.total = total;
        }
    }
}

/// Why an install stopped. `erased` says whether the flash had been erased by then: if not,
/// nothing was written and the display is as it was.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InstallError {
    Board { error: BoardError, erased: bool },
    WrongChip { expected: String, found: String },
    FlashTooSmall { needed: u64, found: u64 },
    SecureDownload,
}

impl InstallError {
    pub fn erased(&self) -> bool {
        matches!(self, InstallError::Board { erased: true, .. })
    }
}

/// Installs a package on a board: connect to its loader, check the chip and flash size, erase the
/// whole flash, write each part and then the app image at its offset, and restart into the new
/// firmware, which lets go of the port. Nothing is written before both checks pass. Then `after`
/// runs (provisioning), once the port is free.
pub fn install<T>(
    mut board: Box<dyn Board>,
    loaded: &Loaded,
    plan: &Plan,
    progress: &ProgressCell,
    after: impl FnOnce() -> T,
) -> Result<T, InstallError> {
    let fail = |error: BoardError, erased: bool| InstallError::Board { error, erased };
    progress.set(Step::Connecting);
    let info = board.connect().map_err(|e| fail(e, false))?;
    if !info.chip.eq_ignore_ascii_case(&plan.chip) {
        return Err(InstallError::WrongChip {
            expected: plan.chip.clone(),
            found: info.chip,
        });
    }
    if info.flash_bytes < plan.min_flash_bytes {
        return Err(InstallError::FlashTooSmall {
            needed: plan.min_flash_bytes,
            found: info.flash_bytes,
        });
    }
    // The bootloader and partition table sit below 0x8000, which a locked loader won't write.
    if info.secure_download {
        return Err(InstallError::SecureDownload);
    }
    progress.set(Step::Erasing);
    board.erase_all().map_err(|e| fail(e, true))?;
    let erased = true;
    let total = plan.total();
    let mut before = 0u64;
    progress.written(0, total);
    for (i, w) in plan.writes.iter().enumerate() {
        progress.set(Step::Writing {
            name: w.name.clone(),
            index: i + 1,
            count: plan.writes.len(),
        });
        let bytes = loaded.bytes.get(w.start..w.start + w.len).ok_or_else(|| {
            fail(
                BoardError::Other("the package is shorter than its plan".into()),
                erased,
            )
        })?;
        board
            .write(w.offset, bytes, &mut |n| {
                progress.written(before + n.min(w.len as u64), total)
            })
            .map_err(|e| fail(e, erased))?;
        before += w.len as u64;
        progress.written(before, total);
    }
    progress.set(Step::Restarting);
    board.restart().map_err(|e| fail(e, erased))?;
    progress.set(Step::SettingUp);
    Ok(after())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use firmware_package::{build_and_sign, Release, SigningKey, UsbLayout, UsbPartFile};
    use hub_core::api::DisplayFirmware as HubFirmware;
    use hub_protocol::device::DeviceInfo;
    use hub_protocol::firmware::FirmwareInfo;

    fn ts(s: &str) -> Timestamp {
        Timestamp::parse(s).unwrap()
    }

    fn label(key_id: &str, label: &str) -> SignerView {
        SignerView {
            key_id: key_id.into(),
            fingerprint: fingerprint(key_id),
            label: label.into(),
            updated_at: ts("2026-11-17T09:00:00Z"),
        }
    }

    #[test]
    fn signers_are_named_by_the_app_then_the_owner_never_the_package() {
        let id = "3f9a01c2".repeat(8);
        assert_eq!(fingerprint(&id), "3f9a-01c2-3f9a-01c2-3f9a-01c2-3f9a-01c2");
        assert_eq!(signer(&id, &[]).name(), "Unknown maker");
        assert_eq!(signer(&id, &[label(&id, "Kea Displays")]).name(), "Kea Displays");
        // Built into the app wins over any label on the hub.
        let official = signer_with(&id, &[label(&id, "Kea Displays")], &[id.as_str()]);
        assert_eq!(official.kind, SignerKind::Official);
        assert_eq!(official.name(), "Official Display Hub firmware");
        assert!(OFFICIAL_KEYS.is_empty(), "no official key exists yet");
    }

    fn package(id: &str, version: &str, signer: &str) -> FirmwarePackageView {
        FirmwarePackageView {
            package_id: id.repeat(64),
            device_type: "desk".into(),
            hardware_revisions: vec!["rev-a".into()],
            firmware_version: version.into(),
            security_counter: 2,
            summary: Some(format!("Release {version}")),
            signer_key_id: signer.repeat(64),
            signer_fingerprint: fingerprint(&signer.repeat(64)),
            signer_label: None,
            size: 1000,
            update_size: 900,
            image_size: 700,
            usb_install: true,
            key_set: None,
            damaged: false,
            uploaded_at: ts("2026-11-17T09:00:00Z"),
            uploaded_by_role: "admin".into(),
            uploaded_by: "a".into(),
            uploaded_by_label: None,
            offered_to: vec![],
            evicted: None,
        }
    }

    fn display(keys: Option<Vec<&str>>, version: &str, available: Option<&str>) -> DisplayView {
        DisplayView {
            id: "d".into(),
            name: "Desk".into(),
            enrolled_at: ts("2026-11-17T09:00:00Z"),
            last_seen_at: None,
            device: Some(DeviceInfo {
                device_type: "desk".into(),
                firmware_version: version.into(),
                hardware_revision: "rev-a".into(),
                protocol_version: 1,
                firmware: keys.map(|k| FirmwareInfo {
                    key_ids: k.iter().map(|k| k.repeat(64)).collect(),
                    security_floor: 2,
                    slot_bytes: 1 << 20,
                    attempts_left: None,
                }),
            }),
            pages: None,
            firmware: Some(HubFirmware {
                reported: None,
                offer: None,
                available_package_id: available.map(|a| a.repeat(64)),
                auto_update: false,
            }),
        }
    }

    #[test]
    fn an_update_is_available_only_when_signed_by_a_key_the_display_trusts() {
        let packages = vec![
            package("a", "1.1.0", "k"),
            package("b", "1.2.0", "k"),
            package("c", "1.3.0-beta.1", "k"),
            package("e", "1.4.0", "x"),
        ];
        let d = display(Some(vec!["k"]), "1.2.0", Some("c"));
        let fw = display_firmware(&d, Some(&packages), &[], None);
        let up = fw.available.unwrap();
        assert_eq!(
            (up.version.as_str(), up.pre_release, up.older),
            ("1.3.0-beta.1", true, false)
        );
        assert_eq!(
            fw.signed_by.unwrap().key_id,
            "k".repeat(64),
            "1.2.0 on the hub matches"
        );
        // 1.1.0 is older; 1.4.0 is signed by a key it doesn't trust; 1.2.0 is what it runs.
        let others: Vec<(&str, bool)> = fw.others.iter().map(|c| (c.version.as_str(), c.older)).collect();
        assert_eq!(others, [("1.1.0", true)]);

        // The hub's choice signed by an untrusted key is not shown (the hub would never choose it,
        // but the app chooses for itself).
        let d = display(Some(vec!["k"]), "1.2.0", Some("e"));
        let fw = display_firmware(&d, Some(&packages), &[], None);
        assert_ne!(fw.available.unwrap().package_id, "e".repeat(64));
        assert!(fw.others.iter().all(|o| o.package_id != "e".repeat(64)));
        // Below the floor isn't offered either.
        let mut low = package("f", "9.0.0", "k");
        low.security_counter = 1;
        let fw = display_firmware(&display(Some(vec!["k"]), "1.2.0", None), Some(&[low]), &[], None);
        assert!(fw.others.is_empty());
    }

    fn versions(choices: &[Choice]) -> Vec<&str> {
        choices.iter().map(|c| c.version.as_str()).collect()
    }

    #[test]
    fn the_headline_is_the_newest_full_release_unless_the_display_runs_a_pre_release() {
        let packages = vec![
            package("a", "1.3.0", "k"),
            package("b", "1.4.0-beta.1", "k"),
            package("c", "1.2.5", "k"),
            package("d", "1.0.0", "k"),
        ];
        let fw = |running: &str| {
            display_firmware(
                &display(Some(vec!["k"]), running, None),
                Some(&packages),
                &[],
                None,
            )
        };
        // A full release first, the pre-release under the others.
        let f = fw("1.2.0");
        assert_eq!(f.available.as_ref().unwrap().version, "1.3.0");
        assert_eq!(versions(&f.others), ["1.4.0-beta.1", "1.2.5", "1.0.0"]);
        // Already on a pre-release: the newest of them all.
        let f = fw("1.3.0-beta.2");
        assert_eq!(f.available.as_ref().unwrap().version, "1.4.0-beta.1");
        // No newer full release: the pre-release.
        let f = fw("1.3.0");
        assert_eq!(f.available.as_ref().unwrap().version, "1.4.0-beta.1");
        assert!(f.available.as_ref().unwrap().asks_twice());
        // Nothing newer at all.
        let f = fw("2.0.0");
        assert!(f.available.is_none());
        assert!(f.others.iter().all(|o| o.older));
    }

    #[test]
    fn a_version_that_cant_be_read_makes_every_choice_unsure() {
        let packages = vec![package("a", "1.3.0", "k")];
        let f = display_firmware(
            &display(Some(vec!["k"]), "dev build", None),
            Some(&packages),
            &[],
            None,
        );
        assert!(f.available.is_none());
        let o = &f.others[0];
        assert!(o.unsure && !o.older && o.asks_twice(), "{o:?}");
    }

    #[test]
    fn a_package_too_big_for_the_spare_space_isnt_offered() {
        // The display has 1 MiB spare, which is what the image alone is compared with.
        let mut fits = package("a", "1.3.0", "k");
        fits.image_size = 1 << 20;
        let mut too_big = package("b", "1.4.0", "k");
        too_big.image_size = (1 << 20) + 1;
        let d = display(Some(vec!["k"]), "1.2.0", None);
        assert!(accepts(&d, &fits));
        assert!(!accepts(&d, &too_big));
        let f = display_firmware(&d, Some(&[fits, too_big]), &[], None);
        assert_eq!(f.available.unwrap().version, "1.3.0");
        assert!(f.others.is_empty());
    }

    #[test]
    fn a_damaged_package_is_never_an_update_or_a_choice() {
        // The newest package is damaged on the hub: it shows no key_set, so it would otherwise pass.
        let d = display(Some(vec!["k"]), "1.2.0", None);
        let fine = package("a", "1.3.0", "k");
        let mut damaged = package("b", "1.4.0", "k");
        damaged.damaged = true;
        assert!(accepts(&d, &fine));
        assert!(!accepts(&d, &damaged));
        let f = display_firmware(&d, Some(&[fine, damaged.clone()]), &[], None);
        assert_eq!(f.available.unwrap().version, "1.3.0");
        assert!(f.others.is_empty());
        // Nor the only choice.
        let f = display_firmware(&d, Some(&[damaged]), &[], None);
        assert!(f.available.is_none() && f.others.is_empty());
    }

    #[test]
    fn a_package_that_drops_a_trusted_key_needs_a_higher_counter() {
        // The display trusts k and j at floor 2.
        let d = display(Some(vec!["k", "j"]), "1.4.0", None);
        let mut keeps = package("a", "1.3.0", "k");
        keeps.key_set = Some(vec!["k".repeat(64), "j".repeat(64)]);
        let mut drops = package("b", "1.2.0", "k");
        drops.key_set = Some(vec!["k".repeat(64)]);
        let mut raised = package("c", "1.5.0", "k");
        raised.key_set = Some(vec!["k".repeat(64)]);
        raised.security_counter = 3;
        assert!(accepts(&d, &keeps));
        assert!(
            !accepts(&d, &drops),
            "an older release at the same counter that drops j"
        );
        assert!(accepts(&d, &raised));
        let f = display_firmware(&d, Some(&[keeps, drops, raised]), &[], None);
        assert_eq!(f.available.unwrap().version, "1.5.0");
        let others: Vec<_> = f.others.iter().map(|o| o.version.as_str()).collect();
        assert_eq!(others, ["1.3.0"]);
    }

    #[test]
    fn fingerprints_match_the_package_library() {
        let key = SigningKey::generate().unwrap().0;
        let id = key.public_key().key_id();
        assert_eq!(fingerprint(&id.to_hex()), id.fingerprint());
    }

    #[test]
    fn a_display_out_of_attempts_is_paused() {
        let mut d = display(Some(vec!["k"]), "1.2.0", None);
        let f = display_firmware(&d, Some(&[]), &[], None);
        assert!(!f.paused, "not reported means not paused");
        d.device
            .as_mut()
            .unwrap()
            .firmware
            .as_mut()
            .unwrap()
            .attempts_left = Some(0);
        assert!(display_firmware(&d, Some(&[]), &[], None).paused);
    }

    #[test]
    fn a_package_names_itself_only_in_quotes() {
        let (loaded, _) = usb_package("esp32s3", true);
        assert_eq!(loaded.title(), "\"desk\" 1.0.0");
        assert_eq!(loaded.trusted_keys(&[]).len(), 2);
    }

    #[test]
    fn a_display_with_no_keys_or_old_firmware_says_so() {
        let fw = display_firmware(&display(Some(vec![]), "1.0.0", None), Some(&[]), &[], None);
        assert_eq!(fw.trust, Trust::NoKeys);
        let fw = display_firmware(&display(None, "1.0.0", None), Some(&[]), &[], None);
        assert_eq!(fw.trust, Trust::NotReported);
        let mut d = display(Some(vec!["k"]), "1.0.0", None);
        d.firmware = None;
        assert!(display_firmware(&d, None, &[], None).hub_unaware);
    }

    fn offer(state: &str, reason: Option<&str>, updated: &str) -> FirmwareOfferView {
        FirmwareOfferView {
            offer_id: "0".repeat(32),
            package_id: "b".repeat(64),
            firmware_version: "1.3.0".into(),
            state: state.into(),
            reason: reason.map(Into::into),
            reported_version: None,
            made_at: ts("2026-11-17T09:00:00Z"),
            expires_at: ts("2026-11-18T09:00:00Z"),
            updated_at: ts(updated),
            ended_at: None,
        }
    }

    #[test]
    fn offer_states_map_to_phases() {
        let now = Some(ts("2026-11-17T09:10:00Z"));
        let p = |s, r, u| phase(&offer(s, r, u), now);
        assert_eq!(p("offered", None, "2026-11-17T09:00:00Z"), Phase::Waiting);
        assert_eq!(p("downloading", None, "2026-11-17T09:09:30Z"), Phase::Downloading);
        assert_eq!(p("downloading", None, "2026-11-17T09:05:00Z"), Phase::Restarting);
        assert_eq!(
            p("installed", Some("seen_in_heartbeat"), "2026-11-17T09:05:00Z"),
            Phase::Installed
        );
        assert_eq!(
            p("refused", Some("downgrade"), "2026-11-17T09:05:00Z"),
            Phase::Refused(ReportReason::Downgrade)
        );
        assert_eq!(
            p("failed", Some("gremlins"), "2026-11-17T09:05:00Z"),
            Phase::Failed(ReportReason::Unrecognised)
        );
        assert_eq!(p("rolled_back", None, "2026-11-17T09:05:00Z"), Phase::RolledBack);
        assert_eq!(
            p("cancelled", Some("by_admin"), "2026-11-17T09:05:00Z"),
            Phase::Cancelled("by_admin".into())
        );
        assert_eq!(p("expired", None, "2026-11-17T09:05:00Z"), Phase::Expired);
        assert_eq!(
            p("exploded", None, "2026-11-17T09:05:00Z"),
            Phase::Unknown("exploded".into())
        );
        assert!(Phase::Restarting.is_open() && !Phase::Installed.is_open());
        assert!(Phase::Unknown("x".into()).is_problem() && !Phase::Cancelled(String::new()).is_problem());

        let mut d = display(Some(vec!["k"]), "1.2.0", None);
        assert!(!any_open_offer(std::slice::from_ref(&d), now));
        d.firmware.as_mut().unwrap().offer = Some(offer("offered", None, "2026-11-17T09:00:00Z"));
        assert!(any_open_offer(&[d], now));
    }

    /// A package with a bootloader at 0, a partition table at 0x8000 and the app at 0x10000.
    pub(crate) fn usb_package(chip: &str, key_set: bool) -> (Loaded, SigningKey) {
        let key = SigningKey::generate().unwrap().0;
        let spare = SigningKey::generate().unwrap().0;
        let release = Release {
            device_type: "desk".into(),
            hardware_revisions: vec!["simulated".into()],
            firmware_version: "1.0.0".into(),
            security_counter: 4,
            summary: Some("First release.".into()),
            key_set: key_set.then(|| vec![key.public_key(), spare.public_key()]),
            usb_install: Some(UsbLayout {
                chip: chip.into(),
                min_flash_bytes: 4 * 1024 * 1024,
                app_offset: 0x10000,
                parts: vec![
                    UsbPartFile {
                        name: "bootloader".into(),
                        offset: 0,
                        bytes: vec![1; 300],
                    },
                    UsbPartFile {
                        name: "partition-table".into(),
                        offset: 0x8000,
                        bytes: vec![2; 200],
                    },
                ],
            }),
        };
        let built = build_and_sign(&key, &release, &[9; 5000]).unwrap();
        (Loaded::check(built.bytes).unwrap(), key)
    }

    #[test]
    fn the_plan_writes_each_part_then_the_app_as_raw_bytes_at_its_offset() {
        let (loaded, _) = usb_package("esp32s3", false);
        let plan = Plan::of(&loaded.checked).unwrap();
        assert_eq!(plan.chip, "esp32s3");
        assert_eq!(plan.min_flash_bytes, 4 * 1024 * 1024);
        let at: Vec<(&str, u32, usize)> = plan
            .writes
            .iter()
            .map(|w| (w.name.as_str(), w.offset, w.len))
            .collect();
        assert_eq!(
            at,
            [
                ("bootloader", 0, 300),
                ("partition-table", 0x8000, 200),
                ("app", 0x10000, 5000)
            ]
        );
        let bytes = |w: &Write| &loaded.bytes[w.start..w.start + w.len];
        assert!(bytes(&plan.writes[0]).iter().all(|b| *b == 1));
        assert!(bytes(&plan.writes[1]).iter().all(|b| *b == 2));
        assert!(bytes(&plan.writes[2]).iter().all(|b| *b == 9));
        assert_eq!(plan.total(), 5500);

        // A package without USB-install parts can't be planned.
        let key = SigningKey::generate().unwrap().0;
        let built = build_and_sign(
            &key,
            &Release {
                device_type: "desk".into(),
                hardware_revisions: vec!["r".into()],
                firmware_version: "1.0.0".into(),
                ..Release::default()
            },
            &[1; 10],
        )
        .unwrap();
        assert_eq!(Plan::of(&built.checked), Err(PlanError::NotForUsb));
    }

    #[test]
    fn the_bundle_trusts_the_key_set_or_else_the_signer_and_starts_at_its_counter() {
        let (with, key) = usb_package("esp32s3", true);
        let fw = bundle_firmware(&with.checked);
        assert_eq!(fw.trusted_keys.len(), 2);
        assert_eq!(fw.trusted_keys[0], key.public_key().to_hex());
        assert_eq!(fw.security_counter, 4);
        assert!(fw.problems().is_empty());
        let (without, key) = usb_package("esp32s3", false);
        let fw = bundle_firmware(&without.checked);
        assert_eq!(fw.trusted_keys, [key.public_key().to_hex()]);
        // The simulator, the reference display, builds the same section.
        assert_eq!(
            fw,
            display_simulator::firmware::store::bundle_for(&without.checked)
        );
    }

    /// A board that records what was done to it.
    pub(crate) struct FakeBoard {
        pub info: Result<BoardInfo, BoardError>,
        pub fail_write: Option<BoardError>,
        pub log: Arc<Mutex<Vec<String>>>,
    }

    impl FakeBoard {
        pub(crate) fn new(chip: &str, flash: u64) -> (Self, Arc<Mutex<Vec<String>>>) {
            let log = Arc::new(Mutex::new(Vec::new()));
            (
                FakeBoard {
                    info: Ok(BoardInfo {
                        chip: chip.into(),
                        flash_bytes: flash,
                        secure_download: false,
                    }),
                    fail_write: None,
                    log: log.clone(),
                },
                log,
            )
        }
    }

    impl Board for FakeBoard {
        fn connect(&mut self) -> Result<BoardInfo, BoardError> {
            self.log.lock().unwrap().push("connect".into());
            self.info.clone()
        }
        fn erase_all(&mut self) -> Result<(), BoardError> {
            self.log.lock().unwrap().push("erase".into());
            Ok(())
        }
        fn write(
            &mut self,
            offset: u32,
            bytes: &[u8],
            progress: &mut dyn FnMut(u64),
        ) -> Result<(), BoardError> {
            self.log
                .lock()
                .unwrap()
                .push(format!("write {offset:#x} {} {}", bytes.len(), bytes[0]));
            if let Some(e) = self.fail_write.take() {
                return Err(e);
            }
            progress(bytes.len() as u64);
            Ok(())
        }
        fn restart(self: Box<Self>) -> Result<(), BoardError> {
            self.log.lock().unwrap().push("restart".into());
            Ok(())
        }
    }

    impl Drop for FakeBoard {
        fn drop(&mut self) {
            self.log.lock().unwrap().push("port closed".into());
        }
    }

    #[test]
    fn installing_erases_writes_parts_then_the_app_restarts_and_lets_go_before_provisioning() {
        let (loaded, _) = usb_package("esp32s3", false);
        let plan = Plan::of(&loaded.checked).unwrap();
        let (board, log) = FakeBoard::new("esp32s3", 16 * 1024 * 1024);
        let progress = ProgressCell::default();
        let l = log.clone();
        let r = install(Box::new(board), &loaded, &plan, &progress, || {
            l.lock().unwrap().push("provision".into());
            7
        });
        assert_eq!(r, Ok(7));
        assert_eq!(
            *log.lock().unwrap(),
            [
                "connect",
                "erase",
                "write 0x0 300 1",
                "write 0x8000 200 2",
                "write 0x10000 5000 9",
                "restart",
                "port closed",
                "provision"
            ]
        );
        let p = progress.get();
        assert_eq!((p.step, p.written, p.total), (Step::SettingUp, 5500, 5500));
    }

    #[test]
    fn the_wrong_chip_or_too_little_flash_stops_before_anything_is_written() {
        let (loaded, _) = usb_package("esp32s3", false);
        let plan = Plan::of(&loaded.checked).unwrap();
        let run =
            |board: FakeBoard| install(Box::new(board), &loaded, &plan, &ProgressCell::default(), || ());

        let (board, log) = FakeBoard::new("esp32c3", 16 << 20);
        assert_eq!(
            run(board),
            Err(InstallError::WrongChip {
                expected: "esp32s3".into(),
                found: "esp32c3".into()
            })
        );
        assert_eq!(*log.lock().unwrap(), ["connect", "port closed"]);

        let (board, log) = FakeBoard::new("ESP32S3", 2 << 20);
        assert_eq!(
            run(board),
            Err(InstallError::FlashTooSmall {
                needed: 4 << 20,
                found: 2 << 20
            })
        );
        assert_eq!(*log.lock().unwrap(), ["connect", "port closed"]);

        let (mut board, log) = FakeBoard::new("esp32s3", 16 << 20);
        board.info = Ok(BoardInfo {
            secure_download: true,
            ..board.info.clone().unwrap()
        });
        assert_eq!(run(board), Err(InstallError::SecureDownload));
        assert_eq!(*log.lock().unwrap(), ["connect", "port closed"]);

        let (mut board, _) = FakeBoard::new("esp32s3", 16 << 20);
        board.info = Err(BoardError::NotInDownloadMode("no sync".into()));
        let e = run(board).unwrap_err();
        assert!(!e.erased());
        assert_eq!(
            e,
            InstallError::Board {
                error: BoardError::NotInDownloadMode("no sync".into()),
                erased: false
            }
        );
    }

    #[test]
    fn a_write_that_fails_after_the_erase_says_the_display_was_erased_and_never_provisions() {
        let (loaded, _) = usb_package("esp32s3", false);
        let plan = Plan::of(&loaded.checked).unwrap();
        let (mut board, log) = FakeBoard::new("esp32s3", 16 << 20);
        board.fail_write = Some(BoardError::Lost("cable".into()));
        let e = install(Box::new(board), &loaded, &plan, &ProgressCell::default(), || {
            panic!("no provisioning after a failed write")
        })
        .unwrap_err();
        assert!(e.erased());
        assert_eq!(
            *log.lock().unwrap(),
            ["connect", "erase", "write 0x0 300 1", "port closed"]
        );
    }
}
