//! The firmware routes (`docs/firmware-updates.md`, "Protocol additions"): uploads from admins and
//! publishers, the shelf, offers, signers' labels, publishers, and the two device routes, the
//! download and the report.
//!
//! Uploads and downloads stream through a fixed-size buffer, so a 16 MiB package never sits in
//! memory. They alone get the long time limit ([`shelf::FIRMWARE_REQUEST_TIMEOUT`]); every other
//! route here keeps the hub's usual limits. An upload that sends nothing for a minute is dropped
//! sooner, and a download that runs past its limit loses its connection even if the display has
//! stopped reading.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use axum::body::{Body, Bytes, HttpBody};
use bytes::BytesMut;
use hub_core::api::{
    DisplayFirmware, EvictedPackage, FirmwareOfferRequest, FirmwareOfferView, FirmwarePackageView,
    NewPublisher, PublisherInvite, PublisherPairRequest, PublisherPairResponse, PublisherView, SignerLabel,
    SignerView,
};
use hub_protocol::firmware::{is_lower_hex, FirmwareReport, KEY_ID_HEX_LEN, PACKAGE_ID_HEX_LEN};
use hub_protocol::limits::{FIRMWARE_OFFER_LIFETIME_SECONDS, MAX_FIRMWARE_DOWNLOADS_PER_WINDOW};
use tokio::io::{AsyncRead, AsyncWriteExt, ReadBuf};
use tokio::sync::OwnedSemaphorePermit;

use super::*;
use crate::firmware::{self as shelf, MAX_UPLOAD_BYTES, MIN_FREE_DISK_BYTES};
use crate::http::CloseConnection;
use crate::store::firmware::{
    KeySet, NewOffer, OfferRow, PackageRow, ReportOutcome, CANCELLED_PACKAGE_DELETED,
};

fn conflict(message: impl Into<String>) -> ApiError {
    ApiError::new(StatusCode::CONFLICT, ErrorCode::Conflict, message)
}

fn no_package() -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        ErrorCode::NotFound,
        "The hub doesn't hold that package. Upload it first.",
    )
}

impl Hub {
    /// Issues a one-time secret for a firmware maker's `hub-firmware login`.
    pub fn new_publisher_invite(&self, label: &str, by: &str) -> Result<PublisherInvite, StoreError> {
        let label = clean_title(label).unwrap_or_else(|| "Publisher".into());
        let (_, code) = new_code();
        let now = self.clock.now();
        let expires = now.plus_seconds(CODE_LIFETIME_SECONDS);
        self.store
            .add_code(&hash(code.expose()), "publisher-pair", &label, now, expires)?;
        self.store
            .audit(now, "publisher_invited", &format!("{label} by {by}"))?;
        Ok(PublisherInvite {
            secret: code.expose().to_string(),
            expires_at: expires,
            identity: self.identity.info(),
        })
    }

    /// The firmware offer to put in a display's snapshot, if it has an open one.
    pub(super) fn snapshot_offer(
        &self,
        store: &Store,
        device: &str,
    ) -> Result<Option<hub_protocol::firmware::FirmwareUpdate>, StoreError> {
        let now = self.clock.now();
        let Some(offer) = store.open_offer(device, now)? else {
            return Ok(None);
        };
        Ok(store
            .firmware_package(&offer.package_id)?
            .map(|p| hub_protocol::firmware::FirmwareUpdate {
                offer_id: offer.id,
                package_id: p.id,
                size: p.update_size,
            }))
    }

    /// What a display last said about itself: the buffered heartbeat, or else the stored one.
    pub(super) fn latest_info(&self, device: &str, stored: Option<DeviceInfo>) -> Option<DeviceInfo> {
        self.seen().get(device).and_then(|s| s.info.clone()).or(stored)
    }

    /// The packages folder.
    pub fn firmware_dir(&self) -> std::path::PathBuf {
        shelf::dir(&self.config.data_dir)
    }

    /// Brings the packages folder and its rows into agreement. Run when the hub starts serving.
    pub fn tidy_firmware(&self) -> Result<shelf::Tidied, StoreError> {
        let done = shelf::tidy(&self.firmware_dir(), &self.store, self.clock.now())?;
        if !done.dropped_rows.is_empty() || done.removed_files > 0 {
            tracing::info!(
                dropped = done.dropped_rows.len(),
                removed_files = done.removed_files,
                "tidied the firmware folder"
            );
            self.store.audit(
                self.clock.now(),
                "firmware_tidied",
                &format!(
                    "{} rows without files dropped, {} stray files removed",
                    done.dropped_rows.len(),
                    done.removed_files
                ),
            )?;
        }
        Ok(done)
    }
}

/// A firmware report or a firmware member of a device report, with a bad `firmware` member dropped:
/// a display with buggy key reporting can still enrol and check in, it just can't be offered updates.
pub(super) fn drop_bad_firmware(info: &mut DeviceInfo) {
    if let Some(problems) = info.firmware.as_ref().map(|f| f.problems()) {
        if !problems.is_empty() {
            tracing::warn!(problems = %problems.join("; "), "ignoring a display's firmware report that breaks the rules");
            info.firmware = None;
        }
    }
}

