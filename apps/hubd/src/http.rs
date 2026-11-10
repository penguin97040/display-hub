//! Shared HTTP pieces: structured errors, protocol version checks and request limits.

use std::time::Duration;

use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use hub_protocol::{ErrorBody, ErrorCode, MIN_SUPPORTED_PROTOCOL_VERSION, PROTOCOL_HEADER, PROTOCOL_VERSION};

/// Time allowed for a whole request, including reading the body.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Requests handled at once. A home hub has a handful of clients.
pub const MAX_CONCURRENT_REQUESTS: usize = 64;

/// An error response with a structured body.
#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub body: ErrorBody,
}

impl ApiError {
    pub fn new(status: StatusCode, code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            status,
            body: ErrorBody::new(code, message),
        }
    }

    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::new(StatusCode::SERVICE_UNAVAILABLE, ErrorCode::Unavailable, message)
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, ErrorCode::BadRequest, message)
    }

    pub fn not_found() -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            ErrorCode::NotFound,
            "There is nothing at this address.",
        )
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(self.body)).into_response()
    }
}

/// Checks the protocol version a display says it speaks. A request without the header is taken
/// as version 1, the only version so far.
pub fn check_protocol(headers: &HeaderMap) -> Result<(), ApiError> {
    let Some(v) = headers.get(PROTOCOL_HEADER) else {
        return Ok(());
    };
    let supported = v
        .to_str()
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
        .is_some_and(|n| (MIN_SUPPORTED_PROTOCOL_VERSION..=PROTOCOL_VERSION).contains(&n));
    if supported {
        Ok(())
    } else {
        Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            ErrorCode::IncompatibleVersion,
            format!(
                "This hub supports display protocol versions {MIN_SUPPORTED_PROTOCOL_VERSION} to {PROTOCOL_VERSION}. \
                 Update the hub or the display so they match."
            ),
        ))
    }
}

/// Turns tower's timeout and overload errors into structured responses.
pub async fn handle_layer_error(err: tower::BoxError) -> ApiError {
    if err.is::<tower::timeout::error::Elapsed>() {
        ApiError::new(
            StatusCode::REQUEST_TIMEOUT,
            ErrorCode::Unavailable,
            "The request took too long.",
        )
    } else {
        ApiError::unavailable("The hub is busy. Try again shortly.")
    }
}
