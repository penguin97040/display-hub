//! The hub over real HTTPS on loopback, with pinned clients and a fake clock: pairing, enrolment,
//! roles, revocation, identity pinning, limits and persistence.

use std::path::Path;
use std::sync::Arc;

use hub_client::{ClientError, HubClient};
use hub_core::api::{ManualFigures, ManualMetric, NewSubscription, SubscriptionChange};
use hub_core::{Clock, FakeClock, Service, Source, Window};
use hub_protocol::device::DeviceInfo;
use hub_protocol::pairing::PairingString;
use hub_protocol::{DataQuality, ErrorAction, ErrorCode};
use hubd::api::Hub;
use hubd::config::Config;
use tokio::sync::oneshot;

/// The items of a page that must be a meter.
fn items(page: &hub_protocol::Page) -> Vec<hub_protocol::MeterItem> {
    match page.content().unwrap() {
        Some(hub_protocol::PageContent::Meter(m)) => m.items,
        other => panic!("not a meter: {other:?}"),
    }
}

const NOW: &str = "2026-11-10T20:00:00Z";

struct Running {
    hub: Arc<Hub>,
    port: u16,
    stop: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<()>,
}

impl Running {
    async fn start(dir: &Path, clock: &FakeClock) -> Running {
        let config = Config {
            data_dir: dir.to_path_buf(),
            ..Config::default()
        };
        let hub = hubd::open_hub(config, Arc::new(clock.clone())).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = oneshot::channel();
        let h = hub.clone();
        let task = tokio::spawn(async move {
            hubd::serve_https(h, listener, async {
                let _ = rx.await;
            })
            .await
            .unwrap();
        });
        Running {
            hub,
            port,
            stop: Some(tx),
            task,
        }
    }

    fn client(&self) -> HubClient {
        HubClient::pinned("127.0.0.1", self.port, vec![self.hub.identity.spki_sha256]).unwrap()
    }

    fn pairing_secret(&self) -> String {
        let raw = self.hub.new_admin_pairing("Test PC").unwrap();
        PairingString {
            host: "127.0.0.1".into(),
            port: self.port,
            spki_sha256: self.hub.identity.spki_sha256,
            secret: raw,
        }
        .secret_text()
    }

    async fn admin(&self) -> String {
        self.client()
            .pair(&self.pairing_secret(), "Test PC")
            .await
            .unwrap()
            .admin_token
    }

    async fn stop(mut self) {
        let _ = self.stop.take().unwrap().send(());
        self.task.await.unwrap();
    }
}

fn info(rev: &str) -> DeviceInfo {
    DeviceInfo {
        device_type: "llm-usage-display".into(),
        firmware_version: "0.0.0-simulator".into(),
        hardware_revision: rev.into(),
        protocol_version: 1,
    }
}

async fn enrolled(r: &Running, admin: &str, name: &str) -> (String, String) {
    let grant = r.client().new_enrolment(admin, name).await.unwrap();
    let e = r.client().enrol(&grant.secret, info("simulated")).await.unwrap();
    (e.device_id, e.device_token)
}

fn code(e: &ClientError) -> Option<ErrorCode> {
    e.code()
}

#[tokio::test]
async fn admin_pairing_works_once_and_reuse_raises_an_alarm() {
    let dir = tempfile::tempdir().unwrap();
    let clock = FakeClock::parse(NOW);
    let r = Running::start(dir.path(), &clock).await;
    let secret = r.pairing_secret();
    let paired = r.client().pair(&secret, "Test PC").await.unwrap();
    assert!(paired.admin_token.starts_with("dha_"));
    assert_eq!(
        paired.identity.spki_sha256,
        hub_protocol::pairing::hex(&r.hub.identity.spki_sha256)
    );
    let status = r.client().status(&paired.admin_token).await.unwrap();
    assert_eq!(status.admins, 1);

    let again = r.client().pair(&secret, "Someone else").await.unwrap_err();
    assert_eq!(code(&again), Some(ErrorCode::EnrolmentUsed));
    let events = r.hub.store.audit_events(10).unwrap();
    assert!(
        events.iter().any(|(_, e, _)| e == "ALARM_pairing_code_reused"),
        "{events:?}"
    );
    r.stop().await;
}

#[tokio::test]
async fn expired_or_unknown_pairing_codes_fail() {
    let dir = tempfile::tempdir().unwrap();
    let clock = FakeClock::parse(NOW);
    let r = Running::start(dir.path(), &clock).await;
    let secret = r.pairing_secret();
    clock.advance(601);
    let e = r.client().pair(&secret, "Late").await.unwrap_err();
    assert_eq!(code(&e), Some(ErrorCode::EnrolmentExpired));
    let e = r
        .client()
        .pair("AAAAAAAAAAAAAAAAAAAAAA", "Guess")
        .await
        .unwrap_err();
    assert_eq!(code(&e), Some(ErrorCode::EnrolmentExpired));
    r.stop().await;
}