fn labels(store: &Store) -> Result<HashMap<String, String>, StoreError> {
    Ok(store
        .firmware_signers()?
        .into_iter()
        .map(|(k, l, _)| (k, l))
        .collect())
}

// ----- uploads -----

pub(super) async fn upload_admin(
    State(hub): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Body,
) -> Result<(StatusCode, Json<FirmwarePackageView>), ApiError> {
    let who = authenticate(&hub, &headers, peer.ip(), Role::Admin).await?;
    upload(hub, Role::Admin, who, &headers, body).await
}

pub(super) async fn upload_publisher(
    State(hub): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Body,
) -> Result<(StatusCode, Json<FirmwarePackageView>), ApiError> {
    let who = authenticate(&hub, &headers, peer.ip(), Role::Publisher).await?;
    upload(hub, Role::Publisher, who, &headers, body).await
}

/// Removes an upload's temporary file when dropped, unless it was kept.
struct UploadFile(Option<std::path::PathBuf>);

impl Drop for UploadFile {
    fn drop(&mut self) {
        if let Some(p) = self.0.take() {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// `POST /v1/admin/firmware` and `/v1/publisher/firmware`: the raw package as the body. Streamed to
/// a temporary file, checked by `firmware_package::check`, then kept under its package id.
async fn upload(
    hub: AppState,
    role: Role,
    who: String,
    headers: &HeaderMap,
    mut body: Body,
) -> Result<(StatusCode, Json<FirmwarePackageView>), ApiError> {
    let Some(len) = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
    else {
        return Err(ApiError::new(
            StatusCode::LENGTH_REQUIRED,
            ErrorCode::BadRequest,
            "Send the package with a Content-Length.",
        ));
    };
    if len > MAX_UPLOAD_BYTES {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            ErrorCode::PayloadTooLarge,
            format!("A firmware package is at most {MAX_UPLOAD_BYTES} bytes; this one is {len}."),
        ));
    }
    if len < firmware_package::limits::MIN_PACKAGE_LEN {
        return Err(ApiError::bad_request(format!(
            "This isn't a valid firmware package: {}",
            firmware_package::PackageError::TooShort(len)
        )));
    }
    let Ok(_permit) = hub.upload_slots.clone().try_acquire_owned() else {
        return Err(ApiError::unavailable(
            "Another package is being uploaded. Try again in a moment.",
        ));
    };
    let dir = hub.firmware_dir();
    shelf::ensure_dir(&dir).map_err(|e| {
        tracing::error!(error = %e, "couldn't make the firmware folder");
        ApiError::unavailable("The hub couldn't make its firmware folder. Check the hub's log.")
    })?;
    if let Some(free) = shelf::free_bytes(&dir) {
        if free < len.saturating_add(MIN_FREE_DISK_BYTES) {
            return Err(conflict(format!(
                "The hub's disk is nearly full: it keeps at least {} MiB free, and this package would leave \
                 less. Free some space on the hub, or remove a package.",
                MIN_FREE_DISK_BYTES / (1024 * 1024)
            )));
        }
    }

    // Streamed to disk, never held in memory.
    let path = shelf::upload_path(&dir);
    let guard = UploadFile(Some(path.clone()));
    let file = shelf::create_upload(&path).map_err(|e| {
        tracing::error!(error = %e, "couldn't create an upload file");
        ApiError::unavailable("The hub couldn't save the upload. Check the hub's log.")
    })?;
    let mut file = tokio::fs::File::from_std(file);
    let mut got: u64 = 0;
    let disk = |e: std::io::Error| {
        tracing::error!(error = %e, "couldn't write an upload");
        ApiError::unavailable("The hub couldn't save the upload. Check the hub's log.")
    };
    let idle = hub.firmware_time_limits().upload_idle;
    loop {
        let next = std::future::poll_fn(|cx| Pin::new(&mut body).poll_frame(cx));
        let Ok(frame) = tokio::time::timeout(idle, next).await else {
            return Err(ApiError::new(
                StatusCode::REQUEST_TIMEOUT,
                ErrorCode::Unavailable,
                format!(
                    "The upload sent nothing for {} s, so the hub gave up on it. Try again.",
                    idle.as_secs()
                ),
            ));
        };
        let Some(frame) = frame else { break };
        let frame = frame.map_err(|_| ApiError::bad_request("The upload was cut short. Try again."))?;
        let Ok(data) = frame.into_data() else { continue };
        got += data.len() as u64;
        if got > len {
            return Err(ApiError::bad_request(
                "The upload was longer than its Content-Length.",
            ));
        }
        file.write_all(&data).await.map_err(disk)?;
    }
    if got != len {
        return Err(ApiError::bad_request("The upload was cut short. Try again."));
    }
    file.sync_all().await.map_err(disk)?;
    drop(file);

    let check_path = path.clone();
    let checked = tokio::task::spawn_blocking(move || firmware_package::check_file(&check_path))
        .await
        .map_err(|_| ApiError::unavailable("The hub hit an internal fault."))?;
    let now = hub.clock.now();
    let checked = match checked {
        Ok(c) => c,
        Err(e) => {
            let why = e.to_string();
            let w = who.clone();
            db(&hub, move |s| {
                s.audit(now, "firmware_upload_refused", &format!("by {w}: {why}"))
            })
            .await?;
            return Err(ApiError::bad_request(format!(
                "This isn't a valid firmware package: {e}."
            )));
        }
    };

    let row = shelf::row_from(&checked, role, &who, now);
    let publisher = (role == Role::Publisher).then(|| who.clone());
    let (status, view) = db_api(&hub, move |s| {
        let dest = shelf::package_path(&dir, &row.id);
        if let Some(existing) = s.firmware_package(&row.id)? {
            if dest.is_file() && existing.key_set != KeySet::Unreadable {
                // Uploading the same file twice is harmless.
                let offered = s.offered_to(now)?.remove(&existing.id).unwrap_or_default();
                return Ok((
                    StatusCode::OK,
                    shelf::package_view(&existing, &labels(s)?, offered),
                ));
            }
            // A row whose file went missing, or whose key_set can't be read: start again.
            s.remove_firmware_package(&row.id, crate::store::firmware::CANCELLED_PACKAGE_MISSING, now)?;
        }
        let stored = s.firmware_packages()?;
        // The same release signed again is the same release: the stored file stays.
        if let Some(existing) = shelf::same_release(&dir, &stored, &checked) {
            let offered = s.offered_to(now)?.remove(&existing.id).unwrap_or_default();
            return Ok((
                StatusCode::OK,
                shelf::package_view(existing, &labels(s)?, offered),
            ));
        }
        let on_offer = s.packages_on_offer(now)?;
        let evict = shelf::plan_room(&stored, &row, &on_offer, publisher.as_deref())
            .map_err(|why| conflict(why.message(&row.device_type)))?;
        let evicted = evict
            .as_ref()
            .and_then(|id| stored.iter().find(|p| &p.id == id))
            .map(|p| EvictedPackage {
                package_id: p.id.clone(),
                device_type: p.device_type.clone(),
                firmware_version: p.firmware_version.clone(),
            });
        shelf::keep_upload(&path, &dest).map_err(|e| {
            tracing::error!(error = %e, "couldn't keep an upload");
            ApiError::unavailable("The hub couldn't save the upload. Check the hub's log.")
        })?;
        let evict: Vec<String> = evict.into_iter().collect();
        if let Err(e) = s.add_firmware_package(&row, &evict, now) {
            let _ = std::fs::remove_file(&dest);
            return Err(e.into());
        }
        if let Some(gone) = &evicted {
            let _ = std::fs::remove_file(shelf::package_path(&dir, &gone.package_id));
            s.audit(
                now,
                "firmware_evicted",
                &format!(
                    "{} {} {} to make room for {}",
                    gone.package_id, gone.device_type, gone.firmware_version, row.id
                ),
            )?;
        }
        s.audit(
            now,
            "firmware_uploaded",
            &format!(
                "{} {} {} by {} {}",
                row.id,
                row.device_type,
                row.firmware_version,
                row.uploaded_by_role.as_str(),
                row.uploaded_by
            ),
        )?;
        let kept = s.firmware_package(&row.id)?.unwrap_or(row);
        let mut view = shelf::package_view(&kept, &labels(s)?, Vec::new());
        view.evicted = evicted;
        Ok((StatusCode::CREATED, view))
    })
    .await?;
    // A kept upload was renamed; the guard removes a duplicate's temporary file.
    drop(guard);
    Ok((status, Json(view)))
}

// ----- the shelf -----

async fn list(hub: &AppState, with_offers: bool) -> Result<Json<Vec<FirmwarePackageView>>, ApiError> {
    let now = hub.clock.now();
    let views = db(hub, move |s| {
        let labels = labels(s)?;
        let mut offered = if with_offers {
            s.offered_to(now)?
        } else {
            HashMap::new()
        };
        Ok(s.firmware_packages()?
            .iter()
            .map(|p| {
                let offered = offered.remove(&p.id).unwrap_or_default();
                shelf::package_view(p, &labels, offered)
            })
            .collect())
    })
    .await?;
    Ok(Json(views))
}

pub(super) async fn list_admin(
    State(hub): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Result<Json<Vec<FirmwarePackageView>>, ApiError> {
    authenticate(&hub, &headers, peer.ip(), Role::Admin).await?;
    list(&hub, true).await
}

/// A publisher sees the shelf, but not which displays anything is on offer to.
pub(super) async fn list_publisher(
    State(hub): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Result<Json<Vec<FirmwarePackageView>>, ApiError> {
    authenticate(&hub, &headers, peer.ip(), Role::Publisher).await?;
    list(&hub, false).await
}

/// `GET /v1/admin/firmware/{package_id}/file`: the whole package, for a USB install from the app.
pub(super) async fn package_file(
    State(hub): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    authenticate(&hub, &headers, peer.ip(), Role::Admin).await?;
    if !is_lower_hex(&id, PACKAGE_ID_HEX_LEN) {
        return Err(no_package());
    }
    let pkg = db(&hub, move |s| s.firmware_package(&id))
        .await?
        .ok_or_else(no_package)?;
    let file = open_package(&hub, &pkg, pkg.size).await?;
    Ok(stream_file(&hub, file, pkg.size, None))
}

pub(super) async fn delete_package(
    State(hub): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let admin = authenticate(&hub, &headers, peer.ip(), Role::Admin).await?;
    if !is_lower_hex(&id, PACKAGE_ID_HEX_LEN) {
        return Err(no_package());
    }
    let now = hub.clock.now();
    let dir = hub.firmware_dir();
    let found = db(&hub, move |s| {
        let found = s.remove_firmware_package(&id, CANCELLED_PACKAGE_DELETED, now)?;
        if found {
            let _ = std::fs::remove_file(shelf::package_path(&dir, &id));
            s.audit(now, "firmware_deleted", &format!("{id} by {admin}"))?;
        }
        Ok(found)
    })
    .await?;
    if found {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(no_package())
    }
}

// ----- offers -----

/// `PUT /v1/admin/devices/{id}/firmware-update`: the **Update** button.
pub(super) async fn put_offer(
    State(hub): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(device): Path<String>,
    ApiJson(req): ApiJson<FirmwareOfferRequest>,
) -> Result<Json<FirmwareOfferView>, ApiError> {
    let admin = authenticate(&hub, &headers, peer.ip(), Role::Admin).await?;
    if !is_lower_hex(&req.package_id, PACKAGE_ID_HEX_LEN) {
        return Err(ApiError::bad_request(format!(
            "A package id is {PACKAGE_ID_HEX_LEN} lower-case hex digits."
        )));
    }
    let now = hub.clock.now();
    let h = hub.clone();
    let offer = db_api(&hub, move |s| {
        let row = s.device(&device)?.ok_or_else(ApiError::not_found)?;
        let pkg = s.firmware_package(&req.package_id)?.ok_or_else(no_package)?;
        let info = h.latest_info(&device, row.info);
        shelf::courtesy(info.as_ref(), &pkg).map_err(conflict)?;
        let id: String = crate::secret::random_bytes::<16>()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let offer = s.make_offer(&NewOffer {
            id: &id,
            device_id: &device,
            package_id: &pkg.id,
            from_version: info.as_ref().map(|i| i.firmware_version.as_str()),
            made_by: &admin,
            now,
            expires_at: now.plus_seconds(FIRMWARE_OFFER_LIFETIME_SECONDS),
        })?;
        s.audit(
            now,
            "firmware_offered",
            &format!("{id}: {} {} to {device} by {admin}", pkg.id, pkg.firmware_version),
        )?;
        Ok(offer)
    })
    .await?;
    Ok(Json(shelf::offer_view(&offer, now)))
}

pub(super) async fn delete_offer(
    State(hub): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(device): Path<String>,
) -> Result<StatusCode, ApiError> {
    let admin = authenticate(&hub, &headers, peer.ip(), Role::Admin).await?;
    let now = hub.clock.now();
    let cancelled = db(&hub, move |s| {
        let n = s.cancel_offer(&device, now)?;
        if n {
            s.audit(now, "firmware_offer_cancelled", &format!("{device} by {admin}"))?;
        }
        Ok(n)
    })
    .await?;
    if cancelled {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::new(
            StatusCode::NOT_FOUND,
            ErrorCode::NotFound,
            "That display has no update on offer.",
        ))
    }
}

/// Each display's `firmware`, for `GET /v1/admin/devices`.
pub(super) fn display_firmware(
    infos: &[(String, Option<DeviceInfo>, bool)],
    packages: &[PackageRow],
    offers: &HashMap<String, OfferRow>,
    now: Timestamp,
) -> HashMap<String, DisplayFirmware> {
    infos
        .iter()
        .map(|(id, info, auto_update)| {
            (
                id.clone(),
                DisplayFirmware {
                    reported: info.as_ref().and_then(|i| i.firmware.clone()),
                    offer: offers.get(id).map(|o| shelf::offer_view(o, now)),
                    available_package_id: shelf::best_package(info.as_ref(), packages).map(|p| p.id.clone()),
                    auto_update: *auto_update,
                },
            )
        })
        .collect()
}

// ----- signers -----

fn signer_view((key_id, label, updated_at): (String, String, Timestamp)) -> SignerView {
    SignerView {
        fingerprint: shelf::fingerprint(&key_id),
        key_id,
        label,
        updated_at,
    }
}

fn bad_key_id() -> ApiError {
    ApiError::bad_request(format!("A key id is {KEY_ID_HEX_LEN} lower-case hex digits."))
}

pub(super) async fn list_signers(
    State(hub): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Result<Json<Vec<SignerView>>, ApiError> {
    authenticate(&hub, &headers, peer.ip(), Role::Admin).await?;
    let rows = db(&hub, |s| s.firmware_signers()).await?;
    Ok(Json(rows.into_iter().map(signer_view).collect()))
}

pub(super) async fn put_signer(
    State(hub): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(key_id): Path<String>,
    ApiJson(req): ApiJson<SignerLabel>,
) -> Result<Json<SignerView>, ApiError> {
    let admin = authenticate(&hub, &headers, peer.ip(), Role::Admin).await?;
    if !is_lower_hex(&key_id, KEY_ID_HEX_LEN) {
        return Err(bad_key_id());
    }
    let label = clean_title(&req.label)
        .ok_or_else(|| ApiError::bad_request("The label needs some printable text."))?;
    if firmware_package::label::reserved_label(&label) {
        return Err(ApiError::bad_request(
            "That name is kept for firmware whose key is built into the app, so a label can't use it. \
             Name the key after its maker instead.",
        ));
    }
    let now = hub.clock.now();
    let (k, l) = (key_id.clone(), label.clone());
    db(&hub, move |s| {
        s.set_signer_label(&k, &l, now)?;
        s.audit(now, "firmware_signer_labelled", &format!("{k} as {l} by {admin}"))
    })
    .await?;
    Ok(Json(signer_view((key_id, label, now))))
}

pub(super) async fn delete_signer(
    State(hub): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(key_id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let admin = authenticate(&hub, &headers, peer.ip(), Role::Admin).await?;
    if !is_lower_hex(&key_id, KEY_ID_HEX_LEN) {
        return Err(bad_key_id());
    }
    let now = hub.clock.now();
    let found = db(&hub, move |s| {
        let found = s.remove_signer_label(&key_id)?;
        if found {
            s.audit(now, "firmware_signer_unlabelled", &format!("{key_id} by {admin}"))?;
        }
        Ok(found)
    })
    .await?;
    if found {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::not_found())
    }
}

// ----- publishers -----

pub(super) async fn list_publishers(
    State(hub): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Result<Json<Vec<PublisherView>>, ApiError> {
    authenticate(&hub, &headers, peer.ip(), Role::Admin).await?;
    let rows = db(&hub, |s| s.publishers()).await?;
    Ok(Json(
        rows.into_iter()
            .map(|(id, label, created_at, revoked)| PublisherView {
                id,
                label,
                created_at,
                revoked,
            })
            .collect(),
    ))
}

pub(super) async fn new_publisher(
    State(hub): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    ApiJson(req): ApiJson<NewPublisher>,
) -> Result<(StatusCode, Json<PublisherInvite>), ApiError> {
    let admin = authenticate(&hub, &headers, peer.ip(), Role::Admin).await?;
    let h = hub.clone();
    let invite = db(&hub, move |_| h.new_publisher_invite(&req.label, &admin)).await?;
    Ok((StatusCode::CREATED, Json(invite)))
}

pub(super) async fn revoke_publisher(
    State(hub): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let admin = authenticate(&hub, &headers, peer.ip(), Role::Admin).await?;
    let now = hub.clock.now();
    let n = db(&hub, move |s| {
        let n = s.revoke(Role::Publisher, &id, now)?;
        if n > 0 {
            s.audit(now, "publisher_revoked", &format!("{id} by {admin}"))?;
        }
        Ok(n)
    })
    .await?;
    if n == 0 {
        return Err(ApiError::not_found());
    }
    Ok(StatusCode::NO_CONTENT)
}

pub(super) async fn publisher_pair(
    State(hub): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    ApiJson(req): ApiJson<PublisherPairRequest>,
) -> Result<Json<PublisherPairResponse>, ApiError> {
    let ip = peer.ip();
    if hub.code_attempts.blocked(ip, Instant::now()) {
        return Err(too_many(&hub.code_attempts, ip));
    }
    hub.code_attempts.hit(ip, Instant::now());
    let now = hub.clock.now();
    let h = hash(req.secret.trim());
    let (outcome, label) = db(&hub, move |s| s.use_code(&h, "publisher-pair", now)).await?;
    match outcome {
        CodeUse::Accepted => {
            let subject = new_id("p");
            let token = new_token(Role::Publisher);
            let label = label.unwrap_or_else(|| "Publisher".into());
            let (sub2, th) = (subject.clone(), hash(token.expose()));
            db(&hub, move |s| {
                s.add_credential(&th, Role::Publisher, &sub2, &label, now)?;
                s.audit(now, "publisher_paired", &format!("{sub2} ({label}) from {ip}"))
            })
            .await?;
            tracing::info!(publisher = %subject, "publisher paired");
            Ok(Json(PublisherPairResponse {
                publisher_id: subject,
                publisher_token: token.expose().to_string(),
            }))
        }
        CodeUse::AlreadyUsed => {
            tracing::warn!(%ip, "a publisher string was used twice: someone else may have seen it");
            db(&hub, move |s| {
                s.audit(now, "ALARM_publisher_code_reused", &format!("from {ip}"))
            })
            .await?;
            Err(ApiError::new(
                StatusCode::CONFLICT,
                ErrorCode::EnrolmentUsed,
                "This publisher string was already used. If that wasn't you, ask the hub's owner to check its publishers.",
            ))
        }
        CodeUse::Unknown => Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            ErrorCode::EnrolmentExpired,
            "This publisher string has expired or doesn't exist. Ask the hub's owner for a new one.",
        )),
    }
}

// ----- device routes -----

fn no_offer() -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        ErrorCode::NotFound,
        "There is no open firmware offer for this display and package.",
    )
}

