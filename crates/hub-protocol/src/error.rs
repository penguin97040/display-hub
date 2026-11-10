//! Structured error bodies: `{"error": {"code": "...", "message": "..."}}`.
//!
//! Codes are grouped so a display can act without understanding the message: re-provision,
//! retry later, or report an incompatible version.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// No credential, or one the hub does not recognise.
    Unauthorised,
    /// A valid credential that may not do this.
    Forbidden,
    /// This display's credential was revoked. Re-provision it.
    TokenRevoked,
    /// The credential belongs to a display the hub no longer knows. Re-provision it.
    UnknownDevice,
    /// The enrolment or pairing code was already used.
    EnrolmentUsed,
    /// The enrolment or pairing code has expired, or never existed.
    EnrolmentExpired,
    /// The request's protocol version is not supported.
    IncompatibleVersion,
    /// The request was malformed or broke a bound.
    BadRequest,
    PayloadTooLarge,
    NotFound,
    Conflict,
    /// Too many requests; wait and retry.
    RateLimited,
    /// The hub can't serve this now; retry with backoff.
    Unavailable,
    /// A code from a newer revision. Treat it as transient.
    #[serde(other)]
    Unrecognised,
}

/// What a display should do about an error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorAction {
    /// Stop polling and show that the display must be set up again.
    Reprovision,
    /// Keep the last good data and retry with backoff.
    Retry,
    /// Show that the hub and display software don't match.
    Incompatible,
    /// A fault in the request itself; retry slowly and report it.
    Fault,
}

impl ErrorCode {
    pub fn action(self) -> ErrorAction {
        use ErrorCode::*;
        match self {
            TokenRevoked | UnknownDevice | EnrolmentUsed | EnrolmentExpired => ErrorAction::Reprovision,
            IncompatibleVersion => ErrorAction::Incompatible,
            RateLimited | Unavailable | Unrecognised => ErrorAction::Retry,
            Unauthorised | Forbidden | BadRequest | PayloadTooLarge | NotFound | Conflict => {
                ErrorAction::Fault
            }
        }
    }

    pub fn as_str(self) -> &'static str {
        use ErrorCode::*;
        match self {
            Unauthorised => "unauthorised",
            Forbidden => "forbidden",
            TokenRevoked => "token_revoked",
            UnknownDevice => "unknown_device",
            EnrolmentUsed => "enrolment_used",
            EnrolmentExpired => "enrolment_expired",
            IncompatibleVersion => "incompatible_version",
            BadRequest => "bad_request",
            PayloadTooLarge => "payload_too_large",
            NotFound => "not_found",
            Conflict => "conflict",
            RateLimited => "rate_limited",
            Unavailable => "unavailable",
            Unrecognised => "unrecognised",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ErrorBody {
    pub error: ErrorDetail,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ErrorDetail {
    pub code: ErrorCode,
    /// Plain en-NZ for people. Never contains credentials or account details.
    pub message: String,
}

impl ErrorBody {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            error: ErrorDetail {
                code,
                message: message.into(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_codes_are_transient() {
        let b: ErrorBody =
            serde_json::from_str(r#"{"error":{"code":"from_the_future","message":"x"}}"#).unwrap();
        assert_eq!(b.error.code, ErrorCode::Unrecognised);
        assert_eq!(b.error.code.action(), ErrorAction::Retry);
    }

    #[test]
    fn wire_names_match_serde() {
        use ErrorCode::*;
        for c in [
            Unauthorised,
            Forbidden,
            TokenRevoked,
            UnknownDevice,
            EnrolmentUsed,
            EnrolmentExpired,
            IncompatibleVersion,
            BadRequest,
            PayloadTooLarge,
            NotFound,
            Conflict,
            RateLimited,
            Unavailable,
        ] {
            assert_eq!(serde_json::to_value(c).unwrap(), c.as_str());
        }
    }
}
