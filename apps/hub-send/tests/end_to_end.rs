//! The `hub-send` tool against a real hub over pinned HTTPS: invited by the manager, logged in from
//! the helper string with the token kept in a file, then pages sent to feeds. The file store is for
//! Unix only, so the tests that use it run there; the credential-store test runs everywhere.

#[cfg(unix)]
use std::io::Write;
#[cfg(unix)]
use std::path::Path;
#[cfg(unix)]
use std::process::{Command, Output, Stdio};
use std::sync::Arc;

use base64::Engine;
use hub_client::HubClient;
use hub_core::api::NewHelper;
#[cfg(unix)]
use hub_core::api::NewPage;
#[cfg(unix)]
use hub_core::Source;
use hub_protocol::pairing::PairingString;

#[cfg(unix)]
const BIN: &str = env!("CARGO_BIN_EXE_hub-send");

/// Runs `hub-send` with the file store in `dir`, feeding `stdin`.
#[cfg(unix)]
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

#[cfg(unix)]
fn text(o: &Output) -> (String, String) {
    (
        String::from_utf8_lossy(&o.stdout).into_owned(),
        String::from_utf8_lossy(&o.stderr).into_owned(),
    )
}

#[cfg(unix)]
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
                    template: None,
                    // What the owner chose for the note page, for the helper to read.
                    settings: (id == "garden-note")
                        .then(|| serde_json::json!({"bed": "beans"}).as_object().unwrap().clone()),
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
        let settings = run(&cfg2, &["settings", "garden"], "");
        let private_settings = run(&cfg2, &["settings", "private"], "");
        (
            login,
            token,
            ok,
            piped,
            big,
            forbidden,
            bad_json,
            status,
            (settings, private_settings),
        )
    })
    .await
    .unwrap();
    let (login, token, ok, piped, big, forbidden, bad_json, status, (settings, private_settings)) = outputs;

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

    // The helper reads what the owner chose for its feed's pages, but not another feed's.
    let (out, err) = text(&settings);
    assert!(settings.status.success(), "{err}");
    let read: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(
        read,
        serde_json::json!({"pages": [
            {"page_id": "garden-note", "template": null, "settings": {"bed": "beans"}},
            {"page_id": "garden-count", "template": null, "settings": {}}
        ]})
    );
    let (_, err) = text(&private_settings);
    assert!(!private_settings.status.success());
    assert!(err.contains("403"), "{err}");

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

/// Two profiles paired with the same hub, sharing one credential store, keep separate tokens: logging
/// in the second doesn't overwrite the first, and logging out of one leaves the other working.
#[tokio::test(flavor = "multi_thread")]
async fn two_profiles_on_one_hub_keep_their_own_tokens() {
    use hub_send::Profile;
    use os_secrets::{MemoryStore, TokenStore};

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
    let pin = hub.identity.spki_sha256;
    let manager = HubClient::pinned("127.0.0.1", port, vec![pin]).unwrap();
    let raw = hub.new_admin_pairing("PC").unwrap();
    let admin = manager
        .pair(
            &PairingString {
                host: "127.0.0.1".into(),
                port,
                spki_sha256: pin,
                secret: raw,
            }
            .secret_text(),
            "PC",
        )
        .await
        .unwrap()
        .admin_token;
    let mut strings = Vec::new();
    for label in ["First", "Second", "Second again"] {
        let invite = manager
            .new_helper(
                &admin,
                &NewHelper {
                    label: label.into(),
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
        strings.push(
            PairingString {
                host: "127.0.0.1".into(),
                port,
                spki_sha256: pin,
                secret,
            }
            .encode(),
        );
    }

    let store = MemoryStore::default();
    let profile = |name: &str| {
        Profile::new(
            "hub-send",
            root.path().join(name),
            Box::new(store.clone()) as Box<dyn TokenStore>,
        )
    };
    let (a, b) = (profile("a"), profile("b"));
    let now = hub_protocol::Timestamp::from_unix(1_900_000_000).unwrap();
    let cfg_a = a.login(&strings[0], now).await.unwrap();
    let cfg_b = b.login(&strings[1], now).await.unwrap();
    assert_ne!(cfg_a.helper_id, cfg_b.helper_id);
    assert_eq!(store.items.lock().unwrap().len(), 2, "one entry each");
    let token_of = |cfg: &hub_send::Config| {
        store
            .load(&format!("token-{}", cfg.helper_id))
            .unwrap()
            .map(|t| t.to_string())
    };
    let (ta, tb) = (token_of(&cfg_a).unwrap(), token_of(&cfg_b).unwrap());
    assert_ne!(ta, tb, "logging in B left A's token alone");
    assert!(a.open().is_ok() && b.open().is_ok());

    // Logging out of A leaves B's token in place.
    a.logout().unwrap();
    assert_eq!(token_of(&cfg_b).as_deref(), Some(tb.as_str()));
    assert!(b.open().is_ok());
    assert!(token_of(&cfg_a).is_none());

    // Logging B in again, without logging out, replaces its token rather than leaving the old one.
    let cfg_b2 = b.login(&strings[2], now).await.unwrap();
    assert_ne!(cfg_b2.helper_id, cfg_b.helper_id);
    assert!(token_of(&cfg_b).is_none(), "the earlier token is gone");
    assert!(token_of(&cfg_b2).is_some());
    assert_eq!(store.items.lock().unwrap().len(), 1);
    assert!(b.open().is_ok());

    let _ = tx.send(());
    server.await.unwrap().unwrap();
}
