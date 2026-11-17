//! `hub-firmware`: makes signing keys, and signs and checks firmware packages, for people who make
//! display firmware. See "The signing tool" in `docs/firmware-updates.md`.
//!
//! This build has the offline commands (`keygen`, `sign`, `inspect`). The commands that talk to a hub
//! (`login`, `add`, `publish`) come with the hub's publisher routes: each is one more [`Cmd`] variant
//! and one more function.

mod esp_image;
mod passphrase;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};
use firmware_package::keyfile::{self, MAX_KEY_FILE_BYTES};
use firmware_package::limits::{MAX_IMAGE_SIZE, MAX_PACKAGE_LEN};
use firmware_package::{build_and_sign, check_file, PublicKey, Release, UsbLayout, UsbPartFile};

const TOOL: &str = "hub-firmware";

/// Makes signing keys, and signs and checks Display Hub firmware packages.
#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Make a new signing key file, protected by a passphrase, and print its fingerprint. The
    /// passphrase is asked for on the terminal (or read as one line from standard input).
    Keygen {
        /// The key file to create. It must not exist yet.
        #[arg(long)]
        out: PathBuf,
    },
    /// Build and sign a package from an app image, then check it.
    Sign(Box<SignArgs>),
    /// Check a package and print its manifest, signer fingerprint and package id. Needs no key.
    Inspect {
        /// The package file (`.dhfw`).
        package: PathBuf,
    },
}

#[derive(Args)]
struct SignArgs {
    /// The signing key file, from `keygen`.
    #[arg(long)]
    key: PathBuf,
    /// The app image: the ESP-IDF application `.bin`, exactly as built.
    #[arg(long)]
    image: PathBuf,
    /// The device type, for example `llm-usage-display`.
    #[arg(long)]
    device_type: String,
    /// A hardware revision this image is for. Give it once for each revision (up to 8).
    #[arg(long = "hardware", required = true)]
    hardware: Vec<String>,
    /// The firmware version, like `1.2.0` or `1.3.0-beta.1`.
    #[arg(long = "version")]
    firmware_version: String,
    /// The security counter. Raise it in any release that fixes a security hole or retires a key.
    #[arg(long)]
    security_counter: u32,
    /// A short note for the app to show, up to 280 characters.
    #[arg(long)]
    summary: Option<String>,
    /// A key displays should trust from this release on: a key file, or a file holding the 130 hex
    /// digits of a public key. Give it once for each key (up to 3); together they are the whole set.
    #[arg(long = "key-set")]
    key_set: Vec<PathBuf>,
    /// An extra part for a first USB install, as `name@offset=file` (like
    /// `bootloader@0x0=bootloader.bin`). Give it once for each part, in flash order.
    #[arg(long = "usb-part")]
    usb_part: Vec<String>,
    /// Where a USB install writes the app image, like `0x10000`.
    #[arg(long)]
    app_offset: Option<String>,
    /// The chip the image is built for, like `esp32s3`.
    #[arg(long)]
    chip: Option<String>,
    /// The smallest flash chip the layout fits, in bytes, like `16777216` or `0x1000000`.
    #[arg(long)]
    min_flash: Option<String>,
    /// The package file to write.
    #[arg(long)]
    out: PathBuf,
}

