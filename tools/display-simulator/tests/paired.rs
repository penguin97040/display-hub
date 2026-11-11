//! M2's exit check: a simulated display set up from a provisioning bundle syncs over pinned HTTPS
//! with its own credential, sees page changes, and shows the right state once revoked.

use std::sync::Arc;

use display_simulator::render::{count_pages, screen};
use display_simulator::{paired, Device, Link, NoJitter, Problem};
use hub_client::HubClient;
use hub_core::api::NewSubscription;
use hub_core::{Clock, FakeClock, Service, Source};
use hub_protocol::pairing::PairingString;
use hubd::config::Config;

#[tokio::test]
async fn a_bundle_enrols_the_display_which_then_follows_the_hub() {
    let dir = tempfile::tempdir().unwrap();
    let state_dir = dir.path().join("display");
    let clock = FakeClock::parse("2026-11-10T20:00:00Z");
    let hub = hubd::open_hub(
        Config {
            data_dir: dir.path().join("hub"),
            ..Config::default()
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

    // The manager's side: pair, add pages, ask for an enrolment, build the bundle.
    let raw = hub.new_admin_pairing("PC").unwrap();
    let pairing = PairingString {
        host: "127.0.0.1".into(),
        port,
        spki_sha256: hub.identity.spki_sha256,
        secret: raw,
    };
    let manager = HubClient::pinned(&pairing.host, pairing.port, vec![pairing.spki_sha256]).unwrap();
    let admin = manager
        .pair(&pairing.secret_text(), "PC")
        .await
        .unwrap()
        .admin_token;
    for service in Service::ALL {
        manager
            .add_subscription(
                &admin,
                &NewSubscription {
                    service,
                    source: Source::Demo,
                    title: None,
                    id: None,
                },
            )
            .await
            .unwrap();
    }
    let grant = manager.new_enrolment(&admin, "Desk").await.unwrap();
    // The first host is a dead address, as a stale DHCP lease would be; the display tries the next.
    let bundle =
        hub_client::provisioning::bundle(&grant, vec!["127.0.0.2".into(), "127.0.0.1".into()], port, None)
            .unwrap();
    assert_eq!(bundle.hub.trust_anchors[0].certificate_pem, hub.identity.cert_pem);

    // The display's side.
    let state = paired::enrol(&bundle, &state_dir).await.unwrap();
    assert!(paired::enrol(&bundle, &dir.path().join("second"))
        .await
        .unwrap_err()
        .contains("already used"));
    let mut device = Device::new(Some(paired::cache_file(&state_dir)), Box::new(NoJitter));
    device.accept(clock.now(), paired::snapshot(&state).await);
    assert_eq!(count_pages(&screen(&device, clock.now())), 3);
    paired::heartbeat(&state).await.unwrap();

    // Changes in the manager reach the next poll, with no change on the display.
    manager.remove_subscription(&admin, "chatgpt-plus").await.unwrap();
    manager
        .reorder(&admin, vec!["opencode-go-plus".into(), "claude-max".into()])
        .await
        .unwrap();
    device.accept(
        clock.now(),
        paired::snapshot(&paired::load(&state_dir).unwrap()).await,
    );
    let s = screen(&device, clock.now());
    assert_eq!(count_pages(&s), 2);
    assert!(
        s.find("OpenCode Go Plus").unwrap() < s.find("Claude Max").unwrap(),
        "{s}"
    );
    let displays = manager.displays(&admin).await.unwrap();
    assert_eq!(displays.len(), 1);
    assert_eq!(
        displays[0].device.as_ref().unwrap().hardware_revision,
        "simulated"
    );

    // Revoked: the display keeps its saved pages but says it needs setting up again.
    manager.revoke_display(&admin, &state.device_id).await.unwrap();
    device.accept(clock.now(), paired::snapshot(&state).await);
    assert!(matches!(
        device.link,
        Link::Trouble {
            problem: Problem::NeedsSetup(_),
            ..
        }
    ));
    let s = screen(&device, clock.now());
    assert!(s.contains("needs setting up again"), "{s}");
    assert_eq!(count_pages(&s), 2);

    // A factory reset erases the credentials.
    paired::reset(&state_dir).unwrap();
    assert!(paired::load(&state_dir).is_err());
    let _ = tx.send(());
    server.await.unwrap().unwrap();
}
