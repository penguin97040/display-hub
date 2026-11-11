//! Conformance vectors for display firmware: one hub response in, exactly what the display must do
//! and show out (`protocol/v1/vectors/`). They are generated from this reference implementation, and
//! `display-simulator conform check` proves the committed files still match it.
//!
//! Firmware ports the vectors to its own unit tests: feed `response.status` and `response.body`, plus
//! `held_seconds` (how long since the snapshot arrived, by a monotonic clock), and compare its state
//! with `expect`. Nothing here depends on Rust.

use std::path::Path;

use hub_core::freshness::Freshness;
use hub_protocol::limits::MAX_SNAPSHOT_BYTES;
use hub_protocol::{receive, ErrorAction, ErrorBody, ReceiveError, Snapshot};
use serde::{Deserialize, Serialize};

pub const FORMAT: &str = "display-hub.conformance";
pub const VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Vector {
    pub format: String,
    pub version: u32,
    pub name: String,
    pub description: String,
    pub response: Response,
    /// Seconds since the snapshot arrived, measured by the display's monotonic clock.
    pub held_seconds: i64,
    pub expect: Expect,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Response {
    /// The HTTP status of `GET /v1/device/snapshot`.
    pub status: u16,
    /// The exact body, as text.
    pub body: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Expect {
    /// `show`: draw `pages`. `unreadable`: keep the last good snapshot and say the hub sent
    /// something unreadable. Otherwise the error group (`docs/protocol-v1.md`, "Errors"):
    /// `reprovision`, `incompatible`, `retry` or `fault`, keeping the last good snapshot.
    pub outcome: String,
    /// When to poll next: `refresh` (wait `refresh_after_seconds`, ±10%), `backoff` (15 s doubling to
    /// 15 min, ±20%) or `hourly`.
    pub next_poll: String,
    pub refresh_after_seconds: Option<u32>,
    /// The error code, if the hub sent one.
    pub error_code: Option<String>,
    pub pages: Vec<PageExpect>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PageExpect {
    pub id: String,
    pub title: String,
    /// What the screen says about the figures: DEMO, REPORTED, ESTIMATE, MANUAL or UNAVAILABLE.
    pub quality_label: String,
    pub note: Option<String>,
    /// Age on the hub's clock (`generated_at - updated_at + held_seconds`); `null` if never updated.
    pub age_seconds: Option<i64>,
    pub stale: bool,
    /// Empty for an unavailable page: its figures are never drawn.
    pub metrics: Vec<MetricExpect>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MetricExpect {
    pub id: String,
    pub label: String,
    /// `null` is unknown: draw "unknown", never 0%.
    pub used_percent: Option<f64>,
    /// Seconds until reset on the hub's clock, while it's in the future.
    pub resets_in_seconds: Option<i64>,
    /// The reset time has passed: show "reset due" instead of a countdown.
    pub reset_due: bool,
}

fn show(s: &Snapshot, held: i64) -> Expect {
    let now = s.generated_at.plus_seconds(held);
    let pages = s
        .pages
        .iter()
        .map(|p| {
            let (age_seconds, stale) = match Freshness::of(p, s.generated_at, held) {
                Freshness::Never => (None, false),
                Freshness::Fresh { age_seconds } => (Some(age_seconds), false),
                Freshness::Stale { age_seconds } => (Some(age_seconds), true),
            };
            let metrics = if p.data_quality.shows_figures() {
                p.metrics
                    .iter()
                    .map(|m| {
                        let left = m.resets_at.map(|r| now.seconds_until(r));
                        MetricExpect {
                            id: m.id.clone(),
                            label: m.label.clone(),
                            used_percent: m.used_percent,
                            resets_in_seconds: left.filter(|l| *l > 0),
                            reset_due: left.is_some_and(|l| l <= 0),
                        }
                    })
                    .collect()
            } else {
                Vec::new()
            };
            PageExpect {
                id: p.id.clone(),
                title: p.title.clone(),
                quality_label: p.data_quality.label().into(),
                note: p.note.clone(),
                age_seconds,
                stale,
                metrics,
            }
        })
        .collect();
    Expect {
        outcome: "show".into(),
        next_poll: "refresh".into(),
        refresh_after_seconds: Some(s.refresh_after_seconds),
        error_code: None,
        pages,
    }
}

fn keep(outcome: &str, next_poll: &str, code: Option<String>) -> Expect {
    Expect {
        outcome: outcome.into(),
        next_poll: next_poll.into(),
        refresh_after_seconds: None,
        error_code: code,
        pages: Vec::new(),
    }
}

/// The reference behaviour: what a display does with one response.
pub fn expect(status: u16, body: &[u8], held_seconds: i64) -> Expect {
    if status == 200 {
        return match receive(body) {
            Ok(s) => show(&s, held_seconds),
            Err(ReceiveError::IncompatibleVersion(_)) => keep("incompatible", "hourly", None),
            Err(_) => keep("unreadable", "backoff", None),
        };
    }
    let parsed = (body.len() <= MAX_SNAPSHOT_BYTES)
        .then(|| serde_json::from_slice::<ErrorBody>(body).ok())
        .flatten();
    match parsed {
        Some(b) => {
            let code = Some(b.error.code.as_str().to_string());
            match b.error.code.action() {
                ErrorAction::Reprovision => keep("reprovision", "hourly", code),
                ErrorAction::Incompatible => keep("incompatible", "hourly", code),
                ErrorAction::Retry => keep("retry", "backoff", code),
                ErrorAction::Fault => keep("fault", "backoff", code),
            }
        }
        None if status >= 500 => keep("retry", "backoff", None),
        None => keep("fault", "backoff", None),
    }
}

fn error(code: &str, message: &str) -> String {
    format!(r#"{{"error":{{"code":"{code}","message":"{message}"}}}}"#)
}

/// Every case, built from `protocol/v1/examples` and the cases below.
/// One case: name, description, HTTP status, body and seconds held.
pub type Case = (String, String, u16, String, i64);

pub fn cases(protocol_dir: &Path) -> std::io::Result<Vec<Case>> {
    let mut out = Vec::new();
    for (kind, what) in [
        ("valid", "A valid snapshot"),
        ("invalid", "A snapshot that breaks the protocol"),
    ] {
        let mut files: Vec<_> = std::fs::read_dir(protocol_dir.join("examples").join(kind))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "json"))
            .collect();
        files.sort();
        for f in files {
            let name = f.file_stem().unwrap_or_default().to_string_lossy().into_owned();
            let body = std::fs::read_to_string(&f)?;
            out.push((
                format!("{kind}-{name}"),
                format!("{what}: examples/{kind}/{name}.json."),
                200,
                body,
                0,
            ));
        }
    }
    let demo = std::fs::read_to_string(protocol_dir.join("examples/valid/demo-three-pages.json"))?;
    let manual = std::fs::read_to_string(protocol_dir.join("examples/valid/manual-and-estimated.json"))?;
    out.push((
        "held-until-stale".into(),
        "The demo snapshot, held for 70 minutes without a new one: its pages are now stale.".into(),
        200,
        demo,
        4200,
    ));
    out.push((
        "held-past-reset".into(),
        "Figures held for 25 days, past their reset time: show \"reset due\", not a countdown.".into(),
        200,
        manual,
        25 * 86_400,
    ));
    out.push((
        "oversize".into(),
        format!("A body over {MAX_SNAPSHOT_BYTES} bytes, refused before parsing."),
        200,
        format!(
            r#"{{"protocol_version":1,"padding":"{}"}}"#,
            "x".repeat(MAX_SNAPSHOT_BYTES)
        ),
        0,
    ));
    out.push((
        "not-json".into(),
        "A captive portal or proxy page instead of JSON.".into(),
        200,
        "<html><body>Sign in to Wi-Fi</body></html>".into(),
        0,
    ));
    for (name, desc, status, body) in [
        (
            "error-token-revoked",
            "The display was removed in the manager.",
            401,
            error("token_revoked", "This display's access was revoked."),
        ),
        (
            "error-unknown-device",
            "The hub no longer knows this display.",
            401,
            error("unknown_device", "Unknown display."),
        ),
        (
            "error-unauthorised",
            "No usable credential: a fault, not a reason to forget the hub.",
            401,
            error("unauthorised", "The hub doesn't recognise this credential."),
        ),
        (
            "error-forbidden",
            "A credential for another role.",
            403,
            error("forbidden", "Not allowed."),
        ),
        (
            "error-incompatible-version",
            "The hub doesn't serve this display's protocol version.",
            400,
            error("incompatible_version", "Unsupported protocol version."),
        ),
        (
            "error-rate-limited",
            "Too many requests.",
            429,
            error("rate_limited", "Slow down."),
        ),
        (
            "error-unavailable",
            "The hub is busy.",
            503,
            error("unavailable", "Try again shortly."),
        ),
        (
            "error-unknown-code",
            "A code from a newer hub: treated as transient.",
            503,
            error("from_the_future", "New."),
        ),
        (
            "error-not-found",
            "A path the hub doesn't serve.",
            404,
            error("not_found", "Not found."),
        ),
        (
            "error-bad-gateway-html",
            "A proxy's error page, with no structured error.",
            502,
            "<html>Bad Gateway</html>".into(),
        ),
        (
            "error-teapot-text",
            "A client error with no structured error.",
            418,
            "no".into(),
        ),
    ] {
        out.push((name.into(), desc.into(), status, body, 0));
    }
    Ok(out)
}

pub fn vectors(protocol_dir: &Path) -> std::io::Result<Vec<Vector>> {
    Ok(cases(protocol_dir)?
        .into_iter()
        .map(|(name, description, status, body, held_seconds)| Vector {
            format: FORMAT.into(),
            version: VERSION,
            expect: expect(status, body.as_bytes(), held_seconds),
            name,
            description,
            response: Response { status, body },
            held_seconds,
        })
        .collect())
}

/// Writes every vector as `<name>.json` into `dir`, replacing older ones.
pub fn write(protocol_dir: &Path, dir: &Path) -> std::io::Result<usize> {
    std::fs::create_dir_all(dir)?;
    for e in std::fs::read_dir(dir)? {
        let p = e?.path();
        if p.extension().is_some_and(|x| x == "json") {
            std::fs::remove_file(p)?;
        }
    }
    let all = vectors(protocol_dir)?;
    for v in &all {
        let mut text = serde_json::to_string_pretty(v).expect("serialises");
        text.push('\n');
        std::fs::write(dir.join(format!("{}.json", v.name)), text)?;
    }
    Ok(all.len())
}

/// Checks every vector file in `dir` against the reference behaviour. Returns the failures.
pub fn check(dir: &Path) -> std::io::Result<(usize, Vec<String>)> {
    let mut files: Vec<_> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    files.sort();
    let mut failures = Vec::new();
    for f in &files {
        let name = f.display().to_string();
        let v: Vector = match serde_json::from_slice(&std::fs::read(f)?) {
            Ok(v) => v,
            Err(e) => {
                failures.push(format!("{name}: not a vector ({e})"));
                continue;
            }
        };
        let got = expect(v.response.status, v.response.body.as_bytes(), v.held_seconds);
        if got != v.expect {
            failures.push(format!(
                "{name}: expected {:?}, the reference gives {:?}",
                v.expect, got
            ));
        }
    }
    Ok((files.len(), failures))
}
