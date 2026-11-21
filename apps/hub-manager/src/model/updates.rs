//! The model's firmware parts (`docs/firmware-updates.md`, "The app's part"): the packages on the
//! hub, signer names, publishers, Update on a display card, and Install firmware over USB. The rules
//! themselves are in `crate::firmware`; this keeps the state and turns presses into operations.

use std::path::PathBuf;
use std::time::Instant;

use hub_client::usb::{UsbDisplay, UsbError};
use hub_client::ClientError;
use hub_core::api::{FirmwarePackageView, PublisherInvite, PublisherView, SignerView};
use hub_core::model::clean_title;
use hub_protocol::firmware::BundleFirmware;
use hub_protocol::provisioning::{Bundle, Wifi};
use hub_protocol::Timestamp;
use zeroize::Zeroizing;

use super::{minutes, wifi_of, Busy, Confirm, EnrolStage, Model, Op, PackageSays, Refresh};
use crate::field::Field;
use crate::firmware::{
    self, bundle_firmware, display_firmware, package_title, Bundled, DisplayFirmware, Loaded, Plan, Progress,
    ProgressCell, Signer, SignerKind,
};
use crate::guidance::{signer_phrase, Guidance, Next, Tone};

/// How often the open app checks the hub while a display has an update on offer.
pub const FIRMWARE_WATCH_EVERY: std::time::Duration = std::time::Duration::from_secs(15);

/// The firmware lists from the hub. `None` in [`Refresh`] when the hub doesn't have them (an older
/// hub, without signed updates).
#[derive(Clone, Debug, Default)]
pub struct Shelf {
    pub packages: Vec<FirmwarePackageView>,
    pub signers: Vec<SignerView>,
    pub publishers: Vec<PublisherView>,
}

/// Where a package for a USB install comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PackageSource {
    File(PathBuf),
    /// One the hub holds, by package id.
    Hub(String),
}

/// Everything a first install needs, for the worker. `Debug` never shows the Wi-Fi password.
pub struct InstallJob {
    pub port: String,
    pub loaded: Loaded,
    pub plan: Plan,
    /// The display's name on the hub.
    pub name: String,
    pub hosts: Vec<String>,
    pub hub_port: u16,
    pub wifi: Option<Wifi>,
    pub firmware: BundleFirmware,
    /// The owner's name for the signing key, saved on the hub once the install worked.
    pub label: Option<(String, String)>,
    pub progress: ProgressCell,
}

impl std::fmt::Debug for InstallJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InstallJob")
            .field("port", &self.port)
            .field("loaded", &self.loaded)
            .field("name", &self.name)
            .field("wifi", &self.wifi.as_ref().map(|w| &w.ssid))
            .finish_non_exhaustive()
    }
}

/// What happened after the flash was written and the board restarted.
#[derive(Debug)]
pub enum AfterFlash {
    /// The display took the setup details, with the package's keys.
    Sent(UsbDisplay),
    /// The hub couldn't make setup details.
    Hub(ClientError),
    /// The display didn't take them. The bundle is kept, so they can be sent again.
    Usb { error: UsbError, bundle: Box<Bundle> },
}

impl AfterFlash {
    /// Whether the hub could be reached after the flash: only a failure to reach it at all says it
    /// couldn't.
    pub fn hub_reachable(&self) -> bool {
        !matches!(self, AfterFlash::Hub(e) if e.is_connection())
    }
}

/// What became of the owner's name for the maker's key, after an install.
#[derive(Debug, Default)]
pub enum MakerName {
    /// There was no name to save, or it was saved.
    #[default]
    Done,
    /// The hub refused it, or the request failed.
    Failed(ClientError),
    /// Not tried: the hub couldn't be reached after the install.
    Unreachable,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InstallStage {
    /// Choose a package.
    Choose,
    /// A package is checked: fill in the details and choose the port.
    Ready,
    Running {
        port: String,
    },
    Done {
        name: String,
        port: String,
    },
}

/// Install firmware over USB.
pub struct InstallFlow {
    pub open: bool,
    pub stage: InstallStage,
    pub loaded: Option<Loaded>,
    pub name: Field,
    pub ssid: Field,
    pub password: Field,
    /// The owner's name for an unknown signer, optional.
    pub maker: Field,
    pub other_port: Field,
    pub progress: ProgressCell,
    /// Whether the ports have been looked for since a package was chosen.
    pub scanned: bool,
    /// The displays there were when the install started, to notice the new one.
    pub known: Vec<String>,
}

impl Default for InstallFlow {
    fn default() -> Self {
        let mut name = Field::new(48);
        name.set("Desk");
        Self {
            open: false,
            stage: InstallStage::Choose,
            loaded: None,
            name,
            ssid: Field::new(32),
            password: Field::secret(63),
            maker: Field::new(48),
            other_port: Field::new(256),
            progress: ProgressCell::default(),
            scanned: false,
            known: Vec::new(),
        }
    }
}

/// Keys from an install whose setup details weren't sent, waiting for Add a display. They go only
/// to the port the firmware was installed through, and only while the owner leaves them in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingKeys {
    pub firmware: BundleFirmware,
    /// The port the firmware was installed through.
    pub port: String,
    /// The keys, as people see them.
    pub keys: Vec<Signer>,
    /// The owner hasn't chosen to leave them out.
    pub include: bool,
}

/// The owner's name for a key, being edited.
pub struct LabelEdit {
    pub key_id: String,
    pub field: Field,
}

/// The firmware screen's own state.
pub struct FirmwareUi {
    /// The official firmware package that came with the app, if one did and it checked out.
    pub bundled: Option<Bundled>,
    pub install: InstallFlow,
    pub label_edit: Option<LabelEdit>,
    pub publisher_label: Field,
    /// The `dh1.` string for `hub-firmware login`. A secret until used or expired.
    pub publisher_string: Option<Zeroizing<String>>,
    pub publisher_expires: Option<Timestamp>,
}

impl Default for FirmwareUi {
    fn default() -> Self {
        Self {
            bundled: None,
            install: InstallFlow::default(),
            label_edit: None,
            publisher_label: Field::new(48),
            publisher_string: None,
            publisher_expires: None,
        }
    }
}

/// One package, as the Firmware screen lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackageRow {
    pub id: String,
    pub device_type: String,
    pub version: String,
    /// The hardware it is built for, as the maker names it.
    pub hardware_revisions: Vec<String>,
    pub signer: Signer,
    pub summary: Option<String>,
    pub size: u64,
    pub usb_install: bool,
    /// The hub couldn't read what it holds about it: it is offered to no display and not used for
    /// an install over USB, until it is removed and added again.
    pub damaged: bool,
    /// The names of the displays it is on offer to.
    pub offered_to: Vec<String>,
    pub uploaded_at: Timestamp,
}

impl PackageRow {
    /// Whether the Firmware screen offers it for an install over USB.
    pub fn for_usb_install(&self) -> bool {
        self.usb_install && !self.damaged
    }
}

/// One signing key the app knows of, for naming.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyRow {
    pub signer: Signer,
    pub packages: usize,
    /// The displays that trust it.
    pub trusted_by: Vec<String>,
    /// The owner has named it.
    pub labelled: bool,
}

impl Model {
    pub fn shelf(&self) -> Option<&Shelf> {
        self.last.as_ref()?.firmware.as_ref()
    }

    pub fn firmware_packages(&self) -> &[FirmwarePackageView] {
        self.shelf().map(|s| s.packages.as_slice()).unwrap_or_default()
    }

    pub fn signer_labels(&self) -> &[SignerView] {
        self.shelf().map(|s| s.signers.as_slice()).unwrap_or_default()
    }

    pub fn publishers(&self) -> &[PublisherView] {
        self.shelf().map(|s| s.publishers.as_slice()).unwrap_or_default()
    }

    pub fn signer(&self, key_id: &str) -> Signer {
        firmware::signer(key_id, self.signer_labels())
    }

    /// A display's name on the hub, by id.
    pub fn display_name(&self, id: &str) -> String {
        self.displays()
            .iter()
            .find(|d| d.id == id)
            .map_or_else(|| "a removed display".into(), |d| d.name.clone())
    }

    pub fn package_rows(&self) -> Vec<PackageRow> {
        let mut rows: Vec<PackageRow> = self
            .firmware_packages()
            .iter()
            .map(|p| PackageRow {
                id: p.package_id.clone(),
                device_type: p.device_type.clone(),
                version: p.firmware_version.clone(),
                hardware_revisions: p.hardware_revisions.clone(),
                signer: self.signer(&p.signer_key_id),
                summary: p.summary.clone(),
                size: p.size,
                usb_install: p.usb_install,
                damaged: p.damaged,
                offered_to: p.offered_to.iter().map(|d| self.display_name(d)).collect(),
                uploaded_at: p.uploaded_at,
            })
            .collect();
        // By device type, newest first.
        rows.sort_by(|a, b| {
            a.device_type
                .cmp(&b.device_type)
                .then_with(|| firmware::order_versions(&b.version, &a.version))
        });
        rows
    }

    /// Every key the app knows of: from packages, from what displays trust, and from labels.
    pub fn key_rows(&self) -> Vec<KeyRow> {
        let mut ids: Vec<String> = Vec::new();
        let mut add = |id: &str| {
            if !ids.iter().any(|x| x == id) {
                ids.push(id.to_string());
            }
        };
        for p in self.firmware_packages() {
            add(&p.signer_key_id);
        }
        for d in self.displays() {
            for k in d
                .device
                .as_ref()
                .and_then(|i| i.firmware.as_ref())
                .map(|f| f.key_ids.as_slice())
                .unwrap_or_default()
            {
                add(k);
            }
        }
        for l in self.signer_labels() {
            add(&l.key_id);
        }
        ids.into_iter()
            .map(|id| KeyRow {
                packages: self
                    .firmware_packages()
                    .iter()
                    .filter(|p| p.signer_key_id == id)
                    .count(),
                trusted_by: self
                    .displays()
                    .iter()
                    .filter(|d| {
                        d.device
                            .as_ref()
                            .and_then(|i| i.firmware.as_ref())
                            .is_some_and(|f| f.key_ids.contains(&id))
                    })
                    .map(|d| d.name.clone())
                    .collect(),
                labelled: self.signer_labels().iter().any(|l| l.key_id == id),
                signer: self.signer(&id),
            })
            .collect()
    }

    /// A display card's firmware section.
    pub fn display_firmware(&self, id: &str, now: Instant) -> Option<DisplayFirmware> {
        let d = self.displays().iter().find(|d| d.id == id)?;
        Some(display_firmware(
            d,
            self.shelf().map(|s| s.packages.as_slice()),
            self.signer_labels(),
            self.hub_now(now),
        ))
    }

