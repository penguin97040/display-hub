//! Development mode: plain HTTP on this computer only, with sample figures and no authentication.
//!
//! It exists for trying the display simulator and protocol out before any hardware or pairing.
//! It refuses every address except loopback, and must never be deployed.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::error_handling::HandleErrorLayer;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use hub_core::{build_snapshot, Clock, Source, StoredContent, StoredPage};
use hub_protocol::limits::{MAX_DEVICE_REQUEST_BYTES, MAX_SNAPSHOT_BYTES};
use hub_protocol::DataQuality;
use hub_protocol::Health;
use serde_json::json;
use tower::ServiceBuilder;

use crate::http::{check_protocol, handle_layer_error, ApiError, MAX_CONCURRENT_REQUESTS, REQUEST_TIMEOUT};

/// Which sample pages to serve.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Scenario {
    /// Claude Max, ChatGPT Plus and OpenCode Go Plus, all with fresh demo figures.
    Demo,
    /// Claude Max fresh, ChatGPT Plus unavailable and OpenCode Go Plus stale.
    Mixed,
    /// No pages at all.
    Empty,
}

/// A fault to reproduce, so a display's error handling can be tried.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Fault {
    None,
    /// A body cut off part-way through.
    Malformed,
    /// A snapshot from a future protocol version.
    WrongVersion,
    /// A body larger than any display accepts.
    Oversize,
    /// A 503 with a structured error.
    ServerError,
    /// An answer slower than a display waits for.
    Slow,
}

#[derive(Debug, thiserror::Error)]
#[error(
    "development mode only listens on this computer (127.0.0.1 or ::1), not {0}. \
     It has no encryption or authentication, so it must never be reachable from a network."
)]
pub struct NotLoopback(pub SocketAddr);

/// Refuses any address other than loopback.
pub fn check_bind(addr: SocketAddr) -> Result<(), NotLoopback> {
    if addr.ip().is_loopback() {
        Ok(())
    } else {
        Err(NotLoopback(addr))
    }
}

#[derive(Clone)]
pub struct DevState {
    pub clock: Arc<dyn Clock>,
    pub scenario: Scenario,
    pub fault: Fault,
    pub refresh_after_seconds: u32,
}

pub fn router(state: DevState) -> Router {
    Router::new()
        .route("/v1/health", get(health))
        .route("/v1/device/snapshot", get(snapshot))
        .fallback(|| async { ApiError::not_found() })
        .layer(DefaultBodyLimit::max(MAX_DEVICE_REQUEST_BYTES))
        .layer(
            ServiceBuilder::new()
                .layer(HandleErrorLayer::new(handle_layer_error))
                .load_shed()
                .concurrency_limit(MAX_CONCURRENT_REQUESTS)
                .timeout(REQUEST_TIMEOUT),
        )
        .with_state(state)
}

async fn health() -> Json<Health> {
    Json(Health::ok())
}

/// A sample meter page. Its items reset `hours` from `now`, so the countdowns always show.
fn sample(clock: &dyn Clock, id: &str, title: &str, source: Source, shares: &[(f64, i64)]) -> StoredPage {
    let now = clock.now();
    let labels = [
        ("five-hour", "Five-hour allowance used"),
        ("weekly", "Weekly allowance used"),
        ("monthly", "Monthly allowance used"),
    ];
    let items: Vec<_> = shares
        .iter()
        .zip(labels)
        .map(|(&(percent, hours), (id, label))| {
            json!({"id": id, "label": label, "percent": percent, "resets_at": now.plus_seconds(hours * 3600)})
        })
        .collect();
    StoredPage {
        id: id.into(),
        title: title.into(),
        kind: "meter".into(),
        source,
        feed: Some(id.into()),
        stale_after_seconds: source.stale_after_seconds(),
        content: Some(StoredContent {
            quality: DataQuality::Demo,
            updated_at: Some(now),
            data: Some(json!({ "items": items })),
            note: Some("Sample figures, not real usage".into()),
        }),
    }
}

/// The pages a scenario shows, with any stored figures.
pub fn scenario_pages(scenario: Scenario, clock: &dyn Clock) -> Vec<StoredPage> {
    let claude = || {
        sample(
            clock,
            "claude-max",
            "Claude Max",
            Source::Demo,
            &[(42.0, 2), (24.0, 120)],
        )
    };
    match scenario {
        Scenario::Demo => vec![
            claude(),
            sample(
                clock,
                "chatgpt-plus",
                "ChatGPT Plus",
                Source::Demo,
                &[(63.0, 2), (38.0, 120)],
            ),
            sample(
                clock,
                "opencode-go-plus",
                "OpenCode Go Plus",
                Source::Demo,
                &[(17.0, 2), (31.0, 120), (9.0, 480)],
            ),
        ],
        Scenario::Mixed => {
            // Demo figures sent two hours ago, on a source that goes stale after 30 minutes.
            let mut stale = sample(
                clock,
                "opencode-go-plus",
                "OpenCode Go Plus",
                Source::Helper,
                &[(17.0, 2), (31.0, 120), (9.0, 480)],
            );
            if let Some(c) = stale.content.as_mut() {
                c.updated_at = Some(clock.now().plus_seconds(-7200));
            }
            let mut none = sample(clock, "chatgpt-plus", "ChatGPT Plus", Source::None, &[]);
            none.content = None;
            vec![claude(), none, stale]
        }
        Scenario::Empty => Vec::new(),
    }
}

async fn snapshot(State(st): State<DevState>, headers: HeaderMap) -> Result<Response, ApiError> {
    check_protocol(&headers)?;
    let json = |body: Vec<u8>| ([(header::CONTENT_TYPE, "application/json")], body).into_response();
    let snap = build_snapshot(
        st.clock.now(),
        st.refresh_after_seconds,
        &scenario_pages(st.scenario, &*st.clock),
    );
    match st.fault {
        Fault::None => {}
        Fault::Malformed => {
            let body = serde_json::to_vec(&snap).expect("serialises");
            return Ok(json(body[..body.len() / 2].to_vec()));
        }
        Fault::WrongVersion => {
            let mut v = serde_json::to_value(&snap).expect("serialises");
            v["protocol_version"] = 2.into();
            return Ok(json(serde_json::to_vec(&v).expect("serialises")));
        }
        Fault::Oversize => {
            let mut v = serde_json::to_value(&snap).expect("serialises");
            v["padding"] = "x".repeat(MAX_SNAPSHOT_BYTES).into();
            return Ok(json(serde_json::to_vec(&v).expect("serialises")));
        }
        Fault::ServerError => return Err(ApiError::unavailable("Simulated outage (development mode).")),
        Fault::Slow => tokio::time::sleep(REQUEST_TIMEOUT + std::time::Duration::from_secs(5)).await,
    }
    let body = snap.to_wire().map_err(|v| {
        tracing::error!(violations = ?v, "refusing to send an invalid snapshot");
        ApiError::unavailable("The hub could not build a valid snapshot.")
    })?;
    Ok((StatusCode::OK, json(body)).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_loopback_addresses_are_allowed() {
        assert!(check_bind("127.0.0.1:8080".parse().unwrap()).is_ok());
        assert!(check_bind("[::1]:8080".parse().unwrap()).is_ok());
        for bad in ["0.0.0.0:8080", "[::]:8080", "192.168.1.20:8080", "10.0.0.1:1"] {
            assert!(check_bind(bad.parse().unwrap()).is_err(), "{bad}");
        }
    }
}
