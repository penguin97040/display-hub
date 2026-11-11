//! Runs the model's operations: network calls on a tokio runtime, and blocking work (the credential
//! store, files, the serial port) on its blocking threads. GPUI awaits the result (`app.rs`).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use hub_client::HubClient;
use hub_core::{Clock, SystemClock};
use hub_protocol::pairing::hex;
use hub_protocol::provisioning::Bundle;
use zeroize::Zeroizing;

use crate::model::{Failure, Op, Outcome, PortInfo, Refresh};
use crate::secrets::TokenStore;
use crate::settings::{self, SavedHub};

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

async fn refresh(s: &Session) -> Result<Refresh, Failure> {
    let t = s.token.as_str();
    let (status, pages, displays, preview) = tokio::try_join!(
        s.client.status(t),
        s.client.subscriptions(t),
        s.client.displays(t),
        s.client.preview_snapshot(t),
    )
    .map_err(Failure::Client)?;
    Ok(Refresh {
        status,
        pages,
        displays,
        preview,
    })
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
                refresh: refresh(&session).await?,
            })
        }
        Op::Refresh => Ok(Outcome::Refreshed(refresh(&ctx.session()?).await?)),
        Op::AddPage(new) => {
            let s = ctx.session()?;
            s.client.add_subscription(&s.token, &new).await.map_err(c)?;
            Ok(Outcome::Refreshed(refresh(&s).await?))
        }
        Op::ChangePage { id, change, figures } => {
            let s = ctx.session()?;
            s.client
                .change_subscription(&s.token, &id, &change)
                .await
                .map_err(c)?;
            if let Some(f) = figures {
                s.client.set_manual(&s.token, &id, &f).await.map_err(c)?;
            }
            Ok(Outcome::Refreshed(refresh(&s).await?))
        }
        Op::RemovePage { id } => {
            let s = ctx.session()?;
            s.client.remove_subscription(&s.token, &id).await.map_err(c)?;
            Ok(Outcome::Refreshed(refresh(&s).await?))
        }
        Op::Reorder { ids } => {
            let s = ctx.session()?;
            s.client.reorder(&s.token, ids).await.map_err(c)?;
            Ok(Outcome::Refreshed(refresh(&s).await?))
        }
        Op::RevokeDisplay { id } => {
            let s = ctx.session()?;
            s.client.revoke_display(&s.token, &id).await.map_err(c)?;
            Ok(Outcome::Refreshed(refresh(&s).await?))
        }
        Op::NewEnrolment { name } => {
            let s = ctx.session()?;
            let grant = s.client.new_enrolment(&s.token, &name).await.map_err(c)?;
            Ok(Outcome::Enrolment {
                grant,
                refresh: refresh(&s).await?,
            })
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
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path).map_err(fail)?;
    f.write_all(&serde_json::to_vec_pretty(bundle).expect("serialises"))
        .map_err(fail)?;
    f.sync_all().map_err(fail)
}

#[cfg(test)]
mod tests {
    //! The manager's whole path against a real hub over pinned HTTPS: pairing through the model,
    //! page changes, and a simulated display picking them up on its next poll.

    use std::time::Instant;

    use display_simulator::render::{count_pages, screen};
    use display_simulator::{paired, Device, NoJitter};
    use hub_core::{FakeClock, Service, Source};
    use hub_protocol::pairing::PairingString;

    use super::*;
    use crate::model::{Model, Outcome};
    use crate::secrets::MemoryStore;

    async fn run(model: &mut Model, ctx: &Arc<Ctx>, op: Option<Op>) {
        let op = op.expect("an operation");
        let result = execute(ctx.clone(), op).await;
        model.finish(result, Instant::now());
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

        // Pages, through the model.
        for service in [Service::ClaudeMax, Service::ChatgptPlus] {
            m.add.service = service;
            m.add.source = Source::Demo;
            let op = m.add_page();
            run(&mut m, &ctx, op).await;
        }
        m.add.service = Service::OpencodeGoPlus;
        m.add.source = Source::Manual;
        let op = m.add_page();
        run(&mut m, &ctx, op).await;
        assert_eq!(m.pages().len(), 3);
        m.begin_edit("opencode-go-plus", Instant::now());
        m.edit.as_mut().unwrap().metrics[0].percent.set("37.5");
        m.edit.as_mut().unwrap().metrics[0].resets_in.set("2 h");
        let op = m.save_edit(Instant::now());
        run(&mut m, &ctx, op).await;
        let rows = m.page_rows(Instant::now());
        assert_eq!(rows[2].quality, "MANUAL");
        assert_eq!(rows[2].metrics[0].used_percent, Some(37.5));
        assert_eq!(rows[2].metrics[1].used_percent, None);
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