    /// Whether the app should check the hub more often: an update is on offer.
    pub(super) fn watching_offers(&self, now: Instant) -> bool {
        firmware::any_open_offer(self.displays(), self.hub_now(now))
    }

    /// "within about 5 minutes", from how often displays check.
    pub fn poll_text(&self) -> String {
        minutes(
            self.last
                .as_ref()
                .map_or(300, |r| r.preview.refresh_after_seconds),
        )
    }

    // ---- packages ---------------------------------------------------------------------------------

    /// Add a firmware file: checked here, then sent to the hub.
    pub fn upload_firmware(&mut self, path: PathBuf) -> Option<Op> {
        self.notice = None;
        self.start(Busy::Uploading, Op::UploadFirmware { path })
    }

    /// The official firmware that came with the app.
    pub fn bundled(&self) -> Option<&Bundled> {
        self.fw.bundled.as_ref()
    }

    /// Whether the hub already holds the official firmware that came with the app, as good as new.
    pub fn bundled_on_hub(&self) -> bool {
        self.bundled().is_some_and(|b| {
            let id = b.package_id();
            self.firmware_packages()
                .iter()
                .any(|p| p.package_id == id && !p.damaged)
        })
    }

    /// Add the official firmware that came with the app to the hub. It was checked when the app
    /// started.
    pub fn upload_bundled(&mut self) -> Option<Op> {
        let loaded = Box::new(self.bundled()?.loaded.clone());
        self.notice = None;
        self.start(Busy::Uploading, Op::UploadLoaded { loaded })
    }

    pub fn ask_remove_package(&mut self, id: &str) {
        let Some(p) = self.package_rows().into_iter().find(|p| p.id == id) else {
            return;
        };
        let mut detail = format!(
            "This is the package for hardware {}. It is deleted from the hub. Displays already running it keep \
             it.",
            quoted_list(&p.hardware_revisions)
        );
        if !p.offered_to.is_empty() {
            detail.push_str(&format!(
                " It is on offer to {}: that update is cancelled.",
                p.offered_to.join(", ")
            ));
        }
        self.confirm = Some(Confirm::RemovePackage {
            id: id.to_string(),
            title: package_title(&p.device_type, &p.version),
            detail,
        });
    }

    /// Whether the hub keeps firmware (packages, key names and publishers). An older hub doesn't.
    pub fn hub_has_firmware(&self) -> bool {
        self.shelf().is_some()
    }

    // ---- updates ---------------------------------------------------------------------------------

    /// The Update button: asks first, naming the version, the signer and the summary.
    pub fn ask_update(&mut self, display: &str, package: &str, now: Instant) {
        let Some(fw) = self.display_firmware(display, now) else {
            return;
        };
        let Some(choice) = fw
            .available
            .iter()
            .chain(fw.others.iter())
            .find(|c| c.package_id == package)
            .cloned()
        else {
            return;
        };
        let name = self.display_name(display);
        let current = fw.version.clone().unwrap_or_else(|| "its current version".into());
        let when = if fw.paused {
            format!(
                "{name} has used up its update attempts for now, so it waits about four hours switched on \
                 before it installs it."
            )
        } else {
            format!(
                "{name} installs it at its next check, within about {}.",
                self.poll_text()
            )
        };
        let detail = format!(
            "Signed by {}. {when} If the new firmware doesn't start properly, it goes back to {current} by \
             itself.",
            signer_phrase(&choice.signer),
        );
        let second = choice.asks_twice().then(|| {
            let mut why = Vec::new();
            if choice.unsure {
                why.push(format!(
                    "{name} reports its version as \"{current}\", which this app can't compare with {}, so it \
                     can't tell whether this is newer or older.",
                    choice.version
                ));
            }
            if choice.older {
                why.push(format!(
                    "{} is older than {current}, which {name} runs now. Going back is allowed while the older \
                     release has the same security level, for example to undo an update that went wrong. Pages \
                     and settings are kept.",
                    choice.version
                ));
            }
            if choice.pre_release {
                why.push(format!(
                    "{} is a pre-release: a test version from its maker, which may not work as well as a full \
                     release.",
                    choice.version
                ));
            }
            let title = match (choice.older, choice.pre_release) {
                (true, true) => format!("{} is an older pre-release", choice.version),
                (true, false) => format!("{} is an older version", choice.version),
                (false, true) => format!("{} is a pre-release", choice.version),
                (false, false) => format!("{} may be older than {current}", choice.version),
            };
            (title, format!("{} Install it anyway?", why.join(" ")))
        });
        let title = if choice.older {
            format!("Go back to {} on {name}?", choice.version)
        } else if choice.unsure {
            format!("Install {} on {name}?", choice.version)
        } else {
            format!("Update {name} to {}?", choice.version)
        };
        self.confirm = Some(Confirm::Update {
            display: display.to_string(),
            package: package.to_string(),
            title,
            detail,
            says: PackageSays {
                facts: Vec::new(),
                note: choice.summary.clone(),
            },
            second,
        });
    }

    pub fn cancel_offer(&mut self, display: &str) -> Option<Op> {
        let name = self.display_name(display);
        self.start(
            Busy::Saving {
                done: format!("Cancelled the update for {name}."),
            },
            Op::CancelOffer {
                display: display.to_string(),
            },
        )
    }

    /// The Update confirmation was accepted: a second question for an older version or a
    /// pre-release, else the offer.
    pub(super) fn confirm_update(
        &mut self,
        display: String,
        package: String,
        second: Option<(String, String)>,
    ) -> Option<Op> {
        if let Some((title, detail)) = second {
            self.confirm = Some(Confirm::UpdateAgain {
                display,
                package,
                title,
                detail,
            });
            return None;
        }
        self.offer(display, package)
    }

    pub(super) fn offer(&mut self, display: String, package: String) -> Option<Op> {
        let name = self.display_name(&display);
        let version = self
            .firmware_packages()
            .iter()
            .find(|p| p.package_id == package)
            .map_or_else(String::new, |p| format!(" {}", p.firmware_version));
        let done = format!(
            "Offered{version} to {name}. It installs it at its next check, within about {}. Its card shows how it \
             goes.",
            self.poll_text()
        );
        self.start(Busy::Offering { done }, Op::OfferFirmware { display, package })
    }

    // ---- signer names ------------------------------------------------------------------------------

    pub fn begin_label(&mut self, key_id: &str) {
        let mut field = Field::new(48);
        if let Some(l) = self.signer_labels().iter().find(|l| l.key_id == key_id) {
            field.set(&l.label);
        }
        self.fw.label_edit = Some(LabelEdit {
            key_id: key_id.to_string(),
            field,
        });
    }

    pub fn cancel_label(&mut self) {
        self.fw.label_edit = None;
    }

    /// Saves the owner's name for a key. An empty name removes it.
    pub fn save_label(&mut self) -> Option<Op> {
        let edit = self.fw.label_edit.as_ref()?;
        let key_id = edit.key_id.clone();
        let typed = edit.field.trimmed().to_string();
        let label = if typed.is_empty() {
            None
        } else {
            match clean_title(&typed) {
                Some(l) if firmware::reserved_label(&l) => return self.problem(reserved_name()),
                Some(l) => Some(l),
                None => {
                    return self.problem(Guidance {
                        tone: Tone::Problem,
                        title: "Names are 1 to 48 characters".into(),
                        detail: "Plain text, such as the maker's name.".into(),
                        next: Next::None,
                    })
                }
            }
        };
        let done = match &label {
            Some(l) => format!(
                "The hub now calls the key {} \"{l}\".",
                firmware::fingerprint(&key_id)
            ),
            None => format!(
                "Removed the hub's name for the key {}.",
                firmware::fingerprint(&key_id)
            ),
        };
        let op = self.start(Busy::Naming { done }, Op::SetSignerLabel { key_id, label });
        if op.is_some() {
            self.fw.label_edit = None;
        }
        op
    }

    // ---- publishers --------------------------------------------------------------------------------

    pub fn create_publisher(&mut self) -> Option<Op> {
        let Some(label) = clean_title(self.fw.publisher_label.trimmed()) else {
            return self.problem(Guidance {
                tone: Tone::Problem,
                title: "Give the publisher a name".into(),
                detail: "Such as the maker, or the computer they build firmware on.".into(),
                next: Next::None,
            });
        };
        self.fw.publisher_string = None;
        self.start(Busy::Saving { done: String::new() }, Op::NewPublisher { label })
    }

    pub fn ask_revoke_publisher(&mut self, id: &str) {
        if let Some(p) = self.publishers().iter().find(|p| p.id == id) {
            self.confirm = Some(Confirm::RevokePublisher {
                id: p.id.clone(),
                label: p.label.clone(),
            });
        }
    }

    pub(super) fn take_publisher_invite(&mut self, invite: PublisherInvite) {
        match self.invite_string(&invite.secret) {
            Some(s) => {
                self.fw.publisher_string = Some(Zeroizing::new(s));
                self.fw.publisher_expires = Some(invite.expires_at);
                self.fw.publisher_label.clear();
            }
            None => self.notice = Some(not_understood()),
        }
    }

    // ---- Install firmware --------------------------------------------------------------------------

    pub fn open_install(&mut self) {
        if matches!(self.fw.install.stage, InstallStage::Running { .. }) {
            return;
        }
        self.fw.install = InstallFlow {
            open: true,
            ..InstallFlow::default()
        };
        self.section = super::Section::Firmware;
    }

    pub fn close_install(&mut self) {
        if !matches!(self.fw.install.stage, InstallStage::Running { .. }) {
            self.fw.install = InstallFlow::default();
        }
    }

    /// Reads and checks a package for a USB install, from a file or the hub.
    pub fn load_package(&mut self, from: PackageSource) -> Option<Op> {
        self.notice = None;
        self.start(Busy::Loading, Op::LoadPackage { from })
    }

    /// Install firmware with the official package that came with the app. It is already read and
    /// checked, so the next thing is to look for the display's port.
    pub fn use_bundled(&mut self) -> Option<Op> {
        if self.busy.is_some() || self.fw.install.stage != InstallStage::Choose {
            return None;
        }
        let loaded = self.bundled()?.loaded.clone();
        self.notice = None;
        self.take_loaded(loaded);
        self.first_install_scan()
    }

    pub(super) fn take_loaded(&mut self, loaded: Loaded) {
        if Plan::of(&loaded.checked).is_err() {
            self.notice = Some(Guidance {
                tone: Tone::Problem,
                title: "This package can't be installed over USB".into(),
                detail: format!(
                    "{} has no USB-install parts, which a first install needs. It can still update displays that \
                     already trust its maker. Ask the maker for a package made for USB installs.",
                    loaded.title()
                ),
                next: Next::None,
            });
            return;
        }
        let i = &mut self.fw.install;
        i.open = true;
        i.loaded = Some(loaded);
        i.stage = InstallStage::Ready;
        self.section = super::Section::Firmware;
    }

