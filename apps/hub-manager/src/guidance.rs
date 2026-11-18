//! Turns every failure into a message a person can act on: what happened, and what to do next.

use hub_client::usb::UsbError;
use hub_client::ClientError;
use hub_core::api::FirmwareOfferView;
use hub_protocol::firmware::ReportReason;
use hub_protocol::pairing::PairingError;
use hub_protocol::ErrorCode;

use crate::firmware::{BoardError, InstallError, Phase, Progress, Retry, Signer, SignerKind, Step};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    /// Something to know, such as a change that worked.
    Done,
    /// Something went wrong and can be fixed.
    Problem,
    /// Stop: possible impersonation, or a code someone else may have seen.
    Danger,
}

/// What the app offers next to the message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Next {
    Retry,
    PairAgain,
    None,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Guidance {
    pub tone: Tone,
    pub title: String,
    pub detail: String,
    pub next: Next,
}

impl Guidance {
    pub fn done(title: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            tone: Tone::Done,
            title: title.into(),
            detail: detail.into(),
            next: Next::None,
        }
    }

    fn problem(title: &str, detail: impl Into<String>, next: Next) -> Self {
        Self {
            tone: Tone::Problem,
            title: title.into(),
            detail: detail.into(),
            next,
        }
    }

    fn danger(title: &str, detail: impl Into<String>, next: Next) -> Self {
        Self {
            tone: Tone::Danger,
            title: title.into(),
            detail: detail.into(),
            next,
        }
    }
}

/// What was being attempted, which changes what some errors mean.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Doing {
    Pairing,
    /// Adding a firmware file to the hub.
    Uploading,
    /// Offering an update to a display (the Update button).
    Offering,
    /// Saving a name for a signing key.
    Naming,
    Other,
}

/// "1.4 MB", "12 KB": sizes as people read them.
pub fn size_text(bytes: u64) -> String {
    let mb = bytes as f64 / (1024.0 * 1024.0);
    if mb >= 0.1 {
        format!("{mb:.1} MB")
    } else {
        format!("{} KB", bytes.div_ceil(1024))
    }
}

/// A hub message with every "1482752 bytes" in it said as "1.4 MB".
fn sizes_in_words(message: &str) -> String {
    let mut out = String::with_capacity(message.len());
    let mut rest = message;
    while let Some(at) = rest.find(" bytes") {
        let before = &rest[..at];
        let digits = before.len() - before.trim_end_matches(|c: char| c.is_ascii_digit()).len();
        let starts_word = before[..before.len() - digits]
            .chars()
            .last()
            .is_none_or(|c| !c.is_alphanumeric());
        match before[before.len() - digits..].parse::<u64>() {
            Ok(n) if digits > 0 && starts_word => {
                out.push_str(&before[..before.len() - digits]);
                out.push_str(&size_text(n));
            }
            _ => {
                out.push_str(before);
                out.push_str(" bytes");
            }
        }
        rest = &rest[at + " bytes".len()..];
    }
    out.push_str(rest);
    out
}

/// The hub's own message, kept short and plain for showing, with sizes in MB or KB.
fn hub_message(e: &ClientError) -> String {
    match e {
        ClientError::Status { body: Some(b), .. } => {
            let plain: String = b
                .error
                .message
                .chars()
                .filter(|c| !c.is_control())
                .take(300)
                .collect();
            sizes_in_words(&plain)
        }
        ClientError::Status { status, body: None } => format!("HTTP {status}"),
        _ => String::new(),
    }
}

/// The hub's message, or `fallback` when it gave none.
fn hub_message_or(e: &ClientError, fallback: &str) -> String {
    match e {
        ClientError::Status { body: Some(b), .. } if !b.error.message.trim().is_empty() => hub_message(e),
        _ => fallback.to_string(),
    }
}

/// The host part of a `host:port` address.
fn host_of(address: &str) -> &str {
    address.rsplit_once(':').map_or(address, |(h, _)| h)
}

/// Technical detail for showing: control characters dropped and length capped.
fn one_line(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).take(300).collect()
}

