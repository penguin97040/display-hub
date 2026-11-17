//! What a display keeps for signed updates, as files in `<state>/firmware/`
//! (`docs/firmware-updates.md`, "What it stores"):
//!
//! | File | Stands for |
//! |---|---|
//! | `board.json` | The board itself: the size of each app partition, and the version its factory image reports. |
//! | `slot-a.bin`, `slot-b.bin` | The two app partitions. Absent until something is written there. |
//! | `otadata.json` | ESP-IDF's OTA data: which slot boots, which is running, and each slot's state and version. |
//! | `keys.json` | The trusted keys and the security floor: an NVS namespace of their own. |
//! | `update.json` | The update record: the last offer acted on, any attempt in progress, what is waiting for a trial boot to succeed, the last result, and the failure count. |
//! | `switches.json` | Test switches, for trying failures by hand. A real display has none. |
//!
//! A factory reset (`paired::reset`) erases the hub details beside this folder and leaves all of it.
//! Only [`install_firmware`], the app's first install over USB, wipes it.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use firmware_package::{CheckedPackage, PublicKey};
use hub_protocol::device::DeviceInfo;
use hub_protocol::firmware::{BundleFirmware, FirmwareInfo, ReportReason, ReportState};
use hub_protocol::Timestamp;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::paired::write_private;

/// What every simulated display reports as its device type and hardware revision.
pub const DEVICE_TYPE: &str = "llm-usage-display";
pub const HARDWARE_REVISION: &str = "simulated";
/// The version the factory image reports, until a signed update or a first install replaces it.
pub const FACTORY_VERSION: &str = "0.0.0-simulator";
/// The size of each app partition, unless set otherwise: 3 MiB, as on a 16 MB ESP32-S3 board.
pub const DEFAULT_SLOT_BYTES: u64 = 3 * 1024 * 1024;

pub fn folder(state: &Path) -> PathBuf {
    state.join("firmware")
}

fn file(state: &Path, name: &str) -> PathBuf {
    folder(state).join(name)
}

/// Reads a JSON file, or `None` if it isn't there.
fn read<T: DeserializeOwned>(path: &Path) -> std::io::Result<Option<T>> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| std::io::Error::other(format!("{} is damaged: {e}", path.display()))),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

fn write<T: Serialize>(path: &Path, value: &T) -> std::io::Result<()> {
    let mut text = serde_json::to_vec_pretty(value).expect("serialises");
    text.push(b'\n');
    write_private(path, &text)
}

/// The simulated board.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Board {
    /// The size of each app partition. Updates are written to the spare one.
    pub slot_bytes: u64,
    /// What the factory image reports as its `firmware_version`.
    pub factory_version: String,
}

impl Default for Board {
    fn default() -> Self {
        Self {
            slot_bytes: DEFAULT_SLOT_BYTES,
            factory_version: FACTORY_VERSION.into(),
        }
    }
}

pub fn board(state: &Path) -> std::io::Result<Board> {
    Ok(read(&file(state, "board.json"))?.unwrap_or_default())
}

pub fn set_board(state: &Path, board: &Board) -> std::io::Result<()> {
    write(&file(state, "board.json"), board)
}

/// The trusted-key store: 0 to 3 public keys (130 hex digits each) and the security floor.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyStore {
    pub trusted_keys: Vec<String>,
    pub security_floor: u32,
}

impl KeyStore {
    /// The stored keys. A stored key that can't be read is left out, so it can never match.
    pub fn keys(&self) -> Vec<PublicKey> {
        self.trusted_keys
            .iter()
            .filter_map(|k| PublicKey::from_hex(k))
            .collect()
    }

    pub fn key_ids(&self) -> Vec<String> {
        self.keys().iter().map(|k| k.key_id().to_hex()).collect()
    }

    /// Whether `keys` is the same set as the stored keys, in any order.
    pub fn same_set(&self, keys: &[String]) -> bool {
        let mut mine = self.trusted_keys.clone();
        let mut theirs = keys.to_vec();
        mine.sort();
        theirs.sort();
        mine == theirs
    }
}

pub fn keys(state: &Path) -> std::io::Result<KeyStore> {
    Ok(read(&file(state, "keys.json"))?.unwrap_or_default())
}