/// `GET /v1/device/firmware/{package_id}`: the update form, only to a display with an open offer
/// for that package. Range requests are ignored: a download always starts from the beginning.
pub(super) async fn device_download(
    State(hub): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    close: Option<axum::Extension<CloseConnection>>,
    headers: HeaderMap,
    Path(package_id): Path<String>,
) -> Result<Response, ApiError> {
    check_protocol(&headers)?;
    let device = authenticate(&hub, &headers, peer.ip(), Role::Device).await?;
    if !is_lower_hex(&package_id, PACKAGE_ID_HEX_LEN) {
        return Err(no_offer());
    }
    let now = hub.clock.now();
    let d = device.clone();
    let pkg = db(&hub, move |s| {
        Ok(match s.open_offer(&d, now)? {
            Some(o) if o.package_id == package_id => s.firmware_package(&package_id)?,
            _ => None,
        })
    })
    .await?
    .ok_or_else(no_offer)?;
    // Every start counts, answered or not.
    let t = Instant::now();
    if hub.download_starts.blocked(device.clone(), t) {
        let wait = hub.download_starts.retry_after(device, t).as_secs().max(1);
        return Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            ErrorCode::RateLimited,
            format!(
                "This display has started {MAX_FIRMWARE_DOWNLOADS_PER_WINDOW} downloads within the hour. Try again in {wait} s."
            ),
        ));
    }
    hub.download_starts.hit(device.clone(), t);
    // One download at a time for each display, and a few across the hub. A newer request from a
    // display always wins: the display only asks again once it has given up on its last download,
    // so one still held is on a connection that died without saying so. The older download it
    // overtakes is closed, or answered `unavailable` if it hadn't started sending yet. But a request
    // that can't be served, because the file can't be read or the hub has no slot for it, never
    // ends a running download: everything that could stop it is checked before it takes over.
    let (slot, file) = match hub.download_slots.clone().try_acquire_owned() {
        // A free slot: only the file is left to check. If it fails, the slot is let go unused.
        Ok(slot) => (slot, open_package(&hub, &pkg, pkg.update_size).await?),
        // None free: check the file first, then take the slot this display's last download holds.
        Err(_) => {
            let file = open_package(&hub, &pkg, pkg.update_size).await?;
            let before = hub
                .downloading
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&device)
                .and_then(std::sync::Weak::upgrade);
            let slot = match before.as_ref().and_then(|b| take_over_slot(b)) {
                Some(slot) => {
                    tracing::info!(%device, "a new firmware download takes over this display's last one");
                    slot
                }
                // The last download ended meanwhile, freeing its slot, or there was none to take.
                None => hub.download_slots.clone().try_acquire_owned().map_err(|_| {
                    ApiError::unavailable(
                        "The hub is already sending firmware to other displays. Try again later.",
                    )
                })?,
            };
            (slot, file)
        }
    };
    let held = Arc::new(Mutex::new(Held {
        close: close.map(|c| c.0),
        ..Held::default()
    }));
    let (turn, before) = DisplayTurn::take(&hub.downloading, &device, &held);
    if let Some(before) = before.upgrade() {
        tracing::info!(%device, "a new firmware download replaces this display's last one");
        before.lock().unwrap_or_else(|e| e.into_inner()).supersede();
    }
    {
        let mut h = held.lock().unwrap_or_else(|e| e.into_inner());
        if h.superseded {
            // A newer download from this display began in the moment since, and wins. Returning
            // lets go of the slot this one holds, whether free or taken over, and of its turn.
            drop(h);
            return Err(ApiError::unavailable(
                "This display started another firmware download, which replaces this one.",
            ));
        }
        h.permit = Some(DownloadPermit {
            _slot: slot,
            _turn: turn,
        });
    }
    tracing::info!(%device, package = %pkg.id, "sending firmware");
    Ok(stream_file(&hub, file, pkg.update_size, Some(held)))
}

