//! A hostile hub (a test double on real pinned TLS, with the hub's own TLS settings) that controls
//! every byte and header of a download: it stalls, cuts the body short, sends a length that doesn't
//! match, drips the body too slowly, serves another maker's package, or replays an old genuine one.
//! The display refuses or gives up each time, and writes nothing it hasn't verified
//! (`docs/firmware-updates.md`, cases 20, 22, 37 and 45 over the network).

use std::collections::VecDeque;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use axum::body::{Body, Bytes, HttpBody};
use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Json, Router};
use display_simulator::firmware::install::Failure;
use display_simulator::firmware::store::{self, KeyStore, SlotName, SlotState};
use display_simulator::firmware::{Acted, Firmware, Limits, Started};
use display_simulator::paired;
use firmware_package::{build_and_sign, Built, Refusal, Release, SigningKey};
use hub_core::FakeClock;
use hub_protocol::firmware::{FirmwareReport, FirmwareUpdate, ReportReason, ReportState};
use hub_protocol::provisioning::{HubLocation, TrustAnchor, HUB_SERVER_NAME};
use tokio::sync::oneshot;

/// What the double sends for a download.
#[derive(Clone)]
struct Script {
    /// The `Content-Length` header, if any.
    content_length: Option<u64>,
    /// The body, in pieces, each after `delay`.
    pieces: Vec<Vec<u8>>,
    delay: Duration,
    /// After the pieces: stall (true) or end.
    stall: bool,
}

#[derive(Clone, Default)]
struct Double {
    script: Arc<Mutex<Option<Script>>>,
    reports: Arc<Mutex<Vec<FirmwareReport>>>,
}

/// A body that plays a script.
struct Played {
    pieces: VecDeque<Bytes>,
    delay: Duration,
    sleep: Option<Pin<Box<tokio::time::Sleep>>>,
    stall: bool,
}

impl HttpBody for Played {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<hyper::body::Frame<Bytes>, std::io::Error>>> {
        if self.pieces.is_empty() {
            return if self.stall {
                Poll::Pending
            } else {
                Poll::Ready(None)
            };
        }
        let delay = self.delay;
        let sleep = self
            .sleep
            .get_or_insert_with(|| Box::pin(tokio::time::sleep(delay)));
        if sleep.as_mut().poll(cx).is_pending() {
            return Poll::Pending;
        }
        self.sleep = None;
        let piece = self.pieces.pop_front().expect("not empty");
        Poll::Ready(Some(Ok(hyper::body::Frame::data(piece))))
    }
}

async fn download(State(d): State<Double>) -> Response {
    let script = d.script.lock().unwrap().clone().expect("a script");
    let mut resp = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/octet-stream");
    if let Some(n) = script.content_length {
        resp = resp.header(header::CONTENT_LENGTH, n);
    }
    resp.body(Body::new(Played {
        pieces: script.pieces.into_iter().map(Bytes::from).collect(),
        delay: script.delay,
        sleep: None,
        stall: script.stall,
    }))
    .unwrap()
}

async fn report(State(d): State<Double>, Json(r): Json<FirmwareReport>) -> StatusCode {
    d.reports.lock().unwrap().push(r);
    StatusCode::NO_CONTENT
}

struct Rig {
    double: Double,
    state: paired::State,
    dir: tempfile::TempDir,
    stop: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<()>,
}

impl Rig {
    async fn start(trusted: &[&SigningKey], floor: u32) -> Rig {
        let identity = hubd::identity::Identity::generate().unwrap();
        let tls = Arc::new(hubd::tls::server_config(&identity).unwrap());
        let double = Double::default();
        let router = Router::new()
            .route("/v1/device/firmware/{id}", get(download))
            .route("/v1/device/firmware/report", post(report))
            .with_state(double.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr: SocketAddr = listener.local_addr().unwrap();
        let (tx, rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            hubd::tls::serve(listener, router, tls, false, async {
                let _ = rx.await;
            })
            .await
            .unwrap();
        });
        let dir = tempfile::tempdir().unwrap();
        store::set_keys(
            dir.path(),
            &KeyStore {
                trusted_keys: trusted.iter().map(|k| k.public_key().to_hex()).collect(),
                security_floor: floor,
            },
        )
        .unwrap();
        let state = paired::State {
            hub: HubLocation {
                hosts: vec!["127.0.0.1".into()],
                port: addr.port(),
                server_name: HUB_SERVER_NAME.into(),
                trust_anchors: vec![TrustAnchor {
                    certificate_pem: identity.cert_pem.clone(),
                    spki_sha256: firmware_package::hex::encode(&identity.spki_sha256),
                }],
            },
            device_id: "d-test".into(),
            device_token: "dhd_test".into(),
        };
        Rig {
            double,
            state,
            dir,
            stop: Some(tx),
            task,
        }
    }

