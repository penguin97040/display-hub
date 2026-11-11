//! The hub's HTTPS API: device routes for displays, admin routes for the desktop manager.
//!
//! Every route except health, pairing and enrolment needs a bearer token, and each token works
//! only for its own role. A display's identity comes from its token, never from the request.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use axum::error_handling::HandleErrorLayer;
use axum::extract::rejection::JsonRejection;
use axum::extract::{ConnectInfo, DefaultBodyLimit, FromRequest, Path, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post, put};
use axum::{Json, Router};
use hub_core::api::*;
use hub_core::model::{clean_note, clean_percent, clean_title, valid_page_id};
use hub_core::{build_snapshot, Clock, MetricValue, PageData, Source, Subscription};
use hub_protocol::device::{DeviceInfo, EnrolRequest, EnrolResponse, Heartbeat};
use hub_protocol::{DataQuality, ErrorCode, Health, Timestamp};
use serde::de::DeserializeOwned;
use tower::ServiceBuilder;

use crate::config::Config;
use crate::http::{check_protocol, handle_layer_error, ApiError, MAX_CONCURRENT_REQUESTS, REQUEST_TIMEOUT};
use crate::identity::Identity;
use crate::ratelimit::Limiter;
use crate::secret::{hash, new_code, new_id, new_token, Role};
use crate::store::{CodeUse, Store, StoreError};

/// One-time codes (pairing and enrolment) last this long.
pub const CODE_LIFETIME_SECONDS: i64 = 600;
/// The largest admin request body.
pub const MAX_ADMIN_REQUEST_BYTES: usize = 16 * 1024;
/// Heartbeats more often than this are accepted but not recorded.
const HEARTBEAT_MIN_INTERVAL_SECONDS: u64 = 60;

/// Everything the routes share.
pub struct Hub {
    pub store: Store,
    pub identity: Arc<Identity>,
    pub clock: Arc<dyn Clock>,
    pub config: Config,
    pub started_at: Timestamp,
    /// Failed sign-ins: 20 a minute per address.
    pub auth_failures: Limiter,
    /// Pairing and enrolment attempts: 10 per 10 minutes per address.
    pub code_attempts: Limiter,
    /// Last-seen times and heartbeat details, flushed to the database now and then.
    seen: Mutex<HashMap<String, Seen>>,
}

#[derive(Clone)]
struct Seen {
    at: Timestamp,
    info: Option<DeviceInfo>,
    heartbeat: Option<Instant>,
}

impl Hub {
    pub fn new(store: Store, identity: Arc<Identity>, clock: Arc<dyn Clock>, config: Config) -> Arc<Self> {
        let started_at = clock.now();
        Arc::new(Self {
            store,
            identity,
            clock,
            config,
            started_at,
            auth_failures: Limiter::new(20, std::time::Duration::from_secs(60)),
            code_attempts: Limiter::new(10, std::time::Duration::from_secs(600)),
            seen: Mutex::new(HashMap::new()),
        })
    }

    fn seen(&self) -> std::sync::MutexGuard<'_, HashMap<String, Seen>> {
        self.seen.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Writes buffered last-seen times to the database.
    pub fn flush_seen(&self) -> Result<(), StoreError> {
        let batch: Vec<_> = self
            .seen()
            .iter()
            .map(|(id, s)| (id.clone(), s.at, s.info.clone()))
            .collect();
        if batch.is_empty() {
            return Ok(());
        }
        self.store.flush_seen(&batch)?;
        self.seen().retain(|_, s| {
            s.heartbeat
                .is_some_and(|h| h.elapsed().as_secs() < HEARTBEAT_MIN_INTERVAL_SECONDS)
        });
        for s in self.seen().values_mut() {
            s.info = None;
        }
        Ok(())
    }

    /// Issues a one-time admin pairing secret, for `hubd pair-admin`.
    pub fn new_admin_pairing(&self, label: &str) -> Result<[u8; 16], StoreError> {
        let (raw, code) = new_code();
        let now = self.clock.now();
        self.store.add_code(
            &hash(code.expose()),
            "admin-pair",
            label,
            now,
            now.plus_seconds(CODE_LIFETIME_SECONDS),
        )?;
        self.store.audit(now, "admin_pairing_issued", label)?;
        Ok(raw)
    }