#[tokio::test]
async fn unpaired_and_unknown_displays_get_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let clock = FakeClock::parse(NOW);
    let r = Running::start(dir.path(), &clock).await;
    let e = r.client().device_snapshot(None).await.unwrap_err();
    assert!(matches!(e, ClientError::Status { status: 401, .. }), "{e:?}");
    assert_eq!(code(&e), Some(ErrorCode::Unauthorised));
    let fake = format!("dhd_{}", "A".repeat(43));
    let e = r.client().device_snapshot(Some(&fake)).await.unwrap_err();
    assert_eq!(code(&e), Some(ErrorCode::Unauthorised));
    r.stop().await;
}

#[tokio::test]
async fn enrolled_displays_see_pages_and_codes_work_once() {
    let dir = tempfile::tempdir().unwrap();
    let clock = FakeClock::parse(NOW);
    let r = Running::start(dir.path(), &clock).await;
    let admin = r.admin().await;
    let c = r.client();
    for (service, source) in [
        (Service::ClaudeMax, Source::Demo),
        (Service::ChatgptPlus, Source::None),
        (Service::OpencodeGoPlus, Source::Manual),
    ] {
        c.add_subscription(
            &admin,
            &NewSubscription {
                service,
                source,
                title: None,
                id: None,
            },
        )
        .await
        .unwrap();
    }
    let grant = c.new_enrolment(&admin, "Desk").await.unwrap();
    let e = c.enrol(&grant.secret, info("simulated")).await.unwrap();
    assert!(e.device_token.starts_with("dhd_"));

    let snap = c.device_snapshot(Some(&e.device_token)).await.unwrap();
    let ids: Vec<_> = snap.pages.iter().map(|p| p.id.as_str()).collect();
    assert_eq!(ids, ["claude-max", "chatgpt-plus", "opencode-go-plus"]);
    assert_eq!(snap.pages[0].data_quality, DataQuality::Demo);
    assert_eq!(snap.pages[1].data_quality, DataQuality::Unavailable);
    assert_eq!(
        snap.pages[2].data_quality,
        DataQuality::Unavailable,
        "manual page with no figures yet"
    );

    let again = c.enrol(&grant.secret, info("simulated")).await.unwrap_err();
    assert_eq!(code(&again), Some(ErrorCode::EnrolmentUsed));
    assert_eq!(again.code().unwrap().action(), ErrorAction::Reprovision);

    let late = c.new_enrolment(&admin, "Kitchen").await.unwrap();
    clock.advance(601);
    let e2 = c.enrol(&late.secret, info("simulated")).await.unwrap_err();
    assert_eq!(code(&e2), Some(ErrorCode::EnrolmentExpired));
    r.stop().await;
}

#[tokio::test]
async fn each_display_gets_its_own_ticked_pages() {
    let dir = tempfile::tempdir().unwrap();
    let clock = FakeClock::parse(NOW);
    let r = Running::start(dir.path(), &clock).await;
    let admin = r.admin().await;
    let c = r.client();
    let add = |service| NewSubscription {
        service,
        source: Source::Demo,
        title: None,
        id: None,
    };
    c.add_subscription(&admin, &add(Service::ClaudeMax))
        .await
        .unwrap();
    let (desk, desk_token) = enrolled(&r, &admin, "Desk").await;
    // A page added after a display enrols shows on it too.
    c.add_subscription(&admin, &add(Service::ChatgptPlus))
        .await
        .unwrap();
    let (kitchen, kitchen_token) = enrolled(&r, &admin, "Kitchen").await;
    let ids = |s: &hub_protocol::Snapshot| s.pages.iter().map(|p| p.id.clone()).collect::<Vec<_>>();
    for token in [&desk_token, &kitchen_token] {
        let snap = c.device_snapshot(Some(token)).await.unwrap();
        assert_eq!(ids(&snap), ["claude-max", "chatgpt-plus"]);
    }

    assert!(r
        .hub
        .store
        .set_display_pages(&kitchen, &["chatgpt-plus".into()])
        .unwrap());
    let snap = c.device_snapshot(Some(&kitchen_token)).await.unwrap();
    assert_eq!(ids(&snap), ["chatgpt-plus"]);
    let snap = c.device_snapshot(Some(&desk_token)).await.unwrap();
    assert_eq!(ids(&snap), ["claude-max", "chatgpt-plus"]);

    // The preview shows one display's pages, or with none named, all of them.
    let preview = c.preview_display_snapshot(&admin, &kitchen).await.unwrap();
    assert_eq!(ids(&preview), ["chatgpt-plus"]);
    let preview = c.preview_snapshot(&admin).await.unwrap();
    assert_eq!(ids(&preview), ["claude-max", "chatgpt-plus"]);
    let unknown = c.preview_display_snapshot(&admin, "d-nope").await.unwrap_err();
    assert_eq!(code(&unknown), Some(ErrorCode::NotFound));

    // A revoked display has no preview, and its ticks are gone.
    c.revoke_display(&admin, &desk).await.unwrap();
    let gone = c.preview_display_snapshot(&admin, &desk).await.unwrap_err();
    assert_eq!(code(&gone), Some(ErrorCode::NotFound));
    assert_eq!(r.hub.store.display_page_ids(&desk).unwrap(), None);
    r.stop().await;
}

