//! Runs pairing, enrolment, polling, heartbeats and revocation with every log level turned on, for
//! every crate, and checks no credential or one-time code appears in the output.

use std::io::Write;
use std::sync::{Arc, Mutex};

use hub_client::HubClient;
use hub_core::api::NewSubscription;
use hub_core::{FakeClock, Service, Source};
use hub_protocol::device::DeviceInfo;
use hub_protocol::pairing::PairingString;
use hubd::config::Config;

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_credential_reaches_the_logs() {
    let capture = Capture::default();
    let writer = capture.clone();
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new("trace"))
        .with_writer(move || writer.clone())
        .with_ansi(false)
        .init();

    let dir = tempfile::tempdir().unwrap();
    let clock = FakeClock::parse("2026-11-10T20:00:00Z");
    let hub = hubd::open_hub(
        Config {
            data_dir: dir.path().into(),
            ..Config::default()
        },
        Arc::new(clock),
    )
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(hubd::serve_https(hub.clone(), listener, async {
        let _ = rx.await;
    }));
    let c = HubClient::pinned("127.0.0.1", port, vec![hub.identity.spki_sha256]).unwrap();

    let mut secrets = Vec::new();
    let raw = hub.new_admin_pairing("PC").unwrap();
    let pairing = PairingString {
        host: "127.0.0.1".into(),
        port,
        spki_sha256: hub.identity.spki_sha256,
        secret: raw,
    };
    secrets.push(pairing.secret_text());
    secrets.push(pairing.encode());
    let admin = c.pair(&pairing.secret_text(), "PC").await.unwrap().admin_token;
    secrets.push(admin.clone());
    c.add_subscription(
        &admin,
        &NewSubscription {
            service: Service::ClaudeMax,
            source: Source::Demo,
            title: None,
            id: None,
        },
    )
    .await
    .unwrap();
    let grant = c.new_enrolment(&admin, "Desk").await.unwrap();
    secrets.push(grant.secret.clone());
    let info = DeviceInfo {
        device_type: "llm-usage-display".into(),
        firmware_version: "1".into(),
        hardware_revision: "sim".into(),
        protocol_version: 1,
    };
    let e = c.enrol(&grant.secret, info.clone()).await.unwrap();
    secrets.push(e.device_token.clone());
    c.device_snapshot(Some(&e.device_token)).await.unwrap();
    c.heartbeat(&e.device_token, &info).await.unwrap();
    let _ = c.enrol(&grant.secret, info).await; // a reuse, which raises an alarm
    let _ = c.pair(&pairing.secret_text(), "again").await;
    c.revoke_display(&admin, &e.device_id).await.unwrap();
    let _ = c.device_snapshot(Some(&e.device_token)).await;
    let _ = tx.send(());
    server.await.unwrap().unwrap();

    let log = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
    assert!(
        log.contains("display enrolled"),
        "the capture works:\n{}",
        &log[..log.len().min(2000)]
    );
    for s in &secrets {
        assert!(!log.contains(s.as_str()), "a secret reached the log");
        // Partial leaks too: the random part of a token on its own.
        let tail = &s[s.len().saturating_sub(16)..];
        assert!(!log.contains(tail), "part of a secret reached the log");
    }
    assert!(
        !log.to_lowercase().contains("bearer "),
        "an Authorization header reached the log"
    );
}