pub fn from_client(e: &ClientError, doing: Doing, address: &str) -> Guidance {
    use ErrorCode::*;
    match e {
        ClientError::IdentityMismatch => Guidance::danger(
            "This isn't your hub",
            format!(
                "The server at {address} answered with a different key from the hub you paired with. Something may \
                 be pretending to be your hub, so nothing was sent. If you reinstalled the hub without restoring its \
                 backup, forget it in Settings and pair again. Otherwise, check your network."
            ),
            Next::None,
        ),
        ClientError::NameNotFound { host, detail } => Guidance::problem(
            "This PC can't find the hub's name",
            format!(
                "This PC couldn't find {host}. Some Windows setups, and VPNs, can't look up .local names. On the Pi, \
                 make a pairing string with its address instead: sudo -u display-hub hubd pair-admin --host <the Pi's \
                 address>. Running hostname -I on the Pi shows its address. Details: {}",
                one_line(detail)
            ),
            Next::Retry,
        ),
        ClientError::Timeout if host_of(address).to_ascii_lowercase().ends_with(".local") => Guidance::problem(
            "Can't reach the hub",
            format!(
                "Nothing answered at {address}. Check the Raspberry Pi is switched on and on the same network as this \
                 PC. If it is, this PC may be slow to look up .local names (some Windows setups and VPNs are): on the \
                 Pi, make a pairing string with its address instead: sudo -u display-hub hubd pair-admin --host <the \
                 Pi's address>. Running hostname -I on the Pi shows its address."
            ),
            Next::Retry,
        ),
        ClientError::Unreachable(_) | ClientError::Timeout => Guidance::problem(
            "Can't reach the hub",
            format!(
                "Nothing answered at {address}. Check the Raspberry Pi is switched on and on the same network as this \
                 PC. Displays keep showing their last figures in the meantime."
            ),
            Next::Retry,
        ),
        ClientError::AddressRefused(m) => {
            Guidance::problem("That address can't be used", m.clone(), Next::None)
        }
        ClientError::Invalid(m) => Guidance::problem("That can't be saved", one_line(m), Next::None),
        ClientError::TooLarge(_) | ClientError::Malformed(_) | ClientError::Snapshot(_) => Guidance::problem(
            "The hub's answer wasn't understood",
            "The hub and this app may be different versions. Update both to the same release.",
            Next::Retry,
        ),
        ClientError::Status { status: 408, .. } if doing == Doing::Uploading => Guidance::problem(
            "The upload stopped part way",
            hub_message_or(e, "The hub gave up waiting for the file. Try again."),
            Next::None,
        ),
        ClientError::Status { .. } => match (e.code(), doing) {
            (Some(EnrolmentExpired), Doing::Pairing) => Guidance::problem(
                "That pairing string has expired",
                "Pairing strings work for 10 minutes. On the Pi, run sudo -u display-hub hubd pair-admin again and \
                 paste the new one.",
                Next::None,
            ),
            (Some(EnrolmentUsed), Doing::Pairing) => Guidance::danger(
                "That pairing string was already used",
                "Each string works once, and the hub has recorded this attempt as an alarm. If you didn't use it \
                 yourself, someone else may have seen it: on the Pi, run hubd admin list and revoke anything you \
                 don't recognise. Then make a new string with hubd pair-admin.",
                Next::None,
            ),
            (Some(Unauthorised | TokenRevoked | Forbidden), _) => Guidance::problem(
                "This PC isn't paired any more",
                "The hub no longer accepts this PC's credential. It may have been revoked with hubd admin revoke. \
                 Pair again with a new string from hubd pair-admin.",
                Next::PairAgain,
            ),
            (Some(RateLimited), _) => Guidance::problem(
                "Too many attempts",
                "The hub is limiting attempts from this PC for a few minutes. Wait, then try again.",
                Next::Retry,
            ),
            (Some(IncompatibleVersion), _) => Guidance::problem(
                "The hub and this app don't match",
                "Update the hub and this app to the same release.",
                Next::None,
            ),
            (Some(Unavailable), _) => Guidance::problem(
                "The hub is busy",
                hub_message_or(e, "It couldn't do that just now. Try again in a moment."),
                Next::Retry,
            ),
            (Some(NotFound), _) => Guidance::problem(
                "That's no longer on the hub",
                "It may have been changed from another PC. The lists have been refreshed.",
                Next::Retry,
            ),
            (Some(Conflict), Doing::Uploading) => {
                Guidance::problem("The hub has no room for this package", hub_message(e), Next::None)
            }
            (Some(Conflict), Doing::Offering) => {
                Guidance::problem("The display would refuse this update", hub_message(e), Next::None)
            }
            (Some(BadRequest), Doing::Naming) => {
                Guidance::problem("The hub won't use that name", hub_message(e), Next::None)
            }
            _ => Guidance::problem("The hub refused that", hub_message(e), Next::None),
        },
    }
}

