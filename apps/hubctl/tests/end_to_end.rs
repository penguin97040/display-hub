//! The helper against a real hub over pinned HTTPS: invited by the manager, logged in from the
//! helper string, then Claude Code status line figures and Codex's limits reaching the right pages.

use std::sync::Arc;

use hub_client::HubClient;
use hub_core::api::{NewCollector, NewSubscription};
use hub_core::{FakeClock, Service, Source, Window};
use hub_protocol::pairing::PairingString;
use hub_protocol::DataQuality;
use hubctl::commands;
use os_secrets::{MemoryStore, TokenStore};

const STATUS_JSON: &str = r#"{"model":{"display_name":"Opus 4.7"},
  "rate_limits":{"five_hour":{"used_percentage":42.5,"resets_at":1900000000},
                 "seven_day":{"used_percentage":17,"resets_at":1900500000}}}"#;

#[tokio::test(flavor = "multi_thread")]
async fn helper_figures_reach_collector_pages() {
    let dir = tempfile::tempdir().unwrap();
    // The hub's clock is real time here: the helper stamps reports with the PC's clock.
    let hub = hubd::open_hub(
        hubd::config::Config {
            data_dir: dir.path().join("hub"),
            ..Default::default()
        },
        Arc::new(hub_core::SystemClock),
    )
    .unwrap();
    let _ = FakeClock::parse("2026-11-11T00:00:00Z");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(hubd::serve_https(hub.clone(), listener, async {
        let _ = rx.await;
    }));

    // The manager's side: pages fed by the helper, and an invite.
    let pin = hub.identity.spki_sha256;
    let manager = HubClient::pinned("127.0.0.1", port, vec![pin]).unwrap();
    let raw = hub.new_admin_pairing("PC").unwrap();
    let pairing = PairingString {
        host: "127.0.0.1".into(),
        port,
        spki_sha256: pin,
        secret: raw,
    };
    let admin = manager
        .pair(&pairing.secret_text(), "PC")
        .await
        .unwrap()
        .admin_token;
    for service in [Service::ClaudeMax, Service::ChatgptPlus] {
        manager
            .add_subscription(
                &admin,
                &NewSubscription {
                    service,
                    source: Source::Collector,
                    title: None,
                    id: None,
                },
            )
            .await
            .unwrap();
    }
    let invite = manager
        .new_collector(
            &admin,
            &NewCollector {
                label: "Desk PC".into(),
                services: vec![Service::ClaudeMax, Service::ChatgptPlus],
            },
        )
        .await
        .unwrap();
    let mut secret = [0u8; 16];
    use base64::Engine;
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

    // The helper's side, on its own thread (the commands run their own runtime, as the binary does).
    let hdir = dir.path().join("helper");
    let store = MemoryStore::default();
    let (d, s) = (hdir.clone(), store.clone());
    let out = tokio::task::spawn_blocking(move || {
        let login = commands::login(&d, &s, &format!("  {helper_string}\n"));
        let mut line = Vec::new();
        commands::claude_statusline(&d, &mut STATUS_JSON.as_bytes(), &mut line, false, false);
        let first = commands::send_pending(&d, &s);
        // A second update within the minute queues nothing.
        let mut ignored = Vec::new();
        commands::claude_statusline(&d, &mut STATUS_JSON.as_bytes(), &mut ignored, true, false);
        let second = commands::send_pending(&d, &s);
        (login, String::from_utf8(line).unwrap(), first, second, ignored)
    })
    .await
    .unwrap();
    let (login, line, first, second, quiet) = out;
    assert!(login.unwrap().contains("ChatGPT Plus and Claude Max"));
    assert_eq!(line.trim(), "Opus 4.7 · 5h 42% · 7d 17%");
    assert!(quiet.is_empty(), "--quiet prints nothing");
    assert_eq!(first.unwrap(), "sent to claude-max");
    assert_eq!(second.unwrap(), "nothing to send", "throttled to once a minute");
    assert!(
        !std::fs::read_to_string(hdir.join("hubctl.json"))
            .unwrap()
            .contains("dhc_"),
        "no token in files"
    );
    assert!(store
        .items
        .lock()
        .unwrap()
        .values()
        .all(|t| t.starts_with("dhc_")));

    let snap = manager.preview_snapshot(&admin).await.unwrap();
    let claude = snap.pages.iter().find(|p| p.id == "claude-max").unwrap();
    assert_eq!(claude.data_quality, DataQuality::ProviderReported);
    assert_eq!(claude.metrics[0].used_percent, Some(42.5));
    assert_eq!(claude.metrics[1].resets_at.unwrap().unix(), 1_900_500_000);

    // Codex, through a stand-in for `codex app-server` that answers as documented.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let fake = dir.path().join("codex");
        std::fs::write(
            &fake,
            "#!/bin/sh\n[ \"$1\" = app-server ] || exit 2\nwhile IFS= read -r line; do\n  case \"$line\" in\n    \
             *'\"initialize\"'*) echo '{\"id\":0,\"result\":{}}' ;;\n    \
             *'account/rateLimits/read'*) echo '{\"id\":1,\"result\":{\"rateLimits\":{\"primary\":{\"usedPercent\":33,\"windowDurationMins\":300,\"resetsAt\":1900000000},\"secondary\":{\"usedPercent\":70,\"windowDurationMins\":10080,\"resetsAt\":1900500000},\"planType\":\"plus\"}}}' ;;\n  esac\ndone\n",
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let (d, s) = (hdir.clone(), store.clone());
        let dry = tokio::task::spawn_blocking({
            let (d, s, f) = (d.clone(), s.clone(), fake.clone());
            move || commands::codex_sync(&d, &s, Some(&f), true)
        })
        .await
        .unwrap();
        assert_eq!(dry.unwrap(), "Codex reports five-hour 33%, weekly 70% (not sent)");
        let sent = tokio::task::spawn_blocking(move || commands::codex_sync(&d, &s, Some(&fake), false))
            .await
            .unwrap();
        assert_eq!(
            sent.unwrap(),
            "Codex reports five-hour 33%, weekly 70%: sent to chatgpt-plus"
        );
        let snap = manager.preview_snapshot(&admin).await.unwrap();
        let gpt = snap.pages.iter().find(|p| p.id == "chatgpt-plus").unwrap();
        assert_eq!(
            (gpt.metrics[0].used_percent, gpt.metrics[1].used_percent),
            (Some(33.0), Some(70.0))
        );
        assert!(gpt.metrics.iter().all(|m| m.used_percent.is_some()));
        let _ = Window::Weekly;
    }

    // Removed in the manager: the helper's next send is refused, and says why in its log.
    let id = manager.collectors(&admin).await.unwrap()[0].id.clone();
    manager.revoke_collector(&admin, &id).await.unwrap();
    std::fs::remove_file(hdir.join("claude-sent.json")).unwrap();
    let (d, s) = (hdir.clone(), store.clone());
    let after = tokio::task::spawn_blocking(move || {
        commands::claude_statusline(&d, &mut STATUS_JSON.as_bytes(), &mut Vec::new(), true, false);
        commands::send_pending(&d, &s)
    })
    .await
    .unwrap();
    assert!(after.unwrap_err().contains("revoked"));
    assert!(std::fs::read_to_string(hdir.join("hubctl.log"))
        .unwrap()
        .contains("revoked"));

    // Logging out forgets the token.
    let account = store.items.lock().unwrap().keys().next().cloned().unwrap();
    commands::logout(&hdir, &store).unwrap();
    assert!(store.load(&account).unwrap().is_none());
    let _ = tx.send(());
    server.await.unwrap().unwrap();
}