/// Ends a running download for a newer one from the same display, handing over its slot so no
/// other display can take it in between. `None` if it holds none, having ended. Its turn is let go
/// here, after its lock, as dropping a turn locks the list of downloads.
fn take_over_slot(before: &Mutex<Held>) -> Option<OwnedSemaphorePermit> {
    let permit = {
        let mut h = before.lock().unwrap_or_else(|e| e.into_inner());
        let permit = h.permit.take()?;
        h.supersede();
        permit
    };
    let DownloadPermit {
        _slot: slot,
        _turn: turn,
    } = permit;
    drop(turn);
    Some(slot)
}

/// The displays being sent a download now, each with what its download holds.
pub(super) type Downloading = Arc<Mutex<HashMap<String, std::sync::Weak<Mutex<Held>>>>>;

impl Hub {
    /// Whether a firmware download to this display is under way.
    pub fn is_downloading(&self, device: &str) -> bool {
        self.downloading
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(device)
    }

    /// How many displays are being sent a firmware download now.
    pub fn downloads_under_way(&self) -> usize {
        self.downloading.lock().unwrap_or_else(|e| e.into_inner()).len()
    }
}

/// A display's turn to download: while it is held, the display is downloading.
struct DisplayTurn {
    device: String,
    downloading: Downloading,
    held: std::sync::Weak<Mutex<Held>>,
}