    /// Issues a one-time enrolment secret for a new display, for the manager or `hubd display-bundle`.
    pub fn new_enrolment_grant(&self, name: &str, by: &str) -> Result<EnrolmentGrant, StoreError> {
        let name = clean_title(name).unwrap_or_else(|| "Display".into());
        let (_, code) = new_code();
        let now = self.clock.now();
        let expires = now.plus_seconds(CODE_LIFETIME_SECONDS);
        self.store
            .add_code(&hash(code.expose()), "device-enrol", &name, now, expires)?;
        self.store
            .audit(now, "enrolment_issued", &format!("{name} by {by}"))?;
        Ok(EnrolmentGrant {
            secret: code.expose().to_string(),
            expires_at: expires,
            identity: self.identity.info(),
        })
    }

    fn snapshot(&self) -> Result<hub_protocol::Snapshot, StoreError> {
        let pages = self.store.subscriptions()?;
        Ok(build_snapshot(
            self.clock.now(),
            self.config.refresh_after_seconds,
            &pages,
        ))
    }
}

type AppState = Arc<Hub>;

impl From<StoreError> for ApiError {
    fn from(e: StoreError) -> Self {
        match e {
            StoreError::Invalid(msg) => ApiError::new(StatusCode::CONFLICT, ErrorCode::Conflict, msg),
            other => {
                tracing::error!(error = %other, "database error");
                ApiError::unavailable("The hub's database is unavailable. Check the hub's log.")
            }
        }
    }
}

/// Runs database work off the async threads: a commit waits for the SD card.
async fn db<T: Send + 'static>(
    hub: &AppState,
    f: impl FnOnce(&Store) -> Result<T, StoreError> + Send + 'static,
) -> Result<T, ApiError> {
    let store = hub.store.clone();
    tokio::task::spawn_blocking(move || f(&store))
        .await
        .map_err(|_| ApiError::unavailable("The hub hit an internal fault."))?
        .map_err(ApiError::from)
}

/// JSON bodies with structured errors instead of axum's plain-text ones.
pub struct ApiJson<T>(pub T);

impl<S: Send + Sync, T: DeserializeOwned> FromRequest<S> for ApiJson<T> {
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        match Json::<T>::from_request(req, state).await {
            Ok(Json(v)) => Ok(ApiJson(v)),
            Err(JsonRejection::BytesRejection(_)) => Err(ApiError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                ErrorCode::PayloadTooLarge,
                "The request body is too large.",
            )),
            Err(e) => Err(ApiError::bad_request(format!(
                "The request body isn't valid: {}",
                e.body_text()
            ))),
        }
    }
}

fn too_many(limiter: &Limiter, ip: IpAddr) -> ApiError {
    let wait = limiter.retry_after(ip, Instant::now()).as_secs().max(1);
    ApiError::new(
        StatusCode::TOO_MANY_REQUESTS,
        ErrorCode::RateLimited,
        format!("Too many attempts from this address. Try again in {wait} s."),
    )
}

/// Checks the bearer token for `role` and returns its subject (an admin or display id).
async fn authenticate(
    hub: &AppState,
    headers: &HeaderMap,
    ip: IpAddr,
    role: Role,
) -> Result<String, ApiError> {
    if hub.auth_failures.blocked(ip, Instant::now()) {
        return Err(too_many(&hub.auth_failures, ip));
    }
    let fail = |code: ErrorCode, msg: &str| {
        hub.auth_failures.hit(ip, Instant::now());
        ApiError::new(StatusCode::UNAUTHORIZED, code, msg)
    };
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|t| !t.is_empty() && t.len() <= 128);
    let Some(token) = token else {
        return Err(fail(
            ErrorCode::Unauthorised,
            "This needs a credential from the hub.",
        ));
    };
    let h = hash(token);
    let Some(cred) = db(hub, move |s| s.credential(&h)).await? else {
        return Err(fail(
            ErrorCode::Unauthorised,
            "The hub doesn't recognise this credential.",
        ));
    };
    if cred.revoked {
        let msg = match cred.role {
            Role::Device => "This display's access was revoked. Set it up again from the desktop manager.",
            Role::Admin => "This admin access was revoked. Pair again with `hubd pair-admin` on the hub.",
            Role::Collector => "This helper's access was revoked. Set it up again from the desktop manager.",
        };
        return Err(fail(ErrorCode::TokenRevoked, msg));
    }
    if cred.role != role {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            ErrorCode::Forbidden,
            "This credential may not do that.",
        ));
    }
    Ok(cred.subject)
}

