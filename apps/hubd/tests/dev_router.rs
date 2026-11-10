//! Development-mode routes, driven directly without a socket.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use hub_core::FakeClock;
use hub_protocol::{ErrorBody, ErrorCode, Health, Snapshot};
use hubd::dev::{router, DevState, Fault, Scenario};
use tower::ServiceExt;

fn app(scenario: Scenario) -> axum::Router {
    router(DevState {
        clock: Arc::new(FakeClock::parse("2026-11-10T20:00:00Z")),
        scenario,
        fault: Fault::None,
        refresh_after_seconds: 300,
    })
}

async fn call(app: axum::Router, req: Request<Body>) -> (StatusCode, Vec<u8>) {
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let body = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap()
        .to_vec();
    (status, body)
}

#[tokio::test]
async fn health_is_minimal() {
    let (status, body) = call(
        app(Scenario::Demo),
        Request::get("/v1/health").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let h: Health = serde_json::from_slice(&body).unwrap();
    assert_eq!(h, Health::ok());
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&body)
            .unwrap()
            .as_object()
            .unwrap()
            .len(),
        2
    );
}

#[tokio::test]
async fn snapshot_is_valid_json_with_a_content_type() {
    let resp = app(Scenario::Mixed)
        .oneshot(
            Request::get("/v1/device/snapshot")
                .header("x-display-protocol", "1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.headers()["content-type"], "application/json");
    let body = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
    let s: Snapshot = serde_json::from_slice(&body).unwrap();
    s.validate().unwrap();
    assert_eq!(s.pages.len(), 3);
}

#[tokio::test]
async fn unsupported_protocol_versions_get_a_structured_error() {
    for v in ["2", "0", "one"] {
        let (status, body) = call(
            app(Scenario::Demo),
            Request::get("/v1/device/snapshot")
                .header("x-display-protocol", v)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{v}");
        let e: ErrorBody = serde_json::from_slice(&body).unwrap();
        assert_eq!(e.error.code, ErrorCode::IncompatibleVersion);
    }
}

#[tokio::test]
async fn unknown_paths_get_a_structured_404() {
    let (status, body) = call(
        app(Scenario::Demo),
        Request::get("/v1/admin/devices").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        serde_json::from_slice::<ErrorBody>(&body).unwrap().error.code,
        ErrorCode::NotFound
    );
}
