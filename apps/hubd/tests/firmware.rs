//! Signed firmware updates through the hub over real HTTPS on loopback (`docs/firmware-updates.md`,
//! "The hub's part"): uploads and their limits, offers and the courtesy checks, the download, the
//! report, and the publisher's narrow role. Every package is signed with a key made here, in memory.

use std::path::Path;
use std::sync::Arc;

use firmware_package::{build_and_sign, Built, Release, SigningKey, UsbLayout, UsbPartFile};
use hub_client::{ClientError, HubClient};
use hub_core::{Clock, FakeClock};
use hub_protocol::device::DeviceInfo;
use hub_protocol::firmware::{FirmwareInfo, FirmwareReport, ReportReason, ReportState};
use hub_protocol::pairing::PairingString;
use hub_protocol::ErrorCode;
use hubd::api::Hub;
use hubd::config::Config;
use tokio::sync::oneshot;

const NOW: &str = "2026-11-17T09:00:00Z";
const TYPE: &str = "llm-usage-display";
const REV: &str = "waveshare-amoled-1.8-v2";

struct Running {
    hub: Arc<Hub>,
    port: u16,
    stop: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<()>,
}

impl Running {
    async fn start(dir: &Path, clock: &FakeClock) -> Running {
        let config = Config {
            data_dir: dir.to_path_buf(),
            ..Config::default()
        };
        let hub = hubd::open_hub(config, Arc::new(clock.clone())).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = oneshot::channel();
        let h = hub.clone();
        let task = tokio::spawn(async move {
            hubd::serve_https(h, listener, async {
                let _ = rx.await;
            })
            .await
            .unwrap();
        });
        Running {
            hub,
            port,
            stop: Some(tx),
            task,
        }
    }

    fn client(&self) -> HubClient {
        HubClient::pinned("127.0.0.1", self.port, vec![self.hub.identity.spki_sha256]).unwrap()
    }

    async fn admin(&self) -> String {
        let raw = self.hub.new_admin_pairing("Test PC").unwrap();
        let secret = PairingString {
            host: "127.0.0.1".into(),
            port: self.port,
            spki_sha256: self.hub.identity.spki_sha256,
            secret: raw,
        }
        .secret_text();
        self.client().pair(&secret, "Test PC").await.unwrap().admin_token
    }

    /// Enrols a display that reports `firmware`.
    async fn display(
        &self,
        admin: &str,
        name: &str,
        rev: &str,
        firmware: Option<FirmwareInfo>,
    ) -> (String, String) {
        let grant = self.client().new_enrolment(admin, name).await.unwrap();
        let e = self
            .client()
            .enrol(
                &grant.secret,
                DeviceInfo {
                    device_type: TYPE.into(),
                    firmware_version: "1.0.0".into(),
                    hardware_revision: rev.into(),
                    protocol_version: 1,
                    firmware,
                },
            )
            .await
            .unwrap();
        (e.device_id, e.device_token)
    }

    async fn stop(mut self) {
        let _ = self.stop.take().unwrap().send(());
        self.task.await.unwrap();
    }
}

fn key() -> SigningKey {
    SigningKey::generate().unwrap().0
}

fn release(version: &str, counter: u32) -> Release {
    Release {
        device_type: TYPE.into(),
        hardware_revisions: vec![REV.into()],
        firmware_version: version.into(),
        security_counter: counter,
        summary: Some(format!("Release {version}")),
        key_set: None,
        usb_install: None,
    }
}

