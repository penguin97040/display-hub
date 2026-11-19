//! The desktop app's USB sender against the firmware's USB handler, over a real pseudo-terminal.
//!
//!   dh-usb-e2e <path to dh_usb_pty_display>
//!
//! It walks through what an owner does with a real board: say hello, set the display up, find it
//! still set up after a restart, be refused a second setup, factory-reset it, be refused other
//! firmware keys, and set it up again with the same keys.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use hub_client::usb::{hello, provision, UsbError};
use hub_protocol::firmware::BundleFirmware;
use hub_protocol::provisioning::Bundle;
use hub_protocol::usb::BAUD_RATE;

/// Two public keys from `protocol/v1/firmware`'s vectors ("maker" and "stranger").
const MAKER: &str = "04795c666d0cdda58bfb27c60b936bba8ad0c8801218dcb6d5f50a73c00a9904229fa366102a70677d0ca5410d6300b8273e102bba8920cef822483339734b7eb3";
const STRANGER: &str = "04d60f588ff97c238828d8b662100faf6cdc3a873a57458203b81eb0ccc498d8518590614975395bf016774b5570df2ad3f95b46e6107ae5ab5d9319e40711849c";

/// The display program, answering on a pty until dropped.
struct Display {
    child: Child,
    port: String,
}

impl Display {
    fn start(program: &Path, state: &Path) -> Self {
        let mut child = Command::new(program)
            .arg(state)
            .stdout(Stdio::piped())
            .spawn()
            .expect("the display program starts");
        let mut port = String::new();
        BufReader::new(child.stdout.take().expect("stdout"))
            .read_line(&mut port)
            .expect("the display program prints its port");
        Display {
            child,
            port: port.trim().to_string(),
        }
    }

    /// Opens the port as the app does (`send_usb` in apps/hub-manager/src/worker.rs).
    fn open(&self) -> Box<dyn serialport::SerialPort> {
        let mut p = serialport::new(&self.port, BAUD_RATE)
            .timeout(Duration::from_millis(100))
            .dtr_on_open(true)
            .open()
            .expect("the pty opens as a serial port");
        let _ = p.write_request_to_send(false);
        p
    }
}