impl DisplayTurn {
    /// The display's turn, for the download that `held` belongs to, and what its last download held
    /// if it is still under way (an empty `Weak` if not).
    fn take(
        downloading: &Downloading,
        device: &str,
        held: &Arc<Mutex<Held>>,
    ) -> (Self, std::sync::Weak<Mutex<Held>>) {
        let held = Arc::downgrade(held);
        let before = downloading
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(device.to_string(), held.clone())
            .unwrap_or_default();
        let turn = Self {
            device: device.to_string(),
            downloading: downloading.clone(),
            held,
        };
        (turn, before)
    }
}

impl Drop for DisplayTurn {
    fn drop(&mut self) {
        let mut d = self.downloading.lock().unwrap_or_else(|e| e.into_inner());
        // Only this download's own turn: a newer one may have taken over.
        if d.get(&self.device).is_some_and(|h| h.ptr_eq(&self.held)) {
            d.remove(&self.device);
        }
    }
}

/// What a display's download holds until it ends: one of the hub's download slots, and its turn.
struct DownloadPermit {
    _slot: OwnedSemaphorePermit,
    _turn: DisplayTurn,
}

/// What a running download holds, shared by its body and its watchdog. Emptied when the download
/// ends, by whichever of the two gets there first, or when a newer download from the same display
/// takes over.
#[derive(Default)]
pub(super) struct Held {
    permit: Option<DownloadPermit>,
    /// The connection the download is on, to close if it runs past its time limit.
    close: Option<CloseConnection>,
    /// Dropped as the download ends, which tells the watchdog to stop waiting.
    done: Option<tokio::sync::oneshot::Sender<()>>,
    /// A newer download from the same display has taken over.
    superseded: bool,
}

