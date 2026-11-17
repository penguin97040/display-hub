//! The hub over real HTTPS on loopback, with pinned clients and a fake clock: pairing, enrolment,
//! roles, revocation, identity pinning, limits and persistence.

use std::path::Path;
use std::sync::Arc;

use hub_client::{ClientError, HubClient};
use hub_core::api::{FeedContent, NewHelper, NewPage, PageChange, PageContentInput, PageView};
use hub_core::{Clock, FakeClock, Source};
use hub_protocol::device::DeviceInfo;
use hub_protocol::pairing::PairingString;
use hub_protocol::{DataQuality, ErrorAction, ErrorCode};
use hubd::api::Hub;
use hubd::config::Config;
use serde_json::{json, Value};
use tokio::sync::oneshot;

/// A meter page fed by `feed`, with the feed as its id.
fn meter(feed: &str, source: Source) -> NewPage {
    NewPage {
        title: feed.into(),
        kind: "meter".into(),
        source,
        feed: Some(feed.into()),
        id: None,
        stale_after_seconds: None,
        template: None,
        settings: None,
    }
}

/// A meter's data with one bar.
fn bar(percent: f64) -> Value {
    json!({"items": [{"id": "a", "label": "A", "percent": percent}]})
}

fn content(data: Value) -> PageContentInput {
    PageContentInput {
        data: Some(data),
        note: None,
    }
}