#[tokio::test]
async fn admin_changes_reach_the_next_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let clock = FakeClock::parse(NOW);
    let r = Running::start(dir.path(), &clock).await;
    let admin = r.admin().await;
    let c = r.client();
    for service in Service::ALL {
        c.add_subscription(
            &admin,
            &NewSubscription {
                service,
                source: Source::Manual,
                title: None,
                id: None,
            },
        )
        .await
        .unwrap();
    }
    let (_, token) = enrolled(&r, &admin, "Desk").await;

    c.reorder(
        &admin,
        vec![
            "opencode-go-plus".into(),
            "claude-max".into(),
            "chatgpt-plus".into(),
        ],
    )
    .await
    .unwrap();
    c.remove_subscription(&admin, "chatgpt-plus").await.unwrap();
    c.change_subscription(
        &admin,
        "claude-max",
        &SubscriptionChange {
            title: Some("  Work Claude ".into()),
            source: None,
        },
    )
    .await
    .unwrap();
    c.set_manual(
        &admin,
        "claude-max",
        &ManualFigures {
            metrics: vec![ManualMetric {
                window: Window::FiveHour,
                used_percent: Some(33.33),
                resets_at: None,
            }],
            note: Some("Typed in".into()),
        },
    )
    .await
    .unwrap();

    let snap = c.device_snapshot(Some(&token)).await.unwrap();
    let ids: Vec<_> = snap.pages.iter().map(|p| p.id.as_str()).collect();
    assert_eq!(ids, ["opencode-go-plus", "claude-max"]);
    assert_eq!(snap.pages[1].title, "Work Claude");
    assert_eq!(snap.pages[1].data_quality, DataQuality::Manual);
    assert_eq!(items(&snap.pages[1])[0].percent, Some(33.3));

    // Reorders must be complete, manual figures only on manual pages, windows must exist.
    let e = c.reorder(&admin, vec!["claude-max".into()]).await.unwrap_err();
    assert_eq!(code(&e), Some(ErrorCode::Conflict));
    c.change_subscription(
        &admin,
        "claude-max",
        &SubscriptionChange {
            title: None,
            source: Some(Source::Demo),
        },
    )
    .await
    .unwrap();
    let e = c
        .set_manual(
            &admin,
            "claude-max",
            &ManualFigures {
                metrics: vec![],
                note: None,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(code(&e), Some(ErrorCode::Conflict));
    let e = c
        .set_manual(
            &admin,
            "opencode-go-plus",
            &ManualFigures {
                metrics: vec![ManualMetric {
                    window: Window::FiveHour,
                    used_percent: Some(101.0),
                    resets_at: None,
                }],
                note: None,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(code(&e), Some(ErrorCode::BadRequest));
    assert!(e.to_string().contains("Percentages must be 0 to 100."), "{e}");
    r.stop().await;
}

#[tokio::test]
async fn roles_are_separate() {
    let dir = tempfile::tempdir().unwrap();
    let clock = FakeClock::parse(NOW);
    let r = Running::start(dir.path(), &clock).await;
    let admin = r.admin().await;
    let (_, device) = enrolled(&r, &admin, "Desk").await;
    let c = r.client();
    for e in [
        c.status(&device).await.unwrap_err(),
        c.subscriptions(&device).await.unwrap_err(),
        c.new_enrolment(&device, "Sneaky").await.unwrap_err(),
        c.displays(&device).await.unwrap_err(),
    ] {
        assert!(matches!(e, ClientError::Status { status: 403, .. }), "{e:?}");
        assert_eq!(code(&e), Some(ErrorCode::Forbidden));
    }
    let e = c.device_snapshot(Some(&admin)).await.unwrap_err();
    assert_eq!(code(&e), Some(ErrorCode::Forbidden));
    r.stop().await;
}

#[tokio::test]
async fn a_display_only_ever_acts_as_itself() {
    let dir = tempfile::tempdir().unwrap();
    let clock = FakeClock::parse(NOW);
    let r = Running::start(dir.path(), &clock).await;
    let admin = r.admin().await;
    let (a_id, a) = enrolled(&r, &admin, "Desk").await;
    let (b_id, _b) = enrolled(&r, &admin, "Kitchen").await;
    let c = r.client();
    // A heartbeat names no device: the hub takes the identity from the token.
    c.heartbeat(&a, &info("board-a")).await.unwrap();
    let displays = c.displays(&admin).await.unwrap();
    let get = |id: &str| displays.iter().find(|d| d.id == id).unwrap().clone();
    assert_eq!(get(&a_id).device.unwrap().hardware_revision, "board-a");
    assert_eq!(
        get(&b_id).device.unwrap().hardware_revision,
        "simulated",
        "B untouched"
    );
    r.stop().await;
}

#[tokio::test]
async fn revoked_displays_and_admins_are_shut_out() {
    let dir = tempfile::tempdir().unwrap();
    let clock = FakeClock::parse(NOW);
    let r = Running::start(dir.path(), &clock).await;
    let admin = r.admin().await;
    let (id, token) = enrolled(&r, &admin, "Lost board").await;
    let c = r.client();
    c.device_snapshot(Some(&token)).await.unwrap();
    c.revoke_display(&admin, &id).await.unwrap();
    let e = c.device_snapshot(Some(&token)).await.unwrap_err();
    assert_eq!(code(&e), Some(ErrorCode::TokenRevoked));
    assert_eq!(e.code().unwrap().action(), ErrorAction::Reprovision);
    assert!(c.displays(&admin).await.unwrap().is_empty());

    let n = r
        .hub
        .store
        .revoke_all(hubd::secret::Role::Admin, clock.now())
        .unwrap();
    assert_eq!(n, 1);
    let e = c.status(&admin).await.unwrap_err();
    assert_eq!(code(&e), Some(ErrorCode::TokenRevoked));
    r.stop().await;
}

#[tokio::test]
async fn a_different_hub_key_is_refused_before_anything_is_sent() {
    let (one, two) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let clock = FakeClock::parse(NOW);
    let real = Running::start(one.path(), &clock).await;
    // An impostor at another address (a spoofed mDNS answer, or a hijacked IP), with its own key.
    let impostor = Running::start(two.path(), &clock).await;
    let secret = real.pairing_secret();
    let tricked = HubClient::pinned("127.0.0.1", impostor.port, vec![real.hub.identity.spki_sha256]).unwrap();
    let e = tricked.pair(&secret, "Test PC").await.unwrap_err();
    assert!(matches!(e, ClientError::IdentityMismatch), "{e:?}");
    assert!(e.is_connection());
    // The secret never reached anyone, so it still works on the real hub.
    real.client().pair(&secret, "Test PC").await.unwrap();
    real.stop().await;
    impostor.stop().await;
}

#[tokio::test]
async fn malformed_and_oversized_bodies_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let clock = FakeClock::parse(NOW);
    let r = Running::start(dir.path(), &clock).await;
    let raw = reqwest::Client::builder()
        .use_preconfigured_tls(hub_client::pin::client_config(vec![r.hub.identity.spki_sha256]))
        .build()
        .unwrap();
    let url = format!("https://127.0.0.1:{}/v1/device/enrol", r.port);
    let send = |body: Vec<u8>| {
        raw.post(&url)
            .header("content-type", "application/json")
            .body(body)
            .send()
    };
    let resp = send(b"{\"enrolment_secret\": 5".to_vec()).await.unwrap();
    assert_eq!(resp.status(), 400);
    let body: hub_protocol::ErrorBody = resp.json().await.unwrap();
    assert_eq!(body.error.code, ErrorCode::BadRequest);
    let resp = send(vec![b' '; 4096]).await.unwrap();
    assert_eq!(resp.status(), 413);
    let resp = raw
        .post(&url)
        .json(&serde_json::json!({"enrolment_secret": "x", "device": {"device_type": "a\u{7}", "firmware_version": "1", "hardware_revision": "1", "protocol_version": 1}}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    r.stop().await;
}

#[tokio::test]
async fn guessing_codes_is_rate_limited() {
    let dir = tempfile::tempdir().unwrap();
    let clock = FakeClock::parse(NOW);
    let r = Running::start(dir.path(), &clock).await;
    let c = r.client();
    let mut last = None;
    for i in 0..12 {
        last = Some(c.pair(&format!("guess{i:017}"), "x").await.unwrap_err());
    }
    assert_eq!(code(last.as_ref().unwrap()), Some(ErrorCode::RateLimited));
    // A correct code from the same address is refused too until the window passes.
    let e = c.pair(&r.pairing_secret(), "x").await.unwrap_err();
    assert_eq!(code(&e), Some(ErrorCode::RateLimited));

    let mut last = None;
    for _ in 0..21 {
        last = Some(c.device_snapshot(Some("dhd_nope")).await.unwrap_err());
    }
    assert_eq!(code(last.as_ref().unwrap()), Some(ErrorCode::RateLimited));
    r.stop().await;
}

#[tokio::test]
async fn data_and_identity_survive_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let clock = FakeClock::parse(NOW);
    let r = Running::start(dir.path(), &clock).await;
    let admin = r.admin().await;
    let c = r.client();
    c.add_subscription(
        &admin,
        &NewSubscription {
            service: Service::ClaudeMax,
            source: Source::Manual,
            title: None,
            id: None,
        },
    )
    .await
    .unwrap();
    c.set_manual(
        &admin,
        "claude-max",
        &ManualFigures {
            metrics: vec![ManualMetric {
                window: Window::Weekly,
                used_percent: Some(61.0),
                resets_at: None,
            }],
            note: None,
        },
    )
    .await
    .unwrap();
    let (id, token) = enrolled(&r, &admin, "Desk").await;
    c.device_snapshot(Some(&token)).await.unwrap();
    let pin = r.hub.identity.spki_sha256;
    r.stop().await;

    clock.advance(3600);
    let r = Running::start(dir.path(), &clock).await;
    assert_eq!(
        r.hub.identity.spki_sha256, pin,
        "same identity, so displays still trust it"
    );
    let snap = r.client().device_snapshot(Some(&token)).await.unwrap();
    assert_eq!(items(&snap.pages[0])[0].percent, Some(61.0));
    assert_eq!(snap.pages[0].updated_at.unwrap().to_string(), NOW);
    let displays = r.client().displays(&admin).await.unwrap();
    assert_eq!(displays[0].id, id);
    assert!(
        displays[0].last_seen_at.is_some(),
        "last-seen times are flushed at shutdown"
    );
    r.stop().await;
}

#[test]
fn a_missing_identity_with_an_existing_database_stops_the_hub() {
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        ..Config::default()
    };
    let clock: Arc<dyn Clock> = Arc::new(FakeClock::parse(NOW));
    drop(hubd::open_hub(config.clone(), clock.clone()).unwrap());
    std::fs::remove_dir_all(config.identity_dir()).unwrap();
    let Err(e) = hubd::open_hub(config, clock) else {
        panic!("opened without its identity")
    };
    assert!(e.to_string().contains("lock them all out"), "{e}");
}

#[test]
fn damaged_or_newer_databases_are_left_alone() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hub.db");
    std::fs::write(&path, b"this is not a database, but it is somebody's file").unwrap();
    let Err(e) = hubd::store::Store::open(&path) else {
        panic!("opened it")
    };
    assert!(matches!(e, hubd::store::StoreError::Corrupt { .. }), "{e}");
    assert_eq!(
        std::fs::read(&path).unwrap(),
        b"this is not a database, but it is somebody's file"
    );

    let path = dir.path().join("new.db");
    drop(hubd::store::Store::open(&path).unwrap());
    rusqlite::Connection::open(&path)
        .unwrap()
        .pragma_update(None, "user_version", 99)
        .unwrap();
    let Err(e) = hubd::store::Store::open(&path) else {
        panic!("opened it")
    };
    assert!(
        matches!(e, hubd::store::StoreError::TooNew { found: 99, .. }),
        "{e}"
    );
}

#[tokio::test]
async fn a_pc_helper_reports_only_its_services_to_collector_pages() {
    use hub_core::api::{NewCollector, UsageReport};
    let dir = tempfile::tempdir().unwrap();
    let clock = FakeClock::parse(NOW);
    let r = Running::start(dir.path(), &clock).await;
    let c = r.client();
    let admin = r.admin().await;
    for (service, source) in [
        (Service::ClaudeMax, Source::Collector),
        (Service::ChatgptPlus, Source::Collector),
        (Service::OpencodeGoPlus, Source::Manual),
    ] {
        c.add_subscription(
            &admin,
            &NewSubscription {
                service,
                source,
                title: None,
                id: None,
            },
        )
        .await
        .unwrap();
    }

    // Invite for Claude Max only; the invite works once.
    let invite = c
        .new_collector(
            &admin,
            &NewCollector {
                label: "Desk PC".into(),
                services: vec![Service::ClaudeMax],
            },
        )
        .await
        .unwrap();
    let paired = c.collector_pair(&invite.secret).await.unwrap();
    assert!(paired.collector_token.starts_with("dhc_"));
    assert_eq!(paired.services, vec![Service::ClaudeMax]);
    let again = c.collector_pair(&invite.secret).await.unwrap_err();
    assert_eq!(again.code(), Some(ErrorCode::EnrolmentUsed));
    let token = paired.collector_token;

    let report = |service, pct: Option<f64>, at: &str| UsageReport {
        service,
        collected_at: hub_protocol::Timestamp::parse(at).unwrap(),
        metrics: vec![
            ManualMetric {
                window: Window::FiveHour,
                used_percent: pct,
                resets_at: Some(hub_protocol::Timestamp::parse("2026-11-10T22:00:00Z").unwrap()),
            },
            ManualMetric {
                window: Window::Weekly,
                used_percent: None,
                resets_at: None,
            },
        ],
    };
    let ok = c
        .report_usage(
            &token,
            &report(Service::ClaudeMax, Some(42.25), "2026-11-10T19:59:00Z"),
        )
        .await
        .unwrap();
    assert_eq!(ok.updated, vec!["claude-max".to_string()]);
    let snap = c.preview_snapshot(&admin).await.unwrap();
    let page = snap.pages.iter().find(|p| p.id == "claude-max").unwrap();
    assert_eq!(page.data_quality, DataQuality::ProviderReported);
    assert_eq!(items(page)[0].percent, Some(42.3));
    assert_eq!(items(page)[1].percent, None, "unknown stays unknown");
    assert_eq!(page.updated_at.unwrap().to_string(), "2026-11-10T19:59:00Z");

    // Outside its scope, from the future, malformed, or with another role's token: refused.
    let other = c
        .report_usage(&token, &report(Service::ChatgptPlus, Some(1.0), NOW))
        .await
        .unwrap_err();
    assert_eq!(other.code(), Some(ErrorCode::Forbidden));
    let future = c
        .report_usage(
            &token,
            &report(Service::ClaudeMax, Some(1.0), "2026-11-10T21:00:00Z"),
        )
        .await;
    assert_eq!(future.unwrap_err().code(), Some(ErrorCode::BadRequest));
    let bad = c
        .report_usage(&token, &report(Service::ClaudeMax, Some(101.0), NOW))
        .await;
    assert_eq!(bad.unwrap_err().code(), Some(ErrorCode::BadRequest));
    assert_eq!(
        c.report_usage(&admin, &report(Service::ClaudeMax, Some(1.0), NOW))
            .await
            .unwrap_err()
            .code(),
        Some(ErrorCode::Forbidden)
    );
    assert_eq!(
        c.status(&token).await.unwrap_err().code(),
        Some(ErrorCode::Forbidden)
    );

    // A manual page of the same kind is never written by a helper.
    let manual = c
        .new_collector(
            &admin,
            &NewCollector {
                label: "x".into(),
                services: vec![Service::OpencodeGoPlus],
            },
        )
        .await
        .unwrap();
    let t2 = c.collector_pair(&manual.secret).await.unwrap().collector_token;
    let none = c
        .report_usage(&t2, &report(Service::OpencodeGoPlus, Some(5.0), NOW))
        .await
        .unwrap();
    assert!(none.updated.is_empty());

    // Listed, then revoked: its token stops working.
    let list = c.collectors(&admin).await.unwrap();
    assert_eq!(list.len(), 2);
    let id = list.iter().find(|x| x.label == "Desk PC").unwrap().id.clone();
    c.revoke_collector(&admin, &id).await.unwrap();
    let revoked = c
        .report_usage(&token, &report(Service::ClaudeMax, Some(1.0), NOW))
        .await
        .unwrap_err();
    assert_eq!(revoked.code(), Some(ErrorCode::TokenRevoked));
    assert!(c
        .collectors(&admin)
        .await
        .unwrap()
        .iter()
        .any(|x| x.id == id && x.revoked));
    r.stop().await;
}

#[tokio::test]
async fn an_upgraded_hub_serves_each_display_what_it_did_before() {
    let dir = tempfile::tempdir().unwrap();
    let clock = FakeClock::parse(NOW);
    // A hub as the schema 2 release left it: its identity, and a database with representative data
    // and credentials whose tokens this test knows.
    Running::start(dir.path(), &clock).await.stop().await;
    for f in ["hub.db", "hub.db-wal", "hub.db-shm"] {
        let _ = std::fs::remove_file(dir.path().join(f));
    }
    {
        let conn = rusqlite::Connection::open(dir.path().join("hub.db")).unwrap();
        hubd::store::create_at_schema(&conn, 2).unwrap();
        conn.execute_batch(include_str!("fixtures/schema-2-data.sql"))
            .unwrap();
        for (token, role, subject) in [
            ("dha_test_admin", "admin", "a-test"),
            ("dhd_test_desk", "device", "d-desk"),
            ("dhd_test_kitchen", "device", "d-kitchen"),
        ] {
            conn.execute(
                "INSERT INTO credentials (token_hash, role, subject, label, created_at) VALUES (?1, ?2, ?3, 'Test', 0)",
                rusqlite::params![&hubd::secret::hash(token)[..], role, subject],
            )
            .unwrap();
        }
    }
    let r = Running::start(dir.path(), &clock).await;
    assert_eq!(r.hub.store.schema_version().unwrap(), 3);
    let expected: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/schema-2-pages.json")).unwrap();
    let c = r.client();
    for token in ["dhd_test_desk", "dhd_test_kitchen"] {
        let snap = c.device_snapshot(Some(token)).await.unwrap();
        assert_eq!(serde_json::to_value(&snap.pages).unwrap(), expected, "{token}");
    }
    let preview = c.preview_snapshot("dha_test_admin").await.unwrap();
    assert_eq!(serde_json::to_value(&preview.pages).unwrap(), expected);
    // The manager's usage view still lists every page, in order.
    let usage: Vec<String> = c
        .subscriptions("dha_test_admin")
        .await
        .unwrap()
        .into_iter()
        .map(|s| s.id)
        .collect();
    let ids: Vec<&str> = expected
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["id"].as_str().unwrap())
        .collect();
    assert_eq!(usage, ids);
    r.stop().await;
}