impl Held {
    fn release(&mut self) {
        self.permit.take();
        self.close.take();
        self.done.take();
    }

    /// Ends this download for a newer one from the same display: closes its connection and lets go
    /// of its slot and turn.
    fn supersede(&mut self) {
        self.superseded = true;
        if let Some(c) = &self.close {
            c.close();
        }
        self.release();
    }
}

/// A stored package's file, checked to hold at least `len` bytes.
async fn open_package(hub: &AppState, pkg: &PackageRow, len: u64) -> Result<tokio::fs::File, ApiError> {
    let path = shelf::package_path(&hub.firmware_dir(), &pkg.id);
    let file = match tokio::fs::File::open(&path).await {
        Ok(f) => f,
        Err(e) => {
            tracing::error!(error = %e, package = %pkg.id, "a stored package's file can't be opened");
            return Err(ApiError::unavailable(
                "The hub couldn't read that package. Check the hub's log.",
            ));
        }
    };
    match file.metadata().await {
        Ok(m) if m.len() >= len => Ok(file),
        _ => {
            tracing::error!(package = %pkg.id, "a stored package's file is shorter than its row says");
            Err(ApiError::unavailable(
                "The hub couldn't read that package. Check the hub's log.",
            ))
        }
    }
}

/// The first `len` bytes of an opened package as the response, with an exact `Content-Length`, no
/// compression and no chunking. A display's download holds its
/// `permit` until the body ends, or until the time limit passes: then a watchdog closes the
/// connection too, since a display that has stopped reading is never asked for more of the body,
/// and only dropping the connection frees the socket and the file.
fn stream_file(hub: &AppState, file: tokio::fs::File, len: u64, held: Option<Arc<Mutex<Held>>>) -> Response {
    let limit = hub.firmware_time_limits().download;
    // Only a display's download has anything for a watchdog to release.
    if let Some(held) = &held {
        let (done, finished) = tokio::sync::oneshot::channel::<()>();
        held.lock().unwrap_or_else(|e| e.into_inner()).done = Some(done);
        let watchdog = held.clone();
        tokio::spawn(async move {
            tokio::select! {
                () = tokio::time::sleep(limit) => {
                    let mut h = watchdog.lock().unwrap_or_else(|e| e.into_inner());
                    if h.permit.is_some() {
                        tracing::warn!("a firmware download ran past its time limit; closing its connection");
                        if let Some(c) = &h.close {
                            c.close();
                        }
                    }
                    h.release();
                }
                _ = finished => {}
            }
        });
    }
    let body = FileBody {
        file,
        remaining: len,
        deadline: tokio::time::Instant::now() + limit,
        held,
        buf: BytesMut::new(),
    };
    (
        [
            (header::CONTENT_TYPE, "application/octet-stream".to_string()),
            (header::CONTENT_LENGTH, len.to_string()),
        ],
        Body::new(body),
    )
        .into_response()
}