pub fn router(hub: AppState) -> Router {
    let device = Router::new()
        .route("/v1/device/enrol", post(device_enrol))
        .route("/v1/device/snapshot", get(device_snapshot))
        .route("/v1/device/heartbeat", post(device_heartbeat))
        .layer(DefaultBodyLimit::max(
            hub_protocol::limits::MAX_DEVICE_REQUEST_BYTES,
        ));
    let admin = Router::new()
        .route("/v1/admin/pair", post(admin_pair))
        .route("/v1/admin/status", get(admin_status))
        .route(
            "/v1/admin/subscriptions",
            get(list_subscriptions).post(add_subscription),
        )
        .route("/v1/admin/subscriptions/order", put(reorder))
        .route(
            "/v1/admin/subscriptions/{id}",
            patch(change_subscription).delete(remove_subscription),
        )
        .route("/v1/admin/subscriptions/{id}/manual", put(set_manual))
        .route("/v1/admin/devices", get(list_devices))
        .route("/v1/admin/devices/{id}", delete(revoke_device))
        .route("/v1/admin/enrolments", post(new_enrolment))
        .route("/v1/admin/preview-snapshot", get(preview_snapshot))
        .layer(DefaultBodyLimit::max(MAX_ADMIN_REQUEST_BYTES));
    Router::new()
        .route("/v1/health", get(|| async { Json(Health::ok()) }))
        .merge(device)
        .merge(admin)
        .fallback(|| async { ApiError::not_found() })
        .layer(middleware::from_fn(log_request))
        .layer(
            ServiceBuilder::new()
                .layer(HandleErrorLayer::new(handle_layer_error))
                .load_shed()
                .concurrency_limit(MAX_CONCURRENT_REQUESTS)
                .timeout(REQUEST_TIMEOUT),
        )
        .with_state(hub)
}

/// One line per request: method, path, status, time and peer. Never headers, query strings or
/// bodies, which can carry credentials.
async fn log_request(ConnectInfo(peer): ConnectInfo<SocketAddr>, req: Request, next: Next) -> Response {
    let (method, path) = (req.method().clone(), req.uri().path().to_string());
    let started = Instant::now();
    let mut resp = next.run(req).await;
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    tracing::info!(
        peer = %peer.ip(),
        %method,
        path = %path.chars().take(120).collect::<String>(),
        status = resp.status().as_u16(),
        ms = started.elapsed().as_millis() as u64,
        "request"
    );
    resp
}

// ----- device routes -----

async fn device_enrol(
    State(hub): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    ApiJson(req): ApiJson<EnrolRequest>,
) -> Result<(StatusCode, Json<EnrolResponse>), ApiError> {
    check_protocol(&headers)?;
    let ip = peer.ip();
    if hub.code_attempts.blocked(ip, Instant::now()) {
        return Err(too_many(&hub.code_attempts, ip));
    }
    hub.code_attempts.hit(ip, Instant::now());
    let problems = req.device.problems();
    if !problems.is_empty() {
        return Err(ApiError::bad_request(problems.join("; ")));
    }
    let now = hub.clock.now();
    let h = hash(&req.enrolment_secret);
    let (outcome, name) = db(&hub, move |s| s.use_code(&h, "device-enrol", now)).await?;
    match outcome {
        CodeUse::Accepted => {
            let id = new_id("d");
            let token = new_token(Role::Device);
            let name = name.unwrap_or_else(|| "Display".into());
            let (id2, th, info) = (id.clone(), hash(token.expose()), req.device.clone());
            db(&hub, move |s| {
                s.add_device(&id2, &name, &info, now)?;
                s.add_credential(&th, Role::Device, &id2, &name, now)?;
                s.audit(now, "device_enrolled", &format!("{id2} from {ip}"))
            })
            .await?;
            tracing::info!(device = %id, "display enrolled");
            Ok((
                StatusCode::CREATED,
                Json(EnrolResponse {
                    device_id: id,
                    device_token: token.expose().to_string(),
                }),
            ))
        }
        CodeUse::AlreadyUsed => {
            tracing::warn!(%ip, "an enrolment code was used twice: someone else may have seen it");
            db(&hub, move |s| {
                s.audit(now, "ALARM_enrolment_code_reused", &format!("from {ip}"))
            })
            .await?;
            Err(ApiError::new(
                StatusCode::CONFLICT,
                ErrorCode::EnrolmentUsed,
                "This enrolment code was already used. Create a new one in the desktop manager.",
            ))
        }
        CodeUse::Unknown => Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            ErrorCode::EnrolmentExpired,
            "This enrolment code has expired or doesn't exist. Create a new one in the desktop manager.",
        )),
    }
}

