//! The simulator against a real development hub, in-process on loopback, with a fake clock.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use display_simulator::render::{count_pages, screen};
use display_simulator::{Device, Link, NoJitter, Problem};
use hub_client::HubClient;
use hub_core::{Clock, FakeClock};
use hub_protocol::{DataQuality, Snapshot, Timestamp};
use hubd::dev::{self, DevState, Fault, Scenario};
use tokio::sync::oneshot;

const NOW: &str = "2026-11-10T20:00:00Z";

struct Hub {
    addr: SocketAddr,
    stop: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<()>,
}

impl Hub {
    async fn start(clock: &FakeClock, scenario: Scenario, fault: Fault) -> Hub {
        Self::start_on("127.0.0.1:0".parse().unwrap(), clock, scenario, fault).await
    }

    async fn start_on(addr: SocketAddr, clock: &FakeClock, scenario: Scenario, fault: Fault) -> Hub {
        let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let state = DevState {
            clock: Arc::new(clock.clone()),
            scenario,
            fault,
            refresh_after_seconds: 300,
        };
        let (tx, rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            hubd::serve_plain(listener, dev::router(state), async {
                let _ = rx.await;
            })
            .await
            .unwrap();
        });
        Hub {
            addr,
            stop: Some(tx),
            task,
        }
    }

    fn client(&self) -> HubClient {
        HubClient::loopback_dev(&format!("http://{}/", self.addr)).unwrap()
    }

    async fn stop(mut self) {
        let _ = self.stop.take().unwrap().send(());
        self.task.await.unwrap();
    }
}

fn device() -> Device {
    Device::new(None, Box::new(NoJitter))
}

#[tokio::test]
async fn shows_the_three_demo_pages_in_order() {
    let clock = FakeClock::parse(NOW);
    let hub = Hub::start(&clock, Scenario::Demo, Fault::None).await;
    let mut d = device();
    let wait = d.accept(clock.now(), hub.client().device_snapshot(None).await);
    assert_eq!(wait, Duration::from_secs(300));
    let s = screen(&d, clock.now());
    assert_eq!(count_pages(&s), 3, "{s}");
    let order: Vec<_> = s.lines().filter(|l| l.starts_with("-- Page")).collect();
    assert!(
        order[0].contains("Claude Max")
            && order[1].contains("ChatGPT Plus")
            && order[2].contains("OpenCode Go Plus")
    );
    assert_eq!(s.matches("DEMO --").count(), 3, "every page says demo:\n{s}");
    assert!(s.contains("Sample figures, not real usage"));
    assert!(s.contains("[ONLINE]"));
    hub.stop().await;
}

#[tokio::test]
async fn shows_unavailable_and_stale_pages_clearly() {
    let clock = FakeClock::parse(NOW);
    let hub = Hub::start(&clock, Scenario::Mixed, Fault::None).await;
    let mut d = device();
    d.accept(clock.now(), hub.client().device_snapshot(None).await);
    let s = screen(&d, clock.now());
    assert_eq!(count_pages(&s), 3);
    assert!(s.contains("ChatGPT Plus") && s.contains("UNAVAILABLE --"), "{s}");
    assert!(s.contains("No supported source connected yet"));
    assert!(
        s.contains("STALE DEMO --") && s.contains("STALE: updated 2 h ago"),
        "{s}"
    );
    hub.stop().await;
}

#[tokio::test]
async fn pages_go_stale_while_the_hub_is_down() {
    let clock = FakeClock::parse(NOW);
    let hub = Hub::start(&clock, Scenario::Demo, Fault::None).await;
    let client = hub.client();
    let mut d = device();
    d.accept(clock.now(), client.device_snapshot(None).await);
    hub.stop().await;
    clock.advance(2 * 3600);
    d.accept(clock.now(), client.device_snapshot(None).await);
    let s = screen(&d, clock.now());
    assert!(s.contains("[OFFLINE] Can't reach the hub"), "{s}");
    assert!(
        s.contains("Showing saved data from 10/11/2026 20:00 UTC (2 h old)"),
        "{s}"
    );
    assert_eq!(s.matches("STALE DEMO").count(), 3, "{s}");
    // The demo five-hour window reset at 22:00 by the hub's clock; the old figure is still shown
    // with its age, and the countdown says the reset is due.
    assert!(s.contains("reset due"), "{s}");
}

#[tokio::test]
async fn bad_responses_keep_the_last_good_snapshot() {
    let clock = FakeClock::parse(NOW);
    let good = Hub::start(&clock, Scenario::Demo, Fault::None).await;
    let mut d = device();
    d.accept(clock.now(), good.client().device_snapshot(None).await);
    let kept = d.held.clone().unwrap();
    good.stop().await;

    for (fault, expect) in [
        (Fault::Malformed, "can't read"),
        (Fault::Oversize, "can't read"),
        (Fault::WrongVersion, "versions don't match"),
        (Fault::ServerError, "busy"),
    ] {
        let hub = Hub::start(&clock, Scenario::Demo, fault).await;
        d.accept(clock.now(), hub.client().device_snapshot(None).await);
        let s = screen(&d, clock.now());
        assert!(matches!(d.link, Link::Trouble { .. }), "{fault:?}");
        assert!(s.to_lowercase().contains(expect), "{fault:?}:\n{s}");
        assert_eq!(
            d.held.as_ref(),
            Some(&kept),
            "{fault:?} replaced the good snapshot"
        );
        assert_eq!(count_pages(&s), 3, "{fault:?}: saved pages still shown");
        hub.stop().await;
    }
}

