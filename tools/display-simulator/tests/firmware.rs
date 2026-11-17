//! Signed firmware updates end to end: the simulated display against a real hub over pinned HTTPS on
//! loopback, sharing one fake clock, so every wait is tested without waiting
//! (`docs/firmware-updates.md`, "What the simulator must test"). Every package is signed with a key
//! made here, in memory, and never kept.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use display_simulator::firmware::install::Failure;
use display_simulator::firmware::store::{self, NextStart, SlotName, SlotState};
use display_simulator::firmware::{Acted, Firmware, Limits, Started, TrialOutcome};
use display_simulator::{paired, usb};
use firmware_package::header::signed_prefix;
use firmware_package::limits::{HEADER_LEN, OFFSET_PUBLIC_KEY, OFFSET_SIGNATURE};
use firmware_package::{build_and_sign, hex, Built, Refusal, Release, SigningKey};
use hub_client::{ClientError, HubClient};
use hub_core::FakeClock;
use hub_protocol::firmware::{BundleFirmware, FirmwareUpdate};
use hub_protocol::limits::{
    FIRMWARE_LONG_WAIT_SECONDS, FIRMWARE_OFFER_LIFETIME_SECONDS, FIRMWARE_RETRY_WAIT_SECONDS,
};
use hub_protocol::pairing::PairingString;
use hub_protocol::ErrorCode;
use hubd::api::Hub;
use hubd::config::Config;
use serde_json::json;
use tokio::sync::oneshot;

const NOW: &str = "2026-11-17T09:00:00Z";
const SLOT: u64 = 1024 * 1024;

struct Rig {
    dir: tempfile::TempDir,
    clock: FakeClock,
    hub: Arc<Hub>,
    port: u16,
    admin: String,
    stop: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<()>,
}

/// One simulated display.
struct Sim {
    dir: PathBuf,
    state: paired::State,
    fw: Firmware,
}

impl Rig {
    async fn start() -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let clock = FakeClock::parse(NOW);
        let hub = hubd::open_hub(
            Config {
                data_dir: dir.path().join("hub"),
                ..Config::default()
            },
            Arc::new(clock.clone()),
        )
        .unwrap();
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
        let raw = hub.new_admin_pairing("PC").unwrap();
        let secret = PairingString {
            host: "127.0.0.1".into(),
            port,
            spki_sha256: hub.identity.spki_sha256,
            secret: raw,
        }
        .secret_text();
        let client = HubClient::pinned("127.0.0.1", port, vec![hub.identity.spki_sha256]).unwrap();
        let admin = client.pair(&secret, "PC").await.unwrap().admin_token;
        Rig {
            dir,
            clock,
            hub,
            port,
            admin,
            stop: Some(tx),
            task,
        }
    }

    fn client(&self) -> HubClient {
        HubClient::pinned("127.0.0.1", self.port, vec![self.hub.identity.spki_sha256]).unwrap()
    }

    /// A display set up over "USB" with a bundle that carries `keys` and `floor`, as after a first
    /// install, then enrolled.
    async fn display(&self, name: &str, keys: &[&SigningKey], floor: u32) -> Sim {
        let dir = self.dir.path().join(name);
        store::set_board(
            &dir,
            &store::Board {
                slot_bytes: SLOT,
                ..store::Board::default()
            },
        )
        .unwrap();
        let grant = self.client().new_enrolment(&self.admin, name).await.unwrap();
        let mut bundle =
            hub_client::provisioning::bundle(&grant, vec!["127.0.0.1".into()], self.port, None).unwrap();
        bundle.firmware = Some(BundleFirmware {
            trusted_keys: keys.iter().map(|k| k.public_key().to_hex()).collect(),
            security_counter: floor,
        });
        let state = paired::enrol(&bundle, &dir).await.unwrap();
        let mut fw = Firmware::open(&dir, Arc::new(self.clock.clone())).unwrap();
        fw.limits = Limits {
            stall: Duration::from_secs(5),
            total: Duration::from_secs(30),
        };
        assert!(matches!(fw.boot().unwrap(), Started::Normal { .. }));
        Sim { dir, state, fw }
    }

    async fn upload(&self, built: &Built) -> String {
        self.client()
            .upload_firmware(&self.admin, built.bytes.clone())
            .await
            .unwrap()
            .0
            .package_id
    }

    async fn offer(&self, sim: &Sim, package_id: &str) -> String {
        self.client()
            .offer_firmware(&self.admin, &sim.state.device_id, package_id)
            .await
            .unwrap()
            .offer_id
    }

    /// The display's open or latest offer, as the app sees it: (state, reason).
    async fn offer_state(&self, sim: &Sim) -> (String, Option<String>) {
        let o = self
            .client()
            .displays(&self.admin)
            .await
            .unwrap()
            .into_iter()
            .find(|d| d.id == sim.state.device_id)
            .unwrap()
            .firmware
            .unwrap()
            .offer
            .unwrap();
        (o.state, o.reason)
    }

    fn stored(&self, package_id: &str) -> PathBuf {
        self.dir
            .path()
            .join("hub")
            .join("firmware")
            .join(format!("{package_id}.dhfw"))
    }

    async fn stop(mut self) {
        let _ = self.stop.take().unwrap().send(());
        self.task.await.unwrap();
    }
}