/// Adds a meter page; a demo one is given sample content, as the manager does.
async fn add_meter(c: &HubClient, admin: &str, feed: &str, source: Source) -> PageView {
    let page = c.add_page(admin, &meter(feed, source)).await.unwrap();
    if source == Source::Demo {
        return c
            .set_page_content(admin, &page.id, &content(bar(42.0)))
            .await
            .unwrap();
    }
    page
}

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
    for (feed, source) in [
        ("garden", Source::Demo),
        ("kitchen", Source::None),
        ("garage", Source::Manual),
    ] {
        add_meter(&c, &admin, feed, source).await;
    }
    let grant = c.new_enrolment(&admin, "Desk").await.unwrap();
    let e = c.enrol(&grant.secret, info("simulated")).await.unwrap();
    assert!(e.device_token.starts_with("dhd_"));

    let snap = c.device_snapshot(Some(&e.device_token)).await.unwrap();
    let ids: Vec<_> = snap.pages.iter().map(|p| p.id.as_str()).collect();
    assert_eq!(ids, ["garden", "kitchen", "garage"]);
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
    add_meter(&c, &admin, "garden", Source::Demo).await;
    let (desk, desk_token) = enrolled(&r, &admin, "Desk").await;
    // A page added after a display enrols shows on it too.
    add_meter(&c, &admin, "kitchen", Source::Demo).await;
    let (kitchen, kitchen_token) = enrolled(&r, &admin, "Kitchen").await;
    let ids = |s: &hub_protocol::Snapshot| s.pages.iter().map(|p| p.id.clone()).collect::<Vec<_>>();
    for token in [&desk_token, &kitchen_token] {
        let snap = c.device_snapshot(Some(token)).await.unwrap();
        assert_eq!(ids(&snap), ["garden", "kitchen"]);
    }

    assert!(r
        .hub
        .store
        .set_display_pages(&kitchen, &["kitchen".into()])
        .unwrap());
    let snap = c.device_snapshot(Some(&kitchen_token)).await.unwrap();
    assert_eq!(ids(&snap), ["kitchen"]);
    let snap = c.device_snapshot(Some(&desk_token)).await.unwrap();
    assert_eq!(ids(&snap), ["garden", "kitchen"]);

    // The preview shows one display's pages, or with none named, all of them.
    let preview = c.preview_display_snapshot(&admin, &kitchen).await.unwrap();
    assert_eq!(ids(&preview), ["kitchen"]);
    let preview = c.preview_snapshot(&admin).await.unwrap();
    assert_eq!(ids(&preview), ["garden", "kitchen"]);
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
    for feed in ["garden", "kitchen", "garage"] {
        add_meter(&c, &admin, feed, Source::Manual).await;
    }
    let (_, token) = enrolled(&r, &admin, "Desk").await;

    c.reorder_pages(&admin, vec!["garage".into(), "garden".into(), "kitchen".into()])
        .await
        .unwrap();
    c.remove_page(&admin, "kitchen").await.unwrap();
    let title = PageChange {
        title: Some("  Vege garden ".into()),
        ..PageChange::default()
    };
    c.change_page(&admin, "garden", &title).await.unwrap();
    let typed = PageContentInput {
        data: Some(bar(33.3)),
        note: Some("Typed in".into()),
    };
    c.set_page_content(&admin, "garden", &typed).await.unwrap();

    let snap = c.device_snapshot(Some(&token)).await.unwrap();
    let ids: Vec<_> = snap.pages.iter().map(|p| p.id.as_str()).collect();
    assert_eq!(ids, ["garage", "garden"]);
    assert_eq!(snap.pages[1].title, "Vege garden");
    assert_eq!(snap.pages[1].data_quality, DataQuality::Manual);
    assert_eq!(items(&snap.pages[1])[0].percent, Some(33.3));

    // Reorders must be complete, content only on manual and demo pages, shares in range.
    let e = c.reorder_pages(&admin, vec!["garden".into()]).await.unwrap_err();
    assert_eq!(code(&e), Some(ErrorCode::Conflict));
    let to_helper = PageChange {
        source: Some(Source::Helper),
        ..PageChange::default()
    };
    c.change_page(&admin, "garden", &to_helper).await.unwrap();
    let e = c
        .set_page_content(&admin, "garden", &content(bar(1.0)))
        .await
        .unwrap_err();
    assert_eq!(code(&e), Some(ErrorCode::Conflict));
    let e = c
        .set_page_content(&admin, "garage", &content(bar(101.0)))
        .await
        .unwrap_err();
    assert_eq!(code(&e), Some(ErrorCode::BadRequest));
    assert!(e.to_string().contains("must be 0-100"), "{e}");

    // A demo page takes content too, labelled demo.
    let to_demo = PageChange {
        source: Some(Source::Demo),
        ..PageChange::default()
    };
    c.change_page(&admin, "garage", &to_demo).await.unwrap();
    let sample = c
        .set_page_content(&admin, "garage", &content(bar(5.0)))
        .await
        .unwrap();
    assert_eq!(sample.content.unwrap().quality, DataQuality::Demo);
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
        c.pages(&device).await.unwrap_err(),
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
    assert_eq!(
        get(&a_id).pages,
        Some(Vec::new()),
        "the hub says what each display shows: nothing, as there are no pages"
    );
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
    add_meter(&c, &admin, "garden", Source::Manual).await;
    c.set_page_content(&admin, "garden", &content(bar(61.0)))
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
async fn a_helper_sends_only_to_its_feeds_and_only_pages_of_the_same_kind() {
    let dir = tempfile::tempdir().unwrap();
    let clock = FakeClock::parse(NOW);
    let r = Running::start(dir.path(), &clock).await;
    let c = r.client();
    let admin = r.admin().await;
    for (feed, source) in [
        ("garden", Source::Helper),
        ("kitchen", Source::Helper),
        ("garage", Source::Manual),
    ] {
        add_meter(&c, &admin, feed, source).await;
    }
    // A second page of the garden feed, of another kind, and a manual one of the same kind.
    let words = NewPage {
        title: "Garden words".into(),
        kind: "text".into(),
        id: Some("garden-words".into()),
        ..meter("garden", Source::Helper)
    };
    c.add_page(&admin, &words).await.unwrap();
    let manual = NewPage {
        id: Some("garden-typed".into()),
        ..meter("garden", Source::Manual)
    };
    c.add_page(&admin, &manual).await.unwrap();

    // Bad invites are refused; this one, for the garden only, works once.
    for feeds in [vec![], vec!["Garden".to_string()], vec!["a--b".to_string()]] {
        let bad = NewHelper {
            label: "x".into(),
            feeds,
        };
        let e = c.new_helper(&admin, &bad).await.unwrap_err();
        assert_eq!(code(&e), Some(ErrorCode::BadRequest), "{bad:?}");
    }
    // Too many feeds is said plainly, not blamed on the names.
    let many = NewHelper {
        label: "x".into(),
        feeds: (0..300).map(|i| format!("feed-{i}")).collect(),
    };
    let e = c.new_helper(&admin, &many).await.unwrap_err();
    assert_eq!(code(&e), Some(ErrorCode::BadRequest));
    assert!(
        e.to_string().contains("at most") && e.to_string().contains("feeds"),
        "{e}"
    );
    assert!(!e.to_string().contains("lower-case"), "{e}");
    let invite = c
        .new_helper(
            &admin,
            &NewHelper {
                label: "Desk PC".into(),
                feeds: vec!["garden".into(), "garden".into()],
            },
        )
        .await
        .unwrap();
    let paired = c.helper_pair(&invite.secret).await.unwrap();
    assert!(paired.helper_token.starts_with("dhc_"));
    assert_eq!(paired.feeds, ["garden"]);
    let again = c.helper_pair(&invite.secret).await.unwrap_err();
    assert_eq!(again.code(), Some(ErrorCode::EnrolmentUsed));
    let token = paired.helper_token;

    let body = |data: Value, at: Option<&str>| FeedContent {
        kind: "meter".into(),
        data,
        collected_at: at.map(|a| hub_protocol::Timestamp::parse(a).unwrap()),
        note: Some("  From the shed ".into()),
    };
    let ok = c
        .put_feed(&token, "garden", &body(bar(42.25), Some("2026-11-10T19:59:00Z")))
        .await
        .unwrap();
    assert_eq!(ok.updated, ["garden"]);
    assert_eq!(
        ok.skipped,
        ["garden-words"],
        "the text page of the feed is left alone"
    );
    let snap = c.preview_snapshot(&admin).await.unwrap();
    let page = |id: &str| snap.pages.iter().find(|p| p.id == id).unwrap().clone();
    let garden = page("garden");
    assert_eq!(garden.data_quality, DataQuality::ProviderReported);
    assert_eq!(items(&garden)[0].percent, Some(42.25), "stored as sent");
    assert_eq!(garden.note.as_deref(), Some("From the shed"));
    assert_eq!(garden.updated_at.unwrap().to_string(), "2026-11-10T19:59:00Z");
    assert_eq!(page("garden-words").data_quality, DataQuality::Unavailable);
    assert_eq!(page("garden-typed").data_quality, DataQuality::Unavailable);
    // A little ahead of the hub's clock is taken as now; with no time at all, it is now.
    for at in [Some("2026-11-10T20:04:00Z"), None] {
        c.put_feed(&token, "garden", &body(bar(1.0), at)).await.unwrap();
        let p = c.page(&admin, "garden").await.unwrap();
        assert_eq!(p.content.unwrap().updated_at.unwrap().to_string(), NOW);
    }

    // Any kind of page can take content from a helper.
    let words_only = FeedContent {
        kind: "text".into(),
        data: json!({"text": "Watered at 6 am"}),
        collected_at: None,
        note: None,
    };
    let ok = c.put_feed(&token, "garden", &words_only).await.unwrap();
    assert_eq!(
        (ok.updated, ok.skipped),
        (vec!["garden-words".into()], vec!["garden".into()])
    );

    // Outside its feeds, from the future, breaking the page rules, or with another role's token:
    // refused, and nothing is stored.
    let before = c.pages(&admin).await.unwrap();
    let other = c
        .put_feed(&token, "kitchen", &body(bar(1.0), None))
        .await
        .unwrap_err();
    assert_eq!(other.code(), Some(ErrorCode::Forbidden));
    let future = c
        .put_feed(&token, "garden", &body(bar(1.0), Some("2026-11-10T21:00:00Z")))
        .await
        .unwrap_err();
    assert_eq!(future.code(), Some(ErrorCode::BadRequest));
    assert!(future.to_string().contains("dated in the future"), "{future}");
    let out_of_range = c
        .put_feed(&token, "garden", &body(bar(101.0), None))
        .await
        .unwrap_err();
    assert_eq!(out_of_range.code(), Some(ErrorCode::BadRequest));
    assert!(
        out_of_range.to_string().contains("must be 0-100"),
        "{out_of_range}"
    );
    let wrong_shape = c
        .put_feed(&token, "garden", &body(json!({"text": "not a meter"}), None))
        .await
        .unwrap_err();
    assert_eq!(wrong_shape.code(), Some(ErrorCode::BadRequest));
    let huge = FeedContent {
        kind: "nearby-flights/1".into(),
        data: json!({"big": "x".repeat(4000)}),
        ..words_only.clone()
    };
    let e = c.put_feed(&token, "garden", &huge).await.unwrap_err();
    assert!(e.to_string().contains("at most 3072 bytes"), "{e}");
    let odd_kind = FeedContent {
        kind: "Not A Kind".into(),
        ..words_only.clone()
    };
    let e = c.put_feed(&token, "garden", &odd_kind).await.unwrap_err();
    assert_eq!(e.code(), Some(ErrorCode::BadRequest));
    assert_eq!(
        c.put_feed(&admin, "garden", &body(bar(1.0), None))
            .await
            .unwrap_err()
            .code(),
        Some(ErrorCode::Forbidden)
    );
    assert_eq!(
        c.status(&token).await.unwrap_err().code(),
        Some(ErrorCode::Forbidden)
    );
    assert_eq!(c.pages(&admin).await.unwrap(), before);

    // A feed no page uses yet is fine: nothing is updated.
    let spare = c
        .new_helper(
            &admin,
            &NewHelper {
                label: "Spare".into(),
                feeds: vec!["garage".into()],
            },
        )
        .await
        .unwrap();
    let t2 = c.helper_pair(&spare.secret).await.unwrap().helper_token;
    let none = c.put_feed(&t2, "garage", &body(bar(5.0), None)).await.unwrap();
    assert!(none.updated.is_empty() && none.skipped.is_empty());

    // Listed, then revoked: its token stops working.
    let list = c.helpers(&admin).await.unwrap();
    assert_eq!(list.len(), 2);
    let desk = list.iter().find(|x| x.label == "Desk PC").unwrap();
    assert_eq!(desk.feeds, ["garden"]);
    let id = desk.id.clone();
    c.revoke_helper(&admin, &id).await.unwrap();
    let revoked = c
        .put_feed(&token, "garden", &body(bar(1.0), None))
        .await
        .unwrap_err();
    assert_eq!(revoked.code(), Some(ErrorCode::TokenRevoked));
    assert!(c
        .helpers(&admin)
        .await
        .unwrap()
        .iter()
        .any(|x| x.id == id && x.revoked));
    r.stop().await;
}