#[tokio::test]
async fn pages_of_any_kind_through_the_admin_api() {
    use hub_core::api::{NewPage, PageChange, PageContentInput};
    use serde_json::json;
    let dir = tempfile::tempdir().unwrap();
    let clock = FakeClock::parse(NOW);
    let r = Running::start(dir.path(), &clock).await;
    let admin = r.admin().await;
    let c = r.client();
    let new = |title: &str, kind: &str| NewPage {
        title: title.into(),
        kind: kind.into(),
        source: Source::Manual,
        feed: None,
        id: None,
        stale_after_seconds: None,
    };
    let usage = c
        .add_subscription(
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
    let flights = c
        .add_page(&admin, &new("Flights overhead!", "nearby-flights/1"))
        .await
        .unwrap();
    assert_eq!(flights.id, "flights-overhead", "an id from the title");
    assert_eq!(flights.stale_after_seconds, Source::Manual.stale_after_seconds());
    let words = c.add_page(&admin, &new("Note", "text")).await.unwrap();
    let again = c.add_page(&admin, &new("Note", "text")).await.unwrap();
    assert_eq!(again.id, "note-2");

    // Bad requests are refused with a reason.
    for bad in [
        new("x", "gauge"),
        new("\u{7}", "text"),
        NewPage {
            feed: Some("Not A Feed".into()),
            ..new("x", "text")
        },
        NewPage {
            stale_after_seconds: Some(5),
            ..new("x", "text")
        },
        NewPage {
            id: Some("-x".into()),
            ..new("x", "text")
        },
    ] {
        let e = c.add_page(&admin, &bad).await.unwrap_err();
        assert_eq!(code(&e), Some(ErrorCode::BadRequest), "{bad:?}");
    }

    // Content is checked against the kind's rules and the size cap, and only manual pages take it.
    let data = json!({"flights": [{"callsign": "ANZ1", "altitude_m": 10000}]});
    let set = |data: serde_json::Value| PageContentInput {
        data: Some(data),
        note: Some("  Seen from the deck ".into()),
    };
    let stored = c
        .set_page_content(&admin, &flights.id, &set(data.clone()))
        .await
        .unwrap();
    let content = stored.content.unwrap();
    assert_eq!(content.data, Some(data.clone()));
    assert_eq!(content.quality, DataQuality::Manual);
    assert_eq!(content.note.as_deref(), Some("Seen from the deck"));
    assert_eq!(c.page(&admin, &flights.id).await.unwrap().content, Some(content));
    let e = c
        .set_page_content(&admin, &words.id, &set(json!({"text": ""})))
        .await
        .unwrap_err();
    assert_eq!(code(&e), Some(ErrorCode::BadRequest));
    assert!(e.to_string().contains("text: must not be empty"), "{e}");
    let e = c
        .set_page_content(&admin, &flights.id, &set(json!({"big": "x".repeat(4000)})))
        .await
        .unwrap_err();
    assert!(e.to_string().contains("at most 3072 bytes"), "{e}");
    let e = c
        .set_page_content(&admin, &usage.id, &set(json!({"items": []})))
        .await
        .unwrap_err();
    assert_eq!(code(&e), Some(ErrorCode::Conflict));
    let missing = c.page(&admin, "nope").await.unwrap_err();
    assert_eq!(code(&missing), Some(ErrorCode::NotFound));

    // Titles and stale times change; the kind and stored content stay.
    let changed = c
        .change_page(
            &admin,
            &flights.id,
            &PageChange {
                title: Some("Planes".into()),
                stale_after_seconds: Some(900),
                ..PageChange::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        (changed.title.as_str(), changed.stale_after_seconds),
        ("Planes", 900)
    );
    assert!(changed.content.is_some());

    // The manager's usage reorder leaves other pages where they are.
    let (display, token) = enrolled(&r, &admin, "Desk").await;
    let order = ["note-2", "claude-max", "note", "flights-overhead"].map(String::from);
    c.reorder_pages(&admin, order.to_vec()).await.unwrap();
    c.reorder(&admin, vec!["claude-max".into()]).await.unwrap();
    let ids: Vec<String> = c.pages(&admin).await.unwrap().into_iter().map(|p| p.id).collect();
    assert_eq!(ids, order);

    // A display's ticks, through the API.
    assert_eq!(c.display_pages(&admin, &display).await.unwrap(), order);
    let ticked = c
        .set_display_pages(&admin, &display, vec!["flights-overhead".into(), "note-2".into()])
        .await
        .unwrap();
    assert_eq!(ticked, ["note-2", "flights-overhead"]);
    let snap = c.device_snapshot(Some(&token)).await.unwrap();
    let shown: Vec<(&str, &str)> = snap
        .pages
        .iter()
        .map(|p| (p.id.as_str(), p.kind.as_str()))
        .collect();
    assert_eq!(
        shown,
        [("note-2", "text"), ("flights-overhead", "nearby-flights/1")]
    );
    assert_eq!(snap.pages[1].data, Some(data));
    let e = c
        .set_display_pages(&admin, &display, vec!["nope".into()])
        .await
        .unwrap_err();
    assert_eq!(code(&e), Some(ErrorCode::Conflict));
    let e = c.display_pages(&admin, "d-nope").await.unwrap_err();
    assert_eq!(code(&e), Some(ErrorCode::NotFound));

    c.remove_page(&admin, "note-2").await.unwrap();
    assert_eq!(
        c.display_pages(&admin, &display).await.unwrap(),
        ["flights-overhead"]
    );
    assert_eq!(c.status(&admin).await.unwrap().pages, 3);
    r.stop().await;
}

#[tokio::test]
async fn the_admin_api_keeps_its_page_rules() {
    use hub_core::api::{NewPage, PageChange, PageContentInput};
    use serde_json::json;
    let dir = tempfile::tempdir().unwrap();
    let clock = FakeClock::parse(NOW);
    let r = Running::start(dir.path(), &clock).await;
    let admin = r.admin().await;
    let c = r.client();
    let new = |title: &str, kind: &str, source, feed: Option<&str>| NewPage {
        title: title.into(),
        kind: kind.into(),
        source,
        feed: feed.map(String::from),
        id: None,
        stale_after_seconds: None,
    };
    let refused = |e: ClientError, words: &str| {
        assert_eq!(code(&e), Some(ErrorCode::BadRequest), "{e}");
        assert!(e.to_string().contains(words), "{e}");
    };

    // Collector pages are meters with a feed, until collectors can fill other kinds.
    let e = c
        .add_page(&admin, &new("T", "text", Source::Collector, Some("claude-max")))
        .await
        .unwrap_err();
    refused(e, "Only a meter page with a feed");
    let e = c
        .add_page(&admin, &new("M", "meter", Source::Collector, None))
        .await
        .unwrap_err();
    refused(e, "Only a meter page with a feed");
    c.add_page(&admin, &new("M", "meter", Source::Collector, Some("claude-max")))
        .await
        .unwrap();
    let words = c
        .add_page(&admin, &new("Words", "text", Source::Manual, None))
        .await
        .unwrap();
    let to_collector = PageChange {
        source: Some(Source::Collector),
        ..PageChange::default()
    };
    let e = c.change_page(&admin, &words.id, &to_collector).await.unwrap_err();
    refused(e, "Only a meter page with a feed");
    assert_eq!(c.page(&admin, &words.id).await.unwrap().source, Source::Manual);
    let usage = c
        .add_subscription(
            &admin,
            &NewSubscription {
                service: Service::ClaudeMax,
                source: Source::Manual,
                title: Some("Mine".into()),
                id: Some("mine".into()),
            },
        )
        .await
        .unwrap();
    let e = c.change_page(&admin, "nope", &to_collector).await.unwrap_err();
    assert_eq!(code(&e), Some(ErrorCode::NotFound));

    // `order` is the reorder route's, never a page's.
    let e = c
        .add_page(
            &admin,
            &NewPage {
                id: Some("order".into()),
                ..new("x", "text", Source::Manual, None)
            },
        )
        .await
        .unwrap_err();
    refused(e, "reserved");
    let order = c
        .add_page(&admin, &new("Order", "text", Source::Manual, None))
        .await
        .unwrap();
    assert_eq!(order.id, "order-2");

    // The hub checks typed-in usage figures against the service, with its old wording.
    let figures = |windows: &[Window]| ManualFigures {
        metrics: windows
            .iter()
            .map(|&window| ManualMetric {
                window,
                used_percent: Some(10.0),
                resets_at: None,
            })
            .collect(),
        note: None,
    };
    let e = c
        .set_manual(&admin, &usage.id, &figures(&[Window::Monthly]))
        .await
        .unwrap_err();
    refused(e, "Claude Max has no monthly window.");
    let e = c
        .set_manual(&admin, &usage.id, &figures(&[Window::FiveHour, Window::FiveHour]))
        .await
        .unwrap_err();
    refused(e, "The five-hour window is listed twice.");
    // Through the generic route too: a meter item that isn't one of the service's windows.
    let e = c
        .set_page_content(
            &admin,
            &usage.id,
            &PageContentInput {
                data: Some(json!({"items": [{"id": "daily", "label": "Daily", "percent": 1}]})),
                note: None,
            },
        )
        .await
        .unwrap_err();
    refused(e, "Claude Max has no daily window.");
    assert_eq!(
        c.page(&admin, &usage.id).await.unwrap().content,
        None,
        "nothing was stored"
    );
    // Shares are rounded to one decimal place, and labels are the service's own.
    let e = c
        .set_page_content(
            &admin,
            &usage.id,
            &PageContentInput {
                data: Some(json!({"items": [{"id": "weekly", "label": "Mine", "percent": 42.25}]})),
                note: None,
            },
        )
        .await
        .unwrap();
    let items = &e.content.unwrap().data.unwrap()["items"];
    assert_eq!(items[0]["percent"], json!(42.3));
    assert_eq!(items[0]["label"], json!("Weekly allowance used"));

    // No figures at all is "no figures yet", as it always was; other meter rules stay strict.
    let saved = c.set_manual(&admin, &usage.id, &figures(&[])).await.unwrap();
    let data = saved.data.unwrap();
    assert_eq!((data.quality, data.metrics.len()), (DataQuality::Manual, 0));
    let preview = c.preview_snapshot(&admin).await.unwrap();
    let page = preview.pages.iter().find(|p| p.id == usage.id).unwrap();
    assert_eq!(page.data_quality, DataQuality::Unavailable);
    assert_eq!(page.note.as_deref(), Some(hub_core::build::NO_FIGURES_NOTE));
    let e = c
        .set_page_content(
            &admin,
            &usage.id,
            &PageContentInput {
                data: Some(json!({"items": [], "extra": 1})),
                note: None,
            },
        )
        .await
        .unwrap_err();
    refused(e, "unknown field");
    // The same holds for any manual meter page, not only usage pages.
    let plain = c
        .add_page(&admin, &new("Gauges", "meter", Source::Manual, None))
        .await
        .unwrap();
    let empty = PageContentInput {
        data: Some(json!({"items": []})),
        note: None,
    };
    let stored = c.set_page_content(&admin, &plain.id, &empty).await.unwrap();
    assert_eq!(stored.content.unwrap().data, Some(json!({"items": []})));

    // Ticks are a set, so repeats don't count towards the limit.
    let display = enrolled(&r, &admin, "Desk").await.0;
    let many = vec![words.id.clone(); 60];
    assert_eq!(
        c.set_display_pages(&admin, &display, many).await.unwrap(),
        std::slice::from_ref(&words.id)
    );
    r.stop().await;
}
