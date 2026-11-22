//! Runs the model's operations: network calls on a tokio runtime, and blocking work (the credential
//! store, files, the serial port) on its blocking threads. GPUI awaits the result (`app.rs`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use hub_client::HubClient;
use hub_core::{Clock, SystemClock};
use hub_protocol::pairing::hex;
use hub_protocol::provisioning::Bundle;
use zeroize::Zeroizing;

use crate::firmware::{self, Board, Loaded};
use crate::model::{
    AddFailure, AfterFlash, Failure, InstallJob, MakerName, Op, Outcome, PackageSource, PortInfo, Refresh,
    Shelf,
};
use crate::settings::{self, SavedHub};
use os_secrets::TokenStore;

fn runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("hub-worker")
            .enable_all()
            .build()
            .expect("the worker runtime starts")
    })
}

/// Runs `f` on the worker runtime. The handle is a future any executor can await.
pub fn spawn<F>(f: F) -> tokio::task::JoinHandle<F::Output>
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    runtime().spawn(f)
}

#[derive(Clone)]
struct Session {
    client: HubClient,
    token: Arc<Zeroizing<String>>,
}

/// Makes the board on a serial port, for a first install. Tests put a fake one in.
pub type Boards = Arc<dyn Fn(&str) -> Box<dyn Board> + Send + Sync>;

/// What the worker needs: where to keep things, and the current connection.
pub struct Ctx {
    pub store: Arc<dyn TokenStore>,
    pub settings_dir: PathBuf,
    session: Mutex<Option<Session>>,
    boards: Boards,
}

impl Ctx {
    pub fn new(store: Arc<dyn TokenStore>, settings_dir: PathBuf) -> Arc<Self> {
        Self::with_boards(
            store,
            settings_dir,
            Arc::new(|port: &str| Box::new(crate::flasher::EspBoard::new(port)) as Box<dyn Board>),
        )
    }

    pub fn with_boards(store: Arc<dyn TokenStore>, settings_dir: PathBuf, boards: Boards) -> Arc<Self> {
        Arc::new(Self {
            store,
            settings_dir,
            session: Mutex::new(None),
            boards,
        })
    }

    /// Picks up the saved hub's credential at start-up. `Ok(false)` means it is missing, so the
    /// owner must pair again.
    pub fn resume(&self, hub: &SavedHub) -> Result<bool, Failure> {
        let Some(token) = self.store.load(&hub.token_account()).map_err(Failure::Store)? else {
            return Ok(false);
        };
        let pin = hub
            .pin()
            .ok_or_else(|| Failure::Settings("the saved hub key is unreadable".into()))?;
        let client = HubClient::pinned(&hub.host, hub.port, vec![pin]).map_err(Failure::Client)?;
        *self.session.lock().unwrap() = Some(Session {
            client,
            token: Arc::new(token),
        });
        Ok(true)
    }

    fn session(&self) -> Result<Session, Failure> {
        self.session.lock().unwrap().clone().ok_or_else(|| {
            Failure::Client(hub_client::ClientError::Status {
                status: 401,
                body: Some(hub_protocol::ErrorBody::new(
                    hub_protocol::ErrorCode::Unauthorised,
                    "not paired",
                )),
            })
        })
    }
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    tokio::task::spawn_blocking(f)
        .await
        .expect("blocking work doesn't panic")
}

/// What each display shows, from the display list (the hub sends it with each display). A display
/// the hub didn't say anything about is left out, so it is never read as having no pages ticked.
fn ticks(displays: &[hub_core::api::DisplayView]) -> HashMap<String, Vec<String>> {
    displays
        .iter()
        .filter_map(|d| Some((d.id.clone(), d.pages.clone()?)))
        .collect()
}

async fn refresh(s: &Session) -> Result<Refresh, Failure> {
    let t = s.token.as_str();
    let (status, pages, displays, preview, helpers) = tokio::try_join!(
        s.client.status(t),
        s.client.pages(t),
        s.client.displays(t),
        s.client.preview_snapshot(t),
        s.client.helpers(t),
    )
    .map_err(Failure::Client)?;
    let ticks = ticks(&displays);
    let (firmware, firmware_error) = match shelf(s).await {
        Ok(f) => (f, None),
        Err(e) => (None, Some(Arc::new(e))),
    };
    Ok(Refresh {
        status,
        pages,
        displays,
        preview,
        helpers,
        ticks,
        firmware,
        firmware_error,
    })
}

/// Whether a failure means the hub has no such route: a hub older than signed updates.
fn no_such_route(e: &hub_client::ClientError) -> bool {
    matches!(
        e,
        hub_client::ClientError::Status {
            status: 404 | 405,
            ..
        }
    )
}

/// The firmware lists. `Ok(None)` from a hub without signed updates, which isn't a failure; any
/// other failure is one, and the model keeps the lists it had.
async fn shelf(s: &Session) -> Result<Option<Shelf>, hub_client::ClientError> {
    let t = s.token.as_str();
    match tokio::try_join!(
        s.client.firmware_packages(t),
        s.client.firmware_signers(t),
        s.client.publishers(t),
    ) {
        Ok((packages, signers, publishers)) => Ok(Some(Shelf {
            packages,
            signers,
            publishers,
        })),
        Err(e) if no_such_route(&e) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Reads a whole package file, refusing one too large to be a package before reading it.
fn read_package(path: &Path) -> Result<Vec<u8>, Failure> {
    let fail = |e: std::io::Error| Failure::ReadFile(format!("{}: {e}", path.display()));
    let len = std::fs::metadata(path).map_err(fail)?.len();
    if len > hub_protocol::limits::MAX_FIRMWARE_PACKAGE_BYTES {
        return Err(Failure::Package(format!(
            "the file is {len} bytes, more than a package can be"
        )));
    }
    std::fs::read(path).map_err(fail)
}

fn check_package(bytes: Vec<u8>) -> Result<Loaded, Failure> {
    Loaded::check(bytes).map_err(|e| Failure::Package(e.to_string()))
}

/// Sends a checked package to the hub, then refreshes.
async fn upload(s: &Session, bytes: Arc<Vec<u8>>) -> Result<Outcome, Failure> {
    let bytes = Arc::try_unwrap(bytes).unwrap_or_else(|b| (*b).clone());
    let (package, new) = s
        .client
        .upload_firmware(&s.token, bytes)
        .await
        .map_err(Failure::Client)?;
    Ok(Outcome::Uploaded {
        package: Box::new(package),
        new,
        refresh: refresh_after(s).await,
    })
}

/// After a first install the board restarts, and a board with the chip's own USB disappears from the
/// PC and comes back. Opens the port again until the display answers, for up to 40 seconds.
pub fn send_after_install(
    port: &str,
    bundle: &Bundle,
) -> Result<hub_client::usb::UsbDisplay, hub_client::usb::UsbError> {
    use hub_client::usb::UsbError;
    let deadline = std::time::Instant::now() + Duration::from_secs(40);
    std::thread::sleep(Duration::from_millis(1500));
    loop {
        match send_usb(port, bundle) {
            Err(UsbError::Port(_) | UsbError::NoAnswer | UsbError::Stopped)
                if std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_secs(1))
            }
            r => return r,
        }
    }
}

