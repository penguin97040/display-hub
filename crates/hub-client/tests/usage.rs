//! The manager's usage pages, through a real hub: they are meter pages there, and their figures are
//! checked here before anything is sent.

use std::sync::Arc;

use hub_client::{ClientError, HubClient};
use hub_core::api::NewPage;
use hub_core::{FakeClock, Source};
use hub_protocol::pairing::PairingString;
use hub_protocol::DataQuality;
use hubd::config::Config;
use usage::{ManualFigures, ManualMetric, NewSubscription, Service, SubscriptionChange, Window};

fn figures(windows: &[(Window, f64)]) -> ManualFigures {
    ManualFigures {
        metrics: windows
            .iter()
            .map(|&(window, pct)| ManualMetric {
                window,
                used_percent: Some(pct),
                resets_at: None,
            })
            .collect(),
        note: None,
    }
}

#[tokio::test]
async fn usage_pages_are_meters_whose_figures_are_checked_before_sending() {
    let dir = tempfile::tempdir().unwrap();
    let clock = FakeClock::parse("2026-11-10T20:00:00Z");
    let hub = hubd::open_hub(
        Config {
            data_dir: dir.path().to_path_buf(),
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
    let pairing = PairingString {
        host: "127.0.0.1".into(),
        port,
        spki_sha256: hub.identity.spki_sha256,
        secret: hub.new_admin_pairing("PC").unwrap(),
    };
    let c = HubClient::pinned("127.0.0.1", port, vec![hub.identity.spki_sha256]).unwrap();
    let admin = c.pair(&pairing.secret_text(), "PC").await.unwrap().admin_token;
    let add = |service, source| NewSubscription {
        service,
        source,
        title: None,
        id: None,
    };

    // A demo usage page is given its service's sample figures.
    let demo = c
        .add_subscription(&admin, &add(Service::ClaudeMax, Source::Demo))
        .await
        .unwrap();
    let data = demo.data.unwrap();
    assert_eq!(data.quality, DataQuality::Demo);
    assert_eq!(data.metrics.len(), 2);
    let notes = NewPage {
        title: "Notes".into(),
        kind: "text".into(),
        source: Source::Manual,
        feed: None,
        id: None,
        stale_after_seconds: None,
    };
    c.add_page(&admin, &notes).await.unwrap();
    let typed = c
        .add_subscription(&admin, &add(Service::OpencodeGoPlus, Source::Manual))
        .await
        .unwrap();
    assert_eq!(typed.data, None);

    // Only usage pages are listed, in order.
    let ids = |v: Vec<usage::SubscriptionView>| v.into_iter().map(|s| s.id).collect::<Vec<_>>();
    assert_eq!(
        ids(c.subscriptions(&admin).await.unwrap()),
        ["claude-max", "opencode-go-plus"]
    );

    // Figures the service can't have are refused before anything is sent, in the owner's terms.
    for (bad, words) in [
        (
            figures(&[(Window::Monthly, 1.0)]),
            "Claude Max has no monthly window.",
        ),
        (
            figures(&[(Window::FiveHour, 1.0), (Window::FiveHour, 2.0)]),
            "The five-hour window is listed twice.",
        ),
        (
            figures(&[(Window::Weekly, 101.0)]),
            "Percentages must be 0 to 100.",
        ),
    ] {
        let e = c.set_manual(&admin, "claude-max", &bad).await.unwrap_err();
        assert!(matches!(&e, ClientError::Invalid(m) if m == words), "{e:?}");
    }
    assert_eq!(
        c.page(&admin, "claude-max")
            .await
            .unwrap()
            .content
            .unwrap()
            .quality,
        DataQuality::Demo,
        "nothing was stored"
    );
    // Shares are rounded to one decimal place, and labels are the windows' own.
    let saved = c
        .set_manual(&admin, "opencode-go-plus", &figures(&[(Window::Monthly, 42.25)]))
        .await
        .unwrap();
    let m = &saved.data.unwrap().metrics[0];
    assert_eq!((m.window, m.used_percent), (Window::Monthly, Some(42.3)));
    let stored = c.page(&admin, "opencode-go-plus").await.unwrap().content.unwrap();
    assert_eq!(
        stored.data.unwrap()["items"][0]["label"],
        serde_json::json!("Monthly allowance used")
    );

    // A page that becomes demo gets the sample figures.
    let change = SubscriptionChange {
        title: None,
        source: Some(Source::Demo),
    };
    let now_demo = c
        .change_subscription(&admin, "opencode-go-plus", &change)
        .await
        .unwrap();
    assert_eq!(now_demo.data.unwrap().metrics.len(), 3);

    // Reordering usage pages leaves other pages where they are.
    let order = ["notes", "claude-max", "opencode-go-plus"].map(String::from);
    c.reorder_pages(&admin, order.to_vec()).await.unwrap();
    c.reorder(&admin, vec!["opencode-go-plus".into(), "claude-max".into()])
        .await
        .unwrap();
    let all: Vec<String> = c.pages(&admin).await.unwrap().into_iter().map(|p| p.id).collect();
    assert_eq!(all, ["notes", "opencode-go-plus", "claude-max"]);
    let _ = tx.send(());
    server.await.unwrap().unwrap();
}