pub fn set_keys(state: &Path, keys: &KeyStore) -> std::io::Result<()> {
    write(&file(state, "keys.json"), keys)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SlotName {
    A,
    B,
}

impl SlotName {
    pub fn other(self) -> Self {
        match self {
            Self::A => Self::B,
            Self::B => Self::A,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::A => "a",
            Self::B => "b",
        }
    }
}

/// A slot's state, as ESP-IDF's app rollback keeps it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SlotState {
    /// Nothing bootable: never written, erased for writing, or abandoned.
    Empty,
    /// Written and set to boot, not yet started (`ESP_OTA_IMG_NEW`).
    New,
    /// Started once, not yet marked good (`ESP_OTA_IMG_PENDING_VERIFY`). A reset now rolls back.
    PendingVerify,
    /// Marked good, or the image the board shipped with (`ESP_OTA_IMG_VALID`).
    Valid,
    /// Failed its trial boot (`ESP_OTA_IMG_INVALID`). Never booted again.
    Invalid,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Slot {
    pub state: SlotState,
    /// The version its image reports. `None` for the factory image, which reports the board's.
    pub firmware_version: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OtaData {
    /// The slot the bootloader starts next.
    pub boot: SlotName,
    /// The slot that is running now.
    pub running: SlotName,
    pub a: Slot,
    pub b: Slot,
}

impl Default for OtaData {
    /// A board as it leaves the factory: its image in slot A, slot B empty.
    fn default() -> Self {
        Self {
            boot: SlotName::A,
            running: SlotName::A,
            a: Slot {
                state: SlotState::Valid,
                firmware_version: None,
            },
            b: Slot {
                state: SlotState::Empty,
                firmware_version: None,
            },
        }
    }
}

impl OtaData {
    pub fn slot(&self, name: SlotName) -> &Slot {
        match name {
            SlotName::A => &self.a,
            SlotName::B => &self.b,
        }
    }

    pub fn slot_mut(&mut self, name: SlotName) -> &mut Slot {
        match name {
            SlotName::A => &mut self.a,
            SlotName::B => &mut self.b,
        }
    }

    /// The slot updates are written to.
    pub fn spare(&self) -> SlotName {
        self.running.other()
    }
}

pub fn ota(state: &Path) -> std::io::Result<OtaData> {
    Ok(read(&file(state, "otadata.json"))?.unwrap_or_default())
}

pub fn set_ota(state: &Path, ota: &OtaData) -> std::io::Result<()> {
    write(&file(state, "otadata.json"), ota)
}

pub fn slot_image(state: &Path, slot: SlotName) -> PathBuf {
    file(state, &format!("slot-{}.bin", slot.as_str()))
}

/// The version the running firmware reports.
pub fn running_version(state: &Path) -> std::io::Result<String> {
    let ota = ota(state)?;
    Ok(match &ota.slot(ota.running).firmware_version {
        Some(v) => v.clone(),
        None => board(state)?.factory_version,
    })
}

/// An attempt that has begun: the offer id is saved before the download is requested, so a restart
/// or power cut part-way still knows about it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attempt {
    pub offer_id: String,
    pub started_at: Timestamp,
}

/// An installed image waiting for its trial boot to succeed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pending {
    pub offer_id: String,
    /// The slot it was written to.
    pub slot: SlotName,
    pub firmware_version: String,
    pub security_counter: u32,
    /// The keys to trust once it is marked good, as hex.
    pub key_set: Option<Vec<String>>,
}

/// The final result of an offer, and whether the hub has answered the report of it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Outcome {
    pub offer_id: String,
    pub state: ReportState,
    pub reason: Option<ReportReason>,
    /// The hub answered the report (with anything at all). Until then it is sent again.
    pub answered: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    /// The last offer acted on. Seeing it again does nothing: one attempt per offer.
    pub last_offer_id: Option<String>,
    pub attempt: Option<Attempt>,
    pub pending: Option<Pending>,
    pub last_result: Option<Outcome>,
    /// When the last attempt began, for the waits between attempts.
    pub last_attempt_at: Option<Timestamp>,
    /// Attempts in a row that didn't end installed.
    pub failures_in_row: u32,
}

pub fn record(state: &Path) -> std::io::Result<Record> {
    Ok(read(&file(state, "update.json"))?.unwrap_or_default())
}

pub fn set_record(state: &Path, record: &Record) -> std::io::Result<()> {
    write(&file(state, "update.json"), record)
}

/// How the next firmware to start behaves, for trying the trial boot's failures.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum NextStart {
    /// It starts and checks in as usual.
    #[default]
    Normal,
    /// It crashes before checking in, so the board resets and rolls back.
    Fails,
    /// It starts, but every check-in fails, so it rolls back after five minutes.
    NoHub,
}

/// Test switches. Tests set them in memory; `switches.json` holds them for trying things by hand.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Switches {
    /// Applies to the next trial boot, then returns to normal.
    #[serde(default)]
    pub next_start: NextStart,
    /// The power goes off once this many image bytes have been written. Applies once.
    #[serde(default)]
    pub power_cut_after_image_bytes: Option<u64>,
    /// Writing the spare partition fails (`flash`). Applies once.
    #[serde(default)]
    pub flash_fails: bool,
}

pub fn switches(state: &Path) -> std::io::Result<Switches> {
    Ok(read(&file(state, "switches.json"))?.unwrap_or_default())
}

pub fn set_switches(state: &Path, switches: &Switches) -> std::io::Result<()> {
    if *switches == Switches::default() {
        match std::fs::remove_file(file(state, "switches.json")) {
            Err(e) if e.kind() != ErrorKind::NotFound => return Err(e),
            _ => return Ok(()),
        }
    }
    write(&file(state, "switches.json"), switches)
}

