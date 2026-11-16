//! Turns every failure into a message a person can act on: what happened, and what to do next.

use hub_client::usb::UsbError;
use hub_client::ClientError;
use hub_protocol::pairing::PairingError;
use hub_protocol::ErrorCode;

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
    Other,
}

/// The hub's own message, kept short and plain for showing.
fn hub_message(e: &ClientError) -> String {
    match e {
        ClientError::Status { body: Some(b), .. } => b
            .error
            .message
            .chars()
            .filter(|c| !c.is_control())
            .take(200)
            .collect(),
        ClientError::Status { status, body: None } => format!("HTTP {status}"),
        _ => String::new(),
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
        ClientError::TooLarge(_) | ClientError::Malformed(_) | ClientError::Snapshot(_) => Guidance::problem(
            "The hub's answer wasn't understood",
            "The hub and this app may be different versions. Update both to the same release.",
            Next::Retry,
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
                "It couldn't do that just now. Try again in a moment.",
                Next::Retry,
            ),
            (Some(NotFound), _) => Guidance::problem(
                "That's no longer on the hub",
                "It may have been changed from another PC. The lists have been refreshed.",
                Next::Retry,
            ),
            (Some(Conflict), _) => Guidance::problem("That clashes with something on the hub", hub_message(e), Next::None),
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
            "This app keeps its credential in the system's credential store (Windows Credential Manager, or the \
             Secret Service on Linux) and never in a plain file. The store didn't work: {detail}"
        ),
        Next::Retry,
    )
}

pub fn from_usb(e: &UsbError) -> Guidance {
    let next = match e {
        UsbError::Port(_) | UsbError::NoAnswer => Next::Retry,
        _ => Next::None,
    };
    let title = match e {
        UsbError::Port(_) => "The USB port didn't work",
        UsbError::NoAnswer => "No display answered on that port",
        UsbError::AlreadyProvisioned => "That display is already set up",
        UsbError::Incompatible(_) => "That display's software doesn't match",
        UsbError::Unexpected => "The display's answer wasn't understood",
        _ => "The display refused the setup details",
    };
    let mut detail = e.to_string();
    if matches!(e, UsbError::Port(_)) {
        detail.push_str(". Close any other program using the port (such as a serial monitor) and try again.");
    }
    Guidance::problem(title, detail, next)
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
    fn hub_messages_are_shown_without_control_characters() {
        let e = ClientError::Status {
            status: 409,
            body: Some(ErrorBody::new(ErrorCode::Conflict, "a\u{1b}[31mb")),
        };
        assert_eq!(from_client(&e, Doing::Other, "x").detail, "a[31mb");
    }
}