fn note_seen(hub: &Hub, device: &str, info: Option<DeviceInfo>) -> bool {
    let now = hub.clock.now();
    let mut seen = hub.seen();
    let entry = seen.entry(device.to_string()).or_insert(Seen {
        at: now,
        info: None,
        heartbeat: None,
    });
    entry.at = now;
    if let Some(info) = info {
        if entry
            .heartbeat
            .is_some_and(|h| h.elapsed().as_secs() < HEARTBEAT_MIN_INTERVAL_SECONDS)
        {
            return false;
        }
        entry.heartbeat = Some(Instant::now());
        entry.info = Some(info);
    }
    true
}

async fn device_snapshot(
    State(hub): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    check_protocol(&headers)?;
    let device = authenticate(&hub, &headers, peer.ip(), Role::Device).await?;
    note_seen(&hub, &device, None);
    let h = hub.clone();
    let snap = db(&hub, move |_| h.snapshot()).await?;
    let body = snap.to_wire().map_err(|v| {
        tracing::error!(violations = ?v, "refusing to send an invalid snapshot");
        ApiError::unavailable("The hub could not build a valid snapshot.")
    })?;
    Ok(([(header::CONTENT_TYPE, "application/json")], body).into_response())
}

async fn device_heartbeat(
    State(hub): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    ApiJson(beat): ApiJson<Heartbeat>,
) -> Result<StatusCode, ApiError> {
    check_protocol(&headers)?;
    let device = authenticate(&hub, &headers, peer.ip(), Role::Device).await?;
    let problems = beat.problems();
    if !problems.is_empty() {
        return Err(ApiError::bad_request(problems.join("; ")));
    }
    note_seen(&hub, &device, Some(beat));
    Ok(StatusCode::NO_CONTENT)
}

// ----- admin routes -----

async fn admin_pair(
    State(hub): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    ApiJson(req): ApiJson<PairRequest>,
) -> Result<Json<PairResponse>, ApiError> {
    let ip = peer.ip();
    if hub.code_attempts.blocked(ip, Instant::now()) {
        return Err(too_many(&hub.code_attempts, ip));
    }
    hub.code_attempts.hit(ip, Instant::now());
    let label = clean_title(&req.label).unwrap_or_else(|| "Desktop manager".into());
    let now = hub.clock.now();
    let h = hash(req.secret.trim());
    let (outcome, _) = db(&hub, move |s| s.use_code(&h, "admin-pair", now)).await?;
    match outcome {
        CodeUse::Accepted => {
            let subject = new_id("a");
            let token = new_token(Role::Admin);
            let (sub2, th, l2) = (subject.clone(), hash(token.expose()), label.clone());
            db(&hub, move |s| {
                s.add_credential(&th, Role::Admin, &sub2, &l2, now)?;
                s.audit(now, "admin_paired", &format!("{sub2} ({l2}) from {ip}"))
            })
            .await?;
            tracing::info!(admin = %subject, %label, "admin paired");
            Ok(Json(PairResponse { admin_token: token.expose().to_string(), identity: hub.identity.info() }))
        }
        CodeUse::AlreadyUsed => {
            tracing::warn!(%ip, "a pairing code was used twice: someone else may have seen it. Check `hubd admin list`.");
            db(&hub, move |s| s.audit(now, "ALARM_pairing_code_reused", &format!("from {ip}"))).await?;
            Err(ApiError::new(
                StatusCode::CONFLICT,
                ErrorCode::EnrolmentUsed,
                "This pairing string was already used. If that wasn't you, run `hubd admin list` on the hub. \
                 Then run `hubd pair-admin` for a new one.",
            ))
        }
        CodeUse::Unknown => Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            ErrorCode::EnrolmentExpired,
            "This pairing string has expired or doesn't exist. Run `hubd pair-admin` on the hub for a new one.",
        )),
    }
}

