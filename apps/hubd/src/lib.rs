//! The Display Hub service. The binary in `main.rs` is a thin command-line layer over this.

pub mod api;
pub mod config;
pub mod dev;
pub mod http;
pub mod identity;
pub mod netpolicy;
pub mod ratelimit;
pub mod secret;
pub mod store;
pub mod tls;

use std::sync::Arc;
use std::time::Duration;

use hub_core::Clock;

use crate::api::Hub;
use crate::config::Config;
use crate::identity::{Identity, IdentityError};
use crate::store::{Store, StoreError};

/// How often buffered last-seen times are written to the database.
pub const SEEN_FLUSH_INTERVAL: Duration = Duration::from_secs(15 * 60);

#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    #[error(transparent)]
    Identity(#[from] IdentityError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("couldn't create the data folder {path}: {source}")]
    DataDir {
        path: std::path::PathBuf,
        source: std::io::Error,
    },
}

/// Opens the database and TLS identity in `config.data_dir`, creating both only for a new hub.
pub fn open_hub(config: Config, clock: Arc<dyn Clock>) -> Result<Arc<Hub>, OpenError> {
    std::fs::create_dir_all(&config.data_dir).map_err(|source| OpenError::DataDir {
        path: config.data_dir.clone(),
        source,
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&config.data_dir, std::fs::Permissions::from_mode(0o700));
    }
    let db_path = config.db_path();
    let new_hub = !Store::exists(&db_path);
    // The identity first: a database must never exist without the identity displays trust.
    let identity = Identity::load_or_create(&config.identity_dir(), new_hub)?;
    if new_hub {
        // Created owner-only before SQLite opens it; its WAL files take the same permissions.
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        opts.open(&db_path).map_err(|source| OpenError::DataDir {
            path: db_path.clone(),
            source,
        })?;
    }
    let store = Store::open(&db_path)?;
    Ok(Hub::new(store, Arc::new(identity), clock, config))
}

/// Serves the hub over HTTPS until `shutdown` resolves, then writes buffered state.
pub async fn serve_https(
    hub: Arc<Hub>,
    listener: tokio::net::TcpListener,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    let tls = Arc::new(tls::server_config(&hub.identity)?);
    let flusher = {
        let hub = hub.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(SEEN_FLUSH_INTERVAL);
            tick.tick().await;
            loop {
                tick.tick().await;
                let h = hub.clone();
                match tokio::task::spawn_blocking(move || h.flush_seen()).await {
                    Ok(Err(e)) => tracing::warn!(error = %e, "couldn't save last-seen times"),
                    Err(e) => tracing::warn!(error = %e, "couldn't save last-seen times"),
                    Ok(Ok(())) => {}
                }
            }
        })
    };
    let result = tls::serve(
        listener,
        api::router(hub.clone()),
        tls,
        hub.config.allow_ipv6,
        shutdown,
    )
    .await;
    flusher.abort();
    if let Err(e) = hub.flush_seen() {
        tracing::warn!(error = %e, "couldn't save last-seen times at shutdown");
    }
    Ok(result?)
}

/// Serves `router` on `listener` until `shutdown` resolves, recording each peer's address.
pub async fn serve_plain(
    listener: tokio::net::TcpListener,
    router: axum::Router,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    axum::serve(
        listener,
        router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown)
    .await
}

/// Resolves on Ctrl+C, or SIGTERM where there is one (systemd stops services with it).
pub async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = term => {},
    }
}