pub fn from_pairing(e: &PairingError) -> Guidance {
    let title = match e {
        PairingError::NotPairing => "That isn't a pairing string",
        PairingError::Damaged => "Part of the pairing string is missing or mistyped",
        PairingError::NoHost => "The pairing string has no hub address",
    };
    Guidance::problem(
        title,
        format!(
            "{e}. On the Pi, run sudo -u display-hub hubd pair-admin, then copy the whole line starting with dh1."
        ),
        Next::None,
    )
}

pub fn from_store(detail: &str) -> Guidance {
    Guidance::problem(
        "No safe place to keep the hub's credential",
        format!(
            "This app keeps its credential in the system's credential store (Windows Credential Manager, the \
             macOS keychain, or the Secret Service on Linux) and never in a plain file. The store didn't work: {detail}"
        ),
        Next::Retry,
    )
}

pub fn from_usb(e: &UsbError) -> Guidance {
    let next = match e {
        UsbError::Port(_) | UsbError::NoAnswer | UsbError::Stopped => Next::Retry,
        _ => Next::None,
    };
    let title = match e {
        UsbError::Port(_) => "The USB port didn't work",
        UsbError::NoAnswer => "No display answered on that port",
        UsbError::Stopped => "The display stopped answering",
        UsbError::AlreadyProvisioned => "That display is already set up",
        UsbError::Incompatible(_) => "That display's software doesn't match",
        UsbError::Unexpected => "The display's answer wasn't understood",
        UsbError::FirmwareKeysLocked => "This display trusts another maker's firmware",
        _ => "The display refused the setup details",
    };
    let mut detail = e.to_string();
    if matches!(e, UsbError::Port(_)) {
        detail.push_str(". Close any other program using the port (such as a serial monitor) and try again.");
    }
    Guidance::problem(title, detail, next)
}

/// A firmware package that failed this app's checks. Nothing was sent or written.
pub fn from_package(detail: &str) -> Guidance {
    let mut d = one_line(detail);
    if let Some(first) = d.get(..1) {
        d = first.to_uppercase() + &d[1..];
    }
    Guidance::problem(
        "This firmware file failed its checks",
        format!(
            "{d}. Nothing was sent or written. The file may be damaged or incomplete: get a fresh copy from its \
             maker."
        ),
        Next::None,
    )
}