impl Drop for Display {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn bundle(keys: Option<&[&str]>) -> Bundle {
    let mut b: Bundle = serde_json::from_str(include_str!(
        "../../../../protocol/v1/examples/device/provisioning-bundle.json"
    ))
    .expect("the example bundle parses");
    b.firmware = keys.map(|keys| BundleFirmware {
        trusted_keys: keys.iter().map(|k| k.to_string()).collect(),
        security_counter: 3,
    });
    assert!(b.problems().is_empty());
    b
}

fn main() {
    let program = PathBuf::from(
        std::env::args()
            .nth(1)
            .expect("usage: dh-usb-e2e <path to dh_usb_pty_display>"),
    );
    let state = std::env::temp_dir().join(format!("dh-usb-e2e-{}", std::process::id()));
    std::fs::create_dir_all(&state).expect("a state folder");

    // A display fresh from a first install: it says who it is, and that it isn't set up.
    let display = Display::start(&program, &state);
    let seen = hello(&mut display.open()).expect("hello is answered");
    assert_eq!(seen.device.device_type, "display-hub-waveshare-amoled-1.8");
    assert_eq!(seen.device.hardware_revision, "waveshare-amoled-1.8-v2");
    assert_eq!(seen.device.protocol_version, 1);
    assert!(seen.device.problems().is_empty(), "{:?}", seen.device.problems());
    assert!(!seen.provisioned);
    let fw = seen.device.firmware.expect("a firmware member");
    assert!(fw.problems().is_empty(), "{:?}", fw.problems());
    assert_eq!(
        (fw.key_ids.len(), fw.security_floor, fw.slot_bytes),
        (0, 0, 6291456)
    );
    assert_eq!(fw.attempts_left, Some(5));
    println!("ok: hello from a new display");

    // A bundle that breaks a rule is refused, and the display stays as it was.
    let mut bad = bundle(Some(&[MAKER]));
    bad.hub.port = 0;
    match provision(&mut display.open(), &bad) {
        Err(UsbError::InvalidBundle(message)) => assert!(message.contains("port"), "{message}"),
        other => panic!("expected invalid_bundle, got {other:?}"),
    }
    println!("ok: a bundle with port 0 is refused as invalid_bundle");

    // The real thing: hello, then the bundle, as Add a display sends them.
    provision(&mut display.open(), &bundle(Some(&[MAKER]))).expect("the bundle is accepted");
    let seen = hello(&mut display.open()).expect("hello");
    assert!(seen.provisioned);
    let fw = seen.device.firmware.expect("a firmware member");
    assert!(fw.problems().is_empty());
    assert_eq!((fw.key_ids.len(), fw.security_floor), (1, 3));
    let maker_id = fw.key_ids[0].clone();
    println!("ok: provisioned, and hello now reports the key and the floor");

    // Sent again: refused. (The app's sender stops at hello, which already says so.)
    assert!(matches!(
        provision(&mut display.open(), &bundle(Some(&[MAKER]))),
        Err(UsbError::AlreadyProvisioned)
    ));
    println!("ok: a second bundle is refused as already set up");

    // A restart: still set up.
    drop(display);
    let display = Display::start(&program, &state);
    assert!(hello(&mut display.open()).expect("hello").provisioned);
    println!("ok: still set up after a restart");

    // A factory reset: not set up, and the key and the floor are still there.
    drop(display);
    let reset = Command::new(&program)
        .arg(&state)
        .arg("--factory-reset")
        .status()
        .expect("the reset runs");
    assert!(reset.success());
    let display = Display::start(&program, &state);
    let seen = hello(&mut display.open()).expect("hello");
    assert!(!seen.provisioned);
    let fw = seen.device.firmware.expect("a firmware member");
    assert_eq!((fw.key_ids, fw.security_floor), (vec![maker_id], 3));
    println!("ok: a factory reset erases the setup and keeps the firmware keys");

    // Other keys are locked out; the same keys set it up again.
    assert!(matches!(
        provision(&mut display.open(), &bundle(Some(&[STRANGER]))),
        Err(UsbError::FirmwareKeysLocked)
    ));
    provision(&mut display.open(), &bundle(Some(&[MAKER]))).expect("the same keys are accepted");
    assert!(hello(&mut display.open()).expect("hello").provisioned);
    println!("ok: other keys are refused as firmware_keys_locked; the same keys are accepted");

    drop(display);
    let _ = std::fs::remove_dir_all(&state);

    same_verdicts_as_the_reference(&program);
    println!("all passed");
}

/// Bundles at the edge of each rule, sent to a display with no keys: the firmware accepts exactly
/// those the reference's own `Bundle::problems()` finds nothing wrong with.
fn same_verdicts_as_the_reference(program: &Path) {
    let state = std::env::temp_dir().join(format!("dh-usb-e2e-rules-{}", std::process::id()));
    std::fs::create_dir_all(&state).expect("a state folder");
    let display = Display::start(program, &state);

    let base = || bundle(None);
    let pem = |len: usize| format!("-----BEGIN CERTIFICATE-----{}", "A".repeat(len - 27));
    let mut cases: Vec<(String, Bundle)> = Vec::new();
    let mut case = |name: &str, change: &dyn Fn(&mut Bundle)| {
        let mut b = base();
        change(&mut b);
        cases.push((name.to_string(), b));
    };

    case("as it is", &|_| {});
    case("another format", &|b| b.format = "display-hub.other".into());
    case("version 2", &|b| b.version = 2);
    case("no hosts", &|b| b.hub.hosts.clear());
    case("four hosts", &|b| {
        b.hub.hosts = vec!["a".into(), "b".into(), "c".into(), "d".into()]
    });
    case("five hosts", &|b| b.hub.hosts = vec!["a".into(); 5]);
    case("an empty host", &|b| b.hub.hosts = vec![String::new()]);
    case("a host of 253 bytes", &|b| b.hub.hosts = vec!["h".repeat(253)]);
    case("a host of 254 bytes", &|b| b.hub.hosts = vec!["h".repeat(254)]);
    case("a host of 253 bytes in two-byte characters", &|b| {
        b.hub.hosts = vec![format!("{}h", "\u{e9}".repeat(126))]
    });
    case("a host of 254 bytes in two-byte characters", &|b| {
        b.hub.hosts = vec!["\u{e9}".repeat(127)]
    });
    for (name, c) in [
        ("a space", ' '),
        ("a tab", '\t'),
        ("a NUL", '\0'),
        ("DEL", '\u{7f}'),
        ("a C1 control", '\u{9f}'),
        ("NEL", '\u{85}'),
        ("a no-break space", '\u{a0}'),
        ("an em space", '\u{2003}'),
        ("a line separator", '\u{2028}'),
        ("an ideographic space", '\u{3000}'),
        ("a zero-width space", '\u{200b}'),
        ("a soft hyphen", '\u{ad}'),
        ("a byte order mark", '\u{feff}'),
        ("an accented letter", '\u{e9}'),
    ] {
        case(&format!("a host with {name}"), &|b| {
            b.hub.hosts = vec![format!("a{c}b")]
        });
    }
    case("port 0", &|b| b.hub.port = 0);
    case("port 65535", &|b| b.hub.port = 65535);
    case("another server name", &|b| {
        b.hub.server_name = "hub.example".into()
    });
    case("no anchors", &|b| b.hub.trust_anchors.clear());
    case("two anchors", &|b| {
        b.hub.trust_anchors.push(b.hub.trust_anchors[0].clone())
    });
    case("three anchors", &|b| {
        b.hub.trust_anchors = vec![b.hub.trust_anchors[0].clone(); 3]
    });
    case("a PEM that isn't one", &|b| {
        b.hub.trust_anchors[0].certificate_pem = "MIIB".into()
    });
    case("a PEM of only its first line", &|b| {
        b.hub.trust_anchors[0].certificate_pem = pem(27)
    });
    case("a PEM of 4096 bytes", &|b| {
        b.hub.trust_anchors[0].certificate_pem = pem(4096)
    });
    case("a PEM of 4097 bytes", &|b| {
        b.hub.trust_anchors[0].certificate_pem = pem(4097)
    });
    case("a PEM with control characters", &|b| {
        b.hub.trust_anchors[0].certificate_pem = format!("{}\0\u{1}\r\n\u{7f}", pem(27))
    });
    case("an upper-case SPKI hash", &|b| {
        b.hub.trust_anchors[0].spki_sha256 = b.hub.trust_anchors[0].spki_sha256.to_uppercase()
    });
    case("a short SPKI hash", &|b| {
        b.hub.trust_anchors[0].spki_sha256 = "ab".repeat(31)
    });
    case("a long SPKI hash", &|b| {
        b.hub.trust_anchors[0].spki_sha256 = "ab".repeat(33)
    });
    case("an empty secret", &|b| b.enrolment.secret = String::new());
    case("a secret of 64 bytes", &|b| b.enrolment.secret = "s".repeat(64));
    case("a secret of 65 bytes", &|b| b.enrolment.secret = "s".repeat(65));
    case("a secret of 64 bytes in 32 characters", &|b| {
        b.enrolment.secret = "\u{e9}".repeat(32)
    });
    case("a secret of 66 bytes in 33 characters", &|b| {
        b.enrolment.secret = "\u{e9}".repeat(33)
    });
    case("no Wi-Fi", &|b| b.wifi = None);
    let wifi = |ssid: String, password: String| {
        move |b: &mut Bundle| {
            let w = b.wifi.as_mut().expect("the example has Wi-Fi");
            w.ssid = ssid.clone();
            w.password = password.clone();
        }
    };
    case("an empty SSID", &wifi(String::new(), "password".into()));
    case("an SSID of 32 bytes", &wifi("n".repeat(32), "password".into()));
    case("an SSID of 33 bytes", &wifi("n".repeat(33), "password".into()));
    case(
        "an SSID of 33 bytes in 11 characters",
        &wifi("\u{20ac}".repeat(11), "password".into()),
    );
    case(
        "an SSID with a space and a NUL",
        &wifi("my net\0".into(), "password".into()),
    );
    for len in [0, 1, 7, 8, 63, 64] {
        case(
            &format!("a password of {len} bytes"),
            &wifi("n".into(), "p".repeat(len)),
        );
    }
    case(
        "a password of 8 bytes in 4 characters",
        &wifi("n".into(), "\u{e9}".repeat(4)),
    );
    case(
        "a password of 64 bytes in 32 characters",
        &wifi("n".into(), "\u{e9}".repeat(32)),
    );
    let firmware = |keys: Vec<String>| {
        move |b: &mut Bundle| {
            b.firmware = Some(BundleFirmware {
                trusted_keys: keys.clone(),
                security_counter: 0,
            })
        }
    };
    case("no firmware keys", &firmware(vec![]));
    case(
        "four firmware keys",
        &firmware(vec![MAKER.into(), STRANGER.into(), MAKER.into(), STRANGER.into()]),
    );
    case(
        "a firmware key twice",
        &firmware(vec![MAKER.into(), MAKER.into()]),
    );
    case(
        "an upper-case firmware key",
        &firmware(vec![MAKER.to_uppercase()]),
    );
    case(
        "a firmware key starting 05",
        &firmware(vec![format!("05{}", &MAKER[2..])]),
    );
    case("a short firmware key", &firmware(vec![MAKER[..128].into()]));

    // The size limit, counted as the reference writes the bundle out: pad two PEMs with line feeds
    // (two bytes each when written) until the bundle is exactly at the limit, and one byte over.
    let sized = |target: usize| {
        let mut b = base();
        b.hub.trust_anchors.push(b.hub.trust_anchors[0].clone());
        for a in &mut b.hub.trust_anchors {
            a.certificate_pem = pem(27);
        }
        let size = |b: &Bundle| serde_json::to_vec(b).expect("serialises").len();
        let mut which = 0;
        while size(&b) + 2 <= target {
            if b.hub.trust_anchors[which].certificate_pem.len() >= 4096 {
                which += 1;
            }
            b.hub.trust_anchors[which].certificate_pem.push('\n');
        }
        if size(&b) < target {
            b.hub.trust_anchors[1].certificate_pem.push('A');
        }
        assert_eq!(size(&b), target);
        b
    };
    let limit = hub_protocol::provisioning::MAX_BUNDLE_BYTES;
    cases.push(("exactly at the size limit".into(), sized(limit)));
    cases.push(("one byte over the size limit".into(), sized(limit + 1)));

    let (mut accepted, mut refused) = (0, 0);
    for (name, b) in &cases {
        let want = b.problems();
        let got = provision(&mut display.open(), b);
        match (&got, want.is_empty()) {
            (Ok(_), true) => {
                accepted += 1;
                // Back to a display that isn't set up, for the next case. No case that is
                // accepted names firmware keys, so it still has none.
                let reset = Command::new(program)
                    .arg(&state)
                    .arg("--factory-reset")
                    .status()
                    .expect("the reset runs");
                assert!(reset.success());
            }
            (Err(UsbError::InvalidBundle(_)), false) => refused += 1,
            _ => panic!("{name}: the firmware said {got:?}, the reference's problems are {want:?}"),
        }
    }
    assert!(
        accepted >= 15 && refused >= 30,
        "{accepted} accepted, {refused} refused"
    );
    println!("ok: {accepted} bundles accepted and {refused} refused, each as the reference would");

    drop(display);
    let _ = std::fs::remove_dir_all(&state);
}
