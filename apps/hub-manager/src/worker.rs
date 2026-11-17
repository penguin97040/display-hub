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

use crate::model::{AddFailure, Failure, Op, Outcome, PortInfo, Refresh};
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

/// What the worker needs: where to keep things, and the current connection.
pub struct Ctx {
    pub store: Arc<dyn TokenStore>,
    pub settings_dir: PathBuf,
    session: Mutex<Option<Session>>,
}

impl Ctx {
    pub fn new(store: Arc<dyn TokenStore>, settings_dir: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            store,
            settings_dir,
            session: Mutex::new(None),
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
    Ok(Refresh {
        status,
        pages,
        displays,
        preview,
        helpers,
        ticks,
    })
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
    // USB ports first: that's where a display is.
    ports.sort_by_key(|p| (!p.description.starts_with("USB"), p.name.clone()));
    ports
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
