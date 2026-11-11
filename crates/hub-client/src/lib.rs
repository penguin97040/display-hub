//! A client for a Display Hub, used by the desktop manager, the display simulator and the PC helper.
//!
//! Every response body is read with a size cap, every request has a timeout, and failures are
//! sorted into kinds a person can act on ([`ClientError`]).

pub mod pin;
pub mod provisioning;

use std::time::Duration;

use hub_core::api::*;
use hub_protocol::device::{DeviceInfo, EnrolRequest, EnrolResponse};
use hub_protocol::limits::MAX_SNAPSHOT_BYTES;
use hub_protocol::{ErrorBody, ReceiveError, Snapshot, PROTOCOL_HEADER, PROTOCOL_VERSION};
use reqwest::Method;
use serde::de::DeserializeOwned;
use serde::Serialize;
use url::Url;

/// The largest response body the client reads from any endpoint.
pub const MAX_RESPONSE_BYTES: usize = 256 * 1024;
const TIMEOUT: Duration = Duration::from_secs(10);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Why a call to the hub failed, in terms a person can act on.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("this address is not allowed: {0}")]
    AddressRefused(String),
    #[error("can't reach the hub: {0}")]
    Unreachable(String),
    #[error(
        "the server at this address isn't the hub you paired with: its key is different. Someone may \
         be impersonating the hub, so nothing was sent. If you reinstalled the hub, pair again."
    )]
    IdentityMismatch,
    #[error("the hub took too long to answer")]
    Timeout,
    #[error("the hub answered with an error ({status}): {}", .body.as_ref().map(|b| b.error.message.as_str()).unwrap_or("no details"))]
    Status { status: u16, body: Option<ErrorBody> },
    #[error("the hub's answer was larger than {0} bytes")]
    TooLarge(usize),
    #[error(transparent)]
    Snapshot(#[from] ReceiveError),
    #[error("the hub's answer was not understood: {0}")]
    Malformed(String),
}

impl ClientError {
    /// Whether the request may have reached the hub at all.
    pub fn is_connection(&self) -> bool {
        matches!(
            self,
            ClientError::Unreachable(_) | ClientError::Timeout | ClientError::IdentityMismatch
        )
    }

    /// The structured error code, when the hub sent one.
    pub fn code(&self) -> Option<hub_protocol::ErrorCode> {
        match self {
            ClientError::Status { body: Some(b), .. } => Some(b.error.code),
            _ => None,
        }
    }
}

/// A connection to one hub.
#[derive(Clone, Debug)]
pub struct HubClient {
    base: Url,
    http: reqwest::Client,
}

