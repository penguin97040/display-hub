//! `hubd pair-helper --local`, `hubd helper list` and `hubd helper revoke` against a hub serving in
//! the same data folder, as the Pi's `display-hub-helper` command uses them.

use std::path::Path;
use std::process::{Command, Output};
use std::sync::Arc;

use hub_client::HubClient;
use hub_core::api::FeedContent;
use hub_protocol::pairing::PairingString;

const BIN: &str = env!("CARGO_BIN_EXE_hubd");

fn hubd(data: &Path, args: &[&str]) -> Output {
    Command::new(BIN)
        .args(args)
        .arg("--data-dir")
        .arg(data)
        .arg("--config")
        .arg(data.join("hubd.toml"))
        .env_remove("HUBD_LOG")
        .output()
        .unwrap()
}

fn stdout(o: &Output) -> String {
    assert!(
        o.status.success(),
        "hubd failed: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    String::from_utf8_lossy(&o.stdout).into_owned()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_local_invite_pairs_over_loopback_and_revoking_cuts_the_helper_off() {
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("hub");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(data.join("hubd.toml"), "").unwrap();
    let hub = hubd::open_hub(
        hubd::config::Config {
            data_dir: data.clone(),
            ..Default::default()
        },
        Arc::new(hub_core::SystemClock),
    )
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(hubd::serve_https(hub.clone(), listener, async {
        let _ = rx.await;
    }));

    let d = data.clone();
    let out = tokio::task::spawn_blocking(move || {
        stdout(&hubd(
            &d,
            &[
                "pair-helper",
                "--local",
                "--feed",
                "pi-status",
                "--label",
                "Pi helper pi-status",
            ],
        ))
    })
    .await
    .unwrap();
    let line = out
        .split_whitespace()
        .find(|w| w.starts_with("dh1."))
        .expect("a helper string");
    let pairing = PairingString::decode(line).unwrap();
    assert_eq!(pairing.host, "127.0.0.1");
    assert_eq!(pairing.spki_sha256, hub.identity.spki_sha256);

    // The default port is 8443; this hub listens elsewhere, so connect to its real port.
    let client = HubClient::pinned(&pairing.host, port, vec![pairing.spki_sha256]).unwrap();
    let paired = client.helper_pair(&pairing.secret_text()).await.unwrap();
    assert_eq!(paired.feeds, vec!["pi-status".to_string()]);
    let content = FeedContent {
        kind: "big_number".into(),
        data: serde_json::json!({ "value": 47.5, "decimals": 1, "unit": "°C" }),
        collected_at: None,
        note: None,
    };
    client
        .put_feed(&paired.helper_token, "pi-status", &content)
        .await
        .unwrap();

    let (d, id) = (data.clone(), paired.helper_id.clone());
    let (listed, revoked, again, relisted, plain, unknown) = tokio::task::spawn_blocking(move || {
        let listed = stdout(&hubd(&d, &["helper", "list"]));
        let revoked = stdout(&hubd(&d, &["helper", "revoke", &id]));
        let again = hubd(&d, &["helper", "revoke", &id]);
        let relisted = stdout(&hubd(&d, &["helper", "list"]));
        let plain = stdout(&hubd(&d, &["helper", "list", "--plain"]));
        let unknown = hubd(&d, &["helper", "revoke", "no-such-helper"]);
        (listed, revoked, again, relisted, plain, unknown)
    })
    .await
    .unwrap();
    assert!(listed.contains(&paired.helper_id), "{listed}");
    assert!(listed.contains("active"), "{listed}");
    assert!(listed.contains("feeds: pi-status"), "{listed}");
    assert!(revoked.contains("Revoked"), "{revoked}");
    assert!(again.status.success(), "revoking twice is fine: {again:?}");
    assert!(
        String::from_utf8_lossy(&again.stdout).contains("already cut off"),
        "{again:?}"
    );
    assert!(!unknown.status.success(), "an unknown id is still refused");
    assert!(
        String::from_utf8_lossy(&unknown.stderr).contains("no helper with id"),
        "{unknown:?}"
    );
    assert_eq!(
        plain,
        format!("{}\trevoked\tPi helper pi-status\n", paired.helper_id),
        "tab-separated for scripts"
    );
    assert!(relisted.contains("REVOKED"), "{relisted}");

    let refused = client
        .put_feed(&paired.helper_token, "pi-status", &content)
        .await
        .unwrap_err();
    assert_eq!(refused.code(), Some(hub_protocol::ErrorCode::TokenRevoked));

    let _ = tx.send(());
    server.await.unwrap().unwrap();
}

#[test]
fn local_and_host_together_are_refused() {
    let root = tempfile::tempdir().unwrap();
    let o = Command::new(BIN)
        .args([
            "pair-helper",
            "--local",
            "--host",
            "hub.local",
            "--feed",
            "a",
            "--data-dir",
        ])
        .arg(root.path())
        .output()
        .unwrap();
    assert!(!o.status.success());
    assert!(!root.path().join("hub.db").exists(), "nothing was made");
}