/// The provisioning half of a first install, once the board has restarted and the port is free:
/// setup details from the hub, with the package's keys, sent over USB.
fn provision_installed(rt: &tokio::runtime::Handle, s: &Session, job: &InstallJob) -> AfterFlash {
    let grant = match rt.block_on(s.client.new_enrolment(&s.token, &job.name)) {
        Ok(g) => g,
        Err(e) => return AfterFlash::Hub(e),
    };
    let mut bundle =
        match hub_client::provisioning::bundle(&grant, job.hosts.clone(), job.hub_port, job.wifi.clone()) {
            Ok(b) => b,
            Err(e) => return AfterFlash::Hub(hub_client::ClientError::Invalid(e)),
        };
    bundle.firmware = Some(job.firmware.clone());
    let problems = bundle.problems();
    if !problems.is_empty() {
        return AfterFlash::Hub(hub_client::ClientError::Invalid(problems.join("; ")));
    }
    match send_after_install(&job.port, &bundle) {
        Ok(d) => AfterFlash::Sent(d),
        Err(error) => AfterFlash::Usb {
            error,
            bundle: Box::new(bundle),
        },
    }
}

/// The refresh that follows a change. The change is already made, so a refresh that fails must not
/// turn its result into an error (a one-time secret would be thrown away, a form would stay open
/// and be pressed again). It comes back as `None`, and the model marks a refresh as due.
async fn refresh_after(s: &Session) -> Option<Refresh> {
    refresh(s).await.ok()
}