impl Sim {
    async fn poll(&self) -> Result<hub_protocol::Snapshot, ClientError> {
        paired::snapshot(&self.state).await
    }

    async fn offered(&self) -> Option<FirmwareUpdate> {
        self.poll().await.unwrap().firmware_update
    }

    /// Polls, and acts on the offer in the snapshot.
    async fn act(&mut self) -> Acted {
        let offer = self.offered().await.expect("an offer in the snapshot");
        self.fw.on_offer(&self.state, &offer).await.unwrap()
    }

    fn version(&self) -> String {
        store::running_version(&self.dir).unwrap()
    }

    fn keys(&self) -> store::KeyStore {
        store::keys(&self.dir).unwrap()
    }

    fn ota(&self) -> store::OtaData {
        store::ota(&self.dir).unwrap()
    }
}

fn key() -> SigningKey {
    SigningKey::generate().unwrap().0
}

fn image(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(29).wrapping_add(seed))
        .collect()
}

fn release(version: &str, counter: u32) -> Release {
    Release {
        device_type: store::DEVICE_TYPE.into(),
        hardware_revisions: vec![store::HARDWARE_REVISION.into()],
        firmware_version: version.into(),
        security_counter: counter,
        summary: Some(format!("Release {version}")),
        key_set: None,
        usb_install: None,
    }
}

fn package(key: &SigningKey, version: &str, counter: u32) -> Built {
    build_and_sign(key, &release(version, counter), &image(40_000, counter as u8)).unwrap()
}

fn image_of(built: &Built) -> &[u8] {
    let start = HEADER_LEN + built.checked.header.manifest_len as usize;
    &built.bytes[start..start + built.checked.manifest.image_size as usize]
}

/// Cases 1, 29 and 46: a valid package is downloaded, checked, written to the spare slot and booted on
/// trial; the trial check-in reaches the hub, so it is marked good, the floor is raised and the hub
/// hears `installed`.
#[tokio::test]
async fn an_update_installs_proves_itself_and_is_marked_good() {
    let r = Rig::start().await;
    let maker = key();
    let mut sim = r.display("desk", &[&maker], 0).await;
    assert_eq!(sim.version(), store::FACTORY_VERSION);
    // A large image, so it arrives in many pieces.
    let built = build_and_sign(&maker, &release("1.2.0", 3), &image(600_000, 5)).unwrap();
    let id = r.upload(&built).await;
    r.offer(&sim, &id).await;

    let Acted::Restarted(Started::Trial { version }) = sim.act().await else {
        panic!("expected a trial boot")
    };
    assert_eq!(version, "1.2.0");
    assert_eq!(r.offer_state(&sim).await.0, "downloading");
    // On trial: running the new image, but the floor isn't raised until it is marked good.
    let ota = sim.ota();
    assert_eq!(
        (ota.running, ota.boot, ota.b.state),
        (SlotName::B, SlotName::B, SlotState::PendingVerify)
    );
    assert_eq!(sim.keys().security_floor, 0);
    // Case 46: what the display wrote is exactly the signed image, and the hub served the start of
    // the uploaded file without anything after it.
    assert_eq!(
        std::fs::read(store::slot_image(&sim.dir, SlotName::B)).unwrap(),
        image_of(&built)
    );
    let served = r
        .client()
        .firmware_download(&sim.state.device_token, &id)
        .await
        .unwrap();
    assert_eq!(
        served.bytes,
        built.bytes[..built.checked.update_form_len() as usize]
    );
    // The same offer is still in the snapshot while it is open: the new firmware doesn't take it.
    let offer = sim.offered().await.unwrap();
    assert_eq!(sim.fw.on_offer(&sim.state, &offer).await.unwrap(), Acted::Busy);

    let result = sim.poll().await;
    assert_eq!(
        sim.fw.check_in_with(&result).unwrap(),
        TrialOutcome::MarkedGood {
            version: "1.2.0".into()
        }
    );
    assert_eq!(sim.ota().b.state, SlotState::Valid);
    assert_eq!(sim.keys().security_floor, 3);
    assert_eq!(sim.keys().key_ids(), [maker.public_key().key_id().to_hex()]);
    assert!(!sim.fw.send_report(&sim.state).await.unwrap());
    assert_eq!(r.offer_state(&sim).await, ("installed".into(), None));
    assert!(sim.offered().await.is_none());
    // The hub hears the new version and floor.
    paired::heartbeat(&sim.state, &paired::device_info(&sim.dir))
        .await
        .unwrap();
    let shown = r.client().displays(&r.admin).await.unwrap()[0]
        .device
        .clone()
        .unwrap();
    assert_eq!(
        (
            shown.firmware_version.as_str(),
            shown.firmware.unwrap().security_floor
        ),
        ("1.2.0", 3)
    );
    // A restart keeps it.
    assert_eq!(
        sim.fw.boot().unwrap(),
        Started::Normal {
            version: "1.2.0".into()
        }
    );
    r.stop().await;
}

