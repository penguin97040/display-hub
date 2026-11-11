//! USB provisioning over a real pseudo-terminal: the manager's sender (`hub_client::usb`) against the
//! simulator's display end, then enrolment with the bundle it received.
#![cfg(unix)]

use std::sync::Arc;
use std::time::Duration;

use display_simulator::{paired, usb};
use hub_client::usb::{hello, provision, UsbError};
use hub_client::HubClient;
use hub_core::FakeClock;
use hub_protocol::pairing::PairingString;
use hub_protocol::usb::BAUD_RATE;
use hubd::config::Config;
use serialport::SerialPort;

/// A pty with the display end answering on a thread, and the path the manager opens.
fn display_on_pty(state: std::path::PathBuf) -> (String, std::thread::JoinHandle<()>) {
    let (mut master, slave) = serialport::TTYPort::pair().unwrap();
    // The manager wants the port to itself, so the display holds only the master end.
    let path = slave.name().unwrap();
    drop(slave);
    let thread = std::thread::spawn(move || {
        // Answer sessions until a bundle is accepted, then once more to refuse the next one.
        while usb::answer(&mut master, &state).unwrap().is_none() {
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = usb::answer(&mut master, &state);
    });
    (path, thread)
}

fn open(path: &str) -> Box<dyn SerialPort> {
    serialport::new(path, BAUD_RATE)
        .timeout(Duration::from_millis(100))
        .open()
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_manager_provisions_a_display_over_usb_which_then_enrols() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("display");
    let hub = hubd::open_hub(
        Config {
            data_dir: dir.path().join("hub"),
            ..Config::default()
        },
        Arc::new(FakeClock::parse("2026-11-10T20:00:00Z")),
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
        secret: hub.new_admin_pairing("PC").unwrap(),
    };
    let manager = HubClient::pinned("127.0.0.1", port, vec![pairing.spki_sha256]).unwrap();
    let admin = manager
        .pair(&pairing.secret_text(), "PC")
        .await
        .unwrap()
        .admin_token;
    let grant = manager.new_enrolment(&admin, "Desk").await.unwrap();
    let bundle = hub_client::provisioning::bundle(&grant, vec!["127.0.0.1".into()], port, None).unwrap();

    let (path, display) = display_on_pty(state.clone());
    let mut port_handle = open(&path);
    let sent = {
        let bundle = bundle.clone();
        tokio::task::spawn_blocking(move || {
            let first = hello(&mut port_handle).unwrap();
            assert!(!first.provisioned);
            let ok = provision(&mut port_handle, &bundle);
            let again = provision(&mut port_handle, &bundle);
            (ok, again)
        })
        .await
        .unwrap()
    };
    let (ok, again) = sent;
    assert_eq!(ok.unwrap().device.hardware_revision, "simulated");
    assert_eq!(again.unwrap_err(), UsbError::AlreadyProvisioned);
    display.join().unwrap();

    // What the display stored is exactly what was sent, and it enrols with it.
    assert!(usb::load_bundle(&state).unwrap() == bundle);
    let enrolled = paired::enrol(&bundle, &state).await.unwrap();
    assert!(usb::load_bundle(&state).is_none(), "the used bundle is discarded");
    assert!(manager
        .displays(&admin)
        .await
        .unwrap()
        .iter()
        .any(|d| d.id == enrolled.device_id));
    assert!(usb::is_provisioned(&state));
    paired::reset(&state).unwrap();
    assert!(!usb::is_provisioned(&state));
    let _ = tx.send(());
    server.await.unwrap().unwrap();
}

#[test]
fn a_bad_bundle_is_refused_and_silence_times_out() {
    let dir = tempfile::tempdir().unwrap();
    let (path, _display) = display_on_pty(dir.path().join("display"));
    let mut port = open(&path);
    let mut bundle: hub_protocol::provisioning::Bundle = serde_json::from_str(include_str!(
        "../../../protocol/v1/examples/device/provisioning-bundle.json"
    ))
    .unwrap();
    bundle.hub.server_name = "example.com".into();
    assert!(
        matches!(provision(&mut port, &bundle), Err(UsbError::InvalidBundle(m)) if m.contains("server_name"))
    );

    // A port with nothing on the other end.
    let (_master, quiet) = serialport::TTYPort::pair().unwrap();
    let quiet_path = quiet.name().unwrap();
    drop(quiet);
    let mut port = open(&quiet_path);
    let started = std::time::Instant::now();
    assert_eq!(hello(&mut port).unwrap_err(), UsbError::NoAnswer);
    assert!(started.elapsed() >= Duration::from_secs(9));
}