/// The page-by-page check happens inside the write: content that fits the feed's stand-in but is too
/// big for one page (its id and title count) is refused, and no page of the feed is changed.
#[tokio::test]
async fn a_feed_write_is_all_or_nothing_across_its_pages() {
    let dir = tempfile::tempdir().unwrap();
    let clock = FakeClock::parse(NOW);
    let r = Running::start(dir.path(), &clock).await;
    let c = r.client();
    let admin = r.admin().await;
    let text_page = |id: &str, title: &str| NewPage {
        title: title.into(),
        kind: "nearby-flights/1".into(),
        source: Source::Helper,
        feed: Some("t".into()),
        id: Some(id.into()),
        stale_after_seconds: None,
        template: None,
        settings: None,
    };
    c.add_page(&admin, &text_page("s", "S")).await.unwrap();
    c.add_page(
        &admin,
        &text_page(
            "a-page-with-a-rather-long-id",
            "A page with a rather long title indeed",
        ),
    )
    .await
    .unwrap();
    let invite = c
        .new_helper(
            &admin,
            &NewHelper {
                label: "Tight".into(),
                feeds: vec!["t".into(), "u".into()],
            },
        )
        .await
        .unwrap();
    let token = c.helper_pair(&invite.secret).await.unwrap().helper_token;
    let body = |n: usize| FeedContent {
        kind: "nearby-flights/1".into(),
        data: json!({"big": "x".repeat(n)}),
        collected_at: None,
        note: None,
    };
    // Feed `u` has no pages, so only the stand-in is checked there. Find a size that passes that but
    // not the long page of `t`.
    let before = c.pages(&admin).await.unwrap();
    let mut found = false;
    for n in (2500..3072).rev() {
        if c.put_feed(&token, "u", &body(n)).await.is_err() {
            continue;
        }
        let e = c.put_feed(&token, "t", &body(n)).await;
        if let Err(e) = e {
            assert_eq!(e.code(), Some(ErrorCode::BadRequest), "{e}");
            assert!(e.to_string().contains("at most 3072 bytes"), "{e}");
            found = true;
            break;
        }
    }
    assert!(found, "a size that fits the stand-in but not the long page");
    let after = c.pages(&admin).await.unwrap();
    let content_of = |pages: &[PageView], id: &str| pages.iter().find(|p| p.id == id).unwrap().clone();
    assert_eq!(
        content_of(&after, "s"),
        content_of(&before, "s"),
        "the short page wasn't written"
    );
    r.stop().await;
}