fn finish(r: Result<String, String>) -> ExitCode {
    match r {
        Ok(m) => {
            println!("{m}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("{TOOL}: {e}");
            ExitCode::FAILURE
        }
    }
}

fn warn(message: &str) {
    eprintln!("{TOOL}: warning: {message}");
}

fn keygen(out: &Path) -> Result<String, String> {
    if out.exists() {
        return Err(keyfile::KeyFileError::Exists(out.display().to_string()).to_string());
    }
    let pass = passphrase::ask_new(&format!(
        "Choose a passphrase for the new key (at least {} characters): ",
        keyfile::MIN_PASSPHRASE_CHARS
    ))?;
    let (key, file) = keyfile::generate(&pass).map_err(|e| e.to_string())?;
    keyfile::write_new(out, &file).map_err(|e| e.to_string())?;
    let public = key.public_key();
    Ok(format!(
        "Made the signing key {}.\n\
         Fingerprint: {}\n\
         Public key:  {}\n\
         Publish the fingerprint where your users will see it. Keep the file and its passphrase safe: \
         copy it to two offline places, and never commit it to a repository.",
        out.display(),
        public.fingerprint(),
        public.to_hex()
    ))
}

/// Reads a whole file of at most `max` bytes.
fn read_bounded(path: &Path, what: &str, max: u64) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let f =
        std::fs::File::open(path).map_err(|e| format!("couldn't open the {what} {}: {e}", path.display()))?;
    let mut out = Vec::new();
    f.take(max + 1)
        .read_to_end(&mut out)
        .map_err(|e| format!("couldn't read the {what} {}: {e}", path.display()))?;
    if out.len() as u64 > max {
        return Err(format!(
            "the {what} {} is larger than {max} bytes",
            path.display()
        ));
    }
    Ok(out)
}

/// A number in decimal, or in hex with `0x` in front.
fn number(flag: &str, s: &str) -> Result<u64, String> {
    let parsed = match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(hex) => u64::from_str_radix(hex, 16),
        None => s.parse::<u64>(),
    };
    parsed.map_err(|_| format!("{flag} {s:?} isn't a number (write it like 65536 or 0x10000)"))
}

/// A `--key-set` entry: a key file's public half, or a file holding a public key in hex.
fn key_set_entry(path: &Path) -> Result<PublicKey, String> {
    let bytes = read_bounded(path, "key-set file", MAX_KEY_FILE_BYTES)?;
    if let Ok(key) = keyfile::public_key(&bytes) {
        return Ok(key);
    }
    let text = std::str::from_utf8(&bytes).unwrap_or_default().trim();
    PublicKey::from_hex(text)
        .filter(PublicKey::is_valid_point)
        .ok_or_else(|| {
            format!(
                "{} is neither a key file nor a public key (130 lower-case hex digits starting 04)",
                path.display()
            )
        })
}

fn usb_layout(a: &SignArgs) -> Result<Option<UsbLayout>, String> {
    let given = [
        !a.usb_part.is_empty(),
        a.app_offset.is_some(),
        a.chip.is_some(),
        a.min_flash.is_some(),
    ];
    if given.iter().all(|g| !g) {
        return Ok(None);
    }
    if !given.iter().all(|g| *g) {
        return Err("a USB install needs all of --usb-part, --app-offset, --chip and --min-flash".into());
    }
    let mut parts = Vec::with_capacity(a.usb_part.len());
    for spec in &a.usb_part {
        let bad =
            || format!("--usb-part {spec:?} isn't name@offset=file (like bootloader@0x0=bootloader.bin)");
        let (name, rest) = spec.split_once('@').ok_or_else(bad)?;
        let (offset, file) = rest.split_once('=').ok_or_else(bad)?;
        if name.is_empty() || file.is_empty() {
            return Err(bad());
        }
        parts.push(UsbPartFile {
            name: name.to_string(),
            offset: number("--usb-part offset", offset)?,
            bytes: read_bounded(Path::new(file), "USB part", MAX_PACKAGE_LEN)?,
        });
    }
    Ok(Some(UsbLayout {
        chip: a.chip.clone().unwrap_or_default(),
        min_flash_bytes: number("--min-flash", a.min_flash.as_deref().unwrap_or_default())?,
        app_offset: number("--app-offset", a.app_offset.as_deref().unwrap_or_default())?,
        parts,
    }))
}

