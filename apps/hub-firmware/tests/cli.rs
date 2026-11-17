//! `hub-firmware` end to end: keygen, sign and inspect, with passphrases fed through standard input.
//! Every key is made here, in a temporary folder, and deleted with it.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

const BIN: &str = env!("CARGO_BIN_EXE_hub-firmware");
const PASSPHRASE: &str = "a long and memorable passphrase";

fn run(args: &[&str], stdin: &str) -> Output {
    let mut child = Command::new(BIN)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // The tool may exit before reading, which is fine.
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

/// The fingerprint line keygen printed.
fn fingerprint(stdout: &str) -> String {
    stdout
        .lines()
        .find_map(|l| l.strip_prefix("Fingerprint: "))
        .unwrap()
        .trim()
        .to_string()
}

/// A stand-in for an ESP-IDF app image: the image magic, and the app description block with its
/// version, where ESP-IDF puts them.
fn esp_image(version: &str) -> Vec<u8> {
    let mut img = vec![0x5au8; 6000];
    img[0] = 0xE9;
    img[32..36].copy_from_slice(&0xABCD_5432u32.to_le_bytes());
    img[48..80].fill(0);
    img[48..48 + version.len()].copy_from_slice(version.as_bytes());
    img
}

fn keygen(dir: &Path, name: &str) -> (std::path::PathBuf, String) {
    let path = dir.join(name);
    let o = run(&["keygen", "--out", p(&path)], &format!("{PASSPHRASE}\n"));
    let (out, err) = text(&o);
    assert!(o.status.success(), "{err}");
    (path, fingerprint(&out))
}

#[test]
fn keygen_sign_and_inspect() {
    let dir = tempfile::tempdir().unwrap();
    let (key, fp) = keygen(dir.path(), "maker.dhkey");
    assert_eq!(fp.len(), 39);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&key).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
    // The passphrase never appears anywhere in the file.
    let contents = std::fs::read_to_string(&key).unwrap();
    assert!(!contents.contains(PASSPHRASE));

    let image = dir.path().join("app.bin");
    std::fs::write(&image, esp_image("1.2.0")).unwrap();
    let out = dir.path().join("llm-usage-display-1.2.0.dhfw");
    let o = run(
        &[
            "sign",
            "--key",
            p(&key),
            "--image",
            p(&image),
            "--device-type",
            "llm-usage-display",
            "--hardware",
            "waveshare-amoled-1.8-v1",
            "--hardware",
            "waveshare-amoled-1.8-v2",
            "--version",
            "1.2.0",
            "--security-counter",
            "3",
            "--summary",
            "Fixes the clock drifting after a long outage.",
            "--out",
            p(&out),
        ],
        &format!("{PASSPHRASE}\n"),
    );
    let (stdout, stderr) = text(&o);
    assert!(o.status.success(), "{stderr}");
    assert!(!stderr.contains("warning"), "{stderr}");
    assert!(stdout.contains(&fp), "{stdout}");

    let checked = firmware_package::check_file(&out).unwrap();
    assert_eq!(checked.signer().fingerprint(), fp);
    assert_eq!(checked.manifest.security_counter, 3);
    assert_eq!(checked.manifest.hardware_revisions.len(), 2);
    assert!(stdout.contains(&checked.package_id.to_hex()), "{stdout}");

    let o = run(&["inspect", p(&out)], "");
    let (stdout, stderr) = text(&o);
    assert!(o.status.success(), "{stderr}");
    assert!(stdout.contains(&format!("Signer fingerprint: {fp}")), "{stdout}");
    assert!(stdout.contains("Key set: none"), "{stdout}");
    assert!(stdout.contains(&format!("Package id: {}", checked.package_id.to_hex())));
    assert!(
        stdout.contains("\"device_type\": \"llm-usage-display\""),
        "{stdout}"
    );
    assert!(stdout.contains("\"summary\": \"Fixes the clock drifting after a long outage.\""));

    // A tampered package is refused by inspect.
    let mut bytes = std::fs::read(&out).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 1;
    let tampered = dir.path().join("tampered.dhfw");
    std::fs::write(&tampered, &bytes).unwrap();
    let o = run(&["inspect", p(&tampered)], "");
    assert!(!o.status.success());
    assert!(text(&o).1.contains("image_sha256"), "{}", text(&o).1);
}