/// What the display says about itself, in enrolment, heartbeats and the USB `hello`.
pub fn device_info(state: &Path) -> std::io::Result<DeviceInfo> {
    let keys = keys(state)?;
    Ok(DeviceInfo {
        device_type: DEVICE_TYPE.into(),
        firmware_version: running_version(state)?,
        hardware_revision: HARDWARE_REVISION.into(),
        protocol_version: hub_protocol::PROTOCOL_VERSION,
        firmware: Some(FirmwareInfo {
            key_ids: keys.key_ids(),
            security_floor: keys.security_floor,
            slot_bytes: board(state)?.slot_bytes,
        }),
    })
}

/// The key store once new firmware is marked good: the floor raised to its security counter if that
/// is higher, and its `key_set`, if it had one, in place of the trusted keys.
pub fn after_good(store: &KeyStore, security_counter: u32, key_set: Option<&[String]>) -> KeyStore {
    KeyStore {
        trusted_keys: key_set.map_or_else(|| store.trusted_keys.clone(), <[String]>::to_vec),
        security_floor: store.security_floor.max(security_counter),
    }
}

/// Why a provisioning bundle's firmware section was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeysLocked;

impl std::fmt::Display for KeysLocked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(
            "this display already trusts a different set of firmware keys; only Install firmware can change them",
        )
    }
}

/// What a provisioning bundle's `firmware` section does to the key store ("What each action may
/// change"): a display with keys keeps them and its floor, and refuses a bundle naming a different
/// set; a display with none takes the bundle's keys and floor, if it has any. `Ok(None)` means
/// nothing changes.
pub fn bundle_keys(
    store: &KeyStore,
    bundle: Option<&BundleFirmware>,
) -> Result<Option<KeyStore>, KeysLocked> {
    match bundle {
        None => Ok(None),
        Some(b) if !store.trusted_keys.is_empty() => {
            if store.same_set(&b.trusted_keys) {
                Ok(None)
            } else {
                Err(KeysLocked)
            }
        }
        Some(b) => Ok(Some(KeyStore {
            trusted_keys: b.trusted_keys.clone(),
            security_floor: b.security_counter,
        })),
    }
}

/// Applies a provisioning bundle's `firmware` section to the display in `state`.
pub fn accept_bundle(
    state: &Path,
    bundle: Option<&BundleFirmware>,
) -> std::io::Result<Result<(), KeysLocked>> {
    match bundle_keys(&keys(state)?, bundle) {
        Ok(Some(new)) => set_keys(state, &new).map(Ok),
        Ok(None) => Ok(Ok(())),
        Err(e) => Ok(Err(e)),
    }
}

/// The firmware section the app puts in the bundle after a first install from `package`: its
/// `key_set` if it has one, otherwise the key that signed it, and its security counter as the floor.
pub fn bundle_for(package: &CheckedPackage) -> BundleFirmware {
    let keys = match &package.manifest.key_set {
        Some(ks) => ks.iter().map(PublicKey::to_hex).collect(),
        None => vec![package.signer().to_hex()],
    };
    BundleFirmware {
        trusted_keys: keys,
        security_counter: package.manifest.security_counter,
    }
}

/// The files in [`folder`] that stand for flash, which a first install erases. `board.json` is the
/// hardware, so it stays.
const FLASH_FILES: [&str; 6] = [
    "slot-a.bin",
    "slot-b.bin",
    "otadata.json",
    "keys.json",
    "update.json",
    "switches.json",
];

/// **Install firmware**, the app's first install over USB: the whole flash is erased (hub details,
/// Wi-Fi, token, saved pages, trusted keys, floor and update record alike), then the package's image
/// is written to the first app slot, which boots. The board's description is kept: it is the
/// hardware, not what is in flash.
///
/// The keys and floor are then set by the provisioning bundle the app sends next (see
/// [`bundle_for`]), which a display with no keys accepts.
pub fn install_firmware(state: &Path, package: &CheckedPackage, bytes: &[u8]) -> std::io::Result<()> {
    let board = board(state)?;
    let image_start = firmware_package::limits::HEADER_LEN + package.header.manifest_len as usize;
    let image = bytes
        .get(image_start..image_start + package.manifest.image_size as usize)
        .ok_or_else(|| std::io::Error::other("the package is shorter than its manifest says"))?;
    if package.manifest.image_size > board.slot_bytes {
        return Err(std::io::Error::other(format!(
            "the image is {} bytes, larger than this board's {}-byte app partitions",
            package.manifest.image_size, board.slot_bytes
        )));
    }
    // Only the files a display keeps are erased, never anything else in the folder it was given.
    crate::paired::reset(state)?;
    for name in FLASH_FILES {
        match std::fs::remove_file(file(state, name)) {
            Err(e) if e.kind() != ErrorKind::NotFound => return Err(e),
            _ => {}
        }
    }
    set_board(state, &board)?;
    write_private(&slot_image(state, SlotName::A), image)?;
    set_ota(
        state,
        &OtaData {
            a: Slot {
                state: SlotState::Valid,
                firmware_version: Some(package.manifest.firmware_version.clone()),
            },
            ..OtaData::default()
        },
    )
}