    fn firmware(&self, clock: &FakeClock) -> Firmware {
        let mut fw = Firmware::open(self.dir.path(), Arc::new(clock.clone())).unwrap();
        fw.limits = Limits {
            stall: Duration::from_millis(300),
            total: Duration::from_secs(2),
        };
        assert!(matches!(fw.boot().unwrap(), Started::Normal { .. }));
        fw
    }

    fn serve(&self, script: Script) {
        *self.double.script.lock().unwrap() = Some(script);
    }

    fn reports(&self) -> Vec<(ReportState, Option<ReportReason>)> {
        self.double
            .reports
            .lock()
            .unwrap()
            .drain(..)
            .map(|r| (r.state, r.reason))
            .collect()
    }

    fn nothing_installed(&self) {
        let ota = store::ota(self.dir.path()).unwrap();
        assert_eq!((ota.boot, ota.running), (SlotName::A, SlotName::A));
        assert_ne!(ota.b.state, SlotState::New);
        assert!(!store::slot_image(self.dir.path(), SlotName::B).exists());
        assert_eq!(
            store::running_version(self.dir.path()).unwrap(),
            store::FACTORY_VERSION
        );
    }

    async fn stop(mut self) {
        let _ = self.stop.take().unwrap().send(());
        self.task.await.unwrap();
    }
}

fn key() -> SigningKey {
    SigningKey::generate().unwrap().0
}

fn package(key: &SigningKey, version: &str, counter: u32) -> Built {
    let release = Release {
        device_type: store::DEVICE_TYPE.into(),
        hardware_revisions: vec![store::HARDWARE_REVISION.into()],
        firmware_version: version.into(),
        security_counter: counter,
        summary: None,
        key_set: None,
        usb_install: None,
    };
    let image: Vec<u8> = (0..50_000u32).map(|i| (i * 13 + counter) as u8).collect();
    build_and_sign(key, &release, &image).unwrap()
}

/// The whole update form, in pieces of `size`.
fn pieces(form: &[u8], size: usize) -> Vec<Vec<u8>> {
    form.chunks(size).map(<[u8]>::to_vec).collect()
}

fn offer(n: u32, form: &[u8]) -> FirmwareUpdate {
    FirmwareUpdate {
        offer_id: format!("{n:032x}"),
        package_id: "ab".repeat(32),
        size: form.len() as u64,
    }
}