/// What to do when a first install over USB stops.
pub fn from_install(e: &InstallError, port: &str) -> Guidance {
    let on = if port.is_empty() {
        "that port".to_string()
    } else {
        port.to_string()
    };
    let after = if e.erased() {
        "The display had been erased, so it won't start properly until an install finishes. Nothing else is \
         affected: try again."
    } else {
        "Nothing was written, so the display is as it was."
    };
    let (title, what) = match e {
        InstallError::WrongChip { expected, found } => (
            "This firmware is for a different chip".to_string(),
            format!(
                "The package is built for an {expected}, and the board on {on} is an {found}. Check this is the \
                 package for this display."
            ),
        ),
        InstallError::FlashTooSmall { needed, found } => (
            "This board's flash is too small".to_string(),
            format!(
                "The package needs at least {} MB of flash, and the board on {on} has {} MB.",
                needed / (1024 * 1024),
                found / (1024 * 1024)
            ),
        ),
        InstallError::SecureDownload => (
            "This board is locked against USB installs".to_string(),
            "Its chip is in Secure Download Mode, so only its maker's own tools can install on it.".to_string(),
        ),
        InstallError::Board { error, .. } => match error {
            BoardError::NoBoard(_) => (
                format!("No board found on {on}"),
                "Check the cable is plugged in and is a data cable (some cables only charge), then choose Look \
                 again and pick the board's port."
                    .to_string(),
            ),
            BoardError::Busy(_) => (
                format!("{on} is busy"),
                "Another program has the port open, such as a serial monitor, the Arduino IDE or another copy of \
                 this app. Close it, or unplug the cable and plug it back in, then try again."
                    .to_string(),
            ),
            BoardError::NotInDownloadMode(_) => (
                "The board didn't start its loader".to_string(),
                "Put it in download mode by hand: unplug the cable, hold the BOOT button while you plug it back \
                 in, then let go. Then try again. (If your board has a RESET button, holding BOOT and pressing \
                 RESET does the same.)"
                    .to_string(),
            ),
            BoardError::Lost(d) => (
                "The connection to the board was lost".to_string(),
                format!(
                    "Check the cable is firmly in, try another USB port or cable, then try again. Details: {}",
                    one_line(d)
                ),
            ),
            BoardError::Other(d) => (
                "The install didn't work".to_string(),
                format!("Details: {}", one_line(d)),
            ),
        },
    };
    Guidance {
        tone: Tone::Problem,
        title,
        detail: format!("{what} {after}"),
        next: Next::None,
    }
}

/// How the app's sentences name a signing key: by the app's own name for an official key, by the
/// hub's name for a key the owner named (which is only as trustworthy as the hub), and always with
/// its fingerprint.
pub fn signer_phrase(s: &Signer) -> String {
    match &s.kind {
        SignerKind::Official => format!("{} ({})", s.name(), s.fingerprint),
        SignerKind::Labelled(l) => format!("the key the hub calls \"{l}\" ({})", s.fingerprint),
        SignerKind::Unknown => format!("a maker this app doesn't know ({})", s.fingerprint),
    }
}

/// What a display card says when the display has used up its update attempts for now.
pub fn paused_words(name: &str) -> String {
    format!(
        "{name} has tried five updates recently and is pausing to protect its memory. It will try again after \
         about four hours switched on. To update now, use Install firmware over USB."
    )
}

/// How to try an offer's package again, by the button the card shows for it.
fn try_again(retry: Option<&Retry>) -> String {
    match retry {
        Some(Retry::Update(v)) => format!("Press Update to {v} to try again."),
        Some(Retry::Install(v)) => {
            format!("To try again, press Install {v}, under Other versions on the hub.")
        }
        None => "To try again, add its package to the hub again.".into(),
    }
}