/// Cases 30, 31 and 36, a crash on start, and case 26: an update that doesn't prove itself rolls
/// back, and the old firmware reports `rolled_back` with its keys and floor as they were. After three
/// failures in a row the next attempt waits six hours.
#[tokio::test]
async fn updates_that_dont_prove_themselves_roll_back() {
    let r = Rig::start().await;
    let maker = key();
    let new = key();
    let mut sim = r.display("desk", &[&maker], 1).await;
    let mut rel = release("1.2.0", 2);
    rel.key_set = Some(vec![new.public_key()]);
    let built = build_and_sign(&maker, &rel, &image(30_000, 1)).unwrap();
    let id = r.upload(&built).await;
    let unchanged = |sim: &Sim| {
        assert_eq!(sim.version(), store::FACTORY_VERSION);
        assert_eq!(sim.keys().key_ids(), [maker.public_key().key_id().to_hex()]);
        assert_eq!(sim.keys().security_floor, 1);
    };

    // Case 30, with case 36's key_set: the new firmware can't reach the hub for five minutes.
    r.offer(&sim, &id).await;
    sim.fw.switches.next_start = NextStart::NoHub;
    assert!(matches!(sim.act().await, Acted::Restarted(Started::Trial { .. })));
    let reached = sim.poll().await;
    assert!(
        reached.is_ok(),
        "the hub is up; the switch keeps the firmware from it"
    );
    assert_eq!(sim.fw.check_in_with(&reached).unwrap(), TrialOutcome::Waiting);
    r.clock.advance(299);
    assert_eq!(sim.fw.tick().unwrap(), TrialOutcome::Waiting);
    r.clock.advance(1);
    assert_eq!(
        sim.fw.tick().unwrap(),
        TrialOutcome::RolledBack(Started::RolledBack {
            version: store::FACTORY_VERSION.into()
        })
    );
    assert_eq!(sim.ota().b.state, SlotState::Invalid);
    assert_eq!(sim.ota().running, SlotName::A);
    unchanged(&sim);
    sim.fw.send_report(&sim.state).await.unwrap();
    assert_eq!(r.offer_state(&sim).await, ("rolled_back".into(), None));

    // Case 31: a restart during the trial boot.
    r.clock.advance(FIRMWARE_RETRY_WAIT_SECONDS as i64);
    r.offer(&sim, &id).await;
    assert!(matches!(sim.act().await, Acted::Restarted(Started::Trial { .. })));
    assert_eq!(
        sim.fw.boot().unwrap(),
        Started::RolledBack {
            version: store::FACTORY_VERSION.into()
        }
    );
    unchanged(&sim);
    sim.fw.send_report(&sim.state).await.unwrap();
    assert_eq!(r.offer_state(&sim).await.0, "rolled_back");

    // New firmware that crashes before it can check in.
    r.clock.advance(FIRMWARE_RETRY_WAIT_SECONDS as i64);
    r.offer(&sim, &id).await;
    sim.fw.switches.next_start = NextStart::Fails;
    assert_eq!(
        sim.act().await,
        Acted::Restarted(Started::RolledBack {
            version: store::FACTORY_VERSION.into()
        })
    );
    unchanged(&sim);
    assert_eq!(r.offer_state(&sim).await.0, "rolled_back");
    assert_eq!(store::record(&sim.dir).unwrap().failures_in_row, 3);

    // Case 26: three failures in a row, so the next attempt waits six hours from the last.
    r.clock.advance(FIRMWARE_RETRY_WAIT_SECONDS as i64);
    r.offer(&sim, &id).await;
    let Acted::Waiting { seconds } = sim.act().await else {
        panic!("expected a wait")
    };
    assert_eq!(seconds, FIRMWARE_LONG_WAIT_SECONDS - FIRMWARE_RETRY_WAIT_SECONDS);
    r.clock.advance(seconds as i64);
    assert!(matches!(sim.act().await, Acted::Restarted(Started::Trial { .. })));
    let result = sim.poll().await;
    assert!(matches!(
        sim.fw.check_in_with(&result).unwrap(),
        TrialOutcome::MarkedGood { .. }
    ));
    // Marked good at last: the key_set replaces the keys, and the count starts again.
    assert_eq!(sim.keys().key_ids(), [new.public_key().key_id().to_hex()]);
    assert_eq!(store::record(&sim.dir).unwrap().failures_in_row, 0);
    r.stop().await;
}