impl HubClient {
    /// Plain HTTP to a hub in development mode on this computer. Any other host is refused, so
    /// this can never send data across a network unencrypted.
    pub fn loopback_dev(base: &str) -> Result<Self, ClientError> {
        let base = Url::parse(base).map_err(|e| ClientError::AddressRefused(e.to_string()))?;
        let loopback = match base.host() {
            Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
            Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
            Some(url::Host::Domain(d)) => d.eq_ignore_ascii_case("localhost"),
            None => false,
        };
        if base.scheme() != "http" || !loopback {
            return Err(ClientError::AddressRefused(
                "development mode only talks plain HTTP to 127.0.0.1, ::1 or localhost".into(),
            ));
        }
        let http = reqwest::Client::builder()
            // Plain HTTP only. TLS is configured to trust no key at all, so it can never succeed.
            .use_preconfigured_tls(pin::client_config(Vec::new()))
            .timeout(TIMEOUT)
            .connect_timeout(CONNECT_TIMEOUT)
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| ClientError::Unreachable(e.to_string()))?;
        Ok(Self { base, http })
    }

    /// HTTPS to a hub identified by the SHA-256 of its public key (one, or two during a key change).
    /// Any other server at `host` is refused before anything is sent.
    pub fn pinned(host: &str, port: u16, pins: Vec<[u8; 32]>) -> Result<Self, ClientError> {
        let authority = if host.contains(':') && !host.starts_with('[') {
            format!("[{host}]")
        } else {
            host.to_string()
        };
        let base = Url::parse(&format!("https://{authority}:{port}/"))
            .map_err(|e| ClientError::AddressRefused(e.to_string()))?;
        if base.host_str().is_none_or(|h| h.is_empty()) {
            return Err(ClientError::AddressRefused(format!(
                "{host:?} isn't a host name or address"
            )));
        }
        let http = reqwest::Client::builder()
            .use_preconfigured_tls(pin::client_config(pins))
            .https_only(true)
            .timeout(TIMEOUT)
            .connect_timeout(CONNECT_TIMEOUT)
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| ClientError::Unreachable(e.to_string()))?;
        Ok(Self { base, http })
    }

    pub fn base(&self) -> &Url {
        &self.base
    }

    fn url(&self, path: &str) -> Url {
        self.base.join(path).expect("relative API paths always join")
    }

    /// `GET /v1/device/snapshot`, read as a display would.
    pub async fn device_snapshot(&self, token: Option<&str>) -> Result<Snapshot, ClientError> {
        let mut req = self
            .http
            .get(self.url("v1/device/snapshot"))
            .header(PROTOCOL_HEADER, PROTOCOL_VERSION);
        if let Some(t) = token {
            req = req.bearer_auth(t);
        }
        let body = self.send(req, MAX_SNAPSHOT_BYTES).await?;
        Ok(hub_protocol::receive(&body)?)
    }

    /// `GET /v1/health`.
    pub async fn health(&self) -> Result<hub_protocol::Health, ClientError> {
        let body = self.send(self.http.get(self.url("v1/health")), 4096).await?;
        serde_json::from_slice(&body).map_err(|e| ClientError::Malformed(e.to_string()))
    }

    fn request(&self, method: Method, path: &str, token: Option<&str>) -> reqwest::RequestBuilder {
        let mut req = self
            .http
            .request(method, self.url(path))
            .header(PROTOCOL_HEADER, PROTOCOL_VERSION);
        if let Some(t) = token {
            req = req.bearer_auth(t);
        }
        req
    }

    async fn json<T: DeserializeOwned>(&self, req: reqwest::RequestBuilder) -> Result<T, ClientError> {
        let body = self.send(req, MAX_RESPONSE_BYTES).await?;
        serde_json::from_slice(&body).map_err(|e| ClientError::Malformed(e.to_string()))
    }

    async fn call<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        token: &str,
        body: Option<&impl Serialize>,
    ) -> Result<T, ClientError> {
        let mut req = self.request(method, path, Some(token));
        if let Some(b) = body {
            req = req.json(b);
        }
        self.json(req).await
    }

    async fn call_empty(&self, method: Method, path: &str, token: &str) -> Result<(), ClientError> {
        self.send(self.request(method, path, Some(token)), 8192)
            .await
            .map(|_| ())
    }

    // ----- display -----

    /// `POST /v1/device/enrol`: the one-time secret from a provisioning bundle, for a device token.
    pub async fn enrol(&self, secret: &str, device: DeviceInfo) -> Result<EnrolResponse, ClientError> {
        let req = EnrolRequest {
            enrolment_secret: secret.to_string(),
            device,
        };
        self.json(self.request(Method::POST, "v1/device/enrol", None).json(&req))
            .await
    }

    /// `POST /v1/device/heartbeat`.
    pub async fn heartbeat(&self, token: &str, device: &DeviceInfo) -> Result<(), ClientError> {
        self.send(
            self.request(Method::POST, "v1/device/heartbeat", Some(token))
                .json(device),
            8192,
        )
        .await
        .map(|_| ())
    }

    // ----- admin -----

    /// `POST /v1/admin/pair`: the secret from a pairing string, for an admin token.
    pub async fn pair(&self, secret: &str, label: &str) -> Result<PairResponse, ClientError> {
        let req = PairRequest {
            secret: secret.to_string(),
            label: label.to_string(),
        };
        self.json(self.request(Method::POST, "v1/admin/pair", None).json(&req))
            .await
    }

    pub async fn status(&self, token: &str) -> Result<HubStatus, ClientError> {
        self.call(Method::GET, "v1/admin/status", token, None::<&()>)
            .await
    }

    pub async fn subscriptions(&self, token: &str) -> Result<Vec<SubscriptionView>, ClientError> {
        self.call(Method::GET, "v1/admin/subscriptions", token, None::<&()>)
            .await
    }

    pub async fn add_subscription(
        &self,
        token: &str,
        new: &NewSubscription,
    ) -> Result<SubscriptionView, ClientError> {
        self.call(Method::POST, "v1/admin/subscriptions", token, Some(new))
            .await
    }

    pub async fn change_subscription(
        &self,
        token: &str,
        id: &str,
        change: &SubscriptionChange,
    ) -> Result<SubscriptionView, ClientError> {
        self.call(
            Method::PATCH,
            &format!("v1/admin/subscriptions/{}", seg(id)),
            token,
            Some(change),
        )
        .await
    }

    pub async fn remove_subscription(&self, token: &str, id: &str) -> Result<(), ClientError> {
        self.call_empty(
            Method::DELETE,
            &format!("v1/admin/subscriptions/{}", seg(id)),
            token,
        )
        .await
    }

    pub async fn reorder(&self, token: &str, ids: Vec<String>) -> Result<Vec<SubscriptionView>, ClientError> {
        self.call(
            Method::PUT,
            "v1/admin/subscriptions/order",
            token,
            Some(&Order { ids }),
        )
        .await
    }

    pub async fn set_manual(
        &self,
        token: &str,
        id: &str,
        figures: &ManualFigures,
    ) -> Result<SubscriptionView, ClientError> {
        self.call(
            Method::PUT,
            &format!("v1/admin/subscriptions/{}/manual", seg(id)),
            token,
            Some(figures),
        )
        .await
    }

    pub async fn displays(&self, token: &str) -> Result<Vec<DisplayView>, ClientError> {
        self.call(Method::GET, "v1/admin/devices", token, None::<&()>)
            .await
    }

    pub async fn revoke_display(&self, token: &str, id: &str) -> Result<(), ClientError> {
        self.call_empty(Method::DELETE, &format!("v1/admin/devices/{}", seg(id)), token)
            .await
    }

    pub async fn new_enrolment(&self, token: &str, name: &str) -> Result<EnrolmentGrant, ClientError> {
        self.call(
            Method::POST,
            "v1/admin/enrolments",
            token,
            Some(&NewEnrolment {
                name: name.to_string(),
            }),
        )
        .await
    }

    pub async fn preview_snapshot(&self, token: &str) -> Result<Snapshot, ClientError> {
        self.call(Method::GET, "v1/admin/preview-snapshot", token, None::<&()>)
            .await
    }

    /// Sends a request and reads at most `cap` bytes of a successful body.
    async fn send(&self, req: reqwest::RequestBuilder, cap: usize) -> Result<Vec<u8>, ClientError> {
        let resp = req.send().await.map_err(classify)?;
        let status = resp.status();
        if !status.is_success() {
            let body = read_capped(resp, 8192).await.ok();
            let parsed = body.and_then(|b| serde_json::from_slice::<ErrorBody>(&b).ok());
            return Err(ClientError::Status {
                status: status.as_u16(),
                body: parsed,
            });
        }
        read_capped(resp, cap).await
    }
}

