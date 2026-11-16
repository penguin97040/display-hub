//! The `hub-send` tool against a real hub over pinned HTTPS: invited by the manager, logged in from
//! the helper string with the token kept in a file, then pages sent to feeds.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::sync::Arc;

use base64::Engine;
use hub_client::HubClient;
use hub_core::api::{NewHelper, NewPage};
use hub_core::Source;
use hub_protocol::pairing::PairingString;

const BIN: &str = env!("CARGO_BIN_EXE_hub-send");

/// Runs `hub-send` with the file store in `dir`, feeding `stdin`.
fn run(dir: &Path, args: &[&str], stdin: &str) -> Output {
    let mut child = Command::new(BIN)
        .arg("--config-dir")
        .arg(dir)
        .args(["--store", "file"])
        .args(args)
        .env_remove("HUB_SEND_DIR")
        .env_remove("HUB_SEND_STORE")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(stdin.as_bytes()).unwrap();
    child.wait_with_output().unwrap()
}

fn text(o: &Output) -> (String, String) {
    (
        String::from_utf8_lossy(&o.stdout).into_owned(),
        String::from_utf8_lossy(&o.stderr).into_owned(),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn login_then_pages_to_allowed_other_kind_and_forbidden_feeds() {
    let root = tempfile::tempdir().unwrap();
    let hub = hubd::open_hub(
        hubd::config::Config {
            data_dir: root.path().join("hub"),
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

    // The manager's side: two pages on one feed (a text and a big number), another feed the helper
    // won't be invited to, and an invite.
    let pin = hub.identity.spki_sha256;
    let manager = HubClient::pinned("127.0.0.1", port, vec![pin]).unwrap();
    let raw = hub.new_admin_pairing("PC").unwrap();
    let admin_pairing = PairingString {
        host: "127.0.0.1".into(),
        port,
        spki_sha256: pin,
        secret: raw,
    };
    let admin = manager
        .pair(&admin_pairing.secret_text(), "PC")
        .await
        .unwrap()
        .admin_token;
    for (id, kind, feed) in [
        ("garden-note", "text", "garden"),
        ("garden-count", "big_number", "garden"),
        ("private-note", "text", "private"),
    ] {
        manager
            .add_page(
                &admin,
                &NewPage {
                    title: id.into(),
                    kind: kind.into(),
                    source: Source::Helper,
                    feed: Some(feed.into()),
                    id: Some(id.into()),
                    stale_after_seconds: None,
                },
            )
            .await
            .unwrap();
    }
    let invite = manager
        .new_helper(
            &admin,
            &NewHelper {
                label: "Garden Pi".into(),
                feeds: vec!["garden".into()],
            },
        )
        .await
        .unwrap();
    let mut secret = [0u8; 16];
    secret.copy_from_slice(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(&invite.secret)
            .unwrap(),
    );
    let helper_string = PairingString {
        host: "127.0.0.1".into(),
        port,
        spki_sha256: pin,
        secret,
    }
    .encode();

    let cfg = root.path().join("helper-config");
    let data = root.path().join("data.json");
    std::fs::write(&data, r#"{"text": "Watered the beans"}"#).unwrap();
    let (cfg2, data2, helper_string2) = (cfg.clone(), data.clone(), helper_string.clone());
    let outputs = tokio::task::spawn_blocking(move || {
        let login = run(&cfg2, &["login"], &format!("  {helper_string2}\n"));
        let token = std::fs::read_to_string(cfg2.join("token")).unwrap_or_default();
        let data = data2.to_str().unwrap();
        let ok = run(
            &cfg2,
            &[
                "page",
                "garden",
                "--kind",
                "text",
                "--data",
                data,
                "--note",
                "from the greenhouse",
            ],
            "",
        );
        // Standard input as the data, and a time given.
        let piped = run(
            &cfg2,
            &[
                "page",
                "garden",
                "--kind",
                "text",
                "--data",
                "-",
                "--collected-at",
                "2026-11-16T09:30:00Z",
            ],
            r#"{"text":"Piped in"}"#,
        );
        let big = run(
            &cfg2,
            &["page", "garden", "--kind", "big_number", "--data", "-"],
            r#"{"value": 7}"#,
        );
        let forbidden = run(&cfg2, &["page", "private", "--kind", "text", "--data", data], "");
        let bad_json = run(
            &cfg2,
            &["page", "garden", "--kind", "text", "--data", "-"],
            "not json",
        );
        let status = run(&cfg2, &["status"], "");
        (login, token, ok, piped, big, forbidden, bad_json, status)
    })
    .await
    .unwrap();
    let (login, token, ok, piped, big, forbidden, bad_json, status) = outputs;

    let (out, err) = text(&login);
    assert!(login.status.success(), "{err}");
    assert!(out.contains("Paired with the hub at 127.0.0.1"), "{out}");
    assert!(out.contains("garden"));
    assert!(token.starts_with("dhc_"), "the token is in the file");
    assert!(!std::fs::read_to_string(cfg.join("connection.json"))
        .unwrap()
        .contains("dhc_"));

    // Allowed feed: the text page is updated; the big number page of the same feed is left alone.
    let (out, err) = text(&ok);
    assert!(ok.status.success(), "{err}");
    assert!(out.contains("Updated 1 page: garden-note"), "{out}");
    assert!(out.contains("garden-count"), "skipped page is named: {out}");
    assert!(piped.status.success());

    // Another kind on the same feed: the big number page is updated, the text page skipped.
    let (out, err) = text(&big);
    assert!(big.status.success(), "{err}");
    assert!(out.contains("Updated 1 page: garden-count"), "{out}");
    assert!(out.contains("garden-note"), "the text page is skipped: {out}");

    // A feed it wasn't invited for is refused, with the hub's message, and exits non-zero.
    let (out, err) = text(&forbidden);
    assert!(!forbidden.status.success());
    assert!(out.is_empty(), "{out}");
    assert!(err.contains("hub-send:") && err.contains("403"), "{err}");

    let (_, err) = text(&bad_json);
    assert!(!bad_json.status.success());
    assert!(err.contains("isn't valid JSON"), "{err}");

    // The hub shows what was sent.
    let snap = manager.preview_snapshot(&admin).await.unwrap();
    let note = snap.pages.iter().find(|p| p.id == "garden-note").unwrap();
    assert_eq!(note.data_quality, hub_protocol::DataQuality::ProviderReported);
    let private = snap.pages.iter().find(|p| p.id == "private-note").unwrap();
    assert_ne!(private.data_quality, hub_protocol::DataQuality::ProviderReported);

    // Nothing the tool printed holds the token.
    let (sout, serr) = text(&status);
    assert!(status.status.success(), "{serr}");
    assert!(sout.contains("Reachable: yes"), "{sout}");
    for o in [&login, &ok, &piped, &big, &forbidden, &bad_json, &status] {
        let (out, err) = text(o);
        assert!(!out.contains(&token) && !err.contains(&token));
    }
    assert!(!std::fs::read_to_string(cfg.join("helper.log"))
        .unwrap()
        .contains(&token));

    // File layout, then logout removes the token and the settings.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&cfg), 0o700);
        assert_eq!(mode(&cfg.join("token")), 0o600);
    }
    let cfg3 = cfg.clone();
    let out = tokio::task::spawn_blocking(move || run(&cfg3, &["logout"], ""))
        .await
        .unwrap();
    assert!(out.status.success());
    assert!(!cfg.join("token").exists());
    assert!(!cfg.join("connection.json").exists());

    let _ = tx.send(());
    server.await.unwrap().unwrap();
}

#[test]
fn a_token_file_others_can_read_stops_the_tool_with_a_clear_message() {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("connection.json"),
            r#"{"host":"127.0.0.1","port":1,"spki_sha256":"00","helper_id":"h","feeds":["garden"],"paired_at":"2026-11-16T09:00:00Z"}"#,
        )
        .unwrap();
        let token = dir.path().join("token");
        std::fs::write(&token, "dhc_not-a-real-token").unwrap();
        std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o644)).unwrap();
        let o = run(dir.path(), &["status"], "");
        let (out, err) = text(&o);
        assert!(!o.status.success());
        assert!(err.contains("can be read by other users"), "{err}");
        assert!(!out.contains("dhc_") && !err.contains("dhc_"));
    }
}