/// Case 32: the hub answers the trial check-in with `token_revoked`. Rolling back wouldn't help, so
/// the new firmware is marked good. Its report gets an answer (an error), so it isn't repeated.
#[tokio::test]
async fn token_revoked_during_the_trial_counts_as_reaching_the_hub() {
    let r = Rig::start().await;
    let maker = key();
    let mut sim = r.display("desk", &[&maker], 0).await;
    let id = r.upload(&package(&maker, "1.2.0", 2)).await;
    r.offer(&sim, &id).await;
    assert!(matches!(sim.act().await, Acted::Restarted(Started::Trial { .. })));
    r.client()
        .revoke_display(&r.admin, &sim.state.device_id)
        .await
        .unwrap();
    let result = sim.poll().await;
    assert_eq!(result.as_ref().unwrap_err().code(), Some(ErrorCode::TokenRevoked));
    assert!(matches!(
        sim.fw.check_in_with(&result).unwrap(),
        TrialOutcome::MarkedGood { .. }
    ));
    assert_eq!(sim.keys().security_floor, 2);
    assert!(!sim.fw.send_report(&sim.state).await.unwrap());
    assert!(store::record(&sim.dir).unwrap().last_result.unwrap().answered);
    r.stop().await;
}

/// Case 33: the power goes off part-way through writing the image. The old firmware starts, and
/// reports `failed` with `download`. A final report the hub doesn't answer is sent again.
#[tokio::test]
async fn a_power_cut_during_the_download_is_reported_as_a_failed_download() {
    let r = Rig::start().await;
    let maker = key();
    let mut sim = r.display("desk", &[&maker], 0).await;
    let id = r.upload(&package(&maker, "1.2.0", 1)).await;
    let offer_id = r.offer(&sim, &id).await;
    sim.fw.switches.power_cut_after_image_bytes = Some(10_000);
    assert_eq!(sim.act().await, Acted::PowerCut);
    assert_eq!(
        std::fs::metadata(store::slot_image(&sim.dir, SlotName::B))
            .unwrap()
            .len(),
        10_000
    );
    // The offer id was saved before the download, so the next start knows.
    let record = store::record(&sim.dir).unwrap();
    assert_eq!(record.attempt.unwrap().offer_id, offer_id);

    // Power back on, with the hub unreachable at first: the report waits.
    let mut fw = Firmware::open(&sim.dir, Arc::new(r.clock.clone())).unwrap();
    assert_eq!(fw.switches, store::Switches::default(), "a switch applies once");
    assert_eq!(
        fw.boot().unwrap(),
        Started::Interrupted {
            version: store::FACTORY_VERSION.into()
        }
    );
    let ota = sim.ota();
    assert_eq!(
        (ota.running, ota.boot, ota.b.state),
        (SlotName::A, SlotName::A, SlotState::Empty)
    );
    let mut away = sim.state.clone();
    away.hub.hosts = vec!["127.0.0.2".into()];
    assert!(
        fw.send_report(&away).await.unwrap(),
        "still waiting for an answer"
    );
    assert!(!fw.send_report(&sim.state).await.unwrap());
    assert_eq!(
        r.offer_state(&sim).await,
        ("failed".into(), Some("download".into()))
    );
    r.stop().await;
}