    /// Looks for ports once, as soon as a package is ready to install.
    pub fn first_install_scan(&mut self) -> Option<Op> {
        if self.fw.install.stage == InstallStage::Ready && !self.fw.install.scanned && self.busy.is_none() {
            self.fw.install.scanned = true;
            return self.list_ports();
        }
        None
    }

    /// Back to choosing a package.
    pub fn choose_again(&mut self) {
        if self.fw.install.stage == InstallStage::Ready {
            self.fw.install.loaded = None;
            self.fw.install.stage = InstallStage::Choose;
        }
    }

    /// The signer of the package chosen for installing.
    pub fn install_signer(&self) -> Option<Signer> {
        Some(self.fw.install.loaded.as_ref()?.signer(self.signer_labels()))
    }

    pub fn install_progress(&self) -> Progress {
        self.fw.install.progress.get()
    }

    /// "Erase and install" on a port: checks the details, then asks, showing the fingerprint again.
    pub fn ask_install(&mut self, port: &str) -> Option<Op> {
        let port = port.trim().to_string();
        let loaded = self.fw.install.loaded.clone()?;
        if port.is_empty() {
            return None;
        }
        if let Err(g) = self.install_details() {
            return self.problem(g);
        }
        let signer = loaded.signer(self.signer_labels());
        let m = &loaded.checked.manifest;
        let who = match signer.kind {
            SignerKind::Official => format!(
                "It is signed by {} (key fingerprint {}).",
                signer.name(),
                signer.fingerprint
            ),
            SignerKind::Labelled(ref l) => format!(
                "It is signed by the key the hub calls \"{l}\", fingerprint {}. The hub keeps that name, so it is \
                 only as trustworthy as the hub: check the fingerprint against the one the maker publishes.",
                signer.fingerprint
            ),
            SignerKind::Unknown => format!(
                "It is signed by a maker this app doesn't know, with key fingerprint {}. Compare it with the \
                 fingerprint the maker publishes, such as on their website or release page, and go on only if \
                 they match.",
                signer.fingerprint
            ),
        };
        let keys: Vec<String> = loaded
            .trusted_keys(self.signer_labels())
            .iter()
            .map(|k| {
                let mut t = signer_phrase(k);
                if k.key_id == signer.key_id {
                    t.push_str(", which signed this package");
                }
                t
            })
            .collect();
        let trusts = match keys.as_slice() {
            [one] => format!("Afterwards it accepts updates only if they are signed by {one}."),
            _ => format!(
                "Afterwards it accepts updates only if they are signed by one of these {} keys: {}.",
                keys.len(),
                keys.join("; ")
            ),
        };
        let mut facts = vec![
            ("Device type".to_string(), m.device_type.clone()),
            ("Version".to_string(), m.firmware_version.clone()),
        ];
        facts.push((
            if m.hardware_revisions.len() == 1 {
                "Hardware".to_string()
            } else {
                "Hardware (any of)".to_string()
            },
            m.hardware_revisions.join(", "),
        ));
        self.notice = None;
        self.confirm = Some(Confirm::Install {
            port: port.clone(),
            title: format!("Erase the display on {port} and install this firmware?"),
            detail: format!(
                "{who} Everything on the display is erased: its hub details, Wi-Fi settings and trusted keys. \
                 {trusts}"
            ),
            says: PackageSays {
                facts,
                note: m.summary.clone(),
            },
        });
        None
    }

    /// The display's name, its Wi-Fi details and any maker's name, checked.
    #[allow(clippy::type_complexity)]
    fn install_details(&self) -> Result<(String, Wifi, Option<String>), Guidance> {
        let i = &self.fw.install;
        let Some(name) = clean_title(i.name.trimmed()) else {
            return Err(Guidance {
                tone: Tone::Problem,
                title: "Give the display a name".into(),
                detail: "Such as Desk or Kitchen, so you can tell displays apart later.".into(),
                next: Next::None,
            });
        };
        let wifi = wifi_of(&i.ssid, &i.password)?.ok_or_else(|| Guidance {
            tone: Tone::Problem,
            title: "Enter the Wi-Fi network's name".into(),
            detail: "Installing erases the display's Wi-Fi settings, so it needs them again. Displays use 2.4 GHz \
                     networks."
                .into(),
            next: Next::None,
        })?;
        // An older hub can't keep names for keys, so the field isn't shown and anything in it is
        // ignored.
        let typed = if self.hub_has_firmware() {
            i.maker.trimmed()
        } else {
            ""
        };
        let maker = if typed.is_empty() {
            None
        } else {
            let m = clean_title(typed).ok_or_else(|| Guidance {
                tone: Tone::Problem,
                title: "The maker's name is 1 to 48 characters".into(),
                detail: "Plain text, or leave it empty.".into(),
                next: Next::None,
            })?;
            if firmware::reserved_label(&m) {
                return Err(reserved_name());
            }
            Some(m)
        };
        Ok((name, wifi, maker))
    }

    /// The confirmation was accepted: flash, then provision.
    pub(super) fn start_install(&mut self, port: String) -> Option<Op> {
        let loaded = self.fw.install.loaded.clone()?;
        let plan = Plan::of(&loaded.checked).ok()?;
        let (name, wifi, maker) = match self.install_details() {
            Ok(d) => d,
            Err(g) => return self.problem(g),
        };
        let signer = loaded.signer(self.signer_labels());
        // A name is saved only for a key the app doesn't name itself, and only when it changes.
        let label = maker
            .filter(|_| signer.kind != SignerKind::Official)
            .filter(|m| signer.kind != SignerKind::Labelled(m.clone()))
            .map(|m| (signer.key_id.clone(), m));
        let progress = ProgressCell::default();
        let job = InstallJob {
            port: port.clone(),
            firmware: bundle_firmware(&loaded.checked),
            loaded,
            plan,
            name,
            hosts: self.bundle_hosts(),
            hub_port: self.bundle_port(),
            wifi: Some(wifi),
            label,
            progress: progress.clone(),
        };
        let op = self.start(
            Busy::Installing { port: port.clone() },
            Op::Install(Box::new(job)),
        )?;
        // Keys left from an earlier install are for that display, not this one.
        self.pending_firmware = None;
        self.fw.install.progress = progress;
        self.fw.install.stage = InstallStage::Running { port };
        self.fw.install.password.clear();
        self.fw.install.known = self.displays().iter().map(|d| d.id.clone()).collect();
        Some(op)
    }

    /// After an install: send the setup details, or say what to do next. `label` says what became
    /// of the owner's name for the maker's key; whichever way the install ended, the notice says if
    /// it wasn't saved. True if the refresh it brought couldn't read the firmware lists.
    #[must_use = "whether the firmware lists were unreadable, for tell_unreadable_lists"]
    pub(super) fn take_installed(
        &mut self,
        after: AfterFlash,
        refresh: Option<Refresh>,
        label: MakerName,
        now: Instant,
    ) -> bool {
        let port = match &self.fw.install.stage {
            InstallStage::Running { port } => port.clone(),
            _ => String::new(),
        };
        let known = std::mem::take(&mut self.fw.install.known);
        let labels = self.signer_labels().to_vec();
        let pending = self.fw.install.loaded.as_ref().map(|l| PendingKeys {
            firmware: bundle_firmware(&l.checked),
            port: port.clone(),
            keys: l.trusted_keys(&labels),
            include: true,
        });
        let title = self
            .fw
            .install
            .loaded
            .as_ref()
            .map(Loaded::title)
            .unwrap_or_default();
        let name = clean_title(self.fw.install.name.trimmed()).unwrap_or_default();
        let maker = clean_title(self.fw.install.maker.trimmed()).unwrap_or_default();
        let unsaved = self.maker_not_saved(&maker, label);
        let unreadable = self.take_refresh_opt(refresh, now);
        // The Displays screen watches for the new display, as after an ordinary setup.
        self.enrol = super::Enrol {
            open: true,
            known,
            ..super::Enrol::default()
        };
        match after {
            AfterFlash::Sent(shown) => {
                self.enrol.stage = EnrolStage::Waiting {
                    how: format!("Installed firmware and sent the setup details to {port}"),
                };
                self.fw.install.stage = InstallStage::Done {
                    name: name.clone(),
                    port: port.clone(),
                };
                let mut detail = format!(
                    "{} {} on {port} accepted its setup details and its maker's keys, as {name}. It joins Wi-Fi and \
                     checks in with the hub; the Displays screen updates when it does.",
                    shown.device.device_type, shown.device.firmware_version
                );
                detail.push_str(&unsaved);
                self.notice = Some(Guidance::done(
                    format!("Installed {title} on the display on {port}"),
                    detail,
                ));
            }
            AfterFlash::Usb { error, mut bundle } => {
                // The firmware is in; only the setup details are missing. They are kept for Send
                // under Displays, and the keys wait to go with them to this port.
                bundle.firmware = None;
                self.enrol.bundle = Some(*bundle);
                self.enrol.stage = EnrolStage::Ready;
                self.enrol.ports = Vec::new();
                self.enrol.scanned = false;
                self.pending_firmware = pending;
                self.fw.install = InstallFlow::default();
                let why = crate::guidance::from_usb(&error);
                self.notice = Some(Guidance {
                    tone: Tone::Problem,
                    title: "The firmware is installed, but the setup details weren't sent".into(),
                    detail: format!(
                        "{}: {} Under Displays, Add a display has them ready to send again to {port}, with the \
                         firmware's keys.{unsaved}",
                        why.title, why.detail
                    ),
                    next: Next::None,
                });
                self.section = super::Section::Displays;
            }
            AfterFlash::Hub(e) => {
                // Without setup details, the next Add a display to this port carries the firmware's
                // keys instead.
                self.pending_firmware = pending;
                self.enrol.stage = EnrolStage::Form;
                self.enrol.name.set(&name);
                self.fw.install = InstallFlow::default();
                let why = crate::guidance::from_client(&e, crate::guidance::Doing::Other, &self.address());
                self.notice = Some(Guidance {
                    tone: Tone::Problem,
                    title: "The firmware is installed, but the hub couldn't make setup details".into(),
                    detail: format!(
                        "{}. {} Set the display up under Displays, Add a display, and send the details to {port}: \
                         the app adds the firmware's keys.{unsaved}",
                        why.title, why.detail
                    ),
                    next: why.next,
                });
                self.section = super::Section::Displays;
            }
        }
        unreadable
    }