#[test]
fn sign_with_usb_parts_and_a_key_set() {
    let dir = tempfile::tempdir().unwrap();
    let (key, fp) = keygen(dir.path(), "everyday.dhkey");
    let (recovery, recovery_fp) = keygen(dir.path(), "recovery.dhkey");
    // A third key given as a bare public key in a text file.
    let (third, third_fp) = keygen(dir.path(), "third.dhkey");
    let third_public = firmware_package::keyfile::public_key(&std::fs::read(&third).unwrap()).unwrap();
    let third_text = dir.path().join("third.pub");
    std::fs::write(&third_text, format!("{}\n", third_public.to_hex())).unwrap();

    let image = dir.path().join("app.bin");
    std::fs::write(&image, esp_image("2.0.0")).unwrap();
    let boot = dir.path().join("bootloader.bin");
    std::fs::write(&boot, vec![1u8; 21072]).unwrap();
    let table = dir.path().join("partition-table.bin");
    std::fs::write(&table, vec![2u8; 3072]).unwrap();
    let out = dir.path().join("pkg.dhfw");
    let boot_spec = format!("bootloader@0x0={}", p(&boot));
    let table_spec = format!("partition-table@0x8000={}", p(&table));
    let o = run(
        &[
            "sign",
            "--key",
            p(&key),
            "--image",
            p(&image),
            "--device-type",
            "llm-usage-display",
            "--hardware",
            "waveshare-amoled-1.8-v2",
            "--version",
            "2.0.0",
            "--security-counter",
            "4",
            "--key-set",
            p(&key),
            "--key-set",
            p(&recovery),
            "--key-set",
            p(&third_text),
            "--usb-part",
            &boot_spec,
            "--usb-part",
            &table_spec,
            "--app-offset",
            "0x10000",
            "--chip",
            "esp32s3",
            "--min-flash",
            "16777216",
            "--out",
            p(&out),
        ],
        &format!("{PASSPHRASE}\n"),
    );
    let (stdout, stderr) = text(&o);
    assert!(o.status.success(), "{stderr}");
    let c = firmware_package::check_file(&out).unwrap();
    let ks: Vec<String> = c
        .manifest
        .key_set
        .unwrap()
        .iter()
        .map(|k| k.fingerprint())
        .collect();
    assert_eq!(ks, [fp, recovery_fp, third_fp]);
    let usb = c.manifest.usb_install.unwrap();
    assert_eq!(usb.app_offset, 0x10000);
    assert_eq!(usb.parts[1].offset, 0x8000);
    assert_eq!(usb.parts[0].size, 21072);
    assert!(
        stdout.contains(&format!("{} bytes go to a display", c.len - 21072 - 3072)),
        "{stdout}"
    );
    // Every key displays will trust afterwards is shown, by sign and by inspect, and as the set
    // includes the signing key there is no warning.
    assert!(!stderr.contains("warning"), "{stderr}");
    let listed = format!(
        "Keys displays will trust after this release (key_set):\n  {}\n  {}\n  {}",
        ks[0], ks[1], ks[2]
    );
    assert!(stdout.contains(&listed), "{stdout}");
    let o = run(&["inspect", p(&out)], "");
    assert!(o.status.success());
    assert!(text(&o).0.contains(&listed), "{}", text(&o).0);

    // A key set that leaves out the signing key is signed, with a warning to raise the counter.
    let retired = dir.path().join("retired.dhfw");
    let o = run(
        &[
            "sign",
            "--key",
            p(&key),
            "--image",
            p(&image),
            "--device-type",
            "llm-usage-display",
            "--hardware",
            "waveshare-amoled-1.8-v2",
            "--version",
            "2.0.0",
            "--security-counter",
            "5",
            "--key-set",
            p(&recovery),
            "--out",
            p(&retired),
        ],
        &format!("{PASSPHRASE}\n"),
    );
    let (stdout, stderr) = text(&o);
    assert!(o.status.success(), "{stderr}");
    assert!(stderr.contains("leaves out the signing key"), "{stderr}");
    assert!(stderr.contains("--security-counter (5)"), "{stderr}");
    assert!(
        stdout.contains(&format!(
            "Keys displays will trust after this release (key_set):\n  {}\n",
            ks[1]
        )),
        "{stdout}"
    );
}