fn image(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

fn package(key: &SigningKey, version: &str, counter: u32) -> Built {
    build_and_sign(
        key,
        &release(version, counter),
        &image(5000, version.len() as u8 + counter as u8),
    )
    .unwrap()
}

/// What a display with `key` trusted reports.
fn trusting(key: &SigningKey, floor: u32, slot: u64) -> FirmwareInfo {
    FirmwareInfo {
        key_ids: vec![key.public_key().key_id().to_hex()],
        security_floor: floor,
        slot_bytes: slot,
    }
}

fn code(e: &ClientError) -> Option<ErrorCode> {
    e.code()
}

fn status(e: &ClientError) -> u16 {
    match e {
        ClientError::Status { status, .. } => *status,
        other => panic!("not a status: {other:?}"),
    }
}

fn files(dir: &Path) -> Vec<String> {
    let mut out: Vec<String> = std::fs::read_dir(dir.join("firmware"))
        .map(|r| {
            r.map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

fn audit(hub: &Hub) -> Vec<String> {
    hub.store
        .audit_events(500)
        .unwrap()
        .into_iter()
        .map(|(_, e, d)| format!("{e} {d}"))
        .collect()
}

#[tokio::test]
async fn uploads_are_checked_kept_once_and_refused_with_the_reason() {
    let dir = tempfile::tempdir().unwrap();
    let clock = FakeClock::parse(NOW);
    let r = Running::start(dir.path(), &clock).await;
    let admin = r.admin().await;
    let c = r.client();
    let k = key();
    let good = package(&k, "1.2.0", 3);

    let (view, created) = c.upload_firmware(&admin, good.bytes.clone()).await.unwrap();
    assert!(created);
    assert_eq!(view.package_id, good.checked.package_id.to_hex());
    assert_eq!(
        (
            view.device_type.as_str(),
            view.firmware_version.as_str(),
            view.security_counter
        ),
        (TYPE, "1.2.0", 3)
    );
    assert_eq!(view.signer_key_id, k.public_key().key_id().to_hex());
    assert_eq!(view.signer_fingerprint, k.public_key().fingerprint());
    assert_eq!(
        (view.size, view.update_size),
        (good.bytes.len() as u64, good.checked.update_form_len())
    );
    assert_eq!(view.uploaded_by_role, "admin");
    assert_eq!(view.uploaded_by_label.as_deref(), Some("Test PC"));
    assert!(!view.usb_install);
    let kept = dir
        .path()
        .join("firmware")
        .join(format!("{}.dhfw", view.package_id));
    assert_eq!(std::fs::read(&kept).unwrap(), good.bytes);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&kept).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let d = std::fs::metadata(dir.path().join("firmware")).unwrap();
        assert_eq!(d.permissions().mode() & 0o777, 0o700);
    }

    // The same file again is harmless: 200, and still one package.
    let (again, created) = c.upload_firmware(&admin, good.bytes.clone()).await.unwrap();
    assert!(!created);
    assert_eq!(again.package_id, view.package_id);
    assert_eq!(c.firmware_packages(&admin).await.unwrap().len(), 1);

    // Damaged packages are refused with the reason, and leave nothing behind.
    let mut bad_signature = good.bytes.clone();
    bad_signature[80] ^= 1;
    let mut bad_image = good.bytes.clone();
    *bad_image.last_mut().unwrap() ^= 1;
    let mut trailing = good.bytes.clone();
    trailing.push(0);
    let truncated = good.bytes[..good.bytes.len() - 1].to_vec();
    for (bytes, why) in [
        (bad_signature, "signature"),
        (bad_image, "image_sha256"),
        (trailing, "add up to"),
        (truncated, "add up to"),
        (b"DHFW".to_vec(), "at least"),
        (vec![0u8; 500], "header"),
    ] {
        let e = c.upload_firmware(&admin, bytes).await.unwrap_err();
        assert_eq!(
            (status(&e), code(&e)),
            (400, Some(ErrorCode::BadRequest)),
            "{why}"
        );
        assert!(e.to_string().contains(why), "{why}: {e}");
    }
    assert_eq!(files(dir.path()), [format!("{}.dhfw", view.package_id)]);

    // Too large is refused before the body is read.
    let huge = vec![0u8; (hub_protocol::limits::MAX_FIRMWARE_PACKAGE_BYTES + 1) as usize];
    match c.upload_firmware(&admin, huge).await {
        Err(e @ ClientError::Status { .. }) => {
            assert_eq!((status(&e), code(&e)), (413, Some(ErrorCode::PayloadTooLarge)))
        }
        // The hub may close the connection before the whole body is sent.
        Err(e) => assert!(e.is_connection(), "{e:?}"),
        Ok(_) => panic!("accepted a package over the limit"),
    }
    assert_eq!(files(dir.path()).len(), 1);

    let log = audit(&r.hub);
    assert!(log.iter().any(|l| l.starts_with("firmware_uploaded ")), "{log:?}");
    assert!(
        log.iter().any(|l| l.starts_with("firmware_upload_refused ")),
        "{log:?}"
    );

    // The whole file can be fetched back for a USB install, and deleted.
    assert_eq!(
        c.firmware_file(&admin, &view.package_id).await.unwrap(),
        good.bytes
    );
    c.delete_firmware(&admin, &view.package_id).await.unwrap();
    let e = c.delete_firmware(&admin, &view.package_id).await.unwrap_err();
    assert_eq!(status(&e), 404);
    assert!(files(dir.path()).is_empty());
    assert!(audit(&r.hub).iter().any(|l| l.starts_with("firmware_deleted ")));
    r.stop().await;
}

#[tokio::test]
async fn a_fourth_package_for_a_device_type_replaces_the_lowest_version_not_on_offer() {
    let dir = tempfile::tempdir().unwrap();
    let clock = FakeClock::parse(NOW);
    let r = Running::start(dir.path(), &clock).await;
    let admin = r.admin().await;
    let c = r.client();
    let k = key();
    let (desk, _) = r
        .display(&admin, "Desk", REV, Some(trusting(&k, 0, 1 << 20)))
        .await;
    let mut ids = Vec::new();
    for v in ["1.1.0", "1.0.0", "1.2.0"] {
        ids.push(
            c.upload_firmware(&admin, package(&k, v, 1).bytes)
                .await
                .unwrap()
                .0
                .package_id,
        );
    }
    // 1.0.0 is on offer, so 1.1.0 is the lowest version that can go.
    c.offer_firmware(&admin, &desk, &ids[1]).await.unwrap();
    let (fourth, _) = c
        .upload_firmware(&admin, package(&k, "1.3.0", 1).bytes)
        .await
        .unwrap();
    let mut left: Vec<String> = c
        .firmware_packages(&admin)
        .await
        .unwrap()
        .into_iter()
        .map(|p| p.firmware_version)
        .collect();
    left.sort();
    assert_eq!(left, ["1.0.0", "1.2.0", "1.3.0"]);
    assert!(!files(dir.path()).contains(&format!("{}.dhfw", ids[0])));
    assert_eq!(files(dir.path()).len(), 3);
    assert!(audit(&r.hub)
        .iter()
        .any(|l| l.starts_with(&format!("firmware_evicted {}", ids[0]))));

    // Another device type has a shelf of its own.
    let mut other = release("9.0.0", 1);
    other.device_type = "weather-display".into();
    let other = build_and_sign(&k, &other, &image(3000, 9)).unwrap();
    c.upload_firmware(&admin, other.bytes).await.unwrap();
    assert_eq!(c.firmware_packages(&admin).await.unwrap().len(), 4);

    // When every one is on offer, there is no room.
    let (a, _) = r.display(&admin, "A", REV, Some(trusting(&k, 0, 1 << 20))).await;
    let (b, _) = r.display(&admin, "B", REV, Some(trusting(&k, 0, 1 << 20))).await;
    let shelf = c.firmware_packages(&admin).await.unwrap();
    let id_of = |v: &str| {
        shelf
            .iter()
            .find(|p| p.firmware_version == v)
            .unwrap()
            .package_id
            .clone()
    };
    c.offer_firmware(&admin, &a, &id_of("1.2.0")).await.unwrap();
    c.offer_firmware(&admin, &b, &fourth.package_id).await.unwrap();
    let e = c
        .upload_firmware(&admin, package(&k, "1.4.0", 1).bytes)
        .await
        .unwrap_err();
    assert_eq!(code(&e), Some(ErrorCode::Conflict));
    assert!(e.to_string().contains("on offer"), "{e}");
    assert_eq!(files(dir.path()).len(), 4);
    r.stop().await;
}

#[tokio::test]
async fn offers_are_refused_when_the_display_would_refuse_them() {
    let dir = tempfile::tempdir().unwrap();
    let clock = FakeClock::parse(NOW);
    let r = Running::start(dir.path(), &clock).await;
    let admin = r.admin().await;
    let c = r.client();
    let maker = key();
    let stranger = key();
    let pkg = c
        .upload_firmware(&admin, package(&maker, "1.2.0", 3).bytes)
        .await
        .unwrap()
        .0;
    let size = 5000;

    let cases: Vec<(&str, &str, Option<FirmwareInfo>, &str)> = vec![
        ("Old firmware", REV, None, "hasn't said which firmware keys"),
        (
            "Other board",
            "another-board-v1",
            Some(trusting(&maker, 0, 1 << 20)),
            "hardware",
        ),
        (
            "Stranger's",
            REV,
            Some(trusting(&stranger, 0, 1 << 20)),
            "doesn't trust the key",
        ),
        (
            "No keys",
            REV,
            Some(FirmwareInfo {
                key_ids: vec![],
                security_floor: 0,
                slot_bytes: 1 << 20,
            }),
            "trusts no firmware keys",
        ),
        (
            "Floor too high",
            REV,
            Some(trusting(&maker, 4, 1 << 20)),
            "security floor is 4",
        ),
        (
            "Small partition",
            REV,
            Some(trusting(&maker, 0, size - 1)),
            "too big",
        ),
    ];
    for (name, rev, fw, why) in cases {
        let (id, _) = r.display(&admin, name, rev, fw).await;
        let e = c.offer_firmware(&admin, &id, &pkg.package_id).await.unwrap_err();
        assert_eq!((status(&e), code(&e)), (409, Some(ErrorCode::Conflict)), "{name}");
        assert!(e.to_string().contains(why), "{name}: {e}");
    }
    // The wrong device type.
    let mut other = release("1.2.0", 3);
    other.device_type = "weather-display".into();
    let other = c
        .upload_firmware(
            &admin,
            build_and_sign(&maker, &other, &image(100, 1)).unwrap().bytes,
        )
        .await
        .unwrap()
        .0;
    let (ok, _) = r
        .display(&admin, "Desk", REV, Some(trusting(&maker, 3, size)))
        .await;
    let e = c
        .offer_firmware(&admin, &ok, &other.package_id)
        .await
        .unwrap_err();
    assert!(e.to_string().contains("weather-display"), "{e}");
    // Unknown displays and packages.
    assert_eq!(
        status(
            &c.offer_firmware(&admin, "d-none", &pkg.package_id)
                .await
                .unwrap_err()
        ),
        404
    );
    assert_eq!(
        status(&c.offer_firmware(&admin, &ok, &"0".repeat(64)).await.unwrap_err()),
        404
    );
    assert_eq!(
        status(&c.offer_firmware(&admin, &ok, "nonsense").await.unwrap_err()),
        400
    );
    // At the floor, exactly fitting: offered.
    let offer = c.offer_firmware(&admin, &ok, &pkg.package_id).await.unwrap();
    assert_eq!(
        (offer.state.as_str(), offer.firmware_version.as_str()),
        ("offered", "1.2.0")
    );
    assert!(audit(&r.hub).iter().any(|l| l.starts_with("firmware_offered ")));
    r.stop().await;
}

#[tokio::test]
async fn one_open_offer_shows_in_the_snapshot_until_it_ends() {
    let dir = tempfile::tempdir().unwrap();
    let clock = FakeClock::parse(NOW);
    let r = Running::start(dir.path(), &clock).await;
    let admin = r.admin().await;
    let c = r.client();
    let k = key();
    let (desk, token) = r
        .display(&admin, "Desk", REV, Some(trusting(&k, 0, 1 << 20)))
        .await;
    let p1 = c
        .upload_firmware(&admin, package(&k, "1.1.0", 1).bytes)
        .await
        .unwrap()
        .0;
    let p2 = c
        .upload_firmware(&admin, package(&k, "1.2.0", 1).bytes)
        .await
        .unwrap()
        .0;

    assert_eq!(
        c.device_snapshot(Some(&token)).await.unwrap().firmware_update,
        None
    );
    let first = c.offer_firmware(&admin, &desk, &p1.package_id).await.unwrap();
    let snap = c.device_snapshot(Some(&token)).await.unwrap();
    let shown = snap.firmware_update.clone().unwrap();
    assert_eq!(
        (shown.offer_id.as_str(), shown.package_id.as_str(), shown.size),
        (first.offer_id.as_str(), p1.package_id.as_str(), p1.update_size)
    );
    snap.validate().unwrap();

    // Pressing Update again replaces it.
    let second = c.offer_firmware(&admin, &desk, &p2.package_id).await.unwrap();
    assert_ne!(first.offer_id, second.offer_id);
    let shown = c
        .device_snapshot(Some(&token))
        .await
        .unwrap()
        .firmware_update
        .unwrap();
    assert_eq!(shown.offer_id, second.offer_id);
    let displays = c.displays(&admin).await.unwrap();
    let fw = displays[0].firmware.clone().unwrap();
    assert_eq!(fw.offer.unwrap().offer_id, second.offer_id);
    assert!(!fw.auto_update);
    assert_eq!(fw.reported.unwrap().slot_bytes, 1 << 20);
    assert_eq!(fw.available_package_id.as_deref(), Some(p2.package_id.as_str()));
    // The list says which displays a package is on offer to.
    let shelf = c.firmware_packages(&admin).await.unwrap();
    let on = |id: &str| {
        shelf
            .iter()
            .find(|p| p.package_id == id)
            .unwrap()
            .offered_to
            .clone()
    };
    assert_eq!(
        (on(&p1.package_id), on(&p2.package_id)),
        (vec![], vec![desk.clone()])
    );

    // Cancelled.
    c.cancel_firmware_offer(&admin, &desk).await.unwrap();
    assert_eq!(
        status(&c.cancel_firmware_offer(&admin, &desk).await.unwrap_err()),
        404
    );
    assert_eq!(
        c.device_snapshot(Some(&token)).await.unwrap().firmware_update,
        None
    );
    let fw = c.displays(&admin).await.unwrap()[0].firmware.clone().unwrap();
    let last = fw.offer.unwrap();
    assert_eq!(
        (last.state.as_str(), last.reason.as_deref()),
        ("cancelled", Some("by_admin"))
    );
    assert!(audit(&r.hub)
        .iter()
        .any(|l| l.starts_with("firmware_offer_cancelled ")));

    // Expired after 24 hours.
    c.offer_firmware(&admin, &desk, &p1.package_id).await.unwrap();
    clock.advance(hub_protocol::limits::FIRMWARE_OFFER_LIFETIME_SECONDS - 1);
    assert!(c
        .device_snapshot(Some(&token))
        .await
        .unwrap()
        .firmware_update
        .is_some());
    clock.advance(1);
    assert_eq!(
        c.device_snapshot(Some(&token)).await.unwrap().firmware_update,
        None
    );
    let e = c.firmware_download(&token, &p1.package_id).await.unwrap_err();
    assert_eq!(code(&e), Some(ErrorCode::NotFound));
    let fw = c.displays(&admin).await.unwrap()[0].firmware.clone().unwrap();
    assert_eq!(fw.offer.unwrap().state, "expired");

    // Deleting the package cancels its offer.
    c.offer_firmware(&admin, &desk, &p1.package_id).await.unwrap();
    c.delete_firmware(&admin, &p1.package_id).await.unwrap();
    assert_eq!(
        c.device_snapshot(Some(&token)).await.unwrap().firmware_update,
        None
    );
    let fw = c.displays(&admin).await.unwrap()[0].firmware.clone().unwrap();
    assert_eq!(fw.offer.unwrap().reason.as_deref(), Some("package_deleted"));

    // Removing the display cancels its offer.
    c.offer_firmware(&admin, &desk, &p2.package_id).await.unwrap();
    c.revoke_display(&admin, &desk).await.unwrap();
    assert!(r.hub.store.open_offer(&desk, clock.now()).unwrap().is_none());
    let latest = r.hub.store.latest_offers(clock.now()).unwrap();
    assert_eq!(latest[&desk].reason.as_deref(), Some("display_removed"));
    assert!(c.firmware_packages(&admin).await.unwrap()[0]
        .offered_to
        .is_empty());
    r.stop().await;
}

/// A package with USB-install parts, so the update form is shorter than the file.
fn with_usb_parts(key: &SigningKey) -> Built {
    let mut rel = release("1.5.0", 2);
    rel.usb_install = Some(UsbLayout {
        chip: "esp32s3".into(),
        min_flash_bytes: 16 * 1024 * 1024,
        app_offset: 0x10000,
        parts: vec![
            UsbPartFile {
                name: "bootloader".into(),
                offset: 0,
                bytes: image(2000, 1),
            },
            UsbPartFile {
                name: "partition-table".into(),
                offset: 0x8000,
                bytes: image(300, 2),
            },
        ],
    });
    // Large enough to pass through the hub in many pieces.
    build_and_sign(key, &rel, &image(3 * 1024 * 1024 + 17, 7)).unwrap()
}

#[tokio::test]
async fn the_download_is_the_update_form_only_for_an_open_offer_within_the_limits() {
    let dir = tempfile::tempdir().unwrap();
    let clock = FakeClock::parse(NOW);
    let r = Running::start(dir.path(), &clock).await;
    let admin = r.admin().await;
    let c = r.client();
    let k = key();
    let built = with_usb_parts(&k);
    let form = built.checked.update_form_len() as usize;
    assert!(form < built.bytes.len());
    let pkg = c.upload_firmware(&admin, built.bytes.clone()).await.unwrap().0;
    assert!(pkg.usb_install);
    let (desk, token) = r
        .display(&admin, "Desk", REV, Some(trusting(&k, 0, 4 << 20)))
        .await;
    let (_, other_token) = r
        .display(&admin, "Other", REV, Some(trusting(&k, 0, 4 << 20)))
        .await;

    // No offer: nothing, for this display or any other.
    let e = c.firmware_download(&token, &pkg.package_id).await.unwrap_err();
    assert_eq!((status(&e), code(&e)), (404, Some(ErrorCode::NotFound)));
    c.offer_firmware(&admin, &desk, &pkg.package_id).await.unwrap();
    let e = c
        .firmware_download(&other_token, &pkg.package_id)
        .await
        .unwrap_err();
    assert_eq!(code(&e), Some(ErrorCode::NotFound));
    assert_eq!(
        status(&c.firmware_download(&token, &"0".repeat(64)).await.unwrap_err()),
        404
    );
    assert_eq!(status(&c.firmware_download(&token, "x").await.unwrap_err()), 404);
    // An admin token is no display's.
    assert_eq!(
        status(&c.firmware_download(&admin, &pkg.package_id).await.unwrap_err()),
        403
    );

    let got = c.firmware_download(&token, &pkg.package_id).await.unwrap();
    assert_eq!(got.bytes, built.bytes[..form]);
    assert_eq!(got.content_length, Some(form as u64));
    assert_eq!(got.transfer_encoding, None);
    assert_eq!(got.content_encoding, None);
    assert_eq!(got.content_type.as_deref(), Some("application/octet-stream"));
    // The update form a display checks is exactly what was signed.
    let shown = c
        .device_snapshot(Some(&token))
        .await
        .unwrap()
        .firmware_update
        .unwrap();
    assert_eq!(shown.size, form as u64);

    // A Range request is ignored: the whole update form, from the start.
    let raw = reqwest::Client::builder()
        .use_preconfigured_tls(hub_client::pin::client_config(vec![r.hub.identity.spki_sha256]))
        .no_proxy()
        .build()
        .unwrap();
    let resp = raw
        .get(format!(
            "https://127.0.0.1:{}/v1/device/firmware/{}",
            r.port, pkg.package_id
        ))
        .bearer_auth(&token)
        .header("range", "bytes=100-")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.bytes().await.unwrap().len(), form);

    // At most two downloads at once across the hub.
    let held = r.hub.download_slots.clone().try_acquire_many_owned(2).unwrap();
    let e = c.firmware_download(&token, &pkg.package_id).await.unwrap_err();
    assert_eq!((status(&e), code(&e)), (503, Some(ErrorCode::Unavailable)));
    drop(held);
    // And six starts per display per hour: three so far (one answered, one with Range, one refused).
    for _ in 0..3 {
        c.firmware_download(&token, &pkg.package_id).await.unwrap();
    }
    let e = c.firmware_download(&token, &pkg.package_id).await.unwrap_err();
    assert_eq!((status(&e), code(&e)), (429, Some(ErrorCode::RateLimited)));
    // Other displays aren't affected.
    let (other, other_token) = r
        .display(&admin, "Third", REV, Some(trusting(&k, 0, 4 << 20)))
        .await;
    c.offer_firmware(&admin, &other, &pkg.package_id).await.unwrap();
    c.firmware_download(&other_token, &pkg.package_id).await.unwrap();
    // Every slot was given back once the bodies ended.
    assert_eq!(
        r.hub.download_slots.available_permits(),
        hub_protocol::limits::MAX_CONCURRENT_FIRMWARE_DOWNLOADS
    );
    r.stop().await;
}

fn report(offer: &str, state: ReportState, reason: Option<ReportReason>, version: &str) -> FirmwareReport {
    FirmwareReport {
        offer_id: offer.into(),
        state,
        reason,
        firmware_version: Some(version.into()),
    }
}

#[tokio::test]
async fn reports_settle_offers_and_a_heartbeat_with_the_new_version_counts_as_installed() {
    let dir = tempfile::tempdir().unwrap();
    let clock = FakeClock::parse(NOW);
    let r = Running::start(dir.path(), &clock).await;
    let admin = r.admin().await;
    let c = r.client();
    let k = key();
    let pkg = c
        .upload_firmware(&admin, package(&k, "1.2.0", 1).bytes)
        .await
        .unwrap()
        .0;
    let (desk, token) = r
        .display(&admin, "Desk", REV, Some(trusting(&k, 0, 1 << 20)))
        .await;
    let state = |c: HubClient, admin: String| async move {
        let fw = c.displays(&admin).await.unwrap()[0].firmware.clone().unwrap();
        let o = fw.offer.unwrap();
        (o.state, o.reason, o.reported_version)
    };

    let offer = c.offer_firmware(&admin, &desk, &pkg.package_id).await.unwrap();
    c.firmware_report(
        &token,
        &report(&offer.offer_id, ReportState::Downloading, None, "1.0.0"),
    )
    .await
    .unwrap();
    assert_eq!(
        state(c.clone(), admin.clone()).await,
        ("downloading".into(), None, Some("1.0.0".into()))
    );
    assert!(c
        .device_snapshot(Some(&token))
        .await
        .unwrap()
        .firmware_update
        .is_some());
    c.firmware_report(
        &token,
        &report(
            &offer.offer_id,
            ReportState::Refused,
            Some(ReportReason::Downgrade),
            "1.0.0",
        ),
    )
    .await
    .unwrap();
    assert_eq!(
        state(c.clone(), admin.clone()).await,
        ("refused".into(), Some("downgrade".into()), Some("1.0.0".into()))
    );
    assert_eq!(
        c.device_snapshot(Some(&token)).await.unwrap().firmware_update,
        None
    );
    // A repeated final report is answered, and changes nothing.
    c.firmware_report(
        &token,
        &report(&offer.offer_id, ReportState::Installed, None, "1.2.0"),
    )
    .await
    .unwrap();
    assert_eq!(state(c.clone(), admin.clone()).await.0, "refused");
    assert!(audit(&r.hub)
        .iter()
        .any(|l| l.starts_with("firmware_result ") && l.contains("refused")));

    // Unknown offers and malformed reports.
    let e = c
        .firmware_report(
            &token,
            &report(&"0".repeat(32), ReportState::Installed, None, "1.2.0"),
        )
        .await
        .unwrap_err();
    assert_eq!(status(&e), 404);
    let e = c
        .firmware_report(
            &token,
            &report("not-an-offer", ReportState::Installed, None, "1.2.0"),
        )
        .await
        .unwrap_err();
    assert_eq!(status(&e), 400);
    // A state from the future is a failure.
    let offer = c.offer_firmware(&admin, &desk, &pkg.package_id).await.unwrap();
    c.firmware_report(
        &token,
        &report(&offer.offer_id, ReportState::Unrecognised, None, "1.0.0"),
    )
    .await
    .unwrap();
    assert_eq!(state(c.clone(), admin.clone()).await.0, "failed");

    // Installed and rolled back, by report.
    for (s, shown) in [
        (ReportState::RolledBack, "rolled_back"),
        (ReportState::Installed, "installed"),
    ] {
        let offer = c.offer_firmware(&admin, &desk, &pkg.package_id).await.unwrap();
        c.firmware_report(&token, &report(&offer.offer_id, s, None, "1.2.0"))
            .await
            .unwrap();
        assert_eq!(state(c.clone(), admin.clone()).await.0, shown);
    }

    // A heartbeat naming the offered version, when the report was lost.
    let (shelf_desk, shelf_token) = r
        .display(&admin, "Shelf", REV, Some(trusting(&k, 0, 1 << 20)))
        .await;
    let offer = c
        .offer_firmware(&admin, &shelf_desk, &pkg.package_id)
        .await
        .unwrap();
    let beat = |v: &str| DeviceInfo {
        device_type: TYPE.into(),
        firmware_version: v.into(),
        hardware_revision: REV.into(),
        protocol_version: 1,
        firmware: Some(trusting(&k, 1, 1 << 20)),
    };
    c.heartbeat(&shelf_token, &beat("1.0.0")).await.unwrap();
    assert!(c
        .device_snapshot(Some(&shelf_token))
        .await
        .unwrap()
        .firmware_update
        .is_some());
    c.heartbeat(&shelf_token, &beat("1.2.0")).await.unwrap();
    assert_eq!(
        c.device_snapshot(Some(&shelf_token))
            .await
            .unwrap()
            .firmware_update,
        None
    );
    let shown = c
        .displays(&admin)
        .await
        .unwrap()
        .into_iter()
        .find(|d| d.id == shelf_desk)
        .unwrap()
        .firmware
        .unwrap();
    let o = shown.offer.unwrap();
    assert_eq!(
        (o.offer_id.as_str(), o.state.as_str(), o.reason.as_deref()),
        (offer.offer_id.as_str(), "installed", Some("seen_in_heartbeat"))
    );
    assert!(audit(&r.hub).iter().any(|l| l.contains("seen in a heartbeat")));

    // A heartbeat whose firmware member breaks the rules is still a heartbeat.
    let mut odd = beat("1.2.0");
    odd.firmware.as_mut().unwrap().key_ids = vec!["NOT HEX".into()];
    c.heartbeat(&shelf_token, &odd).await.unwrap();
    r.stop().await;
}

#[tokio::test]
async fn a_publisher_can_upload_and_list_and_nothing_else() {
    let dir = tempfile::tempdir().unwrap();
    let clock = FakeClock::parse(NOW);
    let r = Running::start(dir.path(), &clock).await;
    let admin = r.admin().await;
    let c = r.client();
    let invite = c.new_publisher(&admin, "Maker's laptop").await.unwrap();
    let paired = c.publisher_pair(&invite.secret).await.unwrap();
    assert!(paired.publisher_token.starts_with("dhp_"));
    let again = c.publisher_pair(&invite.secret).await.unwrap_err();
    assert_eq!(code(&again), Some(ErrorCode::EnrolmentUsed));
    let p = paired.publisher_token;

    let k = key();
    let (view, created) = c
        .publisher_upload(&p, package(&k, "2.0.0", 1).bytes)
        .await
        .unwrap();
    assert!(created);
    assert_eq!(
        (
            view.uploaded_by_role.as_str(),
            view.uploaded_by.as_str(),
            view.uploaded_by_label.as_deref()
        ),
        ("publisher", paired.publisher_id.as_str(), Some("Maker's laptop"))
    );
    let (desk, _) = r
        .display(&admin, "Desk", REV, Some(trusting(&k, 0, 1 << 20)))
        .await;
    c.offer_firmware(&admin, &desk, &view.package_id).await.unwrap();
    // The publisher sees the shelf, but not which displays anything is offered to.
    let seen = c.publisher_packages(&p).await.unwrap();
    assert_eq!(seen.len(), 1);
    assert!(seen[0].offered_to.is_empty());
    assert_eq!(
        c.firmware_packages(&admin).await.unwrap()[0].offered_to,
        [desk.as_str()]
    );

    // Everything else is forbidden.
    let forbidden = |e: ClientError| assert_eq!((status(&e), code(&e)), (403, Some(ErrorCode::Forbidden)));
    forbidden(c.offer_firmware(&p, &desk, &view.package_id).await.unwrap_err());
    forbidden(c.cancel_firmware_offer(&p, &desk).await.unwrap_err());
    forbidden(c.delete_firmware(&p, &view.package_id).await.unwrap_err());
    forbidden(c.firmware_packages(&p).await.unwrap_err());
    forbidden(
        c.upload_firmware(&p, package(&k, "2.0.1", 1).bytes)
            .await
            .unwrap_err(),
    );
    forbidden(c.firmware_file(&p, &view.package_id).await.unwrap_err());
    forbidden(c.displays(&p).await.unwrap_err());
    forbidden(c.status(&p).await.unwrap_err());
    forbidden(c.firmware_signers(&p).await.unwrap_err());
    forbidden(c.publishers(&p).await.unwrap_err());
    forbidden(c.new_publisher(&p, "another").await.unwrap_err());
    forbidden(c.pages(&p).await.unwrap_err());
    forbidden(c.device_snapshot(Some(&p)).await.unwrap_err());
    // And an admin's token isn't a publisher's.
    forbidden(c.publisher_packages(&admin).await.unwrap_err());

    // The app lists and revokes publishers.
    let listed = c.publishers(&admin).await.unwrap();
    assert_eq!((listed.len(), listed[0].revoked), (1, false));
    c.revoke_publisher(&admin, &paired.publisher_id).await.unwrap();
    assert_eq!(
        status(
            &c.revoke_publisher(&admin, &paired.publisher_id)
                .await
                .unwrap_err()
        ),
        404
    );
    let e = c.publisher_packages(&p).await.unwrap_err();
    assert_eq!(code(&e), Some(ErrorCode::TokenRevoked));
    assert!(c.publishers(&admin).await.unwrap()[0].revoked);
    let log = audit(&r.hub);
    for event in [
        "publisher_invited ",
        "publisher_paired ",
        "publisher_revoked ",
        "ALARM_publisher_code_reused ",
    ] {
        assert!(log.iter().any(|l| l.starts_with(event)), "{event}: {log:?}");
    }
    r.stop().await;
}

#[tokio::test]
async fn signers_can_be_labelled_and_the_label_shows_on_their_packages() {
    let dir = tempfile::tempdir().unwrap();
    let clock = FakeClock::parse(NOW);
    let r = Running::start(dir.path(), &clock).await;
    let admin = r.admin().await;
    let c = r.client();
    let k = key();
    let key_id = k.public_key().key_id().to_hex();
    c.upload_firmware(&admin, package(&k, "1.0.0", 0).bytes)
        .await
        .unwrap();
    assert_eq!(c.firmware_packages(&admin).await.unwrap()[0].signer_label, None);
    let v = c
        .set_signer_label(&admin, &key_id, "  Kiwi   Displays ")
        .await
        .unwrap();
    assert_eq!(
        (v.label.as_str(), v.fingerprint.as_str()),
        ("Kiwi Displays", k.public_key().fingerprint().as_str())
    );
    assert_eq!(
        c.firmware_packages(&admin).await.unwrap()[0]
            .signer_label
            .as_deref(),
        Some("Kiwi Displays")
    );
    assert_eq!(c.firmware_signers(&admin).await.unwrap().len(), 1);
    assert_eq!(
        status(&c.set_signer_label(&admin, "nothex", "x").await.unwrap_err()),
        400
    );
    assert_eq!(
        status(&c.set_signer_label(&admin, &key_id, "\u{7}").await.unwrap_err()),
        400
    );
    c.remove_signer_label(&admin, &key_id).await.unwrap();
    assert_eq!(
        status(&c.remove_signer_label(&admin, &key_id).await.unwrap_err()),
        404
    );
    assert!(c.firmware_signers(&admin).await.unwrap().is_empty());
    r.stop().await;
}

#[tokio::test]
async fn at_start_up_rows_without_files_and_stray_files_are_tidied() {
    let dir = tempfile::tempdir().unwrap();
    let clock = FakeClock::parse(NOW);
    let k = key();
    let (kept, lost) = {
        let r = Running::start(dir.path(), &clock).await;
        let admin = r.admin().await;
        let c = r.client();
        let kept = c
            .upload_firmware(&admin, package(&k, "1.0.0", 0).bytes)
            .await
            .unwrap()
            .0;
        let lost = c
            .upload_firmware(&admin, package(&k, "1.1.0", 0).bytes)
            .await
            .unwrap()
            .0;
        r.stop().await;
        (kept.package_id, lost.package_id)
    };
    let fw = dir.path().join("firmware");
    std::fs::remove_file(fw.join(format!("{lost}.dhfw"))).unwrap();
    std::fs::write(fw.join(".upload-0123456789abcdef.tmp"), b"half").unwrap();
    std::fs::write(fw.join(format!("{}.dhfw", "e".repeat(64))), b"orphan").unwrap();
    std::fs::write(fw.join("notes.txt"), b"not ours").unwrap();

    let r = Running::start(dir.path(), &clock).await;
    let admin = r.admin().await;
    let ids: Vec<String> = r
        .client()
        .firmware_packages(&admin)
        .await
        .unwrap()
        .into_iter()
        .map(|p| p.package_id)
        .collect();
    assert_eq!(ids, [kept.as_str()]);
    assert_eq!(
        files(dir.path()),
        [format!("{kept}.dhfw"), "notes.txt".to_string()]
    );
    r.stop().await;
}

/// `hubd pair-publisher --local` prints a string the publisher pairs with.
#[tokio::test(flavor = "multi_thread")]
async fn pair_publisher_prints_a_string_that_pairs_once() {
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("hub");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(data.join("hubd.toml"), "").unwrap();
    // The command line uses the system clock, so the hub does too.
    let clock = FakeClock::at(hub_core::SystemClock.now());
    let r = Running::start(&data, &clock).await;
    let d = data.clone();
    let out = tokio::task::spawn_blocking(move || {
        std::process::Command::new(env!("CARGO_BIN_EXE_hubd"))
            .args(["pair-publisher", "--local", "--label", "CI machine"])
            .arg("--data-dir")
            .arg(&d)
            .arg("--config")
            .arg(d.join("hubd.toml"))
            .env_remove("HUBD_LOG")
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let line = text
        .split_whitespace()
        .find(|w| w.starts_with("dh1."))
        .expect("a publisher string");
    let pairing = PairingString::decode(line).unwrap();
    assert_eq!(
        (pairing.host.as_str(), pairing.spki_sha256),
        ("127.0.0.1", r.hub.identity.spki_sha256)
    );
    let paired = r.client().publisher_pair(&pairing.secret_text()).await.unwrap();
    let admin = r.admin().await;
    let listed = r.client().publishers(&admin).await.unwrap();
    assert_eq!(
        (listed[0].id.as_str(), listed[0].label.as_str()),
        (paired.publisher_id.as_str(), "CI machine")
    );
    r.stop().await;
}