/// What the schema 2 release served from `fixtures/schema-2-data.sql`, as an upgraded hub serves it.
/// Since schema 4, demo figures are stored rather than made up by the hub, and have no reset times.
fn schema_2_pages() -> Value {
    let mut pages: Value = serde_json::from_str(include_str!("fixtures/schema-2-pages.json")).unwrap();
    for p in pages.as_array_mut().unwrap() {
        if p["data_quality"] == "demo" {
            for item in p["data"]["items"].as_array_mut().unwrap() {
                item.as_object_mut().unwrap().remove("resets_at");
            }
        }
    }
    pages
}

/// A hub's data folder with its identity, and a database at `schema` holding `data`.
async fn old_hub(dir: &Path, clock: &FakeClock, schema: i64, data: &str) {
    Running::start(dir, clock).await.stop().await;
    for f in ["hub.db", "hub.db-wal", "hub.db-shm"] {
        let _ = std::fs::remove_file(dir.join(f));
    }
    let conn = rusqlite::Connection::open(dir.join("hub.db")).unwrap();
    hubd::store::create_at_schema(&conn, schema).unwrap();
    conn.execute_batch(data).unwrap();
}

#[tokio::test]
async fn an_upgraded_hub_serves_each_display_what_it_did_before() {
    let dir = tempfile::tempdir().unwrap();
    let clock = FakeClock::parse(NOW);
    // A hub as the schema 2 release left it: its identity, and a database with representative data
    // and credentials whose tokens this test knows.
    old_hub(dir.path(), &clock, 2, include_str!("fixtures/schema-2-data.sql")).await;
    {
        let conn = rusqlite::Connection::open(dir.path().join("hub.db")).unwrap();
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
    assert_eq!(r.hub.store.schema_version().unwrap(), hubd::store::SCHEMA_VERSION);
    let expected = schema_2_pages();
    let c = r.client();
    for token in ["dhd_test_desk", "dhd_test_kitchen"] {
        let snap = c.device_snapshot(Some(token)).await.unwrap();
        assert_eq!(serde_json::to_value(&snap.pages).unwrap(), expected, "{token}");
    }
    let preview = c.preview_snapshot("dha_test_admin").await.unwrap();
    assert_eq!(serde_json::to_value(&preview.pages).unwrap(), expected);
    // The admin list still has every page, in order.
    let listed: Vec<String> = c
        .pages("dha_test_admin")
        .await
        .unwrap()
        .into_iter()
        .map(|p| p.id)
        .collect();
    let ids: Vec<&str> = expected
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["id"].as_str().unwrap())
        .collect();
    assert_eq!(listed, ids);
    r.stop().await;
}

