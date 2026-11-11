//! HTTPS for displays and the manager.
//!
//! Settings chosen for ESP-IDF's mbedTLS as well as modern clients: TLS 1.2 and 1.3, ECDHE with
//! ECDSA and AES-GCM or ChaCha20, an ECDSA P-256 certificate, and records of at most 4 KiB, so a
//! display can use a small receive buffer. Connections from outside the local network are closed
//! before the handshake.

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ConnectInfo;
use axum::Router;
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::server::graceful::GracefulShutdown;
use rustls::ServerConfig;
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tokio_rustls::TlsAcceptor;
use tower::ServiceExt;

use crate::identity::Identity;
use crate::netpolicy;

/// Open connections at once. A home hub has a handful of clients.
pub const MAX_CONNECTIONS: usize = 128;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const HEADER_TIMEOUT: Duration = Duration::from_secs(10);
/// TLS record size limit, for displays with small buffers.
pub const MAX_FRAGMENT: usize = 4096;

pub fn server_config(identity: &Identity) -> Result<ServerConfig, rustls::Error> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])?
        .with_no_client_auth()
        .with_single_cert(vec![identity.cert_der.clone()], identity.private_key())?;
    config.max_fragment_size = Some(MAX_FRAGMENT);
    // Prefer AES-GCM (the provider's order) over the client's: ESP32 chips accelerate AES in
    // hardware, and mbedTLS lists ChaCha20 first.
    config.ignore_client_order = true;
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    // No session tickets or resumption state to protect: every connection does a full handshake.
    config.session_storage = Arc::new(rustls::server::NoServerSessionStorage {});
    config.send_tls13_tickets = 0;
    Ok(config)
}

/// Serves `router` over TLS on `listener` until `shutdown` resolves.
pub async fn serve(
    listener: TcpListener,
    router: Router,
    tls: Arc<ServerConfig>,
    allow_ipv6: bool,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    let acceptor = TlsAcceptor::from(tls);
    let graceful = GracefulShutdown::new();
    let slots = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    tokio::pin!(shutdown);
    loop {
        let (tcp, peer) = tokio::select! {
            r = listener.accept() => match r {
                Ok(c) => c,
                Err(e) => {
                    // Usually too many open files; wait rather than spin.
                    tracing::warn!(error = %e, "accept failed");
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    continue;
                }
            },
            _ = &mut shutdown => break,
        };
        if !netpolicy::allowed(peer.ip(), allow_ipv6) {
            tracing::debug!(peer = %peer.ip(), "refused a connection from outside the local network");
            continue;
        }
        let Ok(permit) = slots.clone().try_acquire_owned() else {
            tracing::warn!("too many connections; refusing one");
            continue;
        };
        let _ = tcp.set_nodelay(true);
        let acceptor = acceptor.clone();
        let router = router.clone();
        let watcher = graceful.watcher();
        tokio::spawn(async move {
            let _permit = permit;
            let tls = match tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(tcp)).await {
                Ok(Ok(s)) => s,
                Ok(Err(e)) => {
                    tracing::debug!(peer = %peer.ip(), error = %e, "TLS handshake failed");
                    return;
                }
                Err(_) => return,
            };
            let service =
                hyper::service::service_fn(move |mut req: hyper::Request<hyper::body::Incoming>| {
                    req.extensions_mut().insert(ConnectInfo::<SocketAddr>(peer));
                    router.clone().oneshot(req.map(axum::body::Body::new))
                });
            let conn = hyper::server::conn::http1::Builder::new()
                .timer(TokioTimer::new())
                .header_read_timeout(HEADER_TIMEOUT)
                .max_buf_size(64 * 1024)
                .serve_connection(TokioIo::new(tls), service);
            let conn = watcher.watch(conn);
            if let Err(e) = conn.await {
                tracing::debug!(peer = %peer.ip(), error = %e, "connection ended with an error");
            }
        });
    }
    tokio::select! {
        _ = graceful.shutdown() => {}
        _ = tokio::time::sleep(Duration::from_secs(5)) => tracing::info!("closing connections that didn't finish"),
    }
    Ok(())
}