/// Case 45, with cases 21, 24, 25 and 27 over the network: a compromised hub serving altered bytes
/// for a genuine package. The display refuses every one, writes nothing it hasn't verified, and the
/// limits on trying hold.
#[tokio::test]
async fn a_compromised_hub_serving_tampered_packages_is_refused() {
    let r = Rig::start().await;
    let maker = key();
    let stranger = key();
    let mut sim = r.display("desk", &[&maker], 0).await;
    let built = package(&maker, "1.2.0", 1);
    let id = r.upload(&built).await;
    let stored = r.stored(&id);
    let original = std::fs::read(&stored).unwrap();
    let tamper = |f: &dyn Fn(&mut Vec<u8>)| {
        let mut b = original.clone();
        f(&mut b);
        std::fs::write(&stored, b).unwrap();
    };
    let nothing_installed = |sim: &Sim| {
        assert_eq!(sim.version(), store::FACTORY_VERSION);
        assert_eq!(sim.ota().boot, SlotName::A);
        assert_ne!(sim.ota().b.state, SlotState::New);
    };

    // Case 27: a package that isn't on offer to this display isn't served.
    let e = paired::firmware_stream(&sim.state, &id).await.unwrap_err();
    assert_eq!(e.code(), Some(ErrorCode::NotFound));

    // One byte of the manifest changed after signing.
    tamper(&|b| b[HEADER_LEN + 5] ^= 0x01);
    r.offer(&sim, &id).await;
    let offer = sim.offered().await.unwrap();
    assert_eq!(
        sim.fw.on_offer(&sim.state, &offer).await.unwrap(),
        Acted::Refused(Refusal::BadSignature)
    );
    assert_eq!(
        r.offer_state(&sim).await,
        ("refused".into(), Some("bad_signature".into()))
    );
    assert!(
        !store::slot_image(&sim.dir, SlotName::B).exists(),
        "nothing written"
    );
    nothing_installed(&sim);
    // Case 24: the same offer seen again does nothing.
    assert_eq!(sim.fw.on_offer(&sim.state, &offer).await.unwrap(), Acted::Ignored);

    // Case 25: a new offer straight away waits ten minutes from the last attempt.
    r.offer(&sim, &id).await;
    assert_eq!(
        sim.act().await,
        Acted::Waiting {
            seconds: FIRMWARE_RETRY_WAIT_SECONDS
        }
    );
    r.clock.advance(FIRMWARE_RETRY_WAIT_SECONDS as i64 - 1);
    assert_eq!(sim.act().await, Acted::Waiting { seconds: 1 });
    r.clock.advance(1);

    // Case 21: one byte of the image changed. It is written, fails its hash and is abandoned.
    tamper(&|b| {
        let last = b.len() - 1;
        b[last] ^= 0x01;
    });
    assert_eq!(sim.act().await, Acted::Failed(Failure::ImageHash));
    assert_eq!(
        r.offer_state(&sim).await,
        ("failed".into(), Some("image_hash".into()))
    );
    assert!(!store::slot_image(&sim.dir, SlotName::B).exists(), "abandoned");
    assert_eq!(sim.ota().b.state, SlotState::Empty);
    nothing_installed(&sim);

    // Another key in the header.
    r.clock.advance(FIRMWARE_RETRY_WAIT_SECONDS as i64);
    tamper(&|b| {
        b[OFFSET_PUBLIC_KEY..OFFSET_SIGNATURE].copy_from_slice(stranger.public_key().as_bytes());
    });
    r.offer(&sim, &id).await;
    assert_eq!(sim.act().await, Acted::Refused(Refusal::UntrustedKey));
    // A package the stranger signed in full, put in the genuine one's place.
    r.clock.advance(FIRMWARE_LONG_WAIT_SECONDS as i64);
    let theirs = build_and_sign(&stranger, &release("6.6.6", 9), &image(40_000, 1)).unwrap();
    assert_eq!(theirs.bytes.len(), original.len());
    tamper(&|b| b.copy_from_slice(&theirs.bytes));
    r.offer(&sim, &id).await;
    assert_eq!(sim.act().await, Acted::Refused(Refusal::UntrustedKey));
    // The genuine bytes signed again without the fixed text, as if for some other purpose.
    r.clock.advance(FIRMWARE_LONG_WAIT_SECONDS as i64);
    let manifest = &original[HEADER_LEN..HEADER_LEN + built.checked.header.manifest_len as usize];
    let prefix = signed_prefix(built.checked.header.manifest_len, &maker.public_key());
    let bare = maker.sign(&[prefix.as_slice(), manifest].concat()).unwrap();
    tamper(&|b| b[OFFSET_SIGNATURE..HEADER_LEN].copy_from_slice(&bare));
    r.offer(&sim, &id).await;
    assert_eq!(sim.act().await, Acted::Refused(Refusal::BadSignature));
    nothing_installed(&sim);
    assert_eq!(store::record(&sim.dir).unwrap().failures_in_row, 5);

    // The genuine file again: installed.
    r.clock.advance(FIRMWARE_LONG_WAIT_SECONDS as i64);
    std::fs::write(&stored, &original).unwrap();
    r.offer(&sim, &id).await;
    assert!(matches!(sim.act().await, Acted::Restarted(Started::Trial { .. })));
    r.stop().await;
}