#[tokio::test]
async fn a_helper_paired_before_the_upgrade_to_feeds_keeps_working() {
    let dir = tempfile::tempdir().unwrap();
    let clock = FakeClock::parse(NOW);
    old_hub(dir.path(), &clock, 3, include_str!("fixtures/schema-3-data.sql")).await;
    let r = Running::start(dir.path(), &clock).await;
    assert_eq!(r.hub.store.schema_version().unwrap(), hubd::store::SCHEMA_VERSION);
    let c = r.client();
    // Its token, issued for services, now sends to the feeds of the same names.
    let body = FeedContent {
        kind: "meter".into(),
        data: json!({"items": [{"id": "weekly", "label": "Weekly allowance used", "percent": 31.0}]}),
        collected_at: None,
        note: None,
    };
    let ok = c.put_feed("dhc_test_helper", "claude-max", &body).await.unwrap();
    assert_eq!(ok.updated, ["claude-max"]);
    let none = c
        .put_feed("dhc_test_helper", "chatgpt-plus", &body)
        .await
        .unwrap();
    assert!(none.updated.is_empty(), "the ChatGPT page is a demo page");
    let e = c.put_feed("dhc_test_helper", "words", &body).await.unwrap_err();
    assert_eq!(e.code(), Some(ErrorCode::Forbidden));
    let snap = c.device_snapshot(Some("dhd_test_desk")).await.unwrap();
    assert_eq!(items(&snap.pages[0])[0].percent, Some(31.0));
    assert_eq!(snap.pages[2].data_quality, DataQuality::Demo);
    // An invite made before the upgrade pairs, for the feeds of the services it named.
    let paired = c.helper_pair("invite-before-upgrade").await.unwrap();
    assert_eq!(paired.feeds, ["claude-max"]);
    let helpers = c.helpers("dha_test_admin").await.unwrap();
    let pc = helpers.iter().find(|h| h.id == "c-pc").unwrap();
    assert_eq!(pc.feeds, ["chatgpt-plus", "claude-max"]);
    r.stop().await;
}