#[tokio::test]
async fn the_display_gives_up_on_or_refuses_whatever_a_hostile_hub_sends() {
    let maker = key();
    let r = Rig::start(&[&maker], 2).await;
    let clock = FakeClock::parse("2026-11-17T09:00:00Z");
    let mut fw = r.firmware(&clock);
    let built = package(&maker, "1.2.0", 2);
    let form = built.bytes.clone();
    let len = form.len() as u64;
    let mut n = 0;
    let mut next = || {
        // Far enough apart that the waits between attempts never apply.
        clock.advance(7 * 3600);
        n += 1;
        offer(n, &form)
    };

    // Case 22: half the body, then nothing at all. The display gives up after the stall limit.
    r.serve(Script {
        content_length: Some(len),
        pieces: vec![form[..form.len() / 2].to_vec()],
        delay: Duration::ZERO,
        stall: true,
    });
    let started = std::time::Instant::now();
    let o = next();
    assert_eq!(
        fw.on_offer(&r.state, &o).await.unwrap(),
        Acted::Failed(Failure::Download)
    );
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(
        r.reports(),
        [
            (ReportState::Downloading, None),
            (ReportState::Failed, Some(ReportReason::Download))
        ]
    );
    r.nothing_installed();

    // Case 22: the body ends early: the connection closes before the Content-Length is reached.
    r.serve(Script {
        content_length: Some(len),
        pieces: vec![form[..form.len() - 10].to_vec()],
        delay: Duration::ZERO,
        stall: false,
    });
    let o = next();
    assert_eq!(
        fw.on_offer(&r.state, &o).await.unwrap(),
        Acted::Failed(Failure::Download)
    );
    r.nothing_installed();
    r.reports();

    // The whole body, dripped so slowly it passes the total time limit without ever stalling.
    r.serve(Script {
        content_length: Some(len),
        pieces: pieces(&form, form.len() / 40),
        delay: Duration::from_millis(100),
        stall: false,
    });
    let o = next();
    assert_eq!(
        fw.on_offer(&r.state, &o).await.unwrap(),
        Acted::Failed(Failure::Download)
    );
    r.nothing_installed();
    r.reports();

    // Case 20: a Content-Length that disagrees with the offer, and none at all (chunked).
    for content_length in [Some(len + 1), Some(len - 1), None] {
        r.serve(Script {
            content_length,
            pieces: vec![form.clone()],
            delay: Duration::ZERO,
            stall: false,
        });
        let o = next();
        assert_eq!(
            fw.on_offer(&r.state, &o).await.unwrap(),
            Acted::Refused(Refusal::TooLarge),
            "{content_length:?}"
        );
        assert_eq!(
            r.reports(),
            [(ReportState::Refused, Some(ReportReason::TooLarge))]
        );
    }

    // Case 45: another maker's package, well formed and correctly signed.
    let theirs = package(&key(), "6.6.6", 9).bytes;
    r.serve(Script {
        content_length: Some(theirs.len() as u64),
        pieces: vec![theirs.clone()],
        delay: Duration::ZERO,
        stall: false,
    });
    n += 1;
    clock.advance(7 * 3600);
    assert_eq!(
        fw.on_offer(&r.state, &offer(n, &theirs)).await.unwrap(),
        Acted::Refused(Refusal::UntrustedKey)
    );
    r.nothing_installed();

    // Case 37 over the network: a genuine old release, replayed below the floor.
    let old = package(&maker, "1.0.0", 1).bytes;
    r.serve(Script {
        content_length: Some(old.len() as u64),
        pieces: vec![old.clone()],
        delay: Duration::ZERO,
        stall: false,
    });
    n += 1;
    clock.advance(7 * 3600);
    assert_eq!(
        fw.on_offer(&r.state, &offer(n, &old)).await.unwrap(),
        Acted::Refused(Refusal::Downgrade)
    );
    r.nothing_installed();

    // The genuine package, in small pieces with short pauses: installed and started on trial.
    r.serve(Script {
        content_length: Some(len),
        pieces: pieces(&form, 1500),
        delay: Duration::from_millis(1),
        stall: false,
    });
    n += 1;
    clock.advance(7 * 3600);
    assert!(matches!(
        fw.on_offer(&r.state, &offer(n, &form)).await.unwrap(),
        Acted::Restarted(Started::Trial { .. })
    ));
    r.stop().await;
}

#[tokio::test]
async fn a_failed_write_is_reported_as_flash_and_nothing_boots() {
    let maker = key();
    let r = Rig::start(&[&maker], 0).await;
    let clock = FakeClock::parse("2026-11-17T09:00:00Z");
    let mut fw = r.firmware(&clock);
    let form = package(&maker, "1.2.0", 1).bytes;
    r.serve(Script {
        content_length: Some(form.len() as u64),
        pieces: vec![form.clone()],
        delay: Duration::ZERO,
        stall: false,
    });
    fw.switches.flash_fails = true;
    assert_eq!(
        fw.on_offer(&r.state, &offer(1, &form)).await.unwrap(),
        Acted::Failed(Failure::Flash)
    );
    assert_eq!(
        r.reports(),
        [
            (ReportState::Downloading, None),
            (ReportState::Failed, Some(ReportReason::Flash))
        ]
    );
    r.nothing_installed();
    assert!(!fw.switches.flash_fails, "a switch applies once");
    r.stop().await;
}
