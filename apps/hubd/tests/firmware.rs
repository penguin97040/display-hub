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
use hubd::firmware::FirmwareTimeLimits;
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
        Self::start_with(dir, clock, FirmwareTimeLimits::default()).await
    }

    /// A hub whose firmware transfers have other time limits.
    async fn start_with(dir: &Path, clock: &FakeClock, limits: FirmwareTimeLimits) -> Running {
        let config = Config {
            data_dir: dir.to_path_buf(),
            ..Config::default()
        };
        let hub = hubd::open_hub_with_firmware_time_limits(config, Arc::new(clock.clone()), limits).unwrap();
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
        attempts_left: None,
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
                attempts_left: None,
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
async fn a_package_that_drops_a_trusted_key_is_only_offered_with_a_higher_counter() {
    let dir = tempfile::tempdir().unwrap();
    let clock = FakeClock::parse(NOW);
    let r = Running::start(dir.path(), &clock).await;
    let admin = r.admin().await;
    let c = r.client();
    let maker = key();
    let retired = key();
    // A release that keeps only the maker's key, at counter 3.
    let mut rel = release("1.2.0", 3);
    rel.key_set = Some(vec![maker.public_key()]);
    let pkg = c
        .upload_firmware(
            &admin,
            build_and_sign(&maker, &rel, &image(5000, 9)).unwrap().bytes,
        )
        .await
        .unwrap()
        .0;
    let maker_id = maker.public_key().key_id().to_hex();
    assert_eq!(pkg.key_set, Some(vec![maker_id.clone()]));
    let listed = c.firmware_packages(&admin).await.unwrap();
    assert_eq!(listed[0].key_set, Some(vec![maker_id.clone()]));

    let both = |floor| FirmwareInfo {
        key_ids: vec![maker_id.clone(), retired.public_key().key_id().to_hex()],
        security_floor: floor,
        slot_bytes: 1 << 20,
        attempts_left: None,
    };
    // At the display's floor, dropping a key it trusts is a downgrade: not offered, not available.
    let (at_floor, _) = r.display(&admin, "At floor", REV, Some(both(3))).await;
    let e = c
        .offer_firmware(&admin, &at_floor, &pkg.package_id)
        .await
        .unwrap_err();
    assert_eq!((status(&e), code(&e)), (409, Some(ErrorCode::Conflict)));
    assert!(e.to_string().contains("stop the display trusting"), "{e}");
    // Above it, the key is retired as the maker meant.
    let (below, _) = r.display(&admin, "Below", REV, Some(both(2))).await;
    let views = c.displays(&admin).await.unwrap();
    let available = |id: &str| {
        views
            .iter()
            .find(|d| d.id == id)
            .and_then(|d| d.firmware.as_ref())
            .and_then(|f| f.available_package_id.clone())
    };
    assert_eq!(available(&at_floor), None);
    assert_eq!(available(&below), Some(pkg.package_id.clone()));
    c.offer_firmware(&admin, &below, &pkg.package_id).await.unwrap();

    // The key_set is kept with the package's row: a file that can't be read meanwhile doesn't make
    // the package look like one without a key_set.
    std::fs::write(
        dir.path()
            .join("firmware")
            .join(format!("{}.dhfw", pkg.package_id)),
        b"damaged",
    )
    .unwrap();
    let e = c
        .offer_firmware(&admin, &at_floor, &pkg.package_id)
        .await
        .unwrap_err();
    assert!(e.to_string().contains("stop the display trusting"), "{e}");
    let views = c.displays(&admin).await.unwrap();
    assert!(views
        .iter()
        .find(|d| d.id == at_floor)
        .and_then(|d| d.firmware.as_ref())
        .is_some_and(|f| f.available_package_id.is_none()));
    assert_eq!(
        c.firmware_packages(&admin).await.unwrap()[0].key_set,
        Some(vec![maker_id.clone()])
    );
    r.stop().await;
}

/// A package whose stored key_set can't be read stays on the shelf but is never offered or called
/// available, and nothing else stops working: the lists, an upload, and a display's snapshot.
#[tokio::test]
async fn a_package_whose_key_set_cant_be_read_is_listed_but_never_offered() {
    let dir = tempfile::tempdir().unwrap();
    let clock = FakeClock::parse(NOW);
    let r = Running::start(dir.path(), &clock).await;
    let admin = r.admin().await;
    let c = r.client();
    let k = key();
    let (desk, token) = r
        .display(&admin, "Desk", REV, Some(trusting(&k, 0, 1 << 20)))
        .await;
    let (hall, _) = r
        .display(&admin, "Hall", REV, Some(trusting(&k, 0, 1 << 20)))
        .await;
    let older = c
        .upload_firmware(&admin, package(&k, "1.1.0", 1).bytes)
        .await
        .unwrap()
        .0;
    let mut rel = release("1.2.0", 1);
    rel.key_set = Some(vec![k.public_key()]);
    let built = build_and_sign(&k, &rel, &image(5000, 7)).unwrap();
    let newer = c.upload_firmware(&admin, built.bytes.clone()).await.unwrap().0;
    let offer = c.offer_firmware(&admin, &desk, &newer.package_id).await.unwrap();

    rusqlite::Connection::open(dir.path().join("hub.db"))
        .unwrap()
        .execute(
            "UPDATE firmware_packages SET key_set = 'not json' WHERE id = ?1",
            [&newer.package_id],
        )
        .unwrap();

    // The shelf still lists both, the damaged one with no key_set shown.
    let shelf = c.firmware_packages(&admin).await.unwrap();
    assert_eq!(shelf.len(), 2);
    let listed = shelf.iter().find(|p| p.package_id == newer.package_id).unwrap();
    assert_eq!(listed.key_set, None);
    // It is never picked as available, and never offered again.
    let views = c.displays(&admin).await.unwrap();
    let available = |id: &str| {
        views
            .iter()
            .find(|d| d.id == id)
            .and_then(|d| d.firmware.as_ref())
            .and_then(|f| f.available_package_id.clone())
    };
    assert_eq!(available(&hall), Some(older.package_id.clone()));
    let e = c
        .offer_firmware(&admin, &hall, &newer.package_id)
        .await
        .unwrap_err();
    assert_eq!((status(&e), code(&e)), (409, Some(ErrorCode::Conflict)));
    assert!(
        e.to_string().contains("couldn't read this package's details"),
        "{e}"
    );
    // The offer already made still reaches its display.
    let shown = c
        .device_snapshot(Some(&token))
        .await
        .unwrap()
        .firmware_update
        .unwrap();
    assert_eq!(shown.offer_id, offer.offer_id);
    // Another package still uploads, and the same file again puts the damaged row right.
    c.upload_firmware(&admin, package(&k, "1.3.0", 1).bytes)
        .await
        .unwrap();
    let (again, created) = c.upload_firmware(&admin, built.bytes).await.unwrap();
    assert!(created);
    assert_eq!(again.key_set, Some(vec![k.public_key().key_id().to_hex()]));
    c.offer_firmware(&admin, &hall, &newer.package_id).await.unwrap();
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

    // Installed and rolled back, by report. An installed report's version is the display's
    // version at once, without waiting for its next heartbeat.
    let running = |c: HubClient, admin: String| async move {
        c.displays(&admin).await.unwrap()[0]
            .device
            .clone()
            .unwrap()
            .firmware_version
    };
    for (s, reported, shown, now_running) in [
        (ReportState::RolledBack, "1.0.0", "rolled_back", "1.0.0"),
        (ReportState::Installed, "1.2.0", "installed", "1.2.0"),
    ] {
        let offer = c.offer_firmware(&admin, &desk, &pkg.package_id).await.unwrap();
        c.firmware_report(&token, &report(&offer.offer_id, s, None, reported))
            .await
            .unwrap();
        assert_eq!(state(c.clone(), admin.clone()).await.0, shown);
        assert_eq!(running(c.clone(), admin.clone()).await, now_running);
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
    // That heartbeat came from the trial boot: the rollback reported afterwards overrules it.
    c.firmware_report(
        &shelf_token,
        &report(&offer.offer_id, ReportState::RolledBack, None, "1.0.0"),
    )
    .await
    .unwrap();
    let o = c
        .displays(&admin)
        .await
        .unwrap()
        .into_iter()
        .find(|d| d.id == shelf_desk)
        .unwrap()
        .firmware
        .unwrap()
        .offer
        .unwrap();
    assert_eq!(
        (
            o.state.as_str(),
            o.reason.as_deref(),
            o.reported_version.as_deref()
        ),
        ("rolled_back", None, Some("1.0.0"))
    );
    let shown = c
        .displays(&admin)
        .await
        .unwrap()
        .into_iter()
        .find(|d| d.id == shelf_desk)
        .unwrap();
    assert_eq!(
        shown.device.unwrap().firmware_version,
        "1.0.0",
        "back to the old version"
    );

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

/// Pairs a publisher and returns its id and token.
async fn publisher(r: &Running, admin: &str, label: &str) -> (String, String) {
    let c = r.client();
    let invite = c.new_publisher(admin, label).await.unwrap();
    let paired = c.publisher_pair(&invite.secret).await.unwrap();
    (paired.publisher_id, paired.publisher_token)
}

#[tokio::test]
async fn a_publisher_makes_room_only_with_its_own_packages_and_uploaders_hear_what_was_removed() {
    let dir = tempfile::tempdir().unwrap();
    let clock = FakeClock::parse(NOW);
    let r = Running::start(dir.path(), &clock).await;
    let admin = r.admin().await;
    let c = r.client();
    let (_, mine) = publisher(&r, &admin, "Maker").await;
    let (_, other) = publisher(&r, &admin, "Another maker").await;
    let k = key();

    let first = c
        .upload_firmware(&admin, package(&k, "1.0.0", 1).bytes)
        .await
        .unwrap()
        .0;
    assert_eq!(first.evicted, None);
    let own_low = c
        .publisher_upload(&mine, package(&k, "1.1.0", 1).bytes)
        .await
        .unwrap()
        .0;
    c.publisher_upload(&mine, package(&k, "1.2.0", 1).bytes)
        .await
        .unwrap();
    // The shelf for this device type is full. The publisher's upload removes its own lowest version,
    // not the admin's lower one, and says so.
    let (view, created) = c
        .publisher_upload(&mine, package(&k, "1.3.0", 1).bytes)
        .await
        .unwrap();
    assert!(created);
    let gone = view.evicted.unwrap();
    assert_eq!(
        (
            gone.package_id.as_str(),
            gone.device_type.as_str(),
            gone.firmware_version.as_str()
        ),
        (own_low.package_id.as_str(), TYPE, "1.1.0")
    );
    assert!(audit(&r.hub)
        .iter()
        .any(|l| l.starts_with(&format!("firmware_evicted {} {TYPE} 1.1.0", own_low.package_id))));
    // Another publisher, with nothing of its own there, can't make room.
    let e = c
        .publisher_upload(&other, package(&k, "2.0.0", 1).bytes)
        .await
        .unwrap_err();
    assert_eq!((status(&e), code(&e)), (409, Some(ErrorCode::Conflict)));
    assert!(e.to_string().contains("someone else uploaded"), "{e}");
    assert_eq!(files(dir.path()).len(), 3);
    // An admin's upload still removes the lowest version, whoever uploaded it.
    let view = c
        .upload_firmware(&admin, package(&k, "1.4.0", 1).bytes)
        .await
        .unwrap()
        .0;
    assert_eq!(view.evicted.unwrap().package_id, first.package_id);
    // The lists never carry `evicted`.
    assert!(c
        .firmware_packages(&admin)
        .await
        .unwrap()
        .iter()
        .all(|p| p.evicted.is_none()));
    r.stop().await;
}

/// The P-256 group order, big-endian.
const P256_N: [u8; 32] = [
    0xff, 0xff, 0xff, 0xff, 0x00, 0x00, 0x00, 0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xbc,
    0xe6, 0xfa, 0xad, 0xa7, 0x17, 0x9e, 0x84, 0xf3, 0xb9, 0xca, 0xc2, 0xfc, 0x63, 0x25, 0x51,
];

/// The same package with its signature's `s` replaced by `n - s`: another valid signature over the
/// same bytes.
fn malleated(package: &[u8]) -> Vec<u8> {
    let mut out = package.to_vec();
    let s = &mut out[73 + 32..73 + 64];
    let mut borrow = 0i16;
    for i in (0..32).rev() {
        let mut d = i16::from(P256_N[i]) - i16::from(s[i]) - borrow;
        borrow = i16::from(d < 0);
        if d < 0 {
            d += 256;
        }
        s[i] = d as u8;
    }
    out
}

#[tokio::test]
async fn the_same_release_signed_again_is_the_release_already_held() {
    let dir = tempfile::tempdir().unwrap();
    let clock = FakeClock::parse(NOW);
    let r = Running::start(dir.path(), &clock).await;
    let admin = r.admin().await;
    let c = r.client();
    let (_, p) = publisher(&r, &admin, "Maker").await;
    let k = key();
    let original = package(&k, "1.2.0", 1);
    let twin = malleated(&original.bytes);
    assert_ne!(twin, original.bytes);
    // Still a valid package, with another package id.
    let checked = firmware_package::check(&twin[..], twin.len() as u64).unwrap();
    assert_ne!(checked.package_id, original.checked.package_id);

    let (kept, created) = c.upload_firmware(&admin, original.bytes.clone()).await.unwrap();
    assert!(created);
    for token in [&admin, &p] {
        let (view, created) = if token == &admin {
            c.upload_firmware(token, twin.clone()).await.unwrap()
        } else {
            c.publisher_upload(token, twin.clone()).await.unwrap()
        };
        assert!(!created, "answered 200");
        assert_eq!(view.package_id, kept.package_id);
    }
    assert_eq!(files(dir.path()), [format!("{}.dhfw", kept.package_id)]);
    assert_eq!(c.firmware_packages(&admin).await.unwrap().len(), 1);
    // A release signed again from scratch is the same release too.
    let resigned = build_and_sign(&k, &release("1.2.0", 1), &image(5000, "1.2.0".len() as u8 + 1)).unwrap();
    assert_ne!(resigned.checked.package_id, original.checked.package_id);
    assert!(!c.upload_firmware(&admin, resigned.bytes).await.unwrap().1);
    // But another key's signature over the same manifest is another release.
    let other = package(&key(), "1.2.0", 1);
    assert!(c.upload_firmware(&admin, other.bytes).await.unwrap().1);
    r.stop().await;
}

/// A raw HTTPS connection to the hub, for what the client library won't do: stop reading a
/// download, or stop sending an upload. `recv_buffer` shrinks the socket's receive buffer, so
/// little of a download can sit unread on this side.
async fn raw(
    r: &Running,
    recv_buffer: Option<u32>,
) -> tokio_rustls::client::TlsStream<tokio::net::TcpStream> {
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    if let Some(b) = recv_buffer {
        socket.set_recv_buffer_size(b).unwrap();
    }
    let tcp = socket
        .connect(std::net::SocketAddr::from(([127, 0, 0, 1], r.port)))
        .await
        .unwrap();
    let config = hub_client::pin::client_config(vec![r.hub.identity.spki_sha256]);
    let name = rustls::pki_types::ServerName::from(std::net::IpAddr::from([127, 0, 0, 1]));
    tokio_rustls::TlsConnector::from(Arc::new(config))
        .connect(name, tcp)
        .await
        .unwrap()
}

/// Reads up to the end of a response's headers, and returns them.
async fn read_head<S: tokio::io::AsyncRead + Unpin>(s: &mut S) -> String {
    use tokio::io::AsyncReadExt;
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        assert_eq!(s.read(&mut byte).await.unwrap(), 1, "the headers ended early");
        head.push(byte[0]);
    }
    String::from_utf8(head).unwrap()
}

/// The largest package there is: far more than the sockets between the hub and a test can hold, so
/// a download a test stops reading stays under way.
fn largest(k: &SigningKey, version: &str) -> Built {
    build_and_sign(
        k,
        &release(version, 1),
        &image(firmware_package::limits::MAX_IMAGE_SIZE as usize, 7),
    )
    .unwrap()
}

/// Starts a download on a raw connection that then stops reading, and returns it once the hub has
/// answered.
async fn stalled_download(
    r: &Running,
    token: &str,
    package_id: &str,
) -> tokio_rustls::client::TlsStream<tokio::net::TcpStream> {
    use tokio::io::AsyncWriteExt;
    let mut s = raw(r, Some(8192)).await;
    s.write_all(
        format!(
            "GET /v1/device/firmware/{package_id} HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {token}\r\n\r\n"
        )
        .as_bytes(),
    )
    .await
    .unwrap();
    let head = read_head(&mut s).await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    s
}

/// Reads a connection until it ends, or for at most a minute, and returns what arrived.
async fn read_to_end<S: tokio::io::AsyncRead + Unpin>(s: &mut S) -> Option<u64> {
    use tokio::io::AsyncReadExt;
    let mut got = 0u64;
    let mut buf = vec![0u8; 64 * 1024];
    tokio::time::timeout(std::time::Duration::from_secs(60), async {
        loop {
            match s.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => got += n as u64,
            }
        }
    })
    .await
    .ok()
    .map(|()| got)
}

#[tokio::test]
async fn a_new_download_from_a_display_takes_over_from_its_last() {
    let dir = tempfile::tempdir().unwrap();
    let clock = FakeClock::parse(NOW);
    let r = Running::start(dir.path(), &clock).await;
    let admin = r.admin().await;
    let c = r.client();
    let k = key();
    let big = largest(&k, "2.0.0");
    let form = big.checked.update_form_len();
    let pkg = c.upload_firmware(&admin, big.bytes).await.unwrap().0;
    let (desk, token) = r
        .display(&admin, "Desk", REV, Some(trusting(&k, 0, 16 << 20)))
        .await;
    let (other, other_token) = r
        .display(&admin, "Other", REV, Some(trusting(&k, 0, 16 << 20)))
        .await;
    c.offer_firmware(&admin, &desk, &pkg.package_id).await.unwrap();
    c.offer_firmware(&admin, &other, &pkg.package_id).await.unwrap();

    // Desk's first download stalls: its Wi-Fi dropped without the connection closing.
    let mut first = stalled_download(&r, &token, &pkg.package_id).await;
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(r.hub.is_downloading(&desk));
    assert_eq!(r.hub.download_slots.available_permits(), 1);
    // Other downloads meanwhile, with the hub's other slot.
    let got = c.firmware_download(&other_token, &pkg.package_id).await.unwrap();
    assert_eq!(got.bytes.len() as u64, form);
    // Desk tries again: the new download is served in full, and the stalled one is closed.
    let got = c.firmware_download(&token, &pkg.package_id).await.unwrap();
    assert_eq!(got.bytes.len() as u64, form);
    let cut = read_to_end(&mut first)
        .await
        .expect("the stalled connection was left open");
    assert!(
        cut < form,
        "{cut} of {form} bytes: the stalled download wasn't cut off"
    );
    // Nothing is left held, and each start counted.
    assert!(!r.hub.is_downloading(&desk));
    assert_eq!(r.hub.downloads_under_way(), 0);
    assert_eq!(
        r.hub.download_slots.available_permits(),
        hub_protocol::limits::MAX_CONCURRENT_FIRMWARE_DOWNLOADS
    );
    r.stop().await;
}

/// Moving an open file aside, which this test needs, works only on Unix.
#[cfg(unix)]
#[tokio::test]
async fn a_download_that_cant_be_served_leaves_the_displays_last_one_running() {
    use tokio::io::AsyncReadExt;
    let dir = tempfile::tempdir().unwrap();
    let clock = FakeClock::parse(NOW);
    let r = Running::start(dir.path(), &clock).await;
    let admin = r.admin().await;
    let c = r.client();
    let k = key();
    let big = largest(&k, "2.0.0");
    let form = big.checked.update_form_len();
    let expected = big.bytes[..form as usize].to_vec();
    let pkg = c.upload_firmware(&admin, big.bytes).await.unwrap().0;
    let (desk, token) = r
        .display(&admin, "Desk", REV, Some(trusting(&k, 0, 16 << 20)))
        .await;
    c.offer_firmware(&admin, &desk, &pkg.package_id).await.unwrap();
    let file = dir
        .path()
        .join("firmware")
        .join(format!("{}.dhfw", pkg.package_id));
    let aside = dir.path().join("aside.dhfw");

    let mut first = stalled_download(&r, &token, &pkg.package_id).await;
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(r.hub.is_downloading(&desk));
    // The file can't be opened for a new download (the running one has it open already): the new
    // one is refused, and the running one is left alone, slot and all.
    std::fs::rename(&file, &aside).unwrap();
    let e = c.firmware_download(&token, &pkg.package_id).await.unwrap_err();
    assert_eq!((status(&e), code(&e)), (503, Some(ErrorCode::Unavailable)));
    assert!(r.hub.is_downloading(&desk));
    assert_eq!(r.hub.download_slots.available_permits(), 1);
    // Nor with every slot taken, when a new one would take over the running one's slot.
    let elsewhere = r.hub.download_slots.clone().try_acquire_owned().unwrap();
    let e = c.firmware_download(&token, &pkg.package_id).await.unwrap_err();
    assert_eq!((status(&e), code(&e)), (503, Some(ErrorCode::Unavailable)));
    assert!(r.hub.is_downloading(&desk));
    assert_eq!(r.hub.download_slots.available_permits(), 0);
    drop(elsewhere);
    std::fs::rename(&aside, &file).unwrap();
    // It goes on to the end.
    let mut rest = vec![0u8; form as usize];
    tokio::time::timeout(std::time::Duration::from_secs(60), first.read_exact(&mut rest))
        .await
        .expect("the first download stopped")
        .unwrap();
    assert!(rest == expected, "the first download's bytes changed");
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(!r.hub.is_downloading(&desk));
    assert_eq!(r.hub.download_slots.available_permits(), 2);

    // With every slot taken, one by another display, a new download from this display takes over
    // its last one's slot rather than being refused.
    let mut second = stalled_download(&r, &token, &pkg.package_id).await;
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let elsewhere = r.hub.download_slots.clone().try_acquire_owned().unwrap();
    let got = c.firmware_download(&token, &pkg.package_id).await.unwrap();
    assert_eq!(got.bytes.len() as u64, form);
    let cut = read_to_end(&mut second)
        .await
        .expect("the stalled connection was left open");
    assert!(
        cut < form,
        "{cut} of {form} bytes: the stalled download wasn't cut off"
    );
    assert!(!r.hub.is_downloading(&desk));
    assert_eq!(r.hub.download_slots.available_permits(), 1);
    drop(elsewhere);
    r.stop().await;
}

/// How many of this process's open files are `path`.
#[cfg(target_os = "linux")]
fn open_handles_to(path: &Path) -> usize {
    std::fs::read_dir("/proc/self/fd")
        .unwrap()
        .filter_map(|e| std::fs::read_link(e.ok()?.path()).ok())
        .filter(|target| target == path)
        .count()
}

#[tokio::test]
async fn a_display_that_stops_reading_loses_its_connection_at_the_time_limit() {
    let dir = tempfile::tempdir().unwrap();
    let clock = FakeClock::parse(NOW);
    let limits = FirmwareTimeLimits {
        download: std::time::Duration::from_secs(2),
        ..Default::default()
    };
    let r = Running::start_with(dir.path(), &clock, limits).await;
    let admin = r.admin().await;
    let c = r.client();
    let k = key();
    let big = largest(&k, "2.0.0");
    let form = big.checked.update_form_len();
    let pkg = c.upload_firmware(&admin, big.bytes).await.unwrap().0;
    let (desk, token) = r
        .display(&admin, "Desk", REV, Some(trusting(&k, 0, 16 << 20)))
        .await;
    c.offer_firmware(&admin, &desk, &pkg.package_id).await.unwrap();

    let mut s = stalled_download(&r, &token, &pkg.package_id).await;
    // The display stops reading. Its download is under way, holding a slot, its turn and the file.
    let file = dir
        .path()
        .join("firmware")
        .join(format!("{}.dhfw", pkg.package_id))
        .canonicalize()
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert!(r.hub.is_downloading(&desk));
    assert_eq!(r.hub.download_slots.available_permits(), 1);
    #[cfg(target_os = "linux")]
    assert_eq!(open_handles_to(&file), 1);
    // Past the limit, the hub has let go of all three while the display still isn't reading: the
    // connection was dropped, since the body was never asked for more.
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    assert_eq!(r.hub.downloads_under_way(), 0);
    assert_eq!(r.hub.download_slots.available_permits(), 2);
    #[cfg(target_os = "linux")]
    assert_eq!(open_handles_to(&file), 0, "the file is still open");
    let _ = &file;
    // What was already sent arrives, then the stream ends early.
    let got = read_to_end(&mut s).await.expect("the connection was left open");
    assert!(got < form, "{got} of {form} bytes: the download wasn't cut off");
    r.stop().await;
}

#[tokio::test]
async fn an_upload_that_sends_nothing_is_dropped_before_the_overall_limit() {
    use tokio::io::AsyncWriteExt;
    let dir = tempfile::tempdir().unwrap();
    let clock = FakeClock::parse(NOW);
    assert_eq!(
        FirmwareTimeLimits::default().upload_idle,
        std::time::Duration::from_secs(60)
    );
    let limits = FirmwareTimeLimits {
        upload_idle: std::time::Duration::from_secs(1),
        ..Default::default()
    };
    let r = Running::start_with(dir.path(), &clock, limits).await;
    let admin = r.admin().await;
    let k = key();
    let built = package(&k, "1.2.0", 1);
    let mut s = raw(&r, None).await;
    s.write_all(
        format!(
            "POST /v1/admin/firmware HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {admin}\r\n\
             Content-Type: application/octet-stream\r\nContent-Length: {}\r\n\r\n",
            built.bytes.len()
        )
        .as_bytes(),
    )
    .await
    .unwrap();
    s.write_all(&built.bytes[..1000]).await.unwrap();
    s.flush().await.unwrap();
    let head = tokio::time::timeout(std::time::Duration::from_secs(10), read_head(&mut s))
        .await
        .expect("answered well within the overall limit");
    assert!(head.starts_with("HTTP/1.1 408"), "{head}");
    // The slot is free, and nothing was kept.
    assert_eq!(r.hub.upload_slots.available_permits(), 1);
    assert!(files(dir.path()).is_empty(), "{:?}", files(dir.path()));
    assert!(r.client().upload_firmware(&admin, built.bytes).await.unwrap().1);
    r.stop().await;
}

#[tokio::test]
async fn a_label_cant_claim_to_be_the_official_firmware() {
    let dir = tempfile::tempdir().unwrap();
    let clock = FakeClock::parse(NOW);
    let r = Running::start(dir.path(), &clock).await;
    let admin = r.admin().await;
    let c = r.client();
    let key_id = key().public_key().key_id().to_hex();
    for bad in [
        "Official Display Hub firmware",
        "official display-hub FIRMWARE!",
        "Display Hub (Official)",
    ] {
        let e = c.set_signer_label(&admin, &key_id, bad).await.unwrap_err();
        assert_eq!(
            (status(&e), code(&e)),
            (400, Some(ErrorCode::BadRequest)),
            "{bad}"
        );
        assert!(e.to_string().contains("built into the app"), "{e}");
    }
    assert!(c.firmware_signers(&admin).await.unwrap().is_empty());
    c.set_signer_label(&admin, &key_id, "Kiwi Displays")
        .await
        .unwrap();
    r.stop().await;
}

#[tokio::test]
async fn attempts_left_is_shown_and_never_blocks_an_offer() {
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
    let none_left = FirmwareInfo {
        attempts_left: Some(0),
        ..trusting(&k, 0, 1 << 20)
    };
    let (desk, _) = r.display(&admin, "Desk", REV, Some(none_left)).await;
    let shown = c.displays(&admin).await.unwrap()[0].firmware.clone().unwrap();
    assert_eq!(shown.reported.unwrap().attempts_left, Some(0));
    // The display refills within hours, and the offer lasts a day.
    c.offer_firmware(&admin, &desk, &pkg.package_id).await.unwrap();
    // Out of range, the firmware member is dropped and the rest kept, as for any bad report.
    let grant = c.new_enrolment(&admin, "Odd").await.unwrap();
    c.enrol(
        &grant.secret,
        DeviceInfo {
            device_type: TYPE.into(),
            firmware_version: "1.0.0".into(),
            hardware_revision: REV.into(),
            protocol_version: 1,
            firmware: Some(FirmwareInfo {
                attempts_left: Some(6),
                ..trusting(&k, 0, 1 << 20)
            }),
        },
    )
    .await
    .unwrap();
    let odd = c
        .displays(&admin)
        .await
        .unwrap()
        .into_iter()
        .find(|d| d.name == "Odd")
        .unwrap();
    assert_eq!(odd.firmware.unwrap().reported, None);
    r.stop().await;
}