#[tokio::test]
async fn pages_of_any_kind_through_the_admin_api() {
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
        template: None,
        settings: None,
    };
    let fed = add_meter(&c, &admin, "garden", Source::Helper).await;
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

    // Content is checked against the kind's rules and the size cap, and helper pages don't take it.
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
        .set_page_content(&admin, &fed.id, &set(json!({"items": []})))
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

    let (display, token) = enrolled(&r, &admin, "Desk").await;
    let order = ["note-2", "garden", "note", "flights-overhead"].map(String::from);
    c.reorder_pages(&admin, order.to_vec()).await.unwrap();
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
        template: None,
        settings: None,
    };
    let refused = |e: ClientError, words: &str| {
        assert_eq!(code(&e), Some(ErrorCode::BadRequest), "{e}");
        assert!(e.to_string().contains(words), "{e}");
    };

    // Helper pages need a feed, whose name is a slug; any kind will do.
    let e = c
        .add_page(&admin, &new("M", "meter", Source::Helper, None))
        .await
        .unwrap_err();
    refused(e, "needs a feed");
    for bad in ["Garden", "garden-", "a--b", "kāpiti"] {
        let e = c
            .add_page(&admin, &new("M", "meter", Source::Helper, Some(bad)))
            .await
            .unwrap_err();
        refused(e, "A feed name is lower-case");
    }
    for (kind, feed) in [
        ("text", "claude-max"),
        ("nearby-flights/1", "opencode-go-plus"),
        ("meter", "chatgpt-plus"),
    ] {
        c.add_page(&admin, &new("Fed", kind, Source::Helper, Some(feed)))
            .await
            .unwrap();
    }
    let words = c
        .add_page(&admin, &new("Words", "text", Source::Manual, None))
        .await
        .unwrap();
    let to_helper = PageChange {
        source: Some(Source::Helper),
        ..PageChange::default()
    };
    let e = c.change_page(&admin, &words.id, &to_helper).await.unwrap_err();
    refused(e, "needs a feed");
    assert_eq!(c.page(&admin, &words.id).await.unwrap().source, Source::Manual);
    let e = c.change_page(&admin, "nope", &to_helper).await.unwrap_err();
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

    // The hub stores meter content as sent: rounding and labels are the sender's business.
    let gauges = c
        .add_page(&admin, &new("Gauges", "meter", Source::Manual, None))
        .await
        .unwrap();
    let stored = c
        .set_page_content(
            &admin,
            &gauges.id,
            &content(json!({"items": [{"id": "daily", "label": "Mine", "percent": 42.25}]})),
        )
        .await
        .unwrap();
    let items = &stored.content.unwrap().data.unwrap()["items"];
    assert_eq!(
        (items[0]["percent"].clone(), items[0]["label"].clone()),
        (json!(42.25), json!("Mine"))
    );

    // No items at all is "no figures yet"; other meter rules stay strict.
    let stored = c
        .set_page_content(&admin, &gauges.id, &content(json!({"items": []})))
        .await
        .unwrap();
    assert_eq!(stored.content.unwrap().data, Some(json!({"items": []})));
    let preview = c.preview_snapshot(&admin).await.unwrap();
    let page = preview.pages.iter().find(|p| p.id == gauges.id).unwrap();
    assert_eq!(page.data_quality, DataQuality::Unavailable);
    assert_eq!(page.note.as_deref(), Some(hub_core::build::NO_FIGURES_NOTE));
    let e = c
        .set_page_content(&admin, &gauges.id, &content(json!({"items": [], "extra": 1})))
        .await
        .unwrap_err();
    refused(e, "unknown field");
    let dupes = json!({"items": [
        {"id": "a", "label": "A", "percent": 1},
        {"id": "a", "label": "B", "percent": 2}
    ]});
    let e = c
        .set_page_content(&admin, &gauges.id, &content(dupes))
        .await
        .unwrap_err();
    assert_eq!(code(&e), Some(ErrorCode::BadRequest));

    // Ticks are a set, so repeats don't count towards the limit.
    let display = enrolled(&r, &admin, "Desk").await.0;
    let many = vec![words.id.clone(); 60];
    assert_eq!(
        c.set_display_pages(&admin, &display, many).await.unwrap(),
        std::slice::from_ref(&words.id)
    );
    r.stop().await;
}