pub async fn execute(ctx: Arc<Ctx>, op: Op) -> Result<Outcome, Failure> {
    let c = Failure::Client;
    match op {
        Op::Pair { pairing, label } => {
            // Check the store first: a token the hub issued but this PC can't keep would be stranded.
            let store = ctx.store.clone();
            blocking(move || store.check()).await.map_err(Failure::Store)?;
            let client =
                HubClient::pinned(&pairing.host, pairing.port, vec![pairing.spki_sha256]).map_err(c)?;
            let resp = client.pair(&pairing.secret_text(), &label).await.map_err(c)?;
            // TLS already proved the key; this catches a hub reporting a different one.
            if resp.identity.spki_sha256 != hex(&pairing.spki_sha256) {
                return Err(c(hub_client::ClientError::IdentityMismatch));
            }
            let hub = SavedHub::new(
                &pairing.host,
                pairing.port,
                &pairing.spki_sha256,
                &label,
                SystemClock.now(),
            );
            let token = Zeroizing::new(resp.admin_token.clone());
            let (store, account, saved_token) = (ctx.store.clone(), hub.token_account(), token.clone());
            blocking(move || store.save(&account, &saved_token))
                .await
                .map_err(Failure::Store)?;
            let (dir, saved) = (ctx.settings_dir.clone(), hub.clone());
            blocking(move || settings::save(&dir, Some(&saved)))
                .await
                .map_err(Failure::Settings)?;
            let session = Session {
                client,
                token: Arc::new(token),
            };
            *ctx.session.lock().unwrap() = Some(session.clone());
            Ok(Outcome::Paired {
                hub,
                refresh: refresh_after(&session).await,
            })
        }
        Op::Refresh => Ok(Outcome::Refreshed(Some(refresh(&ctx.session()?).await?))),
        Op::AddPages { pages } => {
            let s = ctx.session()?;
            let total = pages.len();
            let mut failed = None;
            for (added, new) in pages.iter().enumerate() {
                if let Err(error) = s.client.add_page_with_samples(&s.token, new).await {
                    // Nothing was added yet: an ordinary failure. Otherwise the pages already added
                    // stay, so say how far it got.
                    if added == 0 {
                        return Err(c(error));
                    }
                    failed = Some(AddFailure {
                        added,
                        total,
                        title: new.title.clone(),
                        error,
                    });
                    break;
                }
            }
            // The pages are on the hub now, so a refresh that fails must not fail the whole
            // operation: the form would stay open and Add would make them twice.
            Ok(Outcome::Added {
                refresh: refresh_after(&s).await,
                failed,
            })
        }
        Op::SaveEdit { id, change, content } => {
            let s = ctx.session()?;
            s.client
                .change_page_with_samples(&s.token, &id, &change)
                .await
                .map_err(c)?;
            if let Some(content) = content {
                s.client
                    .set_page_content(&s.token, &id, &content)
                    .await
                    .map_err(c)?;
            }
            Ok(Outcome::Refreshed(refresh_after(&s).await))
        }
        Op::RemovePage { id } => {
            let s = ctx.session()?;
            s.client.remove_page(&s.token, &id).await.map_err(c)?;
            Ok(Outcome::Refreshed(refresh_after(&s).await))
        }
        Op::Reorder { ids } => {
            let s = ctx.session()?;
            s.client.reorder_pages(&s.token, ids).await.map_err(c)?;
            Ok(Outcome::Refreshed(refresh_after(&s).await))
        }
        Op::RevokeDisplay { id } => {
            let s = ctx.session()?;
            s.client.revoke_display(&s.token, &id).await.map_err(c)?;
            Ok(Outcome::Refreshed(refresh_after(&s).await))
        }
        Op::SetDisplayPages { display, ids } => {
            let s = ctx.session()?;
            s.client
                .set_display_pages(&s.token, &display, ids)
                .await
                .map_err(c)?;
            Ok(Outcome::Refreshed(refresh_after(&s).await))
        }
        Op::NewEnrolment { name } => {
            let s = ctx.session()?;
            let grant = s.client.new_enrolment(&s.token, &name).await.map_err(c)?;
            Ok(Outcome::Enrolment {
                grant,
                refresh: refresh_after(&s).await,
            })
        }
        Op::NewHelper(new) => {
            let s = ctx.session()?;
            let invite = s.client.new_helper(&s.token, &new).await.map_err(c)?;
            Ok(Outcome::HelperInvite {
                invite,
                refresh: refresh_after(&s).await,
            })
        }
        Op::RevokeHelper { id } => {
            let s = ctx.session()?;
            s.client.revoke_helper(&s.token, &id).await.map_err(c)?;
            Ok(Outcome::Refreshed(refresh_after(&s).await))
        }
        Op::SaveGuide { dismissed } => {
            let dir = ctx.settings_dir.clone();
            blocking(move || settings::save_guide_dismissed(&dir, dismissed))
                .await
                .map_err(Failure::Settings)?;
            Ok(Outcome::GuideSaved)
        }
        Op::ListPorts => Ok(Outcome::Ports(blocking(list_ports).await)),
        Op::SendUsb { port, bundle } => blocking(move || send_usb(&port, &bundle))
            .await
            .map(Outcome::Sent)
            .map_err(Failure::Usb),
        Op::SaveBundle { path, bundle } => {
            let out = path.clone();
            blocking(move || save_bundle(&path, &bundle))
                .await
                .map_err(Failure::File)?;
            Ok(Outcome::Saved(out))
        }
        Op::Forget => {
            let hub = settings::load(&ctx.settings_dir).ok().flatten();
            if let Some(h) = hub {
                let (store, account) = (ctx.store.clone(), h.token_account());
                blocking(move || store.delete(&account))
                    .await
                    .map_err(Failure::Store)?;
            }
            let dir = ctx.settings_dir.clone();
            blocking(move || settings::save(&dir, None))
                .await
                .map_err(Failure::Settings)?;
            *ctx.session.lock().unwrap() = None;
            Ok(Outcome::Forgotten)
        }
        Op::UploadFirmware { path } => {
            let s = ctx.session()?;
            let bytes = blocking(move || read_package(&path)).await?;
            // Checked here first, so a damaged file is explained without a round trip.
            let bytes = blocking(move || check_package(bytes).map(|l| l.bytes)).await?;
            upload(&s, bytes).await
        }
        // Checked when the app started; the very bytes that passed are what goes to the hub.
        Op::UploadLoaded { loaded } => upload(&ctx.session()?, loaded.bytes).await,
        Op::RemoveFirmware { id } => {
            let s = ctx.session()?;
            s.client.delete_firmware(&s.token, &id).await.map_err(c)?;
            Ok(Outcome::Refreshed(refresh_after(&s).await))
        }
        Op::OfferFirmware { display, package } => {
            let s = ctx.session()?;
            s.client
                .offer_firmware(&s.token, &display, &package)
                .await
                .map_err(c)?;
            Ok(Outcome::Refreshed(refresh_after(&s).await))
        }
        Op::CancelOffer { display } => {
            let s = ctx.session()?;
            s.client
                .cancel_firmware_offer(&s.token, &display)
                .await
                .map_err(c)?;
            Ok(Outcome::Refreshed(refresh_after(&s).await))
        }
        Op::SetSignerLabel { key_id, label } => {
            let s = ctx.session()?;
            match label {
                Some(l) => s.client.set_signer_label(&s.token, &key_id, &l).await.map(|_| ()),
                None => s.client.remove_signer_label(&s.token, &key_id).await,
            }
            .map_err(c)?;
            Ok(Outcome::Refreshed(refresh_after(&s).await))
        }
        Op::NewPublisher { label } => {
            let s = ctx.session()?;
            let invite = s.client.new_publisher(&s.token, &label).await.map_err(c)?;
            Ok(Outcome::PublisherInvite {
                invite,
                refresh: refresh_after(&s).await,
            })
        }
        Op::RevokePublisher { id } => {
            let s = ctx.session()?;
            s.client.revoke_publisher(&s.token, &id).await.map_err(c)?;
            Ok(Outcome::Refreshed(refresh_after(&s).await))
        }
        Op::LoadPackage { from } => {
            let bytes = match from {
                PackageSource::File(path) => blocking(move || read_package(&path)).await?,
                PackageSource::Hub(id) => {
                    let s = ctx.session()?;
                    s.client.firmware_file(&s.token, &id).await.map_err(c)?
                }
            };
            let loaded = blocking(move || check_package(bytes)).await?;
            Ok(Outcome::PackageLoaded(Box::new(loaded)))
        }
        Op::Install(job) => {
            // Paired first: nothing is erased for a PC that can't then make setup details.
            let s = ctx.session()?;
            let board = (ctx.boards)(&job.port);
            let rt = tokio::runtime::Handle::current();
            let session = s.clone();
            let (after, job) = blocking(move || {
                let after = firmware::install(board, &job.loaded, &job.plan, &job.progress, || {
                    provision_installed(&rt, &session, &job)
                });
                (after, job)
            })
            .await;
            let after = after.map_err(Failure::Install)?;
            // The owner's name for the maker's key, after any install the hub can be told about,
            // whether or not the setup details went: the hub keeps it by key id, so it is harmless
            // before a display trusts the key. A failure here doesn't undo the install: the model
            // says so, and the name can be set on the Firmware screen.
            let label = match &job.label {
                None => MakerName::Done,
                Some(_) if !after.hub_reachable() => MakerName::Unreachable,
                Some((key_id, label)) => match s.client.set_signer_label(&s.token, key_id, label).await {
                    Ok(_) => MakerName::Done,
                    Err(e) => MakerName::Failed(e),
                },
            };
            Ok(Outcome::Installed {
                after,
                refresh: refresh_after(&s).await,
                label,
            })
        }
    }
}