/// Writes `bytes` to `path` through a temporary file in the same folder, so a failed write never
/// leaves half a package behind.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let fail = |e: std::io::Error| format!("couldn't write {}: {e}", path.display());
    let dir = match path.parent() {
        Some(d) if !d.as_os_str().is_empty() => d,
        _ => Path::new("."),
    };
    let name = path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default();
    let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    let written = (|| {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written.map_err(fail)
}

fn sign(a: &SignArgs) -> Result<String, String> {
    // Everything that can be checked without the passphrase is checked first.
    let usb_install = usb_layout(a)?;
    let image = read_bounded(&a.image, "image", MAX_IMAGE_SIZE)?;
    let key_set = if a.key_set.is_empty() {
        None
    } else {
        Some(
            a.key_set
                .iter()
                .map(|p| key_set_entry(p))
                .collect::<Result<Vec<_>, _>>()?,
        )
    };
    for w in esp_image::warnings(&image, &a.firmware_version) {
        warn(&w);
    }
    let key_file = keyfile::read(&a.key).map_err(|e| e.to_string())?;
    let signer = keyfile::public_key(&key_file).map_err(|e| format!("{}: {e}", a.key.display()))?;
    if let Some(ks) = &key_set {
        if !ks.contains(&signer) {
            warn("the key set doesn't include the signing key, so displays that install this release will stop trusting it.");
        }
    }
    let pass = passphrase::ask(&format!(
        "Passphrase for the key {} ({}): ",
        a.key.display(),
        signer.fingerprint()
    ))?;
    let key = keyfile::decrypt(&key_file, &pass).map_err(|e| e.to_string())?;
    drop(pass);

    let release = Release {
        device_type: a.device_type.clone(),
        hardware_revisions: a.hardware.clone(),
        firmware_version: a.firmware_version.clone(),
        security_counter: a.security_counter,
        summary: a.summary.clone(),
        key_set,
        usb_install,
    };
    let built = build_and_sign(&key, &release, &image).map_err(|e| e.to_string())?;
    write_atomic(&a.out, &built.bytes)?;
    let c = &built.checked;
    Ok(format!(
        "Signed {} {} for {} and checked it.\n\
         File: {} ({} bytes; {} bytes go to a display)\n\
         Signer fingerprint: {}\n\
         Package id: {}",
        c.manifest.device_type,
        c.manifest.firmware_version,
        c.manifest.hardware_revisions.join(", "),
        a.out.display(),
        c.len,
        c.update_form_len(),
        c.signer().fingerprint(),
        c.package_id.to_hex()
    ))
}

fn inspect(path: &Path) -> Result<String, String> {
    let c = check_file(path).map_err(|e| format!("{} isn't a valid package: {e}", path.display()))?;
    // Printed through serde_json, which escapes control characters, so unknown members can't play
    // tricks with the terminal.
    let manifest: serde_json::Value =
        serde_json::from_slice(&c.manifest_bytes).map_err(|e| format!("couldn't show the manifest: {e}"))?;
    let pretty = serde_json::to_string_pretty(&manifest).map_err(|e| e.to_string())?;
    Ok(format!(
        "{pretty}\n\
         Signer fingerprint: {}\n\
         Signer key id: {}\n\
         Package id: {}\n\
         Size: {} bytes ({} bytes go to a display)\n\
         The package is well formed and signed by the key above. Compare the fingerprint with the one \
         the firmware's maker publishes: that is what says who made it.",
        c.signer().fingerprint(),
        c.signer_key_id().to_hex(),
        c.package_id.to_hex(),
        c.len,
        c.update_form_len()
    ))
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match &cli.command {
        Cmd::Keygen { out } => finish(keygen(out)),
        Cmd::Sign(a) => finish(sign(a)),
        Cmd::Inspect { package } => finish(inspect(package)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_in_decimal_and_hex() {
        assert_eq!(number("--x", "65536"), Ok(65536));
        assert_eq!(number("--x", "0x10000"), Ok(65536));
        assert_eq!(number("--x", "0X1000"), Ok(4096));
        assert!(number("--x", "").is_err());
        assert!(number("--x", "-1").is_err());
        assert!(number("--x", "0x").is_err());
        assert!(number("--x", "16M").is_err());
    }

    #[test]
    fn the_command_line_is_well_formed() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }
}