#[tokio::test]
async fn template_settings_are_kept_with_pages_and_only_their_helper_reads_them() {
    let dir = tempfile::tempdir().unwrap();
    let clock = FakeClock::parse(NOW);
    let r = Running::start(dir.path(), &clock).await;
    let admin = r.admin().await;
    let c = r.client();
    let obj = |v: Value| v.as_object().unwrap().clone();

    // The manager's side: pages made from built-in templates record the template and settings.
    let pi = hub_templates::built_in_template("pi-status/1").unwrap();
    let new = pi
        .make_pages(&obj(json!({"title": "Shed Pi"})))
        .unwrap()
        .remove(0);
    let page = c.add_page(&admin, &new).await.unwrap();
    assert_eq!(
        (page.id.as_str(), page.template.as_deref()),
        ("pi-status", Some("pi-status/1"))
    );
    assert_eq!(page.settings, Some(obj(json!({"title": "Shed Pi"}))));
    let usage = hub_templates::built_in_template("usage-limits/1").unwrap();
    let new = usage
        .make_pages(&obj(json!({"service": "claude-max", "source": "manual"})))
        .unwrap()
        .remove(0);
    let typed = c.add_page(&admin, &new).await.unwrap();
    // A page made by hand has neither, as before, and lists without them.
    let plain = c
        .add_page(&admin, &meter("pi-status", Source::Helper))
        .await
        .unwrap();
    assert_eq!((plain.template, plain.settings), (None, None));
    let listed = serde_json::to_value(c.page(&admin, &plain.id).await.unwrap()).unwrap();
    assert!(
        listed.get("template").is_none() && listed.get("settings").is_none(),
        "{listed}"
    );

    // Changing the settings replaces them; empty ones remove them; the template stays.
    let change = |v: Value| PageChange {
        settings: Some(obj(v)),
        ..PageChange::default()
    };
    let changed = c
        .change_page(
            &admin,
            "pi-status",
            &change(json!({"title": "Shed Pi", "every": 5})),
        )
        .await
        .unwrap();
    assert_eq!(
        changed.settings,
        Some(obj(json!({"title": "Shed Pi", "every": 5})))
    );
    let cleared = c
        .change_page(&admin, &typed.id, &change(json!({})))
        .await
        .unwrap();
    assert_eq!(
        (cleared.settings, cleared.template.as_deref()),
        (None, Some("usage-limits/1"))
    );

    // Settings that are too big or too deep, and badly written template ids, are refused.
    let refused = |e: ClientError, words: &str| {
        assert_eq!(code(&e), Some(ErrorCode::BadRequest), "{e}");
        assert!(e.to_string().contains(words), "{e}");
    };
    let e = c
        .change_page(&admin, "pi-status", &change(json!({"note": "x".repeat(2100)})))
        .await
        .unwrap_err();
    refused(e, "at most 2048");
    let e = c
        .change_page(&admin, "pi-status", &change(json!({"a": [[[1]]]})))
        .await
        .unwrap_err();
    refused(e, "nest too deeply");
    let e = c
        .add_page(
            &admin,
            &NewPage {
                template: Some("Pi status".into()),
                ..meter("x", Source::Manual)
            },
        )
        .await
        .unwrap_err();
    refused(e, "A template id");
    let e = c
        .add_page(
            &admin,
            &NewPage {
                settings: Some(obj(json!({"big": "x".repeat(2100)}))),
                ..meter("x", Source::Manual)
            },
        )
        .await
        .unwrap_err();
    refused(e, "at most 2048");
    assert_eq!(
        c.status(&admin).await.unwrap().pages,
        3,
        "nothing refused was added"
    );

    // A helper reads the settings of its feed's helper pages, in order, and nothing else.
    let invite = c
        .new_helper(
            &admin,
            &NewHelper {
                label: "Pi".into(),
                feeds: vec!["pi-status".into()],
            },
        )
        .await
        .unwrap();
    let token = c.helper_pair(&invite.secret).await.unwrap().helper_token;
    let read = c.feed_settings(&token, "pi-status").await.unwrap();
    assert_eq!(
        serde_json::to_value(&read).unwrap(),
        json!({"pages": [
            {"page_id": "pi-status", "template": "pi-status/1", "settings": {"title": "Shed Pi", "every": 5}},
            {"page_id": "pi-status-2", "template": null, "settings": {}}
        ]})
    );
    // The usage page's feed wasn't in the invite, and admins and displays can't use this door.
    let e = c.feed_settings(&token, "claude-max").await.unwrap_err();
    assert_eq!(code(&e), Some(ErrorCode::Forbidden));
    let e = c.feed_settings(&admin, "pi-status").await.unwrap_err();
    assert_eq!(code(&e), Some(ErrorCode::Forbidden));
    let e = c.feed_settings("dhc_not_a_token", "pi-status").await.unwrap_err();
    assert_eq!(code(&e), Some(ErrorCode::Unauthorised));
    // A helper page that becomes typed in is no longer the helper's to read.
    let to_manual = PageChange {
        source: Some(Source::Manual),
        ..PageChange::default()
    };
    c.change_page(&admin, "pi-status-2", &to_manual).await.unwrap();
    let read = c.feed_settings(&token, "pi-status").await.unwrap();
    assert_eq!(read.pages.len(), 1);
    r.stop().await;
}