/// Whether the clock is synchronised: the kernel's view (`adjtimex`), or, where the service's
/// sandbox refuses that call (`ProtectClock=`), systemd-timesyncd's marker file.
#[cfg(target_os = "linux")]
pub fn clock_synchronised() -> Option<bool> {
    // SAFETY: adjtimex with modes = 0 only reads the kernel's clock state into the struct.
    let mut tx: libc::timex = unsafe { std::mem::zeroed() };
    let state = unsafe { libc::adjtimex(&mut tx) };
    if state >= 0 {
        return Some(state != libc::TIME_ERROR && tx.status & libc::STA_UNSYNC == 0);
    }
    std::path::Path::new("/run/systemd/timesync/synchronized")
        .exists()
        .then_some(true)
}

#[cfg(not(target_os = "linux"))]
pub fn clock_synchronised() -> Option<bool> {
    None
}

async fn admin_status(
    State(hub): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Result<Json<HubStatus>, ApiError> {
    authenticate(&hub, &headers, peer.ip(), Role::Admin).await?;
    let (subs, displays, admins) = db(&hub, |s| {
        Ok((
            s.subscription_ids()?.len(),
            s.devices()?.len(),
            s.count_active(Role::Admin)?,
        ))
    })
    .await?;
    Ok(Json(HubStatus {
        version: env!("CARGO_PKG_VERSION").into(),
        started_at: hub.started_at,
        now: hub.clock.now(),
        clock_synchronised: clock_synchronised(),
        identity: hub.identity.info(),
        hosts: hub.config.hosts(),
        port: hub.config.listen.port(),
        subscriptions: subs,
        displays,
        admins,
    }))
}

fn view((s, data): (Subscription, Option<PageData>)) -> SubscriptionView {
    SubscriptionView {
        id: s.id,
        service: s.service,
        title: s.title,
        source: s.source,
        data,
    }
}

async fn list_subscriptions(
    State(hub): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Result<Json<Vec<SubscriptionView>>, ApiError> {
    authenticate(&hub, &headers, peer.ip(), Role::Admin).await?;
    Ok(Json(
        db(&hub, |s| s.subscriptions())
            .await?
            .into_iter()
            .map(view)
            .collect(),
    ))
}

async fn add_subscription(
    State(hub): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    ApiJson(req): ApiJson<NewSubscription>,
) -> Result<(StatusCode, Json<SubscriptionView>), ApiError> {
    authenticate(&hub, &headers, peer.ip(), Role::Admin).await?;
    let title = match &req.title {
        Some(t) => {
            clean_title(t).ok_or_else(|| ApiError::bad_request("The title needs some printable text."))?
        }
        None => req.service.default_title().into(),
    };
    let existing = db(&hub, |s| s.subscription_ids()).await?;
    let id = match &req.id {
        Some(id) if valid_page_id(id) => id.clone(),
        Some(_) => {
            return Err(ApiError::bad_request(
                "A page id uses a-z, 0-9 and '-', up to 48 characters.",
            ))
        }
        None => {
            let base = req.service.slug();
            (1..)
                .map(|n| {
                    if n == 1 {
                        base.to_string()
                    } else {
                        format!("{base}-{n}")
                    }
                })
                .find(|c| !existing.contains(c))
                .expect("an unused id exists")
        }
    };
    let sub = Subscription {
        id,
        service: req.service,
        title,
        source: req.source,
    };
    let (s2, now) = (sub.clone(), hub.clock.now());
    db(&hub, move |s| s.add_subscription(&s2, now)).await?;
    Ok((StatusCode::CREATED, Json(view((sub, None)))))
}

async fn change_subscription(
    State(hub): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(id): Path<String>,
    ApiJson(req): ApiJson<SubscriptionChange>,
) -> Result<Json<SubscriptionView>, ApiError> {
    authenticate(&hub, &headers, peer.ip(), Role::Admin).await?;
    let title = match &req.title {
        Some(t) => Some(
            clean_title(t).ok_or_else(|| ApiError::bad_request("The title needs some printable text."))?,
        ),
        None => None,
    };
    let id2 = id.clone();
    let found = db(&hub, move |s| {
        s.update_subscription(&id2, title.as_deref(), req.source)
    })
    .await?;
    if !found {
        return Err(ApiError::not_found());
    }
    db(&hub, move |s| s.subscriptions())
        .await?
        .into_iter()
        .find(|(s, _)| s.id == id)
        .map(|x| Json(view(x)))
        .ok_or_else(ApiError::not_found)
}

async fn remove_subscription(
    State(hub): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    authenticate(&hub, &headers, peer.ip(), Role::Admin).await?;
    if db(&hub, move |s| s.remove_subscription(&id)).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::not_found())
    }
}

