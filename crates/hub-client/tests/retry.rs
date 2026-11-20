//! A read the hub never saw is sent once more on a new connection; nothing that changes anything is.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use hub_client::{ClientError, HubClient};
use hub_core::FakeClock;
use hubd::config::Config;
use tokio::net::TcpListener;

/// A real hub behind a relay that drops the first `drop_first` connections as soon as they arrive,
/// the way a VPN can lose one. Returns the client and the relay's count of connections.
async fn hub_behind_a_flaky_relay(drop_first: usize) -> (HubClient, Arc<AtomicUsize>) {
    let dir = tempfile::tempdir().unwrap();
    let hub = hubd::open_hub(
        Config {
            data_dir: dir.keep(),
            ..Config::default()
        },
        Arc::new(FakeClock::parse("2026-11-10T20:00:00Z")),
    )
    .unwrap();
    let real = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let real_port = real.local_addr().unwrap().port();
    tokio::spawn(hubd::serve_https(hub.clone(), real, std::future::pending::<()>()));

    let relay = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = relay.local_addr().unwrap().port();
    let seen = Arc::new(AtomicUsize::new(0));
    let count = seen.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut client, _)) = relay.accept().await else {
                return;
            };
            if count.fetch_add(1, Ordering::SeqCst) < drop_first {
                drop(client);
                continue;
            }
            tokio::spawn(async move {
                if let Ok(mut upstream) = tokio::net::TcpStream::connect(("127.0.0.1", real_port)).await {
                    let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
                }
            });
        }
    });
    let c = HubClient::pinned("127.0.0.1", port, vec![hub.identity.spki_sha256]).unwrap();
    (c, seen)
}

#[tokio::test]
async fn a_read_is_sent_again_on_a_new_connection() {
    let (c, seen) = hub_behind_a_flaky_relay(1).await;
    let health = c.health().await.expect("the second connection answers");
    assert_eq!(health.status, "ok");
    assert_eq!(seen.load(Ordering::SeqCst), 2, "one dropped, one that worked");
}

#[tokio::test]
async fn a_read_is_not_sent_a_third_time() {
    let (c, seen) = hub_behind_a_flaky_relay(2).await;
    assert!(matches!(c.health().await, Err(ClientError::Unreachable(_))));
    assert_eq!(seen.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn a_request_that_changes_something_is_sent_once() {
    let (c, seen) = hub_behind_a_flaky_relay(1).await;
    let r = c.pair("not-a-real-secret", "PC").await;
    assert!(matches!(r, Err(ClientError::Unreachable(_))), "{r:?}");
    assert_eq!(seen.load(Ordering::SeqCst), 1, "no second try for a POST");
}