/// The line on a display card for its open or last update, and a sentence under it. `retry` is
/// the card's button for the same package, if it shows one.
pub fn offer_words(
    phase: &Phase,
    offer: &FirmwareOfferView,
    running: Option<&str>,
    poll: &str,
    retry: Option<&Retry>,
) -> (String, String) {
    let v = &offer.firmware_version;
    let now_runs = running.unwrap_or("the version it had");
    let still = format!("It still runs {now_runs}.");
    match phase {
        Phase::Waiting => (
            "Waiting for the display to check in".into(),
            format!("It acts on the update to {v} at its next check, within about {poll}."),
        ),
        Phase::Downloading => (
            format!("Downloading {v}"),
            "The display checks the package before it writes anything.".into(),
        ),
        Phase::Restarting => (
            format!("Restarting into {v}"),
            "The display is starting the new firmware and checking in with the hub. If it doesn't start \
             properly within five minutes, it goes back by itself."
                .into(),
        ),
        Phase::Installed => (format!("Updated to {v}"), String::new()),
        Phase::RolledBack => {
            let back = offer
                .reported_version
                .as_deref()
                .or(running)
                .unwrap_or("its previous version");
            (
                format!("Went back to {back}"),
                format!(
                    "The update didn't start properly, so the display went back to {back}. Nothing was lost."
                ),
            )
        }
        Phase::Refused(r) => {
            let why = match r {
                ReportReason::NoTrustedKeys => {
                    "It trusts no firmware keys, so it refuses every update. Use Install firmware over USB once to \
                     give it its maker's keys."
                }
                ReportReason::TooLarge => "The firmware is too big for the display's spare space.",
                ReportReason::BadFormat => {
                    "The package wasn't in a form the display could read. It may be damaged: add the maker's \
                     file to the hub again."
                }
                ReportReason::UntrustedKey => "It isn't signed by a key the display trusts.",
                ReportReason::BadSignature => {
                    "Its signature didn't check out, so it may have been altered. Don't use it: get a fresh copy \
                     from the maker."
                }
                ReportReason::WrongDeviceType => "It is firmware for a different kind of display.",
                ReportReason::WrongHardware => "It isn't built for this display's hardware.",
                ReportReason::Downgrade => {
                    "The display has already installed a security fix this release lacks, so it won't go back \
                     to it."
                }
                _ => "The display gave a reason this app doesn't know.",
            };
            (
                format!("The display refused {v}"),
                format!("{why} Nothing was written. {still}"),
            )
        }
        Phase::Failed(r) => {
            let again = try_again(retry);
            let why = match r {
                ReportReason::Download => {
                    format!("The download didn't finish: the hub may have been busy, or the display's Wi-Fi dropped. {again}")
                }
                ReportReason::ImageHash => {
                    format!("The firmware arrived damaged, so the display threw it away. {again}")
                }
                ReportReason::Flash => format!(
                    "The display couldn't write the firmware to its memory. {again} If it keeps failing, use \
                     Install firmware over USB."
                ),
                _ => "The display reported a problem this app doesn't know.".into(),
            };
            (format!("Updating to {v} failed"), format!("{why} {still}"))
        }
        Phase::Cancelled(reason) => (
            format!("The update to {v} was cancelled"),
            match reason.as_str() {
                "replaced" => "A newer update replaced it.",
                "package_deleted" => "Its package was removed from the hub.",
                "display_removed" => "The display was removed.",
                "package_missing" => "Its package's file is missing from the hub. Add the file again.",
                _ => "It was cancelled before the display installed it.",
            }
            .into(),
        ),
        Phase::Expired => (
            format!("The update to {v} expired"),
            format!(
                "The display didn't check in within 24 hours, so the update was called off. {}",
                try_again(retry)
            ),
        ),
        Phase::Unknown(state) => (
            format!("Updating to {v} failed"),
            format!(
                "The display reported something this app doesn't know ({}). {still}",
                one_line(state)
            ),
        ),
    }
}