/// Cases 34 and 35, and 28: keys change only through a release marked good. A key_set that retires
/// its own signer leaves the display trusting only the keys it names, and the hub, hearing that,
/// won't offer the retired key's packages. An offer left for 24 hours is gone from the snapshot.
#[tokio::test]
async fn keys_change_only_through_a_release_marked_good() {
    let r = Rig::start().await;
    let maker = key();
    let recovery = key();
    let new = key();
    let mut sim = r.display("desk", &[&maker], 0).await;
    let ids = |keys: &[&SigningKey]| {
        keys.iter()
            .map(|k| k.public_key().key_id().to_hex())
            .collect::<Vec<_>>()
    };

    // Case 34: a key_set adding a key.
    let mut rel = release("1.1.0", 1);
    rel.key_set = Some(vec![maker.public_key(), recovery.public_key()]);
    let id = r
        .upload(&build_and_sign(&maker, &rel, &image(20_000, 1)).unwrap())
        .await;
    r.offer(&sim, &id).await;
    assert!(matches!(sim.act().await, Acted::Restarted(Started::Trial { .. })));
    assert_eq!(sim.keys().key_ids(), ids(&[&maker]), "not until marked good");
    let result = sim.poll().await;
    sim.fw.check_in_with(&result).unwrap();
    assert_eq!(sim.keys().key_ids(), ids(&[&maker, &recovery]));
    sim.fw.send_report(&sim.state).await.unwrap();

    // Case 35: a key_set leaving out the key that signed it, with a higher counter.
    r.clock.advance(FIRMWARE_RETRY_WAIT_SECONDS as i64);
    let mut rel = release("2.0.0", 5);
    rel.key_set = Some(vec![recovery.public_key(), new.public_key()]);
    let id = r
        .upload(&build_and_sign(&maker, &rel, &image(20_000, 2)).unwrap())
        .await;
    r.offer(&sim, &id).await;
    assert!(matches!(sim.act().await, Acted::Restarted(Started::Trial { .. })));
    let result = sim.poll().await;
    sim.fw.check_in_with(&result).unwrap();
    sim.fw.send_report(&sim.state).await.unwrap();
    assert_eq!(sim.keys().key_ids(), ids(&[&recovery, &new]));
    assert_eq!(sim.keys().security_floor, 5);
    paired::heartbeat(&sim.state, &paired::device_info(&sim.dir))
        .await
        .unwrap();
    // The retired key's packages, and older ones (case 37), aren't even offered.
    for (k, v, c) in [(&maker, "2.1.0", 5), (&recovery, "1.9.0", 4)] {
        let id = r.upload(&package(k, v, c)).await;
        let e = r
            .client()
            .offer_firmware(&r.admin, &sim.state.device_id, &id)
            .await
            .unwrap_err();
        assert_eq!(e.code(), Some(ErrorCode::Conflict), "{v}");
    }

    // Case 28: an offer left for 24 hours is gone from the snapshot.
    let id = r.upload(&package(&new, "2.2.0", 5)).await;
    r.offer(&sim, &id).await;
    assert!(sim.offered().await.is_some());
    r.clock.advance(FIRMWARE_OFFER_LIFETIME_SECONDS);
    assert!(sim.offered().await.is_none());
    r.stop().await;
}

/// A manifest and image signed as given, for packages the signing tool won't make.
fn raw(key: &SigningKey, manifest: &serde_json::Value, rest: &[&[u8]]) -> Vec<u8> {
    raw_text(key, &serde_json::to_vec(manifest).unwrap(), rest)
}

fn raw_text(key: &SigningKey, manifest: &[u8], rest: &[&[u8]]) -> Vec<u8> {
    let prefix = signed_prefix(manifest.len() as u16, &key.public_key());
    let signature = key
        .sign(&firmware_package::signed_bytes(&prefix, manifest))
        .unwrap();
    let mut out = [prefix.as_slice(), &signature, manifest].concat();
    for r in rest {
        out.extend_from_slice(r);
    }
    out
}

