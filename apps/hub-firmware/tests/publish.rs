//! `hub-firmware login`, `add` and `publish` against a real hub over pinned HTTPS, with the token
//! kept in a file. The file store is for Unix only, so these run there. Every key is made here, in a
//! temporary folder, and deleted with it.
#![cfg(unix)]

use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::sync::Arc;

use base64::Engine;
use hub_protocol::pairing::PairingString;

const BIN: &str = env!("CARGO_BIN_EXE_hub-firmware");
const PASSPHRASE: &str = "a long and memorable passphrase";

fn run(config: &Path, args: &[&str], stdin: &str) -> Output {
    let mut child = Command::new(BIN)
        .arg("--config-dir")
        .arg(config)
        .args(["--store", "file"])
        .args(args)
        .env_remove("HUB_FIRMWARE_DIR")
        .env_remove("HUB_FIRMWARE_STORE")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let _ = child.stdin.take().unwrap().write_all(stdin.as_bytes());
    child.wait_with_output().unwrap()
}

fn text(o: &Output) -> (String, String) {
    (
        String::from_utf8_lossy(&o.stdout).into_owned(),
        String::from_utf8_lossy(&o.stderr).into_owned(),
    )
}

fn p(path: &Path) -> &str {
    path.to_str().unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn login_add_and_publish_reach_the_hubs_shelf() {
    let root = tempfile::tempdir().unwrap();
    let hub = hubd::open_hub(
        hubd::config::Config {
            data_dir: root.path().join("hub"),
            ..Default::default()
        },
        Arc::new(hub_core::SystemClock),
    )
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(hubd::serve_https(hub.clone(), listener, async {
        let _ = rx.await;
    }));
    let invite = hub.new_publisher_invite("Build machine", "test").unwrap();
    let mut secret = [0u8; 16];
    secret.copy_from_slice(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(&invite.secret)
            .unwrap(),
    );
    let publisher_string = PairingString {
        host: "127.0.0.1".into(),
        port,
        spki_sha256: hub.identity.spki_sha256,
        secret,
    }
    .encode();

    let dir = root.path().to_path_buf();
    let cfg = dir.join("tool");
    let outputs = tokio::task::spawn_blocking(move || {
        let mut out = Vec::new();
        // Not paired yet: add says what to do.
        let pkg_dir = dir.join("work");
        std::fs::create_dir_all(&pkg_dir).unwrap();
        out.push(run(&cfg, &["add", p(&pkg_dir.join("none.dhfw"))], ""));
        out.push(run(&cfg, &["login"], &format!("  {publisher_string}\n")));
        // Used once only.
        out.push(run(&cfg, &["login"], &format!("{publisher_string}\n")));

        let key = pkg_dir.join("maker.dhkey");
        out.push(run(
            &cfg,
            &["keygen", "--out", p(&key)],
            &format!("{PASSPHRASE}\n"),
        ));
        let image = pkg_dir.join("app.bin");
        std::fs::write(&image, vec![0x5au8; 4000]).unwrap();
        let sign = |version: &str, out: &Path| {
            vec![
                "--key".to_string(),
                p(&key).into(),
                "--image".into(),
                p(&image).into(),
                "--device-type".into(),
                "llm-usage-display".into(),
                "--hardware".into(),
                "waveshare-amoled-1.8-v2".into(),
                "--version".into(),
                version.into(),
                "--security-counter".into(),
                "1".into(),
                "--out".into(),
                p(out).into(),
            ]
        };
        let first = pkg_dir.join("first.dhfw");
        let mut args = vec!["sign".to_string()];
        args.extend(sign("1.0.0", &first));
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        out.push(run(&cfg, &args, &format!("{PASSPHRASE}\n")));
        out.push(run(&cfg, &["add", p(&first)], ""));
        out.push(run(&cfg, &["add", p(&first)], ""));
        // A damaged package is caught before it is sent.
        let mut bytes = std::fs::read(&first).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
        let damaged = pkg_dir.join("damaged.dhfw");
        std::fs::write(&damaged, bytes).unwrap();
        out.push(run(&cfg, &["add", p(&damaged)], ""));

        let second = pkg_dir.join("second.dhfw");
        let mut args = vec!["publish".to_string()];
        args.extend(sign("1.1.0", &second));
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        out.push(run(&cfg, &args, &format!("{PASSPHRASE}\n")));
        (out, cfg)
    })
    .await
    .unwrap();
    let (out, cfg) = outputs;
    let t: Vec<(String, String)> = out.iter().map(text).collect();

    assert!(!out[0].status.success());
    assert!(t[0].1.contains("isn't paired with a hub"), "{}", t[0].1);
    assert!(out[1].status.success(), "{}", t[1].1);
    assert!(t[1].0.contains("Paired with the hub at 127.0.0.1"), "{}", t[1].0);
    assert!(!out[2].status.success());
    assert!(t[2].1.contains("already used"), "{}", t[2].1);
    // The token is kept owner-only, and never printed.
    {
        use std::os::unix::fs::PermissionsExt;
        let token = std::fs::read_to_string(cfg.join("token")).unwrap();
        assert!(token.starts_with("dhp_"));
        let mode = std::fs::metadata(cfg.join("token")).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        for (o, e) in &t {
            assert!(!o.contains(token.trim()) && !e.contains(token.trim()));
        }
    }
    assert!(out[3].status.success(), "{}", t[3].1);
    assert!(out[4].status.success(), "{}", t[4].1);
    assert!(out[5].status.success(), "{}", t[5].1);
    assert!(t[5].0.starts_with("Added llm-usage-display 1.0.0"), "{}", t[5].0);
    assert!(out[6].status.success(), "{}", t[6].1);
    assert!(t[6].0.starts_with("The hub already had"), "{}", t[6].0);
    assert!(!out[7].status.success());
    assert!(t[7].1.contains("isn't a valid package"), "{}", t[7].1);
    assert!(out[8].status.success(), "{}", t[8].1);
    assert!(t[8].0.contains("Signed llm-usage-display 1.1.0"), "{}", t[8].0);
    assert!(t[8].0.contains("Added llm-usage-display 1.1.0"), "{}", t[8].0);

    let mut shelf: Vec<(String, String)> = hub
        .store
        .firmware_packages()
        .unwrap()
        .into_iter()
        .map(|p| (p.firmware_version, p.uploaded_by_role.as_str().to_string()))
        .collect();
    shelf.sort();
    assert_eq!(
        shelf,
        [
            ("1.0.0".to_string(), "publisher".to_string()),
            ("1.1.0".to_string(), "publisher".to_string())
        ]
    );
    let _ = tx.send(());
    server.await.unwrap().unwrap();
}