#[tokio::test]
async fn reconnects_after_the_hub_restarts() {
    let clock = FakeClock::parse(NOW);
    let hub = Hub::start(&clock, Scenario::Demo, Fault::None).await;
    let addr = hub.addr;
    let client = hub.client();
    let mut d = device();
    d.accept(clock.now(), client.device_snapshot(None).await);
    hub.stop().await;

    clock.advance(60);
    d.accept(clock.now(), client.device_snapshot(None).await);
    assert!(matches!(
        d.link,
        Link::Trouble {
            problem: Problem::Unreachable,
            ..
        }
    ));

    let hub = Hub::start_on(addr, &clock, Scenario::Mixed, Fault::None).await;
    clock.advance(60);
    d.accept(clock.now(), client.device_snapshot(None).await);
    assert!(matches!(d.link, Link::Online { .. }));
    assert!(
        screen(&d, clock.now()).contains("UNAVAILABLE --"),
        "picked up the new pages"
    );
    hub.stop().await;
}

#[tokio::test]
async fn retries_back_off_and_reset_after_success() {
    let clock = FakeClock::parse(NOW);
    let hub = Hub::start(&clock, Scenario::Demo, Fault::ServerError).await;
    let client = hub.client();
    let mut d = device();
    let mut got = Vec::new();
    for _ in 0..8 {
        got.push(
            d.accept(clock.now(), client.device_snapshot(None).await)
                .as_secs(),
        );
    }
    assert_eq!(got, [15, 30, 60, 120, 240, 480, 900, 900]);
    hub.stop().await;
    let ok = Hub::start(&clock, Scenario::Demo, Fault::None).await;
    assert_eq!(
        d.accept(clock.now(), ok.client().device_snapshot(None).await)
            .as_secs(),
        300
    );
    ok.stop().await;
}

#[tokio::test]
async fn the_cache_survives_a_display_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cache.json");
    let clock = FakeClock::parse(NOW);
    let hub = Hub::start(&clock, Scenario::Demo, Fault::None).await;
    let mut d = Device::new(Some(path.clone()), Box::new(NoJitter));
    d.accept(clock.now(), hub.client().device_snapshot(None).await);
    hub.stop().await;

    let restarted = Device::new(Some(path.clone()), Box::new(NoJitter));
    assert_eq!(restarted.held, d.held);
    std::fs::write(&path, b"{ damaged").unwrap();
    assert!(
        Device::new(Some(path), Box::new(NoJitter)).held.is_none(),
        "a damaged cache is ignored"
    );
}

#[tokio::test]
async fn an_empty_hub_says_so() {
    let clock = FakeClock::parse(NOW);
    let hub = Hub::start(&clock, Scenario::Empty, Fault::None).await;
    let mut d = device();
    d.accept(clock.now(), hub.client().device_snapshot(None).await);
    let s = screen(&d, clock.now());
    assert_eq!(count_pages(&s), 0);
    assert!(s.contains("No pages. Add subscriptions in the desktop manager."));
    hub.stop().await;
}

#[tokio::test]
async fn unknown_figures_are_never_shown_as_zero_and_new_qualities_read_as_unavailable() {
    let bytes = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../protocol/v1/examples/valid/stale-provider-reported.json"
    ))
    .unwrap();
    let mut snap: Snapshot = serde_json::from_slice(&bytes).unwrap();
    let mut d = device();
    let now = Timestamp::parse(NOW).unwrap();
    d.accept(now, Ok(snap.clone()));
    let s = screen(&d, now);
    assert!(s.contains("-- unknown"), "{s}");
    assert!(!s.contains(" 0%"), "{s}");

    snap.pages[0].data_quality = DataQuality::Unrecognised;
    d.accept(now, Ok(snap));
    let s = screen(&d, now);
    assert!(s.contains("UNAVAILABLE --") && s.contains("newer hub"), "{s}");
    assert!(!s.contains("70%"), "{s}");
}

#[tokio::test]
async fn a_missing_hub_before_any_sync_shows_no_figures() {
    let clock = FakeClock::parse(NOW);
    let client = HubClient::loopback_dev("http://127.0.0.1:9/").unwrap();
    let mut d = device();
    d.accept(clock.now(), client.device_snapshot(None).await);
    let s = screen(&d, clock.now());
    assert!(s.contains("No data yet"), "{s}");
    assert_eq!(count_pages(&s), 0);
}