fn list_ports() -> Vec<PortInfo> {
    let mut ports: Vec<PortInfo> = serialport::available_ports()
        .unwrap_or_default()
        .into_iter()
        .map(|p| {
            let description = match p.port_type {
                serialport::SerialPortType::UsbPort(u) => {
                    let what = [u.manufacturer, u.product]
                        .into_iter()
                        .flatten()
                        .collect::<Vec<_>>()
                        .join(" ");
                    if what.is_empty() {
                        format!("USB {:04x}:{:04x}", u.vid, u.pid)
                    } else {
                        format!("USB, {what}")
                    }
                }
                serialport::SerialPortType::PciPort => "Built-in serial port".into(),
                serialport::SerialPortType::BluetoothPort => "Bluetooth".into(),
                serialport::SerialPortType::Unknown => "Serial port".into(),
            };
            PortInfo {
                name: p.port_name,
                description,
            }
        })
        .collect();
    drop_mac_dial_in_twins(&mut ports);
    // USB ports first: that's where a display is.
    ports.sort_by_key(|p| (!p.description.starts_with("USB"), p.name.clone()));
    ports
}

/// A Mac lists every serial port twice, as `/dev/cu.NAME` (the one to use) and `/dev/tty.NAME`
/// (which waits for a signal that a USB display never gives). Keeps only the `cu` one.
fn drop_mac_dial_in_twins(ports: &mut Vec<PortInfo>) {
    let names: Vec<String> = ports.iter().map(|p| p.name.clone()).collect();
    ports.retain(|p| match p.name.strip_prefix("/dev/tty.") {
        Some(rest) => !names.contains(&format!("/dev/cu.{rest}")),
        None => true,
    });
}

fn send_usb(port: &str, bundle: &Bundle) -> Result<hub_client::usb::UsbDisplay, hub_client::usb::UsbError> {
    let mut p = serialport::new(port, hub_protocol::usb::BAUD_RATE)
        .timeout(Duration::from_millis(100))
        // Many USB serial devices only send while DTR is set. RTS stays low, because on ESP32
        // boards RTS can hold the chip in reset.
        .dtr_on_open(true)
        .open()
        .map_err(|e| hub_client::usb::UsbError::Port(e.to_string()))?;
    let _ = p.write_request_to_send(false);
    hub_client::usb::provision(&mut p, bundle)
}

fn save_bundle(path: &Path, bundle: &Bundle) -> Result<(), String> {
    use std::io::Write;
    let fail = |e: std::io::Error| format!("couldn't write {}: {e}", path.display());
    // Always a new owner-only file, renamed over the target: writing into an existing file would
    // keep whatever permissions it had, and this one holds a secret and perhaps a Wi-Fi password.
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = path.with_file_name(format!(".{name}.{}.tmp", std::process::id()));
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(&tmp).map_err(fail)?;
    let written = f
        .write_all(&serde_json::to_vec_pretty(bundle).expect("serialises"))
        .and_then(|_| f.sync_all())
        .and_then(|_| std::fs::rename(&tmp, path));
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written.map_err(fail)
}

#[cfg(all(test, unix))]
mod save_tests {
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn saving_over_a_readable_file_leaves_an_owner_only_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("setup.json");
        std::fs::write(&path, "old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let bundle: hub_protocol::provisioning::Bundle = serde_json::from_str(include_str!(
            "../../../protocol/v1/examples/device/provisioning-bundle.json"
        ))
        .unwrap();
        super::save_bundle(&path, &bundle).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(std::fs::read_to_string(&path)
            .unwrap()
            .contains("display-hub.provisioning"));
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            1,
            "no temporary file left"
        );
    }
}

#[cfg(test)]
mod tests {
    //! The manager's whole path against a real hub over pinned HTTPS: pairing through the model,
    //! page changes, and a simulated display picking them up on its next poll.

    use std::time::Instant;

    use display_simulator::render::{count_pages, screen};
    use display_simulator::{paired, Device, NoJitter};
    use hub_core::FakeClock;
    use hub_protocol::pairing::PairingString;

    use super::*;
    use crate::forms::{ContentEdit, Input, Shape};
    use crate::model::{Model, Outcome};
    use crate::shown::Shown;
    use os_secrets::MemoryStore;