/// A file's first bytes as a body of exactly known length, read a piece at a time. Each piece is
/// read straight into the buffer and handed over as its frame without a copy; the buffer's memory
/// is used again once the connection has sent that frame and let it go.
struct FileBody {
    file: tokio::fs::File,
    remaining: u64,
    deadline: tokio::time::Instant,
    held: Option<Arc<Mutex<Held>>>,
    buf: BytesMut,
}

impl FileBody {
    fn release(&self) {
        if let Some(h) = &self.held {
            h.lock().unwrap_or_else(|e| e.into_inner()).release();
        }
    }
}

impl Drop for FileBody {
    fn drop(&mut self) {
        self.release();
    }
}

impl HttpBody for FileBody {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<hyper::body::Frame<Bytes>, std::io::Error>>> {
        let this = self.get_mut();
        if this.remaining == 0 {
            this.release();
            return Poll::Ready(None);
        }
        if tokio::time::Instant::now() >= this.deadline {
            this.release();
            return Poll::Ready(Some(Err(std::io::ErrorKind::TimedOut.into())));
        }
        let want = this.remaining.min(shelf::STREAM_CHUNK_BYTES as u64) as usize;
        // Takes back the memory of the last frame if it has been sent, or else finds more.
        // Zero-filled rather than read into uninitialised memory, so no `unsafe` is needed.
        this.buf.resize(want, 0);
        let mut read = ReadBuf::new(&mut this.buf[..want]);
        match Pin::new(&mut this.file).poll_read(cx, &mut read) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(e)) => Poll::Ready(Some(Err(e))),
            Poll::Ready(Ok(())) => {
                let n = read.filled().len();
                if n == 0 {
                    return Poll::Ready(Some(Err(std::io::ErrorKind::UnexpectedEof.into())));
                }
                this.buf.truncate(n);
                let chunk = this.buf.split().freeze();
                this.remaining -= n as u64;
                if this.remaining == 0 {
                    // Every byte is handed over: the slot is free for the next download now, not
                    // only once the connection gets round to dropping the body.
                    this.release();
                }
                Poll::Ready(Some(Ok(hyper::body::Frame::data(chunk))))
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.remaining == 0
    }