async fn reorder(
    State(hub): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    ApiJson(req): ApiJson<Order>,
) -> Result<Json<Vec<SubscriptionView>>, ApiError> {
    authenticate(&hub, &headers, peer.ip(), Role::Admin).await?;
    let list = db(&hub, move |s| {
        s.reorder(&req.ids)?;
        s.subscriptions()
    })
    .await?;
    Ok(Json(list.into_iter().map(view).collect()))
}

async fn set_manual(
    State(hub): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(id): Path<String>,
    ApiJson(req): ApiJson<ManualFigures>,
) -> Result<Json<SubscriptionView>, ApiError> {
    authenticate(&hub, &headers, peer.ip(), Role::Admin).await?;
    let id2 = id.clone();
    let sub = db(&hub, move |s| s.subscription(&id2))
        .await?
        .ok_or_else(ApiError::not_found)?;
    if sub.source != Source::Manual {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            ErrorCode::Conflict,
            "This page doesn't take manual figures. Change its source to manual first.",
        ));
    }
    let mut metrics = Vec::new();
    for m in &req.metrics {
        if !sub.service.windows().contains(&m.window) {
            return Err(ApiError::bad_request(format!(
                "{} has no {} window.",
                sub.service.default_title(),
                m.window.id()
            )));
        }
        if metrics.iter().any(|x: &MetricValue| x.window == m.window) {
            return Err(ApiError::bad_request(format!(
                "The {} window is listed twice.",
                m.window.id()
            )));
        }
        let used_percent = match m.used_percent {
            Some(p) => {
                Some(clean_percent(p).ok_or_else(|| ApiError::bad_request("Percentages must be 0 to 100."))?)
            }
            None => None,
        };
        metrics.push(MetricValue {
            window: m.window,
            used_percent,
            resets_at: m.resets_at,
        });
    }
    let now = hub.clock.now();
    let data = PageData {
        quality: DataQuality::Manual,
        updated_at: Some(now),
        metrics,
        note: req.note.as_deref().and_then(clean_note),
    };
    let (id2, d2) = (id.clone(), data.clone());
    db(&hub, move |s| s.put_page_data(&id2, &d2, now)).await?;
    Ok(Json(view((sub, Some(data)))))
}

async fn list_devices(
    State(hub): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Result<Json<Vec<DisplayView>>, ApiError> {
    authenticate(&hub, &headers, peer.ip(), Role::Admin).await?;
    let rows = db(&hub, |s| s.devices()).await?;
    let seen = hub.seen();
    Ok(Json(
        rows.into_iter()
            .map(|r| {
                let live = seen.get(&r.id);
                DisplayView {
                    last_seen_at: live.map(|s| s.at).max(r.last_seen_at),
                    device: live.and_then(|s| s.info.clone()).or(r.info),
                    id: r.id,
                    name: r.name,
                    enrolled_at: r.enrolled_at,
                }
            })
            .collect(),
    ))
}

async fn revoke_device(
    State(hub): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let admin = authenticate(&hub, &headers, peer.ip(), Role::Admin).await?;
    let now = hub.clock.now();
    let id2 = id.clone();
    let found = db(&hub, move |s| {
        let found = s.revoke_device(&id2, now)?;
        if found {
            s.audit(now, "device_revoked", &format!("{id2} by {admin}"))?;
        }
        Ok(found)
    })
    .await?;
    hub.seen().remove(&id);
    if found {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::not_found())
    }
}

async fn new_enrolment(
    State(hub): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    ApiJson(req): ApiJson<NewEnrolment>,
) -> Result<(StatusCode, Json<EnrolmentGrant>), ApiError> {
    let admin = authenticate(&hub, &headers, peer.ip(), Role::Admin).await?;
    let h = hub.clone();
    let grant = db(&hub, move |_| h.new_enrolment_grant(&req.name, &admin)).await?;
    Ok((StatusCode::CREATED, Json(grant)))
}

async fn preview_snapshot(
    State(hub): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Result<Json<hub_protocol::Snapshot>, ApiError> {
    authenticate(&hub, &headers, peer.ip(), Role::Admin).await?;
    let h = hub.clone();
    Ok(Json(db(&hub, move |_| h.snapshot()).await?))
}