/// Case 41: the hub refuses uploads that are too large, truncated, have trailing bytes, a bad
/// signature, a wrong part hash, overlapping USB parts or a repeated member name.
#[tokio::test]
async fn the_hub_refuses_damaged_uploads() {
    let r = Rig::start().await;
    let k = key();
    let img = image(5000, 3);
    let boot = image(2000, 4);
    let table = image(300, 5);
    let manifest = |parts: serde_json::Value| {
        json!({
            "device_type": store::DEVICE_TYPE,
            "hardware_revisions": [store::HARDWARE_REVISION],
            "firmware_version": "1.0.0",
            "security_counter": 1,
            "image_size": img.len(),
            "image_sha256": sha256(&img),
            "usb_install": {
                "chip": "esp32s3",
                "min_flash_bytes": 16 * 1024 * 1024,
                "app_offset": 65536,
                "parts": parts,
            }
        })
    };
    let part = |name: &str, offset: u64, bytes: &[u8]| json!({ "name": name, "offset": offset, "size": bytes.len(), "sha256": sha256(bytes) });
    let good = raw(
        &k,
        &manifest(json!([
            part("bootloader", 0, &boot),
            part("partition-table", 32768, &table)
        ])),
        &[&img, &boot, &table],
    );
    assert!(firmware_package::check(std::io::Cursor::new(&good), good.len() as u64).is_ok());
    let mut bad_signature = good.clone();
    bad_signature[OFFSET_SIGNATURE + 3] ^= 0x01;
    let mut trailing = good.clone();
    trailing.push(0);
    let truncated = good[..good.len() - 1].to_vec();
    let mut wrong_part = good.clone();
    *wrong_part.last_mut().unwrap() ^= 0x01;
    let overlapping = raw(
        &k,
        &manifest(json!([
            part("bootloader", 0, &boot),
            part("partition-table", 0, &table)
        ])),
        &[&img, &boot, &table],
    );
    let text = serde_json::to_string(&manifest(json!([part("bootloader", 0, &boot)]))).unwrap();
    let repeated = raw_text(
        &k,
        text.replacen('{', r#"{"summary":"one","summary":"two","#, 1)
            .as_bytes(),
        &[&img, &boot],
    );
    for (bytes, why) in [
        (bad_signature, "signature"),
        (trailing, "add up to"),
        (truncated, "add up to"),
        (wrong_part, "partition-table"),
        (overlapping, "overlap"),
        (repeated, "twice"),
    ] {
        let e = r.client().upload_firmware(&r.admin, bytes).await.unwrap_err();
        assert_eq!(e.code(), Some(ErrorCode::BadRequest), "{why}: {e}");
        assert!(e.to_string().contains(why), "{why}: {e}");
    }
    match r
        .client()
        .upload_firmware(
            &r.admin,
            vec![0u8; (hub_protocol::limits::MAX_FIRMWARE_PACKAGE_BYTES + 1) as usize],
        )
        .await
    {
        Err(e @ ClientError::Status { .. }) => assert_eq!(e.code(), Some(ErrorCode::PayloadTooLarge)),
        // The hub may close the connection before the whole body is sent.
        Err(e) => assert!(e.is_connection(), "{e:?}"),
        Ok(_) => panic!("accepted a package over the limit"),
    }
    assert!(r.client().firmware_packages(&r.admin).await.unwrap().is_empty());
    r.client().upload_firmware(&r.admin, good).await.unwrap();
    r.stop().await;
}

fn sha256(b: &[u8]) -> String {
    hex::encode(ring::digest::digest(&ring::digest::SHA256, b).as_ref())
}

/// Case 38: a factory reset keeps the keys and the floor, while erasing the hub details.
#[tokio::test]
async fn a_factory_reset_keeps_the_keys_and_the_floor() {
    let r = Rig::start().await;
    let maker = key();
    let sim = r.display("desk", &[&maker], 4).await;
    let before = sim.keys();
    paired::reset(&sim.dir).unwrap();
    assert!(paired::load(&sim.dir).is_err());
    assert!(!usb::is_provisioned(&sim.dir));
    assert_eq!(sim.keys(), before);
    let info = paired::device_info(&sim.dir).firmware.unwrap();
    assert_eq!(
        (info.key_ids, info.security_floor),
        (vec![maker.public_key().key_id().to_hex()], 4)
    );
    r.stop().await;
}

/// The app's Install firmware: everything is erased, the image is written to the first slot, and
/// the bundle the app then sends sets the keys and floor from the package. After that only another
/// Install firmware can change them.
#[tokio::test]
async fn install_firmware_wipes_everything_and_the_bundle_sets_the_keys() {
    let r = Rig::start().await;
    let old = key();
    let maker = key();
    let recovery = key();
    let sim = r.display("desk", &[&old], 7).await;
    assert!(paired::load(&sim.dir).is_ok());
    let mut rel = release("1.0.0", 2);
    rel.key_set = Some(vec![maker.public_key(), recovery.public_key()]);
    let built = build_and_sign(&maker, &rel, &image(10_000, 9)).unwrap();

    std::fs::write(sim.dir.join("notes.txt"), "not the display's").unwrap();
    store::install_firmware(&sim.dir, &built.checked, &built.bytes).unwrap();
    assert!(
        sim.dir.join("notes.txt").exists(),
        "only the display's own files are erased"
    );
    std::fs::remove_file(sim.dir.join("notes.txt")).unwrap();
    assert!(paired::load(&sim.dir).is_err(), "hub details erased");
    assert!(!paired::cache_file(&sim.dir).exists());
    assert_eq!(sim.keys(), store::KeyStore::default(), "keys and floor erased");
    assert_eq!(sim.version(), "1.0.0");
    assert_eq!(
        store::board(&sim.dir).unwrap().slot_bytes,
        SLOT,
        "the board is the hardware"
    );
    assert_eq!(store::record(&sim.dir).unwrap(), store::Record::default());
    let mut names: Vec<_> = std::fs::read_dir(&sim.dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(names, ["firmware"]);

    // The app's bundle: the package's key_set and counter. A display with no keys takes them.
    let section = store::bundle_for(&built.checked);
    assert_eq!(section.security_counter, 2);
    store::accept_bundle(&sim.dir, Some(&section)).unwrap().unwrap();
    let ids = |keys: &[&SigningKey]| {
        keys.iter()
            .map(|k| k.public_key().key_id().to_hex())
            .collect::<Vec<_>>()
    };
    assert_eq!(sim.keys().key_ids(), ids(&[&maker, &recovery]));
    assert_eq!(sim.keys().security_floor, 2);
    // Without a key_set, the signer is the one key.
    let plain = package(&maker, "1.0.1", 2);
    assert_eq!(
        store::bundle_for(&plain.checked).trusted_keys,
        [maker.public_key().to_hex()]
    );
    // Another set of keys is now refused.
    let other = BundleFirmware {
        trusted_keys: vec![old.public_key().to_hex()],
        security_counter: 0,
    };
    assert!(store::accept_bundle(&sim.dir, Some(&other)).unwrap().is_err());
    r.stop().await;
}

/// The display's USB answers, over an in-memory port: what `hello` reports, and cases 39 and 40.
mod over_usb {
    use super::*;
    use hub_protocol::provisioning::Bundle;
    use hub_protocol::usb::{encode, FromDisplay, ToDisplay, UsbErrorCode};

    /// Everything the manager sends, up front; everything the display answers, collected.
    struct Port {
        input: std::io::Cursor<Vec<u8>>,
        output: Vec<u8>,
    }

    impl std::io::Read for Port {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.input.read(buf)
        }
    }

    impl std::io::Write for Port {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.output.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn session(dir: &Path, sent: &[ToDisplay]) -> Vec<FromDisplay> {
        let mut port = Port {
            input: std::io::Cursor::new(sent.iter().flat_map(encode).collect()),
            output: Vec::new(),
        };
        let _ = usb::answer(&mut port, dir).unwrap();
        String::from_utf8(port.output)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    fn bundle(keys: Option<(&[&SigningKey], u32)>) -> Bundle {
        let mut b: Bundle = serde_json::from_str(include_str!(
            "../../../protocol/v1/examples/device/provisioning-bundle.json"
        ))
        .unwrap();
        b.firmware = keys.map(|(keys, counter)| BundleFirmware {
            trusted_keys: keys.iter().map(|k| k.public_key().to_hex()).collect(),
            security_counter: counter,
        });
        b
    }

    fn provision(b: Bundle) -> ToDisplay {
        ToDisplay::Provision { bundle: b }
    }

    #[test]
    fn bundles_follow_what_each_action_may_change() {
        let tmp = tempfile::tempdir().unwrap();
        let (maker, recovery, stranger) = (key(), key(), key());

        // Case 40: a display with no keys takes the bundle's, and its floor.
        let fresh = tmp.path().join("fresh");
        let hello = ToDisplay::Hello { protocol_version: 1 };
        let answers = session(
            &fresh,
            &[hello.clone(), provision(bundle(Some((&[&maker, &recovery], 3))))],
        );
        let FromDisplay::Hello { device, provisioned } = &answers[0] else {
            panic!("{answers:?}")
        };
        assert!(!provisioned);
        let reported = device.firmware.clone().unwrap();
        assert_eq!(
            (reported.key_ids.len(), reported.slot_bytes),
            (0, store::DEFAULT_SLOT_BYTES)
        );
        assert_eq!(answers[1], FromDisplay::Provisioned);
        let keys = store::keys(&fresh).unwrap();
        assert_eq!((keys.trusted_keys.len(), keys.security_floor), (2, 3));
        // It reports them from now on.
        let answers = session(&fresh, std::slice::from_ref(&hello));
        let FromDisplay::Hello { device, provisioned } = &answers[0] else {
            panic!()
        };
        assert!(provisioned);
        assert_eq!(device.firmware.as_ref().unwrap().key_ids.len(), 2);

        // Case 39: after a factory reset, different keys are refused and the same set is accepted.
        for (sent, answer) in [
            (
                bundle(Some((&[&stranger], 0))),
                Err(UsbErrorCode::FirmwareKeysLocked),
            ),
            (
                bundle(Some((&[&maker], 3))),
                Err(UsbErrorCode::FirmwareKeysLocked),
            ),
            (bundle(Some((&[&recovery, &maker], 9))), Ok(())),
            (bundle(None), Ok(())),
        ] {
            paired::reset(&fresh).unwrap();
            let got = session(&fresh, &[provision(sent)]);
            match answer {
                Ok(()) => assert_eq!(got, [FromDisplay::Provisioned]),
                Err(code) => {
                    assert!(
                        matches!(&got[0], FromDisplay::Error { code: c, .. } if *c == code),
                        "{got:?}"
                    );
                    assert!(!usb::is_provisioned(&fresh), "nothing stored");
                }
            }
            assert_eq!(store::keys(&fresh).unwrap(), keys, "keys and floor unchanged");
        }

        // Case 40 again: a display with no keys and a bundle without any still has none.
        let bare = tmp.path().join("bare");
        assert_eq!(
            session(&bare, &[provision(bundle(None))]),
            [FromDisplay::Provisioned]
        );
        assert_eq!(store::keys(&bare).unwrap(), store::KeyStore::default());
    }
}