    /// The sentence for a notice when the owner's name for the maker's key wasn't saved, with a
    /// space before it; empty if it was, or there was none.
    fn maker_not_saved(&self, maker: &str, label: MakerName) -> String {
        let later = "You can name the key on the Firmware screen, under Makers' keys";
        match label {
            MakerName::Done => String::new(),
            MakerName::Failed(e) => {
                let why = crate::guidance::from_client(&e, crate::guidance::Doing::Naming, &self.address());
                format!(
                    " The name \"{maker}\" for the maker's key wasn't saved on the hub ({}: {}). {later}.",
                    why.title.to_lowercase(),
                    why.detail.trim_end_matches('.')
                )
            }
            MakerName::Unreachable => format!(
                " The name \"{maker}\" for the maker's key wasn't saved either, as the hub couldn't be reached. \
                 {later}, once the hub answers."
            ),
        }
    }

    /// An install that stopped: back to choosing a port, the package kept.
    pub(super) fn install_failed(&mut self) {
        if matches!(self.fw.install.stage, InstallStage::Running { .. }) {
            self.fw.install.stage = InstallStage::Ready;
        }
        self.fw.install.known.clear();
    }

    /// Setup details with the keys waiting from an install, unless the owner left them out. The
    /// caller has checked the port.
    pub(super) fn bundle_with_pending(&self, mut b: Bundle) -> Bundle {
        if b.firmware.is_none() {
            b.firmware = self
                .pending_firmware
                .as_ref()
                .filter(|p| p.include)
                .map(|p| p.firmware.clone());
        }
        b
    }

    /// Keys waiting from an install, for the Add a display form to show.
    pub fn pending_keys(&self) -> Option<&PendingKeys> {
        self.pending_firmware.as_ref()
    }

    /// Leaves the waiting keys out of the next setup details, or puts them back.
    pub fn include_pending_keys(&mut self, include: bool) {
        if let Some(p) = self.pending_firmware.as_mut() {
            p.include = include;
        }
    }

    pub(super) fn uploaded(&mut self, p: &FirmwarePackageView, new: bool) {
        let signer = self.signer(&p.signer_key_id);
        let title = package_title(&p.device_type, &p.firmware_version);
        let mut detail = format!(
            "Signed by {}. {}",
            signer_phrase(&signer),
            if p.usb_install {
                "It can update displays that trust this key, and do a first install over USB."
            } else {
                "It can update displays that trust this key."
            }
        );
        if let Some(gone) = &p.evicted {
            detail.push_str(&format!(
                " To make room, the hub removed {}.",
                package_title(&gone.device_type, &gone.firmware_version)
            ));
        }
        self.notice = Some(Guidance::done(
            if new {
                format!("Added {title} to the hub")
            } else {
                format!("The hub already had {title}")
            },
            detail,
        ));
    }
}

/// "\"rev-a\", \"rev-b\"": the maker's names, quoted.
fn quoted_list(items: &[String]) -> String {
    items
        .iter()
        .map(|i| format!("\"{i}\""))
        .collect::<Vec<_>>()
        .join(", ")
}

/// A name the app keeps for its own official keys.
fn reserved_name() -> Guidance {
    Guidance {
        tone: Tone::Problem,
        title: "That name is kept for official firmware".into(),
        detail: format!(
            "\"{}\" is what the app calls keys built into it, so a key you name can't use it or anything like \
             it. Name the key after its maker instead.",
            firmware::OFFICIAL_NAME
        ),
        next: Next::None,
    }
}