#[test]
fn sign_warns_about_the_image_but_still_signs() {
    let dir = tempfile::tempdir().unwrap();
    let (key, _) = keygen(dir.path(), "k.dhkey");
    let mismatched = dir.path().join("old.bin");
    std::fs::write(&mismatched, esp_image("1.1.0")).unwrap();
    let not_esp = dir.path().join("readme.bin");
    std::fs::write(&not_esp, b"this is not firmware").unwrap();
    for (image, expected) in [(&mismatched, "\"1.1.0\""), (&not_esp, "ESP-IDF app image")] {
        let out = dir.path().join("out.dhfw");
        let o = run(
            &[
                "sign",
                "--key",
                p(&key),
                "--image",
                p(image),
                "--device-type",
                "d",
                "--hardware",
                "h",
                "--version",
                "1.2.0",
                "--security-counter",
                "0",
                "--out",
                p(&out),
            ],
            &format!("{PASSPHRASE}\n"),
        );
        let (_, stderr) = text(&o);
        assert!(o.status.success(), "{stderr}");
        assert!(stderr.contains("warning"), "{stderr}");
        assert!(stderr.contains(expected), "{stderr}");
    }
}

#[test]
fn refusals() {
    let dir = tempfile::tempdir().unwrap();
    let (key, _) = keygen(dir.path(), "k.dhkey");
    let before = std::fs::read(&key).unwrap();

    // keygen never overwrites, and refuses a short passphrase.
    let o = run(&["keygen", "--out", p(&key)], &format!("{PASSPHRASE}\n"));
    assert!(!o.status.success());
    assert!(text(&o).1.contains("already exists"), "{}", text(&o).1);
    assert_eq!(std::fs::read(&key).unwrap(), before);
    let short = dir.path().join("short.dhkey");
    let o = run(&["keygen", "--out", p(&short)], "too short\n");
    assert!(!o.status.success());
    assert!(text(&o).1.contains("at least 12 characters"));
    assert!(!short.exists());
    let o = run(&["keygen", "--out", p(&short)], "");
    assert!(!o.status.success());
    assert!(!short.exists());

    let image = dir.path().join("app.bin");
    std::fs::write(&image, esp_image("1.0.0")).unwrap();
    let out = dir.path().join("out.dhfw");
    let base = |extra: &[&str]| {
        let mut a = vec![
            "sign",
            "--key",
            p(&key),
            "--image",
            p(&image),
            "--device-type",
            "d",
            "--hardware",
            "h",
            "--version",
            "1.0.0",
            "--security-counter",
            "1",
            "--out",
            p(&out),
        ];
        a.extend_from_slice(extra);
        a.into_iter().map(String::from).collect::<Vec<_>>()
    };
    let sign = |extra: &[&str], pass: &str| {
        let args = base(extra);
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        run(&refs, pass)
    };

    // A wrong passphrase writes nothing.
    let o = sign(&[], "not the right passphrase\n");
    assert!(!o.status.success());
    assert!(text(&o).1.contains("passphrase is wrong"), "{}", text(&o).1);
    assert!(!out.exists());

    // Part of a USB layout.
    let o = sign(&["--chip", "esp32s3"], &format!("{PASSPHRASE}\n"));
    assert!(!o.status.success());
    assert!(text(&o)
        .1
        .contains("--usb-part, --app-offset, --chip and --min-flash"));

    // A badly written part, and an unaligned one.
    let o = sign(
        &[
            "--usb-part",
            "boot",
            "--app-offset",
            "0x10000",
            "--chip",
            "esp32",
            "--min-flash",
            "4194304",
        ],
        &format!("{PASSPHRASE}\n"),
    );
    assert!(!o.status.success());
    assert!(text(&o).1.contains("name@offset=file"));
    let part = format!("boot@0x100={}", p(&image));
    let o = sign(
        &[
            "--usb-part",
            &part,
            "--app-offset",
            "0x10000",
            "--chip",
            "esp32",
            "--min-flash",
            "4194304",
        ],
        &format!("{PASSPHRASE}\n"),
    );
    assert!(!o.status.success());
    assert!(text(&o).1.contains("multiple of 4096"), "{}", text(&o).1);

    // A bad version, which the manifest rules refuse.
    let mut args = base(&[]);
    let v = args.iter().position(|a| a == "1.0.0").unwrap();
    args[v] = "v1".into();
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let o = run(&refs, &format!("{PASSPHRASE}\n"));
    assert!(!o.status.success());
    assert!(text(&o).1.contains("firmware_version"), "{}", text(&o).1);
    assert!(!out.exists());

    // A key-set file that is neither kind.
    let junk = dir.path().join("junk.pub");
    std::fs::write(&junk, "hello").unwrap();
    let o = sign(&["--key-set", p(&junk)], &format!("{PASSPHRASE}\n"));
    assert!(!o.status.success());
    assert!(text(&o).1.contains("neither a key file nor a public key"));

    // inspect on something that isn't a package.
    let o = run(&["inspect", p(&image)], "");
    assert!(!o.status.success());
    assert!(text(&o).1.contains("isn't a valid package"));
}