    #[test]
    fn a_mac_port_is_listed_once() {
        let port = |name: &str| PortInfo {
            name: name.into(),
            description: "USB".into(),
        };
        let mut ports = vec![
            port("/dev/cu.usbmodem1101"),
            port("/dev/tty.usbmodem1101"),
            port("/dev/tty.lonely"),
            port("/dev/ttyACM0"),
        ];
        drop_mac_dial_in_twins(&mut ports);
        let names: Vec<_> = ports.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["/dev/cu.usbmodem1101", "/dev/tty.lonely", "/dev/ttyACM0"]);
    }

    async fn run(model: &mut Model, ctx: &Arc<Ctx>, op: Option<Op>) {
        let op = op.expect("an operation");
        let result = execute(ctx.clone(), op).await;
        model.finish(result, Instant::now());
    }

    /// Adds pages from a built-in template the way the Pages screen does: pick it, fill in the form.
    async fn add_from(model: &mut Model, ctx: &Arc<Ctx>, template: &str, answers: &[(&str, &str)]) {
        let i = model.add.templates.iter().position(|t| t.id == template).unwrap();
        model.pick_template(i);
        let form = model.add.form.as_mut().unwrap();
        for (key, value) in answers {
            let i = form.template.settings.iter().position(|s| s.key == *key).unwrap();
            match form.inputs[i] {
                Input::Choice(_) => form.pick(i, value),
                _ => form.text_mut(i).unwrap().set(value),
            }
        }
        let op = model.submit_add();
        run(model, ctx, op).await;
    }

    #[test]
    fn only_a_missing_route_means_an_older_hub() {
        let status = |status| hub_client::ClientError::Status { status, body: None };
        assert!(no_such_route(&status(404)));
        assert!(no_such_route(&status(405)));
        for other in [401, 403, 409, 500, 503] {
            assert!(!no_such_route(&status(other)), "{other}");
        }
        assert!(!no_such_route(&hub_client::ClientError::Timeout));
    }

    #[test]
    fn ticks_come_from_the_display_list_and_an_older_hub_is_never_read_as_none_ticked() {
        let at = hub_protocol::Timestamp::parse("2026-11-10T20:00:00Z").unwrap();
        let display = |id: &str, pages: Option<Vec<String>>| hub_core::api::DisplayView {
            id: id.into(),
            name: id.into(),
            enrolled_at: at,
            last_seen_at: None,
            device: None,
            pages,
            firmware: None,
        };
        let shown = ticks(&[display("new", Some(vec!["a".into()])), display("old", None)]);
        assert_eq!(
            shown.get("new").map(Vec::as_slice),
            Some(["a".to_string()].as_slice())
        );
        assert!(!shown.contains_key("old"), "no answer is not an empty list");
        // An older hub's list has no `pages` at all.
        let old: hub_core::api::DisplayView = serde_json::from_value(serde_json::json!({
            "id": "d", "name": "D", "enrolled_at": "2026-11-10T20:00:00Z", "last_seen_at": null, "device": null
        }))
        .unwrap();
        assert_eq!(old.pages, None);
        assert!(!serde_json::to_string(&old).unwrap().contains("pages"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn pairing_and_changes_in_the_manager_reach_a_display() {
        let dir = tempfile::tempdir().unwrap();
        let clock = FakeClock::parse("2026-11-10T20:00:00Z");
        let hub = hubd::open_hub(
            hubd::config::Config {
                data_dir: dir.path().join("hub"),
                ..Default::default()
            },
            Arc::new(clock.clone()),
        )
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(hubd::serve_https(hub.clone(), listener, async {
            let _ = rx.await;
        }));
        let pairing = PairingString {
            host: "127.0.0.1".into(),
            port,
            spki_sha256: hub.identity.spki_sha256,
            secret: hub.new_admin_pairing("test").unwrap(),
        };

        // A PC with no credential store is refused before the hub hands out a token.
        let broken = Ctx::new(
            Arc::new(MemoryStore {
                broken: true,
                ..Default::default()
            }),
            dir.path().join("pc0"),
        );
        let mut m = Model::new(None, "Desk PC");
        m.pairing.set(&pairing.encode());
        let op = m.submit_pairing();
        run(&mut m, &broken, op).await;
        assert!(
            m.notice.as_ref().unwrap().title.contains("No safe place"),
            "{:?}",
            m.notice
        );
        assert!(!m.connected());

        let store = MemoryStore::default();
        let ctx = Ctx::new(Arc::new(store.clone()), dir.path().join("pc"));
        m.pairing.set(&pairing.encode());
        let op = m.submit_pairing();
        run(&mut m, &ctx, op).await;
        assert!(m.connected(), "{:?}", m.notice);
        assert!(m.pairing.text.is_empty(), "the pairing string is wiped");
        let saved = settings::load(&ctx.settings_dir).unwrap().unwrap();
        assert_eq!(saved.pin(), Some(hub.identity.spki_sha256));
        let token = store.load(&saved.token_account()).unwrap().unwrap();
        assert!(token.starts_with("dha_"));
        assert!(!std::fs::read_to_string(ctx.settings_dir.join("manager.json"))
            .unwrap()
            .contains("dha_"));

        // The same string can't be used twice.
        let mut again = Model::new(None, "Other PC");
        again.pairing.set(&pairing.encode());
        let other = Ctx::new(Arc::new(MemoryStore::default()), dir.path().join("pc2"));
        let op = again.submit_pairing();
        run(&mut again, &other, op).await;
        assert_eq!(again.notice.as_ref().unwrap().tone, crate::guidance::Tone::Danger);

        // Pages, from templates, through the model. A demo usage page gets its sample figures.
        for service in ["claude-max", "chatgpt-plus"] {
            add_from(
                &mut m,
                &ctx,
                "usage-limits/1",
                &[("service", service), ("source", "demo")],
            )
            .await;
        }
        add_from(
            &mut m,
            &ctx,
            "usage-limits/1",
            &[("service", "opencode-go-plus"), ("source", "manual")],
        )
        .await;
        assert_eq!(m.pages().len(), 3, "{:?}", m.notice);
        assert!(
            matches!(m.page_rows(Instant::now())[0].shown, Shown::Meter(_)),
            "the demo page holds sample figures"
        );
        m.begin_edit("opencode-go-plus", Instant::now());
        let Some(ContentEdit {
            shape: Shape::UsageMeter { rows, .. },
            ..
        }) = m.edit.as_mut().unwrap().content.as_mut()
        else {
            panic!("usage editor")
        };
        rows[0].percent.set("37.5");
        rows[0].resets_in.set("2 h");
        let op = m.save_edit(Instant::now());
        run(&mut m, &ctx, op).await;
        let rows = m.page_rows(Instant::now());
        assert_eq!(rows[2].quality, "MANUAL");
        let Shown::Meter(bars) = &rows[2].shown else {
            panic!("{:?}", rows[2].shown)
        };
        assert_eq!(bars[0].used_percent, Some(37.5));
        assert_eq!(bars[1].used_percent, None);

        // Other shapes and a helper page, listed whatever their kind, and typed-in content edited.
        add_from(&mut m, &ctx, "big-number/1", &[("title", "Water used")]).await;
        add_from(&mut m, &ctx, "pi-status/1", &[]).await;
        assert_eq!(m.pages().len(), 5, "{:?}", m.notice);
        assert!(m.add.setup.is_some(), "the Pi command is offered afterwards");
        let rows = m.page_rows(Instant::now());
        assert_eq!(
            (rows[3].kind.as_str(), rows[3].origin.as_str(), &rows[3].shown),
            ("Big number", "Typed in", &Shown::Empty)
        );
        assert_eq!(rows[4].origin, "Filled by a helper (feed pi-status)");
        m.begin_edit("water-used", Instant::now());
        let Some(ContentEdit {
            shape: Shape::BigNumber { value, unit, .. },
            ..
        }) = m.edit.as_mut().unwrap().content.as_mut()
        else {
            panic!("big number editor")
        };
        value.set("12.5");
        unit.set("L");
        let op = m.save_edit(Instant::now());
        run(&mut m, &ctx, op).await;
        assert_eq!(
            m.page_rows(Instant::now())[3].shown,
            Shown::BigNumber {
                value: "12.5 L".into(),
                caption: None
            }
        );
        // The page's template and settings are kept on the hub, and editing them changes the title.
        assert_eq!(m.pages()[3].template.as_deref(), Some("big-number/1"));
        m.begin_edit("water-used", Instant::now());
        let form = m.edit.as_mut().unwrap().form.as_mut().unwrap();
        form.text_mut(0).unwrap().set("Water today");
        let op = m.save_edit(Instant::now());
        run(&mut m, &ctx, op).await;
        assert_eq!(m.pages()[3].title, "Water today");
        assert_eq!(m.pages()[3].settings.as_ref().unwrap()["title"], "Water today");
        for id in ["water-used", "pi-status"] {
            m.ask_remove_page(id);
            let op = m.confirm_yes();
            run(&mut m, &ctx, op).await;
        }
        assert_eq!(m.pages().len(), 3);

        let op = m.move_page("opencode-go-plus", -1);
        run(&mut m, &ctx, op).await;
        assert_eq!(m.pages()[1].id, "opencode-go-plus");

        // A display, set up from the bundle the model builds.
        m.open_enrol();
        let op = m.create_setup();
        run(&mut m, &ctx, op).await;
        let mut bundle = m.enrol.bundle.clone().expect("a bundle");
        assert_eq!(bundle.hub.port, port, "the port this PC reaches the hub on");
        // The test hub's advertised name doesn't resolve here; the address this PC uses does.
        assert_eq!(bundle.hub.hosts.last().map(String::as_str), Some("127.0.0.1"));
        bundle.hub.hosts.retain(|h| h == "127.0.0.1");
        let display_dir = dir.path().join("display");
        let state = paired::enrol(&bundle, &display_dir).await.unwrap();
        let mut device = Device::new(None, Box::new(NoJitter));
        device.accept(clock.now(), paired::snapshot(&state).await);
        let s = screen(&device, clock.now());
        assert_eq!(count_pages(&s), 3);
        assert!(
            s.find("OpenCode Go Plus").unwrap() < s.find("ChatGPT Plus").unwrap(),
            "{s}"
        );
        assert!(s.contains("37.5"), "{s}");

        // Removing a page in the manager: gone on the display's next poll.
        m.ask_remove_page("chatgpt-plus");
        let op = m.confirm_yes();
        run(&mut m, &ctx, op).await;
        device.accept(clock.now(), paired::snapshot(&state).await);
        assert_eq!(count_pages(&screen(&device, clock.now())), 2);

        // Choosing which pages the display shows: new displays show everything, and a saved
        // change reaches it on its next poll.
        let op = m.refresh();
        run(&mut m, &ctx, op).await;
        let display = m.displays()[0].id.clone();
        let first = m.pages()[0].id.clone();
        assert_eq!(m.tick_view(&display).unwrap().ticked, 2);
        assert_eq!(
            m.page_rows(Instant::now())[0].on_displays.as_deref(),
            Some("On: Desk")
        );
        m.toggle_tick(&display, &first);
        let op = m.save_ticks(&display);
        run(&mut m, &ctx, op).await;
        assert!(!m.tick_view(&display).unwrap().dirty, "{:?}", m.notice);
        assert_eq!(m.tick_view(&display).unwrap().ticked, 1);
        assert_eq!(
            m.page_rows(Instant::now())[0].on_displays.as_deref(),
            Some("Not on any display")
        );
        device.accept(clock.now(), paired::snapshot(&state).await);
        assert_eq!(count_pages(&screen(&device, clock.now())), 1);

        // Removing the display cuts it off.
        let op = m.refresh();
        run(&mut m, &ctx, op).await;
        assert_eq!(m.displays().len(), 1);
        m.ask_revoke(&state.device_id);
        let op = m.confirm_yes();
        run(&mut m, &ctx, op).await;
        assert!(paired::snapshot(&state).await.is_err());

        // Forgetting wipes the credential and settings, and the app starts over.
        m.forget();
        let op = m.confirm_yes();
        run(&mut m, &ctx, op).await;
        assert!(!m.connected());
        assert!(store.load(&saved.token_account()).unwrap().is_none());
        assert_eq!(settings::load(&ctx.settings_dir).unwrap(), None);
        let _ = Outcome::Forgotten;
        let _ = tx.send(());
        server.await.unwrap().unwrap();
    }
}

#[cfg(test)]
mod own_helper_tests {
    //! A maker's own helper, end to end: the page made in the manager, a helper invited for its feed
    //! in the manager, then a page sent with hub-send's library, as `hub-send page` sends it.

    use std::time::Instant;

    use hub_core::api::FeedContent;
    use hub_core::FakeClock;
    use hub_protocol::pairing::PairingString;
    use os_secrets::{MemoryStore, TokenStore};

    use super::*;
    use crate::model::Model;
    use crate::shown::Shown;

    async fn run(model: &mut Model, ctx: &Arc<Ctx>, op: Option<Op>) {
        let op = op.expect("an operation");
        let result = execute(ctx.clone(), op).await;
        model.finish(result, Instant::now());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_page_for_your_own_helper_fills_from_hub_send() {
        let dir = tempfile::tempdir().unwrap();
        let clock = FakeClock::parse("2026-11-10T20:00:00Z");
        let hub = hubd::open_hub(
            hubd::config::Config {
                data_dir: dir.path().join("hub"),
                ..Default::default()
            },
            Arc::new(clock.clone()),
        )
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(hubd::serve_https(hub.clone(), listener, async {
            let _ = rx.await;
        }));
        let pairing = PairingString {
            host: "127.0.0.1".into(),
            port,
            spki_sha256: hub.identity.spki_sha256,
            secret: hub.new_admin_pairing("test").unwrap(),
        };
        let ctx = Ctx::new(Arc::new(MemoryStore::default()), dir.path().join("pc"));
        let mut m = Model::new(None, "Desk PC");
        m.pairing.set(&pairing.encode());
        let op = m.submit_pairing();
        run(&mut m, &ctx, op).await;
        assert!(m.connected(), "{:?}", m.notice);

        // The page, made in the manager: a list, and one of a kind of the maker's own.
        for (title, shape, kind, feed) in [
            ("Bus times", Some("list"), "list", "bus-times"),
            ("Flights", None, "nearby-flights/1", "nearby-flights"),
        ] {
            m.open_helper_page();
            let f = m.add.helper_page.as_mut().unwrap();
            f.title.set(title);
            f.choose_kind(shape);
            f.custom_kind.set(kind);
            f.feed.set(feed);
            let op = m.submit_helper_page();
            run(&mut m, &ctx, op).await;
            assert_eq!(
                m.add.feed.as_ref().map(|s| s.feed.as_str()),
                Some(feed),
                "{:?}",
                m.notice
            );
            m.finish_setup();
        }
        let rows = m.page_rows(Instant::now());
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].origin, "Filled by a helper (feed bus-times)");
        assert_eq!(rows[0].freshness, "Waiting for the helper's first update");
        assert_eq!(rows[1].kind, "Custom: nearby-flights/1");

        // A helper invited for both feeds, from the manager's Settings.
        m.invite_for_feed("bus-times");
        m.helper.other.set("nearby-flights");
        let op = m.create_helper();
        run(&mut m, &ctx, op).await;
        let string = m.helper.string.clone().expect("a helper string");
        assert_eq!(m.invite_programs(), (false, true));

        // The helper's computer: hub-send login, then hub-send page.
        let profile = hub_send::Profile::new(
            "hub-send",
            dir.path().join("helper"),
            Box::new(MemoryStore::default()) as Box<dyn TokenStore>,
        );
        let cfg = profile.login(&string, clock.now()).await.unwrap();
        assert_eq!(cfg.feeds, ["bus-times", "nearby-flights"]);
        let helper = profile.open().unwrap();
        let example = crate::model::example_data("list").unwrap();
        let sent = helper
            .send_page(
                "bus-times",
                &FeedContent {
                    kind: "list".into(),
                    data: serde_json::from_str(example).unwrap(),
                    collected_at: None,
                    note: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(sent.updated, ["bus-times"]);
        let flights = serde_json::json!({"planes": [{"callsign": "NZ123"}]});
        let sent = helper
            .send_page(
                "nearby-flights",
                &FeedContent {
                    kind: "nearby-flights/1".into(),
                    data: flights.clone(),
                    collected_at: None,
                    note: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(sent.updated, ["nearby-flights"]);

        // The manager sees what arrived, and the helper with its feeds.
        let op = m.refresh();
        run(&mut m, &ctx, op).await;
        assert_eq!(m.helpers()[0].feeds, ["bus-times", "nearby-flights"]);
        let rows = m.page_rows(Instant::now());
        assert_eq!(rows[0].quality, "REPORTED");
        let Shown::List(items) = &rows[0].shown else {
            panic!("{:?}", rows[0].shown)
        };
        assert_eq!(items.len(), 1, "{items:?}");
        assert_eq!(rows[1].quality, "REPORTED");
        assert_eq!(
            m.pages()[1].content.as_ref().and_then(|c| c.data.clone()),
            Some(flights)
        );
        let _ = tx.send(());
        server.await.unwrap().unwrap();
    }
}

// Not on macOS, where a pseudo-terminal can't stand in for a USB port (see
// tools/display-simulator/tests/usb.rs).
#[cfg(all(test, unix, not(target_os = "macos")))]
mod install_tests {
    //! Install firmware through the model and the worker, against a real hub over pinned HTTPS and
    //! the simulated display on a pseudo-terminal: the flash goes through a fake board (the
    //! simulator's own `firmware install`), and the provisioning half is the real one.

    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Instant;

    use display_simulator::firmware::store;
    use display_simulator::{paired, usb};
    use firmware_package::{build_and_sign, Release, SigningKey, UsbLayout, UsbPartFile};
    use hub_core::FakeClock;
    use hub_protocol::pairing::PairingString;
    use serialport::SerialPort;

    use super::*;
    use crate::firmware::{BoardError, BoardInfo};
    use crate::model::{EnrolStage, InstallStage, Model};
    use os_secrets::MemoryStore;

    /// Stands in for the board: "writing" is the simulator's own first install, done at restart.
    /// With `wipe` off it leaves the display as it was, as a board that didn't really take the
    /// firmware would.
    struct SimBoard {
        state: PathBuf,
        loaded: Loaded,
        wipe: bool,
    }

    impl Board for SimBoard {
        fn connect(&mut self) -> Result<BoardInfo, BoardError> {
            Ok(BoardInfo {
                chip: "esp32s3".into(),
                flash_bytes: 16 << 20,
                secure_download: false,
            })
        }
        fn erase_all(&mut self) -> Result<(), BoardError> {
            Ok(())
        }
        fn write(&mut self, _: u32, bytes: &[u8], progress: &mut dyn FnMut(u64)) -> Result<(), BoardError> {
            progress(bytes.len() as u64);
            Ok(())
        }
        fn restart(self: Box<Self>) -> Result<(), BoardError> {
            if self.wipe {
                store::install_firmware(&self.state, &self.loaded.checked, &self.loaded.bytes)
                    .map_err(|e| BoardError::Other(e.to_string()))?;
            }
            Ok(())
        }
    }

    fn package(key: &SigningKey, key_set: Option<Vec<firmware_package::PublicKey>>) -> Loaded {
        let release = Release {
            device_type: store::DEVICE_TYPE.into(),
            hardware_revisions: vec![store::HARDWARE_REVISION.into()],
            firmware_version: "2.0.0".into(),
            security_counter: 3,
            summary: Some("The first signed release.".into()),
            key_set,
            usb_install: Some(UsbLayout {
                chip: "esp32s3".into(),
                min_flash_bytes: 4 << 20,
                app_offset: 0x10000,
                parts: vec![UsbPartFile {
                    name: "bootloader".into(),
                    offset: 0,
                    bytes: vec![7; 100],
                }],
            }),
        };
        let built = build_and_sign(key, &release, &[3; 4000]).unwrap();
        Loaded::check(built.bytes).unwrap()
    }

    /// The display end of a pty, answering until `stop` is set.
    fn display_on_pty(state: PathBuf, stop: Arc<AtomicBool>) -> String {
        let (mut master, slave) = serialport::TTYPort::pair().unwrap();
        let path = slave.name().unwrap();
        drop(slave);
        std::thread::spawn(move || {
            // Like a real port, it stays open after a bundle is accepted (the display refuses any
            // more), until the test is done with it.
            while !stop.load(Ordering::Relaxed) {
                let _ = usb::answer(&mut master, &state);
                std::thread::sleep(Duration::from_millis(20));
            }
        });
        path
    }

    async fn run(model: &mut Model, ctx: &Arc<Ctx>, op: Option<Op>) {
        let result = execute(ctx.clone(), op.expect("an operation")).await;
        model.finish(result, Instant::now());
    }

    /// Picks the package, fills in the details, confirms and runs the install on `port`.
    async fn install(m: &mut Model, ctx: &Arc<Ctx>, loaded: Loaded, port: &str, maker: &str) {
        m.open_install();
        m.finish(Ok(Outcome::PackageLoaded(Box::new(loaded))), Instant::now());
        assert_eq!(m.fw.install.stage, InstallStage::Ready, "{:?}", m.notice);
        m.fw.install.name.set("Kitchen");
        m.fw.install.ssid.set("Home");
        m.fw.install.maker.set(maker);
        assert!(m.ask_install(port).is_none());
        let op = m.confirm_yes();
        assert!(matches!(op, Some(Op::Install(_))));
        run(m, ctx, op).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn install_firmware_gives_a_display_its_makers_keys_and_a_locked_one_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("display");
        let hub = hubd::open_hub(
            hubd::config::Config {
                data_dir: dir.path().join("hub"),
                ..Default::default()
            },
            Arc::new(FakeClock::parse("2026-11-17T09:00:00Z")),
        )
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(hubd::serve_https(hub.clone(), listener, async {
            let _ = rx.await;
        }));
        let pairing = PairingString {
            host: "127.0.0.1".into(),
            port,
            spki_sha256: hub.identity.spki_sha256,
            secret: hub.new_admin_pairing("test").unwrap(),
        };

        let maker = SigningKey::generate().unwrap().0;
        let recovery = SigningKey::generate().unwrap().0;
        let first = package(&maker, Some(vec![maker.public_key(), recovery.public_key()]));
        let wipe = Arc::new(AtomicBool::new(true));
        let boards: Boards = {
            let (state, first, wipe) = (state.clone(), first.clone(), wipe.clone());
            Arc::new(move |_port: &str| {
                Box::new(SimBoard {
                    state: state.clone(),
                    loaded: first.clone(),
                    wipe: wipe.load(Ordering::Relaxed),
                }) as Box<dyn Board>
            })
        };
        let ctx = Ctx::with_boards(Arc::new(MemoryStore::default()), dir.path().join("pc"), boards);
        let mut m = Model::new(None, "Desk PC");
        m.pairing.set(&pairing.encode());
        let op = m.submit_pairing();
        run(&mut m, &ctx, op).await;
        assert!(m.connected(), "{:?}", m.notice);
        assert!(m.shelf().is_some(), "this hub lists firmware");

        // The official firmware that came with the app goes to the hub as it was checked, with no
        // file to choose.
        m.fw.bundled = Some(crate::firmware::Bundled {
            loaded: first.clone(),
            board: crate::firmware::OFFICIAL_BOARD,
        });
        assert!(!m.bundled_on_hub());
        let op = m.upload_bundled();
        assert!(matches!(op, Some(Op::UploadLoaded { .. })));
        run(&mut m, &ctx, op).await;
        assert!(m.bundled_on_hub(), "{:?}", m.notice);
        assert_eq!(m.firmware_packages().len(), 1);
        assert_eq!(
            m.firmware_packages()[0].package_id,
            first.checked.package_id.to_hex()
        );

        // A display with no keys takes the package's key set and its security counter.
        let stop = Arc::new(AtomicBool::new(false));
        let path = display_on_pty(state.clone(), stop.clone());
        install(&mut m, &ctx, first.clone(), &path, "Kea Displays").await;
        assert!(
            matches!(m.fw.install.stage, InstallStage::Done { .. }),
            "{:?}",
            m.notice
        );
        assert!(
            m.notice
                .as_ref()
                .unwrap()
                .title
                .starts_with("Installed \"llm-usage-display\" 2.0.0"),
            "{:?}",
            m.notice
        );
        let keys = store::keys(&state).unwrap();
        assert_eq!(
            keys.trusted_keys,
            [maker.public_key().to_hex(), recovery.public_key().to_hex()]
        );
        assert_eq!(keys.security_floor, 3);
        assert_eq!(store::running_version(&state).unwrap(), "2.0.0");
        // The owner's name for the key is on the hub now.
        assert_eq!(
            m.signer(&maker.public_key().key_id().to_hex()).name(),
            "Kea Displays"
        );

        // The display enrols with what it was sent, and the Displays screen sees it join.
        let mut bundle = usb::load_bundle(&state).expect("the display kept the setup details");
        assert!(bundle.firmware.is_some());
        bundle.hub.hosts.retain(|h| h == "127.0.0.1");
        paired::enrol(&bundle, &state).await.unwrap();
        assert!(matches!(m.enrol.stage, EnrolStage::Waiting { .. }));
        let op = m.refresh();
        run(&mut m, &ctx, op).await;
        assert!(
            matches!(&m.enrol.stage, EnrolStage::Joined { name } if name == "Kitchen"),
            "{:?}",
            m.enrol.stage
        );
        let id = m.displays()[0].id.clone();
        let fw = m.display_firmware(&id, Instant::now()).unwrap();
        assert!(matches!(fw.trust, crate::firmware::Trust::Keys(ref k) if k.len() == 2));
        assert_eq!(fw.version.as_deref(), Some("2.0.0"));
        stop.store(true, Ordering::Relaxed);

        // Factory-reset, then an "install" that never reached the flash, of another maker's
        // package: the display still trusts the first maker, refuses the keys, and the app says so
        // in plain words.
        paired::reset(&state).unwrap();
        wipe.store(false, Ordering::Relaxed);
        let stranger = SigningKey::generate().unwrap().0;
        let stop = Arc::new(AtomicBool::new(false));
        let path = display_on_pty(state.clone(), stop.clone());
        m.close_install();
        install(&mut m, &ctx, package(&stranger, None), &path, "Weka Works").await;
        let n = m.notice.as_ref().unwrap();
        assert_eq!(
            n.title,
            "The firmware is installed, but the setup details weren't sent"
        );
        assert!(
            n.detail.contains("This display trusts another maker's firmware")
                && n.detail
                    .contains("Only Install firmware, which erases the display"),
            "{}",
            n.detail
        );
        assert_eq!(
            store::keys(&state).unwrap(),
            keys,
            "nothing changed on the display"
        );
        // The owner's name for the stranger's key is saved all the same: the hub keeps it by key
        // id, whether or not any display trusts the key.
        assert!(!n.detail.contains("wasn't saved"), "{}", n.detail);
        assert_eq!(
            m.signer(&stranger.public_key().key_id().to_hex()).name(),
            "Weka Works"
        );
        stop.store(true, Ordering::Relaxed);

        let _ = tx.send(());
        server.await.unwrap().unwrap();
    }
}