fn not_understood() -> Guidance {
    Guidance {
        tone: Tone::Problem,
        title: "The hub's answer wasn't understood".into(),
        detail: "Update the hub and this app to the same release.".into(),
        next: Next::None,
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{connected, ts};
    use super::*;
    use crate::firmware::tests::usb_package;
    use crate::firmware::{BoardError, InstallError, Phase, Step};
    use crate::model::{Failure, Outcome, Section};
    use hub_core::api::{DisplayFirmware as HubFirmware, DisplayView, FirmwareOfferView};
    use hub_protocol::device::DeviceInfo;
    use hub_protocol::firmware::FirmwareInfo;

    fn package(id: &str, version: &str, signer: &str) -> FirmwarePackageView {
        FirmwarePackageView {
            package_id: id.repeat(64),
            device_type: "desk".into(),
            hardware_revisions: vec!["rev-a".into()],
            firmware_version: version.into(),
            security_counter: 1,
            summary: Some(format!("Release {version}.")),
            signer_key_id: signer.repeat(64),
            signer_fingerprint: firmware::fingerprint(&signer.repeat(64)),
            signer_label: None,
            size: 1_500_000,
            update_size: 1_400_000,
            image_size: 1_395_000,
            usb_install: true,
            key_set: None,
            damaged: false,
            uploaded_at: ts("2026-11-10T19:00:00Z"),
            uploaded_by_role: "admin".into(),
            uploaded_by: "a".into(),
            uploaded_by_label: None,
            offered_to: vec![],
            evicted: None,
        }
    }

    fn display(id: &str, keys: Vec<&str>, version: &str, available: Option<&str>) -> DisplayView {
        DisplayView {
            id: id.into(),
            name: id.to_uppercase(),
            enrolled_at: ts("2026-11-10T19:00:00Z"),
            last_seen_at: None,
            device: Some(DeviceInfo {
                device_type: "desk".into(),
                firmware_version: version.into(),
                hardware_revision: "rev-a".into(),
                protocol_version: 1,
                firmware: Some(FirmwareInfo {
                    key_ids: keys.iter().map(|k| k.repeat(64)).collect(),
                    security_floor: 1,
                    slot_bytes: 1 << 21,
                    attempts_left: None,
                }),
            }),
            pages: Some(vec![]),
            firmware: Some(HubFirmware {
                reported: None,
                offer: None,
                available_package_id: available.map(|a| a.repeat(64)),
                auto_update: false,
            }),
        }
    }

    /// A connected model whose hub holds 1.1.0, 1.2.0 and 1.3.0-beta.1, all signed by key `k`, and
    /// one display running 1.2.0 that trusts `k`, with 1.3.0-beta.1 available.
    fn with_firmware() -> (Model, Instant) {
        let (mut m, now) = connected(vec![]);
        let r = m.last.as_mut().unwrap();
        r.displays = vec![display("d", vec!["k"], "1.2.0", Some("c"))];
        r.firmware = Some(Shelf {
            packages: vec![
                package("a", "1.1.0", "k"),
                package("b", "1.2.0", "k"),
                package("c", "1.3.0-beta.1", "k"),
            ],
            signers: vec![SignerView {
                key_id: "k".repeat(64),
                fingerprint: firmware::fingerprint(&"k".repeat(64)),
                label: "Kea Displays".into(),
                updated_at: ts("2026-11-10T19:00:00Z"),
            }],
            publishers: vec![],
        });
        (m, now)
    }

    #[test]
    fn the_firmware_screen_has_its_own_section_and_lists_packages_newest_first() {
        assert_eq!(Section::ALL[3], Section::Firmware);
        assert_eq!(Section::Firmware.label(), "Firmware");
        let (m, _) = with_firmware();
        let rows = m.package_rows();
        let versions: Vec<&str> = rows.iter().map(|r| r.version.as_str()).collect();
        assert_eq!(versions, ["1.3.0-beta.1", "1.2.0", "1.1.0"]);
        assert_eq!(rows[0].signer.name(), "Kea Displays");
        let keys = m.key_rows();
        assert_eq!(keys.len(), 1);
        assert_eq!(
            (keys[0].packages, keys[0].trusted_by.as_slice()),
            (3, ["D".to_string()].as_slice())
        );
    }

    #[test]
    fn a_damaged_package_is_listed_but_never_the_update_or_an_install_over_usb() {
        let (mut m, _) = with_firmware();
        // The hub couldn't read 1.3.0-beta.1, the update the display would otherwise be shown.
        m.last.as_mut().unwrap().firmware.as_mut().unwrap().packages[2].damaged = true;
        let rows = m.package_rows();
        assert_eq!(rows.len(), 3);
        let damaged = rows.iter().find(|r| r.version == "1.3.0-beta.1").unwrap();
        assert!(damaged.damaged && damaged.usb_install && !damaged.for_usb_install());
        assert!(rows
            .iter()
            .filter(|r| !r.damaged)
            .all(PackageRow::for_usb_install));
        let f = firmware::display_firmware(&m.displays()[0], Some(m.firmware_packages()), &[], None);
        assert!(f.available.is_none());
        let others: Vec<&str> = f.others.iter().map(|o| o.version.as_str()).collect();
        assert_eq!(others, ["1.1.0"]);
    }

    #[test]
    fn update_names_version_signer_and_summary_and_asks_twice_for_a_pre_release() {
        let (mut m, now) = with_firmware();
        let fw = m.display_firmware("d", now).unwrap();
        let up = fw.available.clone().unwrap();
        assert_eq!(up.version, "1.3.0-beta.1");
        m.ask_update("d", &up.package_id, now);
        let (title, detail, action) = m.confirm.as_ref().unwrap().question();
        assert_eq!(title, "Update D to 1.3.0-beta.1?");
        assert!(
            detail.contains("Signed by the key the hub calls \"Kea Displays\" (kkkk-"),
            "{detail}"
        );
        assert!(!detail.contains("Release 1.3.0-beta.1."), "{detail}");
        assert_eq!(
            m.confirm
                .as_ref()
                .unwrap()
                .package_says()
                .unwrap()
                .note
                .as_deref(),
            Some("Release 1.3.0-beta.1.")
        );
        assert_eq!(action, "Update");
        // First yes: the second question, saying why. Nothing is sent yet.
        assert!(m.confirm_yes().is_none());
        let (title, detail, action) = m.confirm.as_ref().unwrap().question();
        assert_eq!(title, "1.3.0-beta.1 is a pre-release");
        assert!(detail.contains("test version"));
        assert_eq!(action, "Install anyway");
        match m.confirm_yes() {
            Some(Op::OfferFirmware { display, package }) => {
                assert_eq!((display.as_str(), package), ("d", "c".repeat(64)))
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn going_back_to_an_older_version_says_why_and_a_newer_full_release_asks_once() {
        let (mut m, now) = with_firmware();
        m.ask_update("d", &"a".repeat(64), now);
        assert_eq!(m.confirm.as_ref().unwrap().question().0, "Go back to 1.1.0 on D?");
        assert!(m.confirm_yes().is_none());
        let (title, detail, _) = m.confirm.as_ref().unwrap().question();
        assert_eq!(title, "1.1.0 is an older version");
        assert!(detail.contains("older than 1.2.0"), "{detail}");
        m.confirm_no();

        // A newer full release: one question, then the offer.
        m.last
            .as_mut()
            .unwrap()
            .firmware
            .as_mut()
            .unwrap()
            .packages
            .push(package("e", "1.4.0", "k"));
        m.last.as_mut().unwrap().displays[0]
            .firmware
            .as_mut()
            .unwrap()
            .available_package_id = Some("e".repeat(64));
        m.ask_update("d", &"e".repeat(64), now);
        assert!(matches!(m.confirm_yes(), Some(Op::OfferFirmware { .. })));
        assert!(matches!(m.busy, Some(Busy::Offering { ref done }) if done.contains("Offered 1.4.0 to D")));
    }

    fn offer(state: &str, reason: Option<&str>) -> FirmwareOfferView {
        FirmwareOfferView {
            offer_id: "0".repeat(32),
            package_id: "c".repeat(64),
            firmware_version: "1.3.0-beta.1".into(),
            state: state.into(),
            reason: reason.map(Into::into),
            reported_version: Some("1.2.0".into()),
            made_at: ts("2026-11-10T19:59:00Z"),
            expires_at: ts("2026-11-11T19:59:00Z"),
            updated_at: ts("2026-11-10T19:59:30Z"),
            ended_at: None,
        }
    }

    #[test]
    fn an_open_offer_can_be_cancelled_and_the_app_checks_more_often() {
        let (mut m, now) = with_firmware();
        m.last.as_mut().unwrap().displays[0]
            .firmware
            .as_mut()
            .unwrap()
            .offer = Some(offer("offered", None));
        let fw = m.display_firmware("d", now).unwrap();
        assert_eq!(fw.offer.as_ref().unwrap().1, Phase::Waiting);
        assert!(m.watching_offers(now));
        // Due after 15 seconds, not 30.
        assert!(m.refresh_due(now + std::time::Duration::from_secs(10)).is_none());
        let op = m.refresh_due(now + std::time::Duration::from_secs(16));
        assert!(matches!(op, Some(Op::Refresh)));
        m.busy = None;
        assert!(matches!(m.cancel_offer("d"), Some(Op::CancelOffer { display }) if display == "d"));
    }

    #[test]
    fn every_offer_phase_has_plain_words() {
        let o = offer("rolled_back", None);
        let (title, detail) =
            crate::guidance::offer_words(&Phase::RolledBack, &o, Some("1.2.0"), "5 minutes", None);
        assert_eq!(title, "Went back to 1.2.0");
        assert_eq!(
            detail,
            "The update didn't start properly, so the display went back to 1.2.0. Nothing was lost."
        );
        let words = |p: Phase| crate::guidance::offer_words(&p, &o, Some("1.2.0"), "5 minutes", None);
        assert_eq!(words(Phase::Waiting).0, "Waiting for the display to check in");
        assert!(words(Phase::Waiting).1.contains("within about 5 minutes"));
        assert_eq!(words(Phase::Downloading).0, "Downloading 1.3.0-beta.1");
        assert_eq!(words(Phase::Restarting).0, "Restarting into 1.3.0-beta.1");
        assert_eq!(words(Phase::Installed).0, "Updated to 1.3.0-beta.1");
        assert!(words(Phase::Expired).1.contains("24 hours"));
        assert!(words(Phase::Cancelled("package_deleted".into()))
            .1
            .contains("removed from the hub"));
        assert!(words(Phase::Unknown("exploded".into())).1.contains("exploded"));
        let mut seen = std::collections::HashSet::new();
        for r in hub_protocol::firmware::ReportReason::ALL {
            let p = if r.is_refusal() {
                Phase::Refused(r)
            } else {
                Phase::Failed(r)
            };
            let (t, d) = words(p);
            assert!(!t.is_empty() && d.ends_with("It still runs 1.2.0."), "{r:?}: {d}");
            assert!(!d.contains("doesn't know"), "{r:?} has its own sentence");
            assert!(seen.insert(d), "{r:?} has a sentence of its own");
        }
    }

    #[test]
    fn a_display_without_keys_points_to_install_firmware() {
        let (mut m, now) = with_firmware();
        m.last.as_mut().unwrap().displays = vec![display("d", vec![], "1.0.0", None)];
        let fw = m.display_firmware("d", now).unwrap();
        assert_eq!(fw.trust, firmware::Trust::NoKeys);
        assert!(fw.available.is_none() && fw.others.is_empty());
        m.open_install();
        assert_eq!(m.section, Section::Firmware);
        assert!(m.fw.install.open);
    }

    #[test]
    fn naming_a_key_and_inviting_a_publisher() {
        let (mut m, _) = with_firmware();
        let key = "k".repeat(64);
        m.begin_label(&key);
        assert_eq!(m.fw.label_edit.as_ref().unwrap().field.text, "Kea Displays");
        m.fw.label_edit.as_mut().unwrap().field.set("  ");
        match m.save_label() {
            Some(Op::SetSignerLabel { key_id, label: None }) => assert_eq!(key_id, key),
            other => panic!("{other:?}"),
        }
        assert!(m.fw.label_edit.is_none());
        m.busy = None;

        assert!(m.create_publisher().is_none(), "a publisher needs a name");
        m.fw.publisher_label.set("Kea build PC");
        assert!(matches!(m.create_publisher(), Some(Op::NewPublisher { label }) if label == "Kea build PC"));
        let invite = PublisherInvite {
            secret: "AAAAAAAAAAAAAAAAAAAAAA".into(),
            expires_at: ts("2026-11-10T20:10:00Z"),
            identity: m.last.as_ref().unwrap().status.identity.clone(),
        };
        m.finish(
            Ok(Outcome::PublisherInvite {
                invite,
                refresh: None,
            }),
            Instant::now(),
        );
        let s = m.fw.publisher_string.as_ref().unwrap();
        let p = hub_protocol::pairing::PairingString::decode(s).unwrap();
        assert_eq!((p.host.as_str(), p.port), ("192.168.1.20", 8443));
        assert!(m.fw.publisher_label.text.is_empty());
    }

    #[test]
    fn removing_a_package_on_offer_says_its_update_is_cancelled() {
        let (mut m, _) = with_firmware();
        m.last.as_mut().unwrap().firmware.as_mut().unwrap().packages[2].offered_to = vec!["d".into()];
        m.ask_remove_package(&"c".repeat(64));
        let (title, detail, _) = m.confirm.as_ref().unwrap().question();
        assert_eq!(title, "Remove \"desk\" 1.3.0-beta.1?");
        assert!(detail.contains("on offer to D"), "{detail}");
        assert!(detail.contains("for hardware \"rev-a\""), "{detail}");
        assert_eq!(m.package_rows()[0].hardware_revisions, ["rev-a"]);
        assert!(matches!(m.confirm_yes(), Some(Op::RemoveFirmware { .. })));
    }

    #[test]
    fn a_name_claiming_to_be_official_is_refused_before_anything_is_sent() {
        let (mut m, _) = with_firmware();
        m.begin_label(&"k".repeat(64));
        for bad in [
            "Official Display Hub firmware",
            "official-display hub",
            "The OFFICIAL Display-Hub build",
        ] {
            m.fw.label_edit.as_mut().unwrap().field.set(bad);
            assert!(m.save_label().is_none(), "{bad}");
            assert_eq!(
                m.notice.as_ref().unwrap().title,
                "That name is kept for official firmware"
            );
        }
        m.fw.label_edit.as_mut().unwrap().field.set("Kea Displays");
        assert!(matches!(m.save_label(), Some(Op::SetSignerLabel { .. })));
        assert!(
            matches!(m.busy, Some(Busy::Naming { ref done }) if done.contains("The hub now calls the key"))
        );
        // The hub refuses one too, and its reason is shown.
        let refused = ClientError::Status {
            status: 400,
            body: Some(hub_protocol::ErrorBody::new(
                hub_protocol::ErrorCode::BadRequest,
                "That name is kept for firmware whose key is built into the app.",
            )),
        };
        m.finish(Err(Failure::Client(refused)), Instant::now());
        let n = m.notice.as_ref().unwrap();
        assert_eq!(n.title, "The hub won't use that name");
        assert!(n.detail.contains("built into the app"));

        // And as the maker's name typed during Install firmware.
        let (mut m, _) = with_firmware();
        ready_to_install(&mut m, false);
        m.fw.install.ssid.set("Home");
        m.fw.install.maker.set("Official Display Hub Firmware!");
        assert!(m.ask_install("COM5").is_none());
        assert!(m.confirm.is_none());
        assert_eq!(
            m.notice.as_ref().unwrap().title,
            "That name is kept for official firmware"
        );
    }

    #[test]
    fn a_display_pausing_its_updates_says_so_and_the_update_waits() {
        let (mut m, now) = with_firmware();
        let d = &mut m.last.as_mut().unwrap().displays[0];
        d.device
            .as_mut()
            .unwrap()
            .firmware
            .as_mut()
            .unwrap()
            .attempts_left = Some(0);
        let fw = m.display_firmware("d", now).unwrap();
        assert!(fw.paused);
        assert!(fw.available.is_some(), "the offer is never blocked");
        assert_eq!(
            crate::guidance::paused_words("D"),
            "D has tried five updates recently and is pausing to protect its memory. It will try again after \
             about four hours switched on. To update now, use Install firmware over USB."
        );
        m.ask_update("d", &"c".repeat(64), now);
        let (_, detail, _) = m.confirm.as_ref().unwrap().question();
        assert!(detail.contains("waits about four hours switched on"), "{detail}");

        let d = &mut m.last.as_mut().unwrap().displays[0];
        d.device
            .as_mut()
            .unwrap()
            .firmware
            .as_mut()
            .unwrap()
            .attempts_left = Some(3);
        assert!(!m.display_firmware("d", now).unwrap().paused);
    }

    #[test]
    fn a_version_the_app_cant_read_asks_twice() {
        let (mut m, now) = with_firmware();
        m.last.as_mut().unwrap().displays[0]
            .device
            .as_mut()
            .unwrap()
            .firmware_version = "custom build".into();
        let fw = m.display_firmware("d", now).unwrap();
        assert!(fw.available.is_none(), "nothing is known to be newer");
        assert!(fw.others.iter().all(|o| o.unsure && o.asks_twice()));
        m.ask_update("d", &"b".repeat(64), now);
        assert_eq!(m.confirm.as_ref().unwrap().question().0, "Install 1.2.0 on D?");
        assert!(m.confirm_yes().is_none(), "a second question first");
        let (title, detail, _) = m.confirm.as_ref().unwrap().question();
        assert_eq!(title, "1.2.0 may be older than custom build");
        assert!(detail.contains("can't compare"), "{detail}");
    }

    #[test]
    fn an_upload_that_made_room_says_which_package_went() {
        let (mut m, now) = with_firmware();
        let mut p = package("e", "1.4.0", "k");
        p.evicted = Some(hub_core::api::EvictedPackage {
            package_id: "a".repeat(64),
            device_type: "desk".into(),
            firmware_version: "1.1.0".into(),
        });
        m.finish(
            Ok(Outcome::Uploaded {
                package: Box::new(p),
                new: true,
                refresh: None,
            }),
            now,
        );
        let n = m.notice.as_ref().unwrap();
        assert_eq!(n.title, "Added \"desk\" 1.4.0 to the hub");
        assert!(
            n.detail.contains("To make room, the hub removed \"desk\" 1.1.0."),
            "{}",
            n.detail
        );
        assert!(n.detail.contains("the key the hub calls \"Kea Displays\""));
    }

    #[test]
    fn firmware_lists_that_fail_to_load_keep_the_last_ones_and_say_so() {
        let (mut m, now) = with_firmware();
        let mut r = m.last.clone().unwrap();
        r.firmware = None;
        r.firmware_error = Some(std::sync::Arc::new(ClientError::Status {
            status: 500,
            body: None,
        }));
        m.finish(Ok(Outcome::Refreshed(Some(r))), now);
        assert!(m.hub_has_firmware(), "not taken for an older hub");
        assert_eq!(m.firmware_packages().len(), 3, "the lists it had");
        assert_eq!(
            m.notice.as_ref().unwrap().title,
            "Couldn't read the firmware lists"
        );
        // From a fresh start there is nothing to keep, but it still isn't an older hub.
        let (mut m, now) = connected(vec![]);
        let mut r = m.last.clone().unwrap();
        r.firmware_error = Some(std::sync::Arc::new(ClientError::Timeout));
        m.finish(Ok(Outcome::Refreshed(Some(r))), now);
        assert!(m.hub_has_firmware());
    }

    #[test]
    fn the_firmware_lists_notice_is_shown_once_a_spell_and_never_over_another() {
        let failing = |m: &Model| {
            let mut r = m.last.clone().unwrap();
            r.firmware = None;
            r.firmware_error = Some(std::sync::Arc::new(ClientError::Timeout));
            r
        };
        let title = |m: &Model| m.notice.as_ref().map(|n| n.title.clone());
        let lists = "Couldn't read the firmware lists".to_string();
        let (mut m, now) = with_firmware();
        let readable = m.last.clone().unwrap();
        // Another notice is showing: it stays, and the failure is told once it has been dismissed.
        m.notice = Some(Guidance::done("Saved", "Something else."));
        m.finish(Ok(Outcome::Refreshed(Some(failing(&m)))), now);
        assert_eq!(title(&m).as_deref(), Some("Saved"));
        m.notice = None;
        m.finish(Ok(Outcome::Refreshed(Some(failing(&m)))), now);
        assert_eq!(title(&m), Some(lists.clone()));
        // Dismissed: the next failing refreshes don't bring it back.
        m.notice = None;
        m.finish(Ok(Outcome::Refreshed(Some(failing(&m)))), now);
        m.finish(Ok(Outcome::Refreshed(Some(failing(&m)))), now);
        assert_eq!(title(&m), None);
        // Readable again, then failing again: told again.
        m.finish(Ok(Outcome::Refreshed(Some(readable))), now);
        assert!(!m.lists_told);
        m.finish(Ok(Outcome::Refreshed(Some(failing(&m)))), now);
        assert_eq!(title(&m), Some(lists));
    }

    #[test]
    fn the_firmware_lists_notice_waits_for_a_notice_the_same_answer_brings() {
        let failing = |m: &Model| {
            let mut r = m.last.clone().unwrap();
            r.firmware = None;
            r.firmware_error = Some(std::sync::Arc::new(ClientError::Timeout));
            r
        };
        let title = |m: &Model| m.notice.as_ref().map(|n| n.title.clone());
        let (mut m, now) = with_firmware();
        // A save whose refresh finds the lists unreadable: the save's own notice is what shows.
        m.busy = Some(Busy::Saving {
            done: "Removed Weather.".into(),
        });
        m.finish(Ok(Outcome::Refreshed(Some(failing(&m)))), now);
        assert_eq!(title(&m).as_deref(), Some("Removed Weather."));
        assert!(!m.lists_told, "not counted as told");
        // Once that has been dismissed, an answer that brings no refresh doesn't say it.
        m.notice = None;
        ready_to_install(&mut m, false);
        assert_eq!(title(&m), None);
        assert!(!m.lists_told);
        // The next refresh that still can't read them does.
        m.finish(Ok(Outcome::Refreshed(Some(failing(&m)))), now);
        assert_eq!(title(&m).as_deref(), Some("Couldn't read the firmware lists"));
        assert!(m.lists_told);
    }

    #[test]
    fn an_old_firmware_lists_failure_is_never_told_by_an_answer_without_a_refresh() {
        let (mut m, now) = with_firmware();
        let mut r = m.last.clone().unwrap();
        r.firmware = None;
        r.firmware_error = Some(std::sync::Arc::new(ClientError::Timeout));
        m.notice = Some(Guidance::done("Saved", "Something else."));
        m.finish(Ok(Outcome::Refreshed(Some(r))), now);
        assert!(m.last.as_ref().unwrap().firmware_error.is_some());
        m.notice = None;
        // Loading a package file brings no refresh: the error left from before stays unsaid.
        let (loaded, _) = usb_package("esp32s3", false);
        m.finish(Ok(Outcome::PackageLoaded(Box::new(loaded))), now);
        assert!(m.notice.is_none(), "{:?}", m.notice);
        assert!(!m.lists_told);
    }

    #[test]
    fn pairing_again_tells_about_unreadable_firmware_lists_afresh() {
        let failing = |m: &Model| {
            let mut r = m.last.clone().unwrap();
            r.firmware = None;
            r.firmware_error = Some(std::sync::Arc::new(ClientError::Timeout));
            r
        };
        let title = |m: &Model| m.notice.as_ref().map(|n| n.title.clone());
        let (mut m, now) = with_firmware();
        m.finish(Ok(Outcome::Refreshed(Some(failing(&m)))), now);
        assert!(m.lists_told);
        m.notice = None;
        // Paired again while the lists still can't be read: the pairing's notice shows first, and
        // the failure is told again at the next refresh, not taken as already said.
        let hub = m.hub.clone().unwrap();
        let refresh = Some(failing(&m));
        m.finish(Ok(Outcome::Paired { hub, refresh }), now);
        assert_eq!(title(&m).as_deref(), Some("Paired"));
        assert!(!m.lists_told);
        m.notice = None;
        m.finish(Ok(Outcome::Refreshed(Some(failing(&m)))), now);
        assert_eq!(title(&m).as_deref(), Some("Couldn't read the firmware lists"));
    }

    #[test]
    fn a_refused_offer_or_upload_gets_a_title_that_says_what_it_was() {
        let conflict = |msg: &str| {
            Failure::Client(ClientError::Status {
                status: 409,
                body: Some(hub_protocol::ErrorBody::new(
                    hub_protocol::ErrorCode::Conflict,
                    msg,
                )),
            })
        };
        let (mut m, now) = with_firmware();
        m.ask_update("d", &"c".repeat(64), now);
        m.confirm_yes();
        assert!(matches!(m.confirm_yes(), Some(Op::OfferFirmware { .. })));
        m.finish(
            Err(conflict(
                "This firmware is 3145728 bytes, too big for the display's spare space of 2097152 bytes.",
            )),
            now,
        );
        let n = m.notice.as_ref().unwrap();
        assert_eq!(n.title, "The display would refuse this update");
        assert_eq!(
            n.detail,
            "This firmware is 3.0 MB, too big for the display's spare space of 2.0 MB."
        );

        assert!(m.upload_firmware("x.dhfw".into()).is_some());
        m.finish(
            Err(conflict(
                "The hub keeps at most 64 MiB of firmware from each publisher, and this package would take yours \
                 past that.",
            )),
            now,
        );
        let n = m.notice.as_ref().unwrap();
        assert_eq!(n.title, "The hub has no room for this package");
        assert!(n.detail.contains("64 MiB"));

        assert!(m.upload_firmware("x.dhfw".into()).is_some());
        m.finish(
            Err(Failure::Client(ClientError::Status {
                status: 408,
                body: Some(hub_protocol::ErrorBody::new(
                    hub_protocol::ErrorCode::Unavailable,
                    "The upload sent nothing for 60 s, so the hub gave up on it. Try again.",
                )),
            })),
            now,
        );
        let n = m.notice.as_ref().unwrap();
        assert_eq!(n.title, "The upload stopped part way");
        assert!(n.detail.contains("sent nothing for 60 s"));
    }

    #[test]
    fn a_failed_write_points_to_the_button_the_card_shows() {
        let (mut m, now) = with_firmware();
        let mut o = offer("failed", Some("flash"));
        // The offer was for 1.1.0, which the card offers under the other versions.
        o.package_id = "a".repeat(64);
        o.firmware_version = "1.1.0".into();
        m.last.as_mut().unwrap().displays[0]
            .firmware
            .as_mut()
            .unwrap()
            .offer = Some(o.clone());
        let fw = m.display_firmware("d", now).unwrap();
        let retry = fw.retry(&o.package_id);
        assert_eq!(retry, Some(firmware::Retry::Install("1.1.0".into())));
        let (_, detail) = crate::guidance::offer_words(
            &fw.offer.as_ref().unwrap().1,
            &o,
            Some("1.2.0"),
            "5 minutes",
            retry.as_ref(),
        );
        assert!(
            detail.contains("To try again, press Install 1.1.0, under Other versions on the hub."),
            "{detail}"
        );
        assert!(!detail.contains("Press Update"), "{detail}");
        // The headline's package says Update.
        assert_eq!(
            fw.retry(&"c".repeat(64)),
            Some(firmware::Retry::Update("1.3.0-beta.1".into()))
        );
        assert_eq!(fw.retry(&"z".repeat(64)), None);
    }

    fn ready_to_install(m: &mut Model, key_set: bool) {
        let (loaded, _) = usb_package("esp32s3", key_set);
        m.finish(Ok(Outcome::PackageLoaded(Box::new(loaded))), Instant::now());
        assert_eq!(m.fw.install.stage, InstallStage::Ready);
    }

    #[test]
    fn the_official_firmware_that_came_with_the_app_needs_no_file() {
        // Without one, there is nothing to use or add.
        let (mut m, _) = connected(vec![]);
        assert!(m.bundled().is_none());
        assert!(!m.bundled_on_hub());
        assert!(m.upload_bundled().is_none());
        assert!(m.use_bundled().is_none());
        assert_eq!(m.fw.install.stage, InstallStage::Choose);
        assert!(!m.fw.install.open);

        let (loaded, _) = usb_package("esp32s3", true);
        let (m0, now) = with_firmware();
        let mut m = m0.with_bundled(Some(firmware::Bundled {
            loaded: loaded.clone(),
        }));
        assert_eq!(m.bundled().map(|b| b.version()), Some("1.0.0"));

        // Install firmware: straight to the details, with the very package that was checked, and
        // looking for the display's port as after choosing a file.
        assert!(matches!(m.use_bundled(), Some(Op::ListPorts)));
        assert_eq!(m.fw.install.stage, InstallStage::Ready);
        assert!(m.fw.install.open);
        m.finish(Ok(Outcome::Ports(vec![])), now);
        assert!(m.use_bundled().is_none(), "one is already chosen");
        assert_eq!(
            m.fw.install.loaded.as_ref().map(|l| l.checked.package_id),
            Some(loaded.checked.package_id)
        );
        // Closing the install keeps the package for next time.
        m.close_install();
        assert!(m.bundled().is_some());

        // Adding it to the hub sends the checked package, and then there is nothing left to add.
        assert!(!m.bundled_on_hub());
        match m.upload_bundled() {
            Some(Op::UploadLoaded { loaded: sent }) => assert_eq!(sent.bytes, loaded.bytes),
            other => panic!("{other:?}"),
        }
        let mut on_hub = package("a", "1.0.0", "k");
        on_hub.package_id = loaded.checked.package_id.to_hex();
        let mut r = m.last.clone().unwrap();
        r.firmware.as_mut().unwrap().packages.push(on_hub.clone());
        m.finish(
            Ok(Outcome::Uploaded {
                package: Box::new(on_hub),
                new: true,
                refresh: Some(r),
            }),
            now,
        );
        assert!(m.bundled_on_hub(), "{:?}", m.notice);
        // A copy the hub can't read doesn't count: adding it again repairs it.
        m.last.as_mut().unwrap().firmware.as_mut().unwrap().packages[3].damaged = true;
        assert!(!m.bundled_on_hub());
    }

    #[test]
    fn a_package_without_usb_parts_cant_be_installed() {
        let (mut m, _) = connected(vec![]);
        let key = firmware_package::SigningKey::generate().unwrap().0;
        let built = firmware_package::build_and_sign(
            &key,
            &firmware_package::Release {
                device_type: "desk".into(),
                hardware_revisions: vec!["r".into()],
                firmware_version: "1.0.0".into(),
                ..Default::default()
            },
            &[1; 10],
        )
        .unwrap();
        let loaded = Loaded::check(built.bytes).unwrap();
        m.finish(Ok(Outcome::PackageLoaded(Box::new(loaded))), Instant::now());
        assert_eq!(m.fw.install.stage, InstallStage::Choose);
        assert_eq!(
            m.notice.as_ref().unwrap().title,
            "This package can't be installed over USB"
        );
    }

    #[test]
    fn install_shows_the_fingerprint_and_asks_before_anything_is_written() {
        let (mut m, _) = connected(vec![]);
        m.last.as_mut().unwrap().firmware = Some(Shelf::default());
        ready_to_install(&mut m, true);
        let signer = m.install_signer().unwrap();
        assert_eq!(signer.kind, SignerKind::Unknown);
        // Wi-Fi is needed: the install erases it.
        assert!(m.ask_install("COM5").is_none());
        assert!(m.confirm.is_none());
        assert_eq!(m.notice.as_ref().unwrap().title, "Enter the Wi-Fi network's name");
        m.fw.install.ssid.set("Home");
        m.fw.install.password.set("correct horse");
        m.fw.install.maker.set("Kea Displays");
        assert!(m.ask_install("COM5").is_none(), "only a question so far");
        let (title, detail, action) = m.confirm.as_ref().unwrap().question();
        assert_eq!(title, "Erase the display on COM5 and install this firmware?");
        assert!(detail.contains("a maker this app doesn't know"));
        assert!(detail.contains(&signer.fingerprint));
        assert!(detail.contains("erased"));
        assert_eq!(action, "Erase and install");
        // Every key the display will trust, and the package's own words kept apart.
        let keys = m.fw.install.loaded.as_ref().unwrap().trusted_keys(&[]);
        assert_eq!(keys.len(), 2);
        for k in &keys {
            assert!(detail.contains(&k.fingerprint), "{detail}");
        }
        assert!(detail.contains("one of these 2 keys"), "{detail}");
        assert!(
            !detail.contains("First release."),
            "the maker's note isn't the app's words"
        );
        let says = m.confirm.as_ref().unwrap().package_says().unwrap();
        assert_eq!(says.note.as_deref(), Some("First release."));
        assert!(says.facts.contains(&("Device type".into(), "desk".into())));
        assert!(says.facts.contains(&("Hardware".into(), "simulated".into())));
        let Some(Op::Install(job)) = m.confirm_yes() else {
            panic!("an install")
        };
        assert_eq!(job.port, "COM5");
        assert_eq!(job.name, "Desk");
        assert_eq!(job.firmware.trusted_keys.len(), 2, "the package's key set");
        assert_eq!(job.firmware.security_counter, 4);
        assert_eq!(job.label, Some((signer.key_id.clone(), "Kea Displays".into())));
        assert_eq!(job.hosts.last().map(String::as_str), Some("192.168.1.20"));
        assert!(!format!("{job:?}").contains("correct horse"));
        assert!(m.fw.install.password.text.is_empty(), "the password isn't kept");
        assert_eq!(m.fw.install.stage, InstallStage::Running { port: "COM5".into() });
        job.progress.set(Step::Erasing);
        assert_eq!(
            m.install_progress().step,
            Step::Erasing,
            "the view sees the worker's progress"
        );
    }

    fn running(m: &mut Model) {
        ready_to_install(m, false);
        m.fw.install.ssid.set("Home");
        m.ask_install("COM5");
        assert!(matches!(m.confirm_yes(), Some(Op::Install(_))));
    }

    #[test]
    fn a_stopped_install_explains_what_to_do_and_keeps_the_package() {
        let (mut m, now) = connected(vec![]);
        running(&mut m);
        m.finish(
            Err(Failure::Install(InstallError::Board {
                error: BoardError::NotInDownloadMode("no sync".into()),
                erased: false,
            })),
            now,
        );
        let n = m.notice.as_ref().unwrap();
        assert_eq!(n.title, "The board didn't start its loader");
        assert!(
            n.detail
                .contains("hold the BOOT button while you plug it back in"),
            "{}",
            n.detail
        );
        assert!(n.detail.contains("If your board has a RESET button"));
        assert!(n.detail.contains("Nothing was written"));
        assert_eq!(m.fw.install.stage, InstallStage::Ready);
        assert!(m.fw.install.loaded.is_some());

        for (e, title) in [
            (
                InstallError::Board {
                    error: BoardError::NoBoard("x".into()),
                    erased: false,
                },
                "No board found on COM5",
            ),
            (
                InstallError::Board {
                    error: BoardError::Busy("x".into()),
                    erased: false,
                },
                "COM5 is busy",
            ),
            (
                InstallError::WrongChip {
                    expected: "esp32s3".into(),
                    found: "esp32c3".into(),
                },
                "This firmware is for a different chip",
            ),
            (
                InstallError::FlashTooSmall {
                    needed: 16 << 20,
                    found: 4 << 20,
                },
                "This board's flash is too small",
            ),
        ] {
            running(&mut m);
            m.finish(Err(Failure::Install(e)), now);
            assert_eq!(m.notice.as_ref().unwrap().title, title);
        }
        running(&mut m);
        m.finish(
            Err(Failure::Install(InstallError::Board {
                error: BoardError::Lost("cable".into()),
                erased: true,
            })),
            now,
        );
        assert!(m
            .notice
            .as_ref()
            .unwrap()
            .detail
            .contains("won't start properly until an install finishes"));
    }

    fn usb_display() -> UsbDisplay {
        UsbDisplay {
            device: DeviceInfo {
                device_type: "desk".into(),
                firmware_version: "1.0.0".into(),
                hardware_revision: "simulated".into(),
                protocol_version: 1,
                firmware: None,
            },
            provisioned: false,
        }
    }

    #[test]
    fn after_the_install_the_displays_screen_watches_for_the_new_display() {
        let (mut m, now) = connected(vec![]);
        running(&mut m);
        m.finish(
            Ok(Outcome::Installed {
                after: AfterFlash::Sent(usb_display()),
                refresh: None,
                label: MakerName::Done,
            }),
            now,
        );
        assert!(matches!(m.fw.install.stage, InstallStage::Done { .. }));
        assert!(matches!(m.enrol.stage, EnrolStage::Waiting { .. }));
        assert!(m
            .notice
            .as_ref()
            .unwrap()
            .title
            .starts_with("Installed \"desk\" 1.0.0"));
    }

    #[test]
    fn a_maker_name_the_hub_didnt_save_is_reported_but_the_install_still_counts() {
        let (mut m, now) = connected(vec![]);
        m.last.as_mut().unwrap().firmware = Some(Shelf::default());
        running(&mut m);
        m.fw.install.maker.set("Kea Displays");
        let refused = ClientError::Status {
            status: 409,
            body: Some(hub_protocol::ErrorBody::new(
                hub_protocol::ErrorCode::Conflict,
                "nope",
            )),
        };
        m.finish(
            Ok(Outcome::Installed {
                after: AfterFlash::Sent(usb_display()),
                refresh: None,
                label: MakerName::Failed(refused),
            }),
            now,
        );
        assert!(matches!(m.fw.install.stage, InstallStage::Done { .. }));
        let n = m.notice.as_ref().unwrap();
        assert_eq!(n.tone, Tone::Done);
        assert!(
            n.detail
                .contains("The name \"Kea Displays\" for the maker's key wasn't saved on the hub"),
            "{}",
            n.detail
        );
        assert!(n.detail.contains("nope"), "{}", n.detail);
    }

    #[test]
    fn a_maker_name_not_saved_is_reported_whichever_way_the_install_ended() {
        let refused = || ClientError::Status {
            status: 409,
            body: Some(hub_protocol::ErrorBody::new(
                hub_protocol::ErrorCode::Conflict,
                "nope",
            )),
        };
        // The setup details weren't taken over USB, and the hub refused the name.
        let (mut m, now) = connected(vec![]);
        m.last.as_mut().unwrap().firmware = Some(Shelf::default());
        running(&mut m);
        m.fw.install.maker.set("Kea Displays");
        let bundle: Bundle = serde_json::from_str(include_str!(
            "../../../../protocol/v1/examples/device/provisioning-bundle.json"
        ))
        .unwrap();
        m.finish(
            Ok(Outcome::Installed {
                after: AfterFlash::Usb {
                    error: UsbError::FirmwareKeysLocked,
                    bundle: Box::new(bundle),
                },
                refresh: None,
                label: MakerName::Failed(refused()),
            }),
            now,
        );
        let d = &m.notice.as_ref().unwrap().detail;
        assert!(
            d.contains("The name \"Kea Displays\" for the maker's key wasn't saved on the hub")
                && d.contains("nope"),
            "{d}"
        );
        // The hub couldn't be reached at all, so the name wasn't tried.
        let (mut m, now) = connected(vec![]);
        m.last.as_mut().unwrap().firmware = Some(Shelf::default());
        running(&mut m);
        m.fw.install.maker.set("Kea Displays");
        let after = AfterFlash::Hub(ClientError::Timeout);
        assert!(!after.hub_reachable());
        m.finish(
            Ok(Outcome::Installed {
                after,
                refresh: None,
                label: MakerName::Unreachable,
            }),
            now,
        );
        let d = &m.notice.as_ref().unwrap().detail;
        assert!(
            d.contains("The name \"Kea Displays\" for the maker's key wasn't saved either, as the hub couldn't be reached"),
            "{d}"
        );
        // A hub that answered with a refusal can still be told the name.
        assert!(AfterFlash::Hub(refused()).hub_reachable());
        assert!(AfterFlash::Sent(usb_display()).hub_reachable());
    }

    #[test]
    fn an_older_hub_gets_no_maker_name() {
        let (mut m, _) = connected(vec![]);
        assert!(!m.hub_has_firmware());
        ready_to_install(&mut m, false);
        m.fw.install.ssid.set("Home");
        m.fw.install.maker.set("Kea Displays");
        m.ask_install("COM5");
        let Some(Op::Install(job)) = m.confirm_yes() else {
            panic!("an install")
        };
        assert_eq!(job.label, None);
    }

    #[test]
    fn keys_locked_or_a_lost_answer_keeps_the_setup_details_with_their_keys() {
        let (mut m, now) = connected(vec![]);
        running(&mut m);
        let mut bundle: Bundle = serde_json::from_str(include_str!(
            "../../../../protocol/v1/examples/device/provisioning-bundle.json"
        ))
        .unwrap();
        bundle.firmware = m.fw.install.loaded.as_ref().map(|l| bundle_firmware(&l.checked));
        m.finish(
            Ok(Outcome::Installed {
                after: AfterFlash::Usb {
                    error: UsbError::FirmwareKeysLocked,
                    bundle: Box::new(bundle),
                },
                refresh: None,
                label: MakerName::Done,
            }),
            now,
        );
        let n = m.notice.as_ref().unwrap();
        assert_eq!(
            n.title,
            "The firmware is installed, but the setup details weren't sent"
        );
        assert!(
            n.detail.contains("This display trusts another maker's firmware"),
            "{}",
            n.detail
        );
        assert!(n.detail.contains("Only Install firmware"), "{}", n.detail);
        assert_eq!(m.section, Section::Displays);
        assert_eq!(m.enrol.stage, EnrolStage::Ready);
        // The keys wait for the same port, and go with the details sent there.
        assert!(m.enrol.bundle.as_ref().unwrap().firmware.is_none());
        assert_eq!(m.pending_keys().unwrap().port, "COM5");
        match m.send_usb("COM5") {
            Some(Op::SendUsb { port, bundle }) => {
                assert_eq!(port, "COM5");
                assert!(bundle.firmware.is_some());
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn when_the_hub_cant_make_setup_details_the_next_setup_carries_the_keys() {
        let (mut m, now) = connected(vec![]);
        running(&mut m);
        let fw = bundle_firmware(&m.fw.install.loaded.as_ref().unwrap().checked);
        m.finish(
            Ok(Outcome::Installed {
                after: AfterFlash::Hub(ClientError::Timeout),
                refresh: None,
                label: MakerName::Done,
            }),
            now,
        );
        assert_eq!(m.pending_keys().map(|p| &p.firmware), Some(&fw));
        assert_eq!(m.pending_keys().unwrap().port, "COM5");
        assert!(m
            .notice
            .as_ref()
            .unwrap()
            .detail
            .contains("adds the firmware's keys"));
        // Add a display: the bundle it makes carries them.
        let op = m.create_setup();
        assert!(matches!(op, Some(Op::NewEnrolment { .. })), "{:?}", m.notice);
        let grant = hub_core::api::EnrolmentGrant {
            secret: "s".repeat(22),
            expires_at: ts("2026-11-10T20:10:00Z"),
            identity: hub_core::api::HubIdentity {
                certificate_pem: include_str!(
                    "../../../../protocol/v1/examples/device/provisioning-bundle.json"
                )
                .split("\"certificate_pem\": \"")
                .nth(1)
                .and_then(|r| r.split('"').next())
                .unwrap()
                .replace("\\n", "\n"),
                spki_sha256: "ab".repeat(32),
                server_name: hub_protocol::provisioning::HUB_SERVER_NAME.into(),
            },
        };
        m.finish(Ok(Outcome::Enrolment { grant, refresh: None }), now);
        assert_eq!(m.enrol.stage, EnrolStage::Ready, "{:?}", m.notice);
        match m.send_usb("COM5") {
            Some(Op::SendUsb { bundle, .. }) => assert_eq!(bundle.firmware, Some(fw)),
            other => panic!("{other:?}"),
        }
    }

    /// A model whose install on COM5 worked but whose setup details the hub couldn't make, with the
    /// setup details then made: the keys are waiting for COM5.
    fn keys_waiting() -> (Model, Instant) {
        let (mut m, now) = connected(vec![]);
        running(&mut m);
        m.finish(
            Ok(Outcome::Installed {
                after: AfterFlash::Hub(ClientError::Timeout),
                refresh: None,
                label: MakerName::Done,
            }),
            now,
        );
        m.enrol.bundle = Some(
            serde_json::from_str(include_str!(
                "../../../../protocol/v1/examples/device/provisioning-bundle.json"
            ))
            .unwrap(),
        );
        m.enrol.bundle.as_mut().unwrap().firmware = None;
        m.enrol.stage = EnrolStage::Ready;
        (m, now)
    }

    #[test]
    fn waiting_keys_go_only_to_the_port_just_flashed_and_can_be_left_out() {
        // Another port: sent without the keys, which are dropped.
        let (mut m, _) = keys_waiting();
        let p = m.pending_keys().unwrap();
        assert!(p.include);
        assert_eq!(p.keys.len(), 1, "the signer, as the package has no key set");
        match m.send_usb("COM9") {
            Some(Op::SendUsb { bundle, .. }) => assert!(bundle.firmware.is_none()),
            other => panic!("{other:?}"),
        }
        assert!(m.pending_keys().is_none());
        m.finish(Ok(Outcome::Sent(usb_display())), Instant::now());
        assert!(
            m.notice
                .as_ref()
                .unwrap()
                .detail
                .contains("weren't sent with them"),
            "{:?}",
            m.notice
        );

        // Left out by the owner: the same port gets the details without them.
        let (mut m, _) = keys_waiting();
        m.include_pending_keys(false);
        match m.send_usb("COM5") {
            Some(Op::SendUsb { bundle, .. }) => assert!(bundle.firmware.is_none()),
            other => panic!("{other:?}"),
        }
        m.finish(Ok(Outcome::Sent(usb_display())), Instant::now());
        assert!(m.pending_keys().is_none(), "done with once sent");

        // Busy: nothing is sent to the other port, and the keys still wait.
        let (mut m, _) = keys_waiting();
        m.busy = Some(Busy::SavingFile);
        assert!(m.send_usb("COM9").is_none());
        assert_eq!(m.pending_keys().unwrap().port, "COM5");

        // Saved to a file: never with the keys. They wait while it is saved, in case that fails, and
        // once it is saved they are dropped, as nothing more can be sent from here: the notice says
        // so.
        let (mut m, _) = keys_waiting();
        match m.save_bundle(PathBuf::from("setup.json")) {
            Some(Op::SaveBundle { bundle, .. }) => assert!(bundle.firmware.is_none()),
            other => panic!("{other:?}"),
        }
        assert!(m.pending_keys().unwrap().include);
        m.finish(Err(Failure::File("The disk is full.".into())), Instant::now());
        assert_eq!(m.pending_keys().unwrap().port, "COM5", "kept after a failed save");
        assert!(m.save_bundle(PathBuf::from("setup.json")).is_some());
        m.finish(Ok(Outcome::Saved(PathBuf::from("setup.json"))), Instant::now());
        assert!(m.pending_keys().is_none());
        assert!(matches!(m.enrol.stage, EnrolStage::Waiting { .. }));
        let n = m.notice.as_ref().unwrap();
        assert!(
            n.detail.contains("just installed on COM5 aren't in it"),
            "{}",
            n.detail
        );
        // Left out by the owner beforehand: nothing to say.
        let (mut m, _) = keys_waiting();
        m.include_pending_keys(false);
        m.save_bundle(PathBuf::from("setup.json"));
        m.finish(Ok(Outcome::Saved(PathBuf::from("setup.json"))), Instant::now());
        assert!(m.pending_keys().is_none());
        assert!(!m.notice.as_ref().unwrap().detail.contains("keys"));

        // Cancelling Add a display drops them; so does starting another install.
        let (mut m, _) = keys_waiting();
        m.close_enrol();
        assert!(m.pending_keys().is_none());
        let (mut m, _) = keys_waiting();
        running(&mut m);
        assert!(m.pending_keys().is_none());

        // Sent with them: the notice says so.
        let (mut m, _) = keys_waiting();
        assert!(matches!(m.send_usb("COM5"), Some(Op::SendUsb { bundle, .. }) if bundle.firmware.is_some()));
        m.finish(Ok(Outcome::Sent(usb_display())), Instant::now());
        assert!(m.notice.as_ref().unwrap().detail.contains("now trusts the keys"));
        assert!(m.pending_keys().is_none());
    }

    #[test]
    fn a_damaged_file_is_explained_and_nothing_is_sent() {
        let (mut m, now) = connected(vec![]);
        let op = m.upload_firmware("x.dhfw".into());
        assert!(matches!(op, Some(Op::UploadFirmware { .. })));
        m.finish(Err(Failure::Package("the package ended early".into())), now);
        let n = m.notice.as_ref().unwrap();
        assert_eq!(n.title, "This firmware file failed its checks");
        assert!(n
            .detail
            .starts_with("The package ended early. Nothing was sent or written."));
    }
}