/// A step of a USB install, for the progress line.
pub fn install_step(p: &Progress) -> String {
    match &p.step {
        Step::Connecting => "Connecting to the board's loader".into(),
        Step::Erasing => "Erasing the whole flash. This can take a minute or two.".into(),
        Step::Writing { name, index, count } => {
            let percent = (p.written * 100).checked_div(p.total).unwrap_or(0);
            let what = if name == "app" { "the app" } else { name.as_str() };
            format!("Writing {what} ({index} of {count}). {percent}% written.")
        }
        Step::Restarting => "Restarting the board into the new firmware".into(),
        Step::SettingUp => "Sending the setup details and the maker's keys to the display".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hub_protocol::ErrorBody;

    #[test]
    fn a_failed_name_lookup_names_the_host_and_offers_the_address_instead() {
        let e = ClientError::NameNotFound {
            host: "raspberrypi.local".into(),
            detail: "dns error\u{1b}".into(),
        };
        let g = from_client(&e, Doing::Pairing, "raspberrypi.local:8443");
        assert_eq!(g.title, "This PC can't find the hub's name");
        assert!(g.detail.contains("couldn't find raspberrypi.local"));
        assert!(g.detail.contains("hubd pair-admin --host <the Pi's address>"));
        assert!(g.detail.contains("Details: dns error"));
        assert!(!g.detail.contains('\u{1b}'));
        assert_eq!(g.next, Next::Retry);
    }

    #[test]
    fn a_timeout_on_a_local_name_suggests_the_address_but_an_ip_does_not() {
        let local = from_client(&ClientError::Timeout, Doing::Pairing, "pi.local:8443");
        assert!(local.detail.contains("hubd pair-admin --host"));
        let ip = from_client(&ClientError::Timeout, Doing::Pairing, "192.168.1.20:8443");
        assert!(!ip.detail.contains("pair-admin"));
    }

    fn status(code: ErrorCode) -> ClientError {
        ClientError::Status {
            status: 401,
            body: Some(ErrorBody::new(code, "msg")),
        }
    }

    #[test]
    fn impersonation_is_a_stop_with_no_retry() {
        let g = from_client(&ClientError::IdentityMismatch, Doing::Other, "pi.local:8443");
        assert_eq!((g.tone, g.next), (Tone::Danger, Next::None));
        assert!(g.detail.contains("pi.local:8443"));
    }

    #[test]
    fn pairing_codes_explain_what_to_do() {
        let used = from_client(&status(ErrorCode::EnrolmentUsed), Doing::Pairing, "x");
        assert_eq!(used.tone, Tone::Danger);
        assert!(used.detail.contains("hubd admin list"));
        assert!(
            from_client(&status(ErrorCode::EnrolmentExpired), Doing::Pairing, "x")
                .title
                .contains("expired")
        );
        let revoked = from_client(&status(ErrorCode::TokenRevoked), Doing::Other, "x");
        assert_eq!(revoked.next, Next::PairAgain);
        let offline = from_client(&ClientError::Timeout, Doing::Other, "x");
        assert_eq!(
            (offline.title.as_str(), offline.next),
            ("Can't reach the hub", Next::Retry)
        );
    }

    #[test]
    fn sizes_in_hub_messages_are_said_in_mb_or_kb() {
        assert_eq!(
            sizes_in_words("This firmware is 1482752 bytes, too big for 1048576 bytes."),
            "This firmware is 1.4 MB, too big for 1.0 MB."
        );
        assert_eq!(sizes_in_words("2048 bytes"), "2 KB");
        // Not a number of bytes: left alone.
        assert_eq!(sizes_in_words("a1 bytes, some bytes"), "a1 bytes, some bytes");
    }

    #[test]
    fn a_busy_hub_says_why_when_it_can() {
        let e = ClientError::Status {
            status: 503,
            body: Some(ErrorBody::new(
                ErrorCode::Unavailable,
                "This display is already downloading firmware. Try again when that download ends.",
            )),
        };
        let g = from_client(&e, Doing::Other, "x");
        assert_eq!(g.title, "The hub is busy");
        assert!(g.detail.contains("already downloading"), "{}", g.detail);
        let bare = ClientError::Status {
            status: 503,
            body: Some(ErrorBody::new(ErrorCode::Unavailable, "")),
        };
        assert!(from_client(&bare, Doing::Other, "x")
            .detail
            .contains("Try again in a moment"));
        // A conflict elsewhere is the hub's own reason, under a plain title.
        let e = ClientError::Status {
            status: 409,
            body: Some(ErrorBody::new(ErrorCode::Conflict, "Something specific.")),
        };
        let g = from_client(&e, Doing::Other, "x");
        assert_eq!(
            (g.title.as_str(), g.detail.as_str()),
            ("The hub refused that", "Something specific.")
        );
    }

    #[test]
    fn hub_messages_are_shown_without_control_characters() {
        let e = ClientError::Status {
            status: 409,
            body: Some(ErrorBody::new(ErrorCode::Conflict, "a\u{1b}[31mb")),
        };
        assert_eq!(from_client(&e, Doing::Other, "x").detail, "a[31mb");
    }
}
