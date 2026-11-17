//! The hub's HTTPS API: device routes for displays, admin routes for the desktop manager.
//!
//! Every route except health, pairing and enrolment needs a bearer token, and each token works
//! only for its own role. A display's identity comes from its token, never from the request.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use axum::error_handling::HandleErrorLayer;
use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{ConnectInfo, DefaultBodyLimit, FromRequest, Path, Query, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use hub_core::api::*;
use hub_core::model::{
    clean_note, clean_title, settings_problem, valid_feed, valid_page_id, valid_template_id, MAX_FEED_LEN,
};
use hub_core::{build_snapshot, Clock, Settings, Source, StoredContent, StoredPage};
use hub_protocol::device::{DeviceInfo, EnrolRequest, EnrolResponse, Heartbeat};
use hub_protocol::shapes::{classify_kind, KindClass};
use hub_protocol::{CantShow, DataQuality, ErrorCode, Health, Page, Timestamp};
use serde::de::DeserializeOwned;
use tower::ServiceBuilder;

use crate::config::Config;
use crate::http::{check_protocol, handle_layer_error, ApiError, MAX_CONCURRENT_REQUESTS, REQUEST_TIMEOUT};
use crate::identity::Identity;
use crate::ratelimit::Limiter;
use crate::secret::{hash, new_code, new_id, new_token, Role};
use crate::store::{CodeUse, ContentWrite, PageUpdate, Store, StoreError, MAX_STORED_PAGES};

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

    /// Issues a one-time secret for a helper that may send to `feeds`, which the caller has checked
    /// with [`check_feeds`].
    pub fn new_helper_invite(
        &self,
        label: &str,
        feeds: &[String],
        by: &str,
    ) -> Result<HelperInvite, StoreError> {
        let mut feeds = feeds.to_vec();
        feeds.sort();
        feeds.dedup();
        let label = clean_title(label).unwrap_or_else(|| "Helper".into());
        let detail = serde_json::to_string(&InviteDetail {
            label: label.clone(),
            feeds,
        })
        .expect("serialises");
        let (_, code) = new_code();
        let now = self.clock.now();
        let expires = now.plus_seconds(CODE_LIFETIME_SECONDS);
        self.store
            .add_code(&hash(code.expose()), "helper-pair", &detail, now, expires)?;
        self.store
            .audit(now, "helper_invited", &format!("{label} by {by}"))?;
        Ok(HelperInvite {
            secret: code.expose().to_string(),
            expires_at: expires,
            identity: self.identity.info(),
        })
    }

    /// The snapshot `display` receives: its first ticked pages. With no display, the first pages
    /// of all. `None` if there is no such active display.
    fn snapshot(&self, display: Option<&str>) -> Result<Option<hub_protocol::Snapshot>, StoreError> {
        let pages = match display {
            Some(id) => match self.store.display_pages(id)? {
                Some(pages) => pages,
                None => return Ok(None),
            },
            None => self.store.pages()?,
        };
        Ok(Some(build_snapshot(
            self.clock.now(),
            self.config.refresh_after_seconds,
            &pages,
        )))
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

/// Like [`db`], for work that can fail with an [`ApiError`] of its own.
async fn db_api<T: Send + 'static>(
    hub: &AppState,
    f: impl FnOnce(&Store) -> Result<T, ApiError> + Send + 'static,
) -> Result<T, ApiError> {
    let store = hub.store.clone();
    tokio::task::spawn_blocking(move || f(&store))
        .await
        .map_err(|_| ApiError::unavailable("The hub hit an internal fault."))?
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
            Role::Helper => "This helper's access was revoked. Set it up again from the desktop manager.",
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
        .route("/v1/admin/pages", get(list_pages).post(add_page))
        .route("/v1/admin/pages/order", put(reorder))
        .route(
            "/v1/admin/pages/{id}",
            get(get_page).patch(change_page).delete(remove_page),
        )
        .route("/v1/admin/pages/{id}/content", put(set_content))
        .route("/v1/admin/devices", get(list_devices))
        .route("/v1/admin/devices/{id}", delete(revoke_device))
        .route(
            "/v1/admin/devices/{id}/pages",
            get(display_pages).put(set_display_pages),
        )
        .route("/v1/admin/enrolments", post(new_enrolment))
        .route("/v1/admin/preview-snapshot", get(preview_snapshot))
        .route("/v1/admin/helpers", get(list_helpers).post(new_helper))
        .route("/v1/admin/helpers/{id}", delete(revoke_helper))
        .layer(DefaultBodyLimit::max(MAX_ADMIN_REQUEST_BYTES));
    // A feed body is a whole page, so it may be as large as an admin one.
    let helper = Router::new()
        .route("/v1/helper/pair", post(helper_pair))
        .route("/v1/helper/feeds/{feed}", put(put_feed))
        .route("/v1/helper/feeds/{feed}/settings", get(feed_settings))
        .layer(DefaultBodyLimit::max(MAX_ADMIN_REQUEST_BYTES));
    Router::new()
        .route("/v1/health", get(|| async { Json(Health::ok()) }))
        .merge(device)
        .merge(admin)
        .merge(helper)
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
    // A display revoked since it authenticated has nothing to show.
    let snap = db(&hub, move |_| h.snapshot(Some(&device)))
        .await?
        .ok_or_else(ApiError::not_found)?;
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
    let (pages, displays, admins) = db(&hub, |s| {
        Ok((
            s.page_ids()?.len(),
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
        pages,
        displays,
        admins,
    }))
}

async fn list_pages(
    State(hub): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Result<Json<Vec<PageView>>, ApiError> {
    authenticate(&hub, &headers, peer.ip(), Role::Admin).await?;
    Ok(Json(db(&hub, |s| s.pages()).await?))
}

async fn get_page(
    State(hub): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<PageView>, ApiError> {
    authenticate(&hub, &headers, peer.ip(), Role::Admin).await?;
    db(&hub, move |s| s.page(&id))
        .await?
        .map(Json)
        .ok_or_else(ApiError::not_found)
}

fn clean_page_title(t: &str) -> Result<String, ApiError> {
    clean_title(t).ok_or_else(|| ApiError::bad_request("The title needs some printable text."))
}

fn check_stale(stale: u32) -> Result<u32, ApiError> {
    let range = hub_protocol::limits::STALE_AFTER_RANGE;
    if range.contains(&stale) {
        Ok(stale)
    } else {
        Err(ApiError::bad_request(format!(
            "A page goes stale after {} to {} seconds.",
            range.start(),
            range.end()
        )))
    }
}

/// Ids the admin routes use for themselves (`/v1/admin/pages/order`), so no page may have them.
const RESERVED_PAGE_IDS: &[&str] = &["order"];

/// A helper page needs a feed for helpers to send to.
fn check_helper(source: Source, feed: Option<&str>) -> Result<(), ApiError> {
    if source == Source::Helper && feed.is_none() {
        return Err(helper_refused());
    }
    Ok(())
}

fn helper_refused() -> ApiError {
    ApiError::bad_request("A page needs a feed before a helper can fill it. Give it one when you add it.")
}

fn bad_feed() -> ApiError {
    ApiError::bad_request(format!(
        "A feed name is lower-case letters and digits, in words joined by single hyphens, such as \
         garden-weather, up to {MAX_FEED_LEN} characters."
    ))
}

/// Checks a helper invite's feeds: at least one, each a valid name.
pub fn check_feeds(feeds: &[String]) -> Result<(), ApiError> {
    if feeds.is_empty() {
        return Err(ApiError::bad_request(
            "Choose at least one feed for the helper to send to.",
        ));
    }
    if feeds.len() > MAX_STORED_PAGES {
        return Err(ApiError::bad_request(format!(
            "A helper can send to at most {MAX_STORED_PAGES} feeds. Choose fewer."
        )));
    }
    if !feeds.iter().all(|f| valid_feed(f)) {
        return Err(bad_feed());
    }
    Ok(())
}

/// A page's settings, checked as hub-core says ([`settings_problem`]). Empty settings are none.
fn check_settings(settings: Option<Settings>) -> Result<Option<Settings>, ApiError> {
    match settings {
        Some(s) => match settings_problem(&s) {
            Some(why) => Err(ApiError::bad_request(why)),
            None => Ok(Some(s).filter(|s| !s.is_empty())),
        },
        None => Ok(None),
    }
}

/// `base` made into a page id: lower case, other characters as hyphens, at most 40 long.
fn id_from(base: &str) -> String {
    let mut id = String::new();
    for c in base.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            id.push(c);
        } else if !id.is_empty() && !id.ends_with('-') {
            id.push('-');
        }
    }
    let id: String = id.chars().take(40).collect();
    match id.trim_end_matches('-') {
        "" => "page".into(),
        id => id.into(),
    }
}

async fn add_page(
    State(hub): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    ApiJson(req): ApiJson<NewPage>,
) -> Result<(StatusCode, Json<PageView>), ApiError> {
    authenticate(&hub, &headers, peer.ip(), Role::Admin).await?;
    let title = clean_page_title(&req.title)?;
    if classify_kind(&req.kind) == KindClass::Invalid {
        return Err(ApiError::bad_request(
            "A page's kind is meter, big_number, text, list or a custom kind such as nearby-flights/1.",
        ));
    }
    if req.feed.as_deref().is_some_and(|f| !valid_feed(f)) {
        return Err(bad_feed());
    }
    check_helper(req.source, req.feed.as_deref())?;
    if req.template.as_deref().is_some_and(|t| !valid_template_id(t)) {
        return Err(ApiError::bad_request(
            "A template id is a name and a version, such as usage-limits/1.",
        ));
    }
    let settings = check_settings(req.settings)?;
    let stale_after_seconds = check_stale(
        req.stale_after_seconds
            .unwrap_or_else(|| req.source.stale_after_seconds()),
    )?;
    let existing = db(&hub, |s| s.page_ids()).await?;
    let id = match &req.id {
        Some(id) if RESERVED_PAGE_IDS.contains(&id.as_str()) => {
            return Err(ApiError::bad_request(format!(
                "The page id {id:?} is reserved. Choose another."
            )))
        }
        Some(id) if valid_page_id(id) => id.clone(),
        Some(_) => {
            return Err(ApiError::bad_request(
                "A page id uses a-z, 0-9 and '-', up to 48 characters.",
            ))
        }
        None => {
            let base = id_from(req.feed.as_deref().unwrap_or(&req.title));
            (1..)
                .map(|n| {
                    if n == 1 {
                        base.clone()
                    } else {
                        format!("{base}-{n}")
                    }
                })
                .find(|c| !existing.contains(c) && !RESERVED_PAGE_IDS.contains(&c.as_str()))
                .expect("an unused id exists")
        }
    };
    let page = StoredPage {
        id,
        title,
        kind: req.kind,
        source: req.source,
        feed: req.feed,
        stale_after_seconds,
        content: None,
        template: req.template,
        settings,
    };
    let (p2, now) = (page.clone(), hub.clock.now());
    db(&hub, move |s| s.add_page(&p2, now)).await?;
    Ok((StatusCode::CREATED, Json(page)))
}

async fn change_page(
    State(hub): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(id): Path<String>,
    ApiJson(req): ApiJson<PageChange>,
) -> Result<Json<PageView>, ApiError> {
    authenticate(&hub, &headers, peer.ip(), Role::Admin).await?;
    let title = req.title.as_deref().map(clean_page_title).transpose()?;
    let stale = req.stale_after_seconds.map(check_stale).transpose()?;
    // Empty settings remove them.
    let settings = req
        .settings
        .map(|s| check_settings(Some(s)).map(Option::unwrap_or_default))
        .transpose()?;
    // The helper rule is checked in the same transaction as the change, and only when the source
    // actually changes.
    let (outcome, page) = db(&hub, move |s| {
        let outcome = s.update_page(&id, title.as_deref(), req.source, stale, settings.as_ref())?;
        Ok((outcome, s.page(&id)?))
    })
    .await?;
    match (outcome, page) {
        (PageUpdate::Updated, Some(page)) => Ok(Json(page)),
        (PageUpdate::HelperNeedsFeed, _) => Err(helper_refused()),
        _ => Err(ApiError::not_found()),
    }
}

async fn remove_page(
    State(hub): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    authenticate(&hub, &headers, peer.ip(), Role::Admin).await?;
    if db(&hub, move |s| s.remove_page(&id)).await? {
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
) -> Result<Json<Vec<PageView>>, ApiError> {
    authenticate(&hub, &headers, peer.ip(), Role::Admin).await?;
    let list = db(&hub, move |s| {
        s.reorder(&req.ids)?;
        s.pages()
    })
    .await?;
    Ok(Json(list))
}

/// Sets what a manual or demo page shows, after checking it as a hub checks a page before sending it.
async fn set_content(
    State(hub): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(id): Path<String>,
    ApiJson(req): ApiJson<PageContentInput>,
) -> Result<Json<PageView>, ApiError> {
    authenticate(&hub, &headers, peer.ip(), Role::Admin).await?;
    let id2 = id.clone();
    let page = db(&hub, move |s| s.page(&id2))
        .await?
        .ok_or_else(ApiError::not_found)?;
    let (source, quality) = match page.source {
        Source::Manual => (Source::Manual, DataQuality::Manual),
        Source::Demo => (Source::Demo, DataQuality::Demo),
        Source::Helper | Source::None => return Err(not_manual()),
    };
    let now = hub.clock.now();
    let note = req.note.as_deref().and_then(clean_note);
    let data = req.data.filter(|d| !d.is_null());
    if let Some(d) = &data {
        check_content(&page, d, &note, quality, now)?;
    }
    let content = StoredContent {
        quality,
        updated_at: Some(now),
        data,
        note,
    };
    let c2 = content.clone();
    // Written only while the page keeps its source, in case it changed since it was read.
    match db(&hub, move |s| s.put_content(&id, source, &c2, now)).await? {
        ContentWrite::Written | ContentWrite::Unchanged => Ok(Json(StoredPage {
            content: Some(content),
            ..page
        })),
        ContentWrite::SourceChanged => Err(not_manual()),
        ContentWrite::NoPage => Err(ApiError::not_found()),
    }
}

/// Checks content for a page as the hub checks a page before sending it, and explains a refusal.
/// A meter with no items means "no figures yet": only that minimum is relaxed, for any meter page,
/// and every other rule still applies.
fn check_content(
    page: &StoredPage,
    data: &serde_json::Value,
    note: &Option<String>,
    quality: DataQuality,
    now: Timestamp,
) -> Result<(), ApiError> {
    let wire = Page {
        id: page.id.clone(),
        title: clean_title(&page.title).unwrap_or_default(),
        kind: page.kind.clone(),
        data_quality: quality,
        updated_at: Some(now),
        stale_after_seconds: page.stale_after_seconds,
        note: note.clone(),
        data: Some(data.clone()),
    };
    match wire.check_allowing_empty_meter() {
        Ok(()) => Ok(()),
        Err(CantShow::TooLarge) => Err(ApiError::bad_request(format!(
            "This page would be {} bytes; a page may be at most {} bytes.",
            wire.size(),
            hub_protocol::limits::MAX_PAGE_BYTES
        ))),
        Err(CantShow::BadData(why)) => Err(ApiError::bad_request(format!(
            "This data doesn't fit a {} page: {why}.",
            page.kind
        ))),
        Err(CantShow::UnknownKind) => Err(ApiError::bad_request("This page's kind isn't one the hub knows.")),
    }
}

fn not_manual() -> ApiError {
    ApiError::new(
        StatusCode::CONFLICT,
        ErrorCode::Conflict,
        "This page doesn't take content from the manager. Change its source to typed in (manual) or demo first.",
    )
}

async fn display_pages(
    State(hub): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<DisplayPages>, ApiError> {
    authenticate(&hub, &headers, peer.ip(), Role::Admin).await?;
    db(&hub, move |s| s.display_page_ids(&id))
        .await?
        .map(|ids| Json(DisplayPages { ids }))
        .ok_or_else(ApiError::not_found)
}

async fn set_display_pages(
    State(hub): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(id): Path<String>,
    ApiJson(req): ApiJson<DisplayPages>,
) -> Result<Json<DisplayPages>, ApiError> {
    authenticate(&hub, &headers, peer.ip(), Role::Admin).await?;
    // Ticks are a set: repeats are dropped before anything else.
    let mut seen = std::collections::HashSet::new();
    let ids: Vec<String> = req.ids.into_iter().filter(|id| seen.insert(id.clone())).collect();
    if ids.len() > MAX_STORED_PAGES {
        return Err(ApiError::bad_request(format!(
            "The hub holds at most {MAX_STORED_PAGES} pages."
        )));
    }
    db(&hub, move |s| {
        if !s.set_display_pages(&id, &ids)? {
            return Ok(None);
        }
        s.display_page_ids(&id)
    })
    .await?
    .map(|ids| Json(DisplayPages { ids }))
    .ok_or_else(ApiError::not_found)
}

// ----- helpers -----

/// What a helper invite's code row holds in its label column, until it is used. Invites made before
/// helpers sent to feeds named services, which are the feeds of the same names.
#[derive(serde::Serialize, serde::Deserialize)]
struct InviteDetail {
    label: String,
    #[serde(alias = "services")]
    feeds: Vec<String>,
}

/// How far a helper's collection time may be ahead of the hub's clock (PC and Pi clocks differ a
/// little).
const REPORT_FUTURE_SKEW_SECONDS: i64 = 300;

async fn new_helper(
    State(hub): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    ApiJson(req): ApiJson<NewHelper>,
) -> Result<(StatusCode, Json<HelperInvite>), ApiError> {
    let admin = authenticate(&hub, &headers, peer.ip(), Role::Admin).await?;
    check_feeds(&req.feeds)?;
    let h = hub.clone();
    let invite = db(&hub, move |_| h.new_helper_invite(&req.label, &req.feeds, &admin)).await?;
    Ok((StatusCode::CREATED, Json(invite)))
}

async fn helper_pair(
    State(hub): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    ApiJson(req): ApiJson<HelperPairRequest>,
) -> Result<Json<HelperPairResponse>, ApiError> {
    let ip = peer.ip();
    if hub.code_attempts.blocked(ip, Instant::now()) {
        return Err(too_many(&hub.code_attempts, ip));
    }
    hub.code_attempts.hit(ip, Instant::now());
    let now = hub.clock.now();
    let h = hash(req.secret.trim());
    let (outcome, detail) = db(&hub, move |s| s.use_code(&h, "helper-pair", now)).await?;
    match outcome {
        CodeUse::Accepted => {
            let invite: InviteDetail = detail
                .as_deref()
                .and_then(|d| serde_json::from_str(d).ok())
                .ok_or_else(|| {
                    ApiError::bad_request("This invite is damaged. Make a new one in the manager.")
                })?;
            let subject = new_id("h");
            let token = new_token(Role::Helper);
            let (sub2, th, feeds) = (subject.clone(), hash(token.expose()), invite.feeds.clone());
            let label = invite.label.clone();
            db(&hub, move |s| {
                s.add_credential(&th, Role::Helper, &sub2, &label, now)?;
                s.add_helper_feeds(&sub2, &feeds)?;
                s.audit(now, "helper_paired", &format!("{sub2} ({label}) from {ip}"))
            })
            .await?;
            tracing::info!(helper = %subject, label = %invite.label, "helper paired");
            Ok(Json(HelperPairResponse {
                helper_id: subject,
                helper_token: token.expose().to_string(),
                feeds: invite.feeds,
            }))
        }
        CodeUse::AlreadyUsed => {
            tracing::warn!(%ip, "a helper invite was used twice: someone else may have seen it");
            db(&hub, move |s| {
                s.audit(now, "ALARM_helper_code_reused", &format!("from {ip}"))
            })
            .await?;
            Err(ApiError::new(
                StatusCode::CONFLICT,
                ErrorCode::EnrolmentUsed,
                "This helper string was already used. If that wasn't you, check the helpers in the manager's Settings.",
            ))
        }
        CodeUse::Unknown => Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            ErrorCode::EnrolmentExpired,
            "This helper string has expired or doesn't exist. Make a new one in the manager's Settings.",
        )),
    }
}

/// `PUT /v1/helper/feeds/{feed}`: a helper's page body, for every helper page of the feed that is
/// of the same kind. It is checked as the hub checks a page before sending it, against each page it
/// would go to, and nothing is stored unless it passes for all of them.
async fn put_feed(
    State(hub): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(feed): Path<String>,
    ApiJson(req): ApiJson<FeedContent>,
) -> Result<Json<FeedAccepted>, ApiError> {
    let subject = authenticate(&hub, &headers, peer.ip(), Role::Helper).await?;
    let allowed = db(&hub, move |s| s.helper_feeds(&subject)).await?;
    if !allowed.contains(&feed) {
        return Err(not_invited());
    }
    if classify_kind(&req.kind) == KindClass::Invalid {
        return Err(ApiError::bad_request(
            "A page's kind is meter, big_number, text, list or a custom kind such as nearby-flights/1.",
        ));
    }
    let now = hub.clock.now();
    let collected_at = req.collected_at.unwrap_or(now);
    if collected_at.seconds_until(now) < -REPORT_FUTURE_SKEW_SECONDS {
        return Err(ApiError::bad_request(
            "This content is dated in the future. Check the clock on the helper's computer is set automatically.",
        ));
    }
    let note = req.note.as_deref().and_then(clean_note);
    let quality = DataQuality::ProviderReported;
    // Checked as a page of this feed would be, even when none takes it yet, and then against each
    // page it goes to: their ids and titles count towards the size cap.
    let stand_in = StoredPage {
        id: feed.clone(),
        title: feed.clone(),
        kind: req.kind.clone(),
        source: Source::Helper,
        feed: Some(feed.clone()),
        stale_after_seconds: Source::Helper.stale_after_seconds(),
        content: None,
        template: None,
        settings: None,
    };
    check_content(&stand_in, &req.data, &note, quality, now)?;
    let content = StoredContent {
        quality,
        // Never later than the hub's own clock, so ages are never negative.
        updated_at: Some(collected_at.min(now)),
        data: Some(req.data),
        note,
    };
    let kind = req.kind;
    // The per-page check runs inside the write, so what was checked is what is replaced.
    let written = db_api(&hub, move |s| {
        let data = content.data.clone().unwrap_or_default();
        let note = content.note.clone();
        s.put_feed_content(&feed, &kind, &content, now, |page| {
            check_content(page, &data, &note, quality, now)
        })
    })
    .await?;
    Ok(Json(FeedAccepted {
        updated: written.updated,
        skipped: written.skipped,
    }))
}

/// `GET /v1/helper/feeds/{feed}/settings`: the template and settings of each helper page of a feed
/// the helper was invited for, so it can do what each page was set up for. Nothing else is read.
async fn feed_settings(
    State(hub): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(feed): Path<String>,
) -> Result<Json<FeedSettings>, ApiError> {
    let subject = authenticate(&hub, &headers, peer.ip(), Role::Helper).await?;
    let allowed = db(&hub, move |s| s.helper_feeds(&subject)).await?;
    if !allowed.contains(&feed) {
        return Err(not_invited());
    }
    let pages = db(&hub, move |s| s.feed_pages(&feed)).await?;
    Ok(Json(FeedSettings {
        pages: pages
            .into_iter()
            .map(|p| PageSettings {
                page_id: p.id,
                template: p.template,
                settings: p.settings.unwrap_or_default(),
            })
            .collect(),
    }))
}

fn not_invited() -> ApiError {
    ApiError::new(
        StatusCode::FORBIDDEN,
        ErrorCode::Forbidden,
        "This helper isn't set up to send to that feed. Invite it for the feed in the manager's Settings.",
    )
}

async fn list_helpers(
    State(hub): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Result<Json<Vec<HelperView>>, ApiError> {
    authenticate(&hub, &headers, peer.ip(), Role::Admin).await?;
    let views = db(&hub, |s| {
        s.helpers()?
            .into_iter()
            .map(|(id, label, created_at, revoked)| {
                let feeds = s.helper_feeds(&id)?;
                Ok(HelperView {
                    id,
                    label,
                    created_at,
                    revoked,
                    feeds,
                })
            })
            .collect::<Result<Vec<_>, StoreError>>()
    })
    .await?;
    Ok(Json(views))
}

async fn revoke_helper(
    State(hub): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let admin = authenticate(&hub, &headers, peer.ip(), Role::Admin).await?;
    let now = hub.clock.now();
    let n = db(&hub, move |s| {
        let n = s.revoke(Role::Helper, &id, now)?;
        if n > 0 {
            s.audit(now, "helper_revoked", &format!("{id} by {admin}"))?;
        }
        Ok(n)
    })
    .await?;
    if n == 0 {
        return Err(ApiError::not_found());
    }
    Ok(StatusCode::NO_CONTENT)
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

/// `GET /v1/admin/preview-snapshot[?display=<id>]`.
#[derive(serde::Deserialize)]
struct PreviewQuery {
    display: Option<String>,
}

/// What a display would receive now, or with no display given, the first pages of all.
async fn preview_snapshot(
    State(hub): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    query: Result<Query<PreviewQuery>, QueryRejection>,
) -> Result<Json<hub_protocol::Snapshot>, ApiError> {
    authenticate(&hub, &headers, peer.ip(), Role::Admin).await?;
    let Query(query) =
        query.map_err(|e| ApiError::bad_request(format!("The query isn't valid: {}", e.body_text())))?;
    let h = hub.clone();
    db(&hub, move |_| h.snapshot(query.display.as_deref()))
        .await?
        .map(Json)
        .ok_or_else(ApiError::not_found)
}