async fn read_capped(mut resp: reqwest::Response, cap: usize) -> Result<Vec<u8>, ClientError> {
    if resp.content_length().is_some_and(|n| n as usize > cap) {
        return Err(ClientError::TooLarge(cap));
    }
    let mut out = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(classify)? {
        if out.len() + chunk.len() > cap {
            return Err(ClientError::TooLarge(cap));
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

fn classify(e: reqwest::Error) -> ClientError {
    if e.is_timeout() {
        ClientError::Timeout
    } else {
        // reqwest's messages name the URL, which holds no credentials here (they go in headers).
        let mut msg = e.to_string();
        let mut src = std::error::Error::source(&e);
        while let Some(s) = src {
            msg = format!("{msg}: {s}");
            src = s.source();
        }
        if msg.contains(pin::MISMATCH) {
            ClientError::IdentityMismatch
        } else {
            ClientError::Unreachable(msg)
        }
    }
}

/// A path segment, percent-encoded so an id can't change the path.
fn seg(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes())
        .collect::<String>()
        .replace('+', "%20")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dev_mode_refuses_anything_but_loopback_http() {
        for ok in ["http://127.0.0.1:8080/", "http://localhost:1/", "http://[::1]:9/"] {
            assert!(HubClient::loopback_dev(ok).is_ok(), "{ok}");
        }
        for bad in [
            "http://192.168.1.5:8080/",
            "https://127.0.0.1/",
            "http://example.com/",
            "file:///x",
        ] {
            assert!(
                matches!(HubClient::loopback_dev(bad), Err(ClientError::AddressRefused(_))),
                "{bad}"
            );
        }
    }
}