    fn size_hint(&self) -> hyper::body::SizeHint {
        hyper::body::SizeHint::with_exact(self.remaining)
    }
}

/// `POST /v1/device/firmware/report`.
pub(super) async fn device_report(
    State(hub): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    ApiJson(report): ApiJson<FirmwareReport>,
) -> Result<StatusCode, ApiError> {
    check_protocol(&headers)?;
    let device = authenticate(&hub, &headers, peer.ip(), Role::Device).await?;
    let problems = report.problems();
    if !problems.is_empty() {
        return Err(ApiError::bad_request(problems.join("; ")));
    }
    note_seen(&hub, &device, None);
    let now = hub.clock.now();
    let d = device.clone();
    let reported = report.firmware_version.clone();
    let outcome = db(&hub, move |s| {
        let device = d;
        let outcome = s.record_report(
            &device,
            &report.offer_id,
            report.state,
            report.reason,
            report.firmware_version.as_deref(),
            now,
        )?;
        if let (ReportOutcome::Recorded { settled: true, .. }, Some(v)) =
            (&outcome, report.firmware_version.as_deref())
        {
            // A final report says what the display runs now (the new version once installed, the
            // old one after a rollback or failure): the app needn't wait for its next heartbeat.
            s.set_reported_firmware_version(&device, v)?;
        }
        if let ReportOutcome::Recorded { offer, settled: true } = &outcome {
            s.audit(
                now,
                "firmware_result",
                &format!(
                    "{}: {} {} on {device}{}",
                    offer.id,
                    offer.state,
                    offer.firmware_version,
                    offer
                        .reason
                        .as_deref()
                        .map(|r| format!(" ({r})"))
                        .unwrap_or_default()
                ),
            )?;
        }
        Ok(outcome)
    })
    .await?;
    if let (ReportOutcome::Recorded { settled: true, .. }, Some(v)) = (&outcome, reported) {
        // And in a buffered heartbeat, which the app reads first.
        if let Some(info) = hub.seen().get_mut(&device).and_then(|s| s.info.as_mut()) {
            info.firmware_version = v;
        }
    }
    match outcome {
        ReportOutcome::NoOffer => Err(ApiError::new(
            StatusCode::NOT_FOUND,
            ErrorCode::NotFound,
            "This display has no firmware offer with that id.",
        )),
        ReportOutcome::Recorded { .. } | ReportOutcome::Unchanged => Ok(StatusCode::NO_CONTENT),
    }
}

/// A heartbeat naming the version on offer counts as installed, if the report itself was lost.
pub(super) async fn check_installed(hub: &AppState, device: &str, version: &str) -> Result<(), ApiError> {
    let now = hub.clock.now();
    let (d, v) = (device.to_string(), version.to_string());
    db(hub, move |s| {
        if !s.open_offer_for_version(&d, &v, now)? {
            return Ok(());
        }
        if let Some(offer) = s.installed_by_heartbeat(&d, &v, now)? {
            s.audit(
                now,
                "firmware_result",
                &format!(
                    "{}: installed {} on {d} (seen in a heartbeat)",
                    offer.id, offer.firmware_version
                ),
            )?;
        }
        Ok(())
    })
    .await
}
