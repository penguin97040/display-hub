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

/// The fingerprint of a key id: its first 32 hex digits in groups of four. An id that isn't hex is
/// shown as it is, cut to that length.
pub fn fingerprint(key_id: &str) -> String {
    let digits: Vec<char> = key_id.chars().take(32).collect();
    digits
        .chunks(4)
        .map(|g| g.iter().collect::<String>())
        .collect::<Vec<_>>()
        .join("-")
}

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

/// A `MAJOR.MINOR.PATCH[-pre]` version, as the manifest rules allow.
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

/// Compares versions as the doc says: three numbers, and with those equal, a version with a
/// pre-release tag is older than one without. Two tags compare part by part, numbers as numbers.
/// `None` if either isn't a version. The hub's `compare_versions` does the same (a test keeps them
/// together).
pub fn compare_versions(a: &str, b: &str) -> Option<Ordering> {
    let (a, b) = (parse_version(a)?, parse_version(b)?);
    Some(a.numbers.cmp(&b.numbers).then_with(|| match (a.pre, b.pre) {
        (None, None) => Ordering::Equal,
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (Some(x), Some(y)) => {
            let (mut xs, mut ys) = (x.split('.'), y.split('.'));
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

/// Whether a version has a pre-release tag (`1.3.0-beta.1`).
pub fn is_pre_release(v: &str) -> bool {
    parse_version(v).is_some_and(|v| v.pre.is_some())
}

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
    pub summary: Option<String>,
    /// Older than what the display runs, or a pre-release: Update asks twice.
    pub older: bool,
    pub pre_release: bool,
}

impl Choice {
    fn of(p: &FirmwarePackageView, running: Option<&str>, labels: &[SignerView]) -> Self {
        Choice {
            package_id: p.package_id.clone(),
            version: p.firmware_version.clone(),
            signer: signer(&p.signer_key_id, labels),
            summary: p.summary.clone(),
            older: running
                .and_then(|r| compare_versions(&p.firmware_version, r))
                .is_some_and(|o| o == Ordering::Less),
            pre_release: is_pre_release(&p.firmware_version),
        }
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
    /// The hub's best update for it, signed by a key it trusts.
    pub available: Option<Choice>,
    /// Other packages it would accept, newest first, for going back or trying a pre-release.
    pub others: Vec<Choice>,
    /// Its open or most recent offer.
    pub offer: Option<(FirmwareOfferView, Phase)>,
    /// The hub doesn't do signed updates (an older hub).
    pub hub_unaware: bool,
}

/// Whether a package passes the checks the app can make for a display, by its last report: the
/// same device type and hardware revision, a key it trusts, and at or above its floor. The hub and
/// the display check again.
pub fn accepts(d: &DisplayView, p: &FirmwarePackageView) -> bool {
    let Some(info) = &d.device else { return false };
    let Some(fw) = &info.firmware else { return false };
    p.device_type == info.device_type
        && p.hardware_revisions.contains(&info.hardware_revision)
        && fw.key_ids.contains(&p.signer_key_id)
        && p.security_counter >= fw.security_floor
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
    let trust = match info.and_then(|i| i.firmware.as_ref()) {
        None => Trust::NotReported,
        Some(f) if f.key_ids.is_empty() => Trust::NoKeys,
        Some(f) => Trust::Keys(f.key_ids.iter().map(|k| signer(k, labels)).collect()),
    };
    let packages = packages.unwrap_or_default();
    let running = version.as_deref();
    let signed_by = info.and_then(|i| {
        packages
            .iter()
            .find(|p| {
                accepts(d, p)
                    && compare_versions(&p.firmware_version, &i.firmware_version) == Some(Ordering::Equal)
            })
            .map(|p| signer(&p.signer_key_id, labels))
    });
    let fw = d.firmware.as_ref();
    // The hub's choice, shown only when it is signed by a key the display reports trusting.
    let available = fw
        .and_then(|f| f.available_package_id.as_deref())
        .and_then(|id| packages.iter().find(|p| p.package_id == id))
        .filter(|p| accepts(d, p))
        .map(|p| Choice::of(p, running, labels));
    let mut others: Vec<&FirmwarePackageView> = packages
        .iter()
        .filter(|p| accepts(d, p))
        .filter(|p| Some(&p.package_id) != available.as_ref().map(|a| &a.package_id))
        .filter(|p| running.is_none_or(|r| compare_versions(&p.firmware_version, r) != Some(Ordering::Equal)))
        .collect();
    others.sort_by(|a, b| {
        compare_versions(&b.firmware_version, &a.firmware_version).unwrap_or(Ordering::Equal)
    });
    let others = others
        .into_iter()
        .map(|p| Choice::of(p, running, labels))
        .collect();
    DisplayFirmware {
        version: version.clone(),
        signed_by,
        trust,
        available,
        others,
        offer: fw.and_then(|f| f.offer.clone()).map(|o| {
            let ph = phase(&o, now);
            (o, ph)
        }),
        hub_unaware: fw.is_none(),
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

    /// "llm-usage-display 1.2.0".
    pub fn title(&self) -> String {
        let m = &self.checked.manifest;
        format!("{} {}", m.device_type, m.firmware_version)
    }
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

    #[test]
    fn versions_compare_as_the_hub_compares_them() {
        use Ordering::*;
        let cases = [
            ("1.2.0", "1.10.0", Some(Less)),
            ("1.2.0", "1.2.0", Some(Equal)),
            ("2.0.0", "1.99.99", Some(Greater)),
            ("1.3.0-beta.1", "1.3.0", Some(Less)),
            ("1.3.0-beta.2", "1.3.0-beta.10", Some(Less)),
            ("1.3.0-beta", "1.3.0-beta.1", Some(Less)),
            ("1.3.0-1", "1.3.0-alpha", Some(Less)),
            ("0.0.0-simulator", "0.0.1", Some(Less)),
            ("1.2", "1.0.0", None),
            ("v1.2.3", "1.0.0", None),
            ("1.2.3-", "1.0.0", None),
        ];
        for (a, b, o) in cases {
            assert_eq!(compare_versions(a, b), o, "{a} {b}");
            assert_eq!(
                hubd::firmware::compare_versions(a, b),
                o,
                "the hub agrees: {a} {b}"
            );
        }
        assert!(is_pre_release("1.3.0-beta.1"));
        assert!(!is_pre_release("1.3.0"));
        assert!(!is_pre_release("nonsense"));
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
            usb_install: true,
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
        // but the app checks).
        let d = display(Some(vec!["k"]), "1.2.0", Some("e"));
        assert!(display_firmware(&d, Some(&packages), &[], None)
            .available
            .is_none());
        // Below the floor isn't offered either.
        let mut low = package("f", "9.0.0", "k");
        low.security_counter = 1;
        let fw = display_firmware(&display(Some(vec!["k"]), "1.2.0", None), Some(&[low]), &[], None);
        assert!(fw.others.is_empty());
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
