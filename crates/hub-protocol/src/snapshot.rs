//! The `dashboard.snapshot` message a display polls for.
//!
//! Parsing is lenient where the contract says displays must be (unknown fields are ignored and an
//! unrecognised `data_quality` reads as [`DataQuality::Unrecognised`]), and [`Snapshot::validate`]
//! is strict, for what a hub sends.

use serde::{Deserialize, Serialize};

use crate::limits::*;
use crate::text::{check_text, TextProblem};
use crate::timestamp::Timestamp;
use crate::PROTOCOL_VERSION;

pub const MESSAGE_TYPE: &str = "dashboard.snapshot";
pub const DASHBOARD_LLM_USAGE: &str = "llm-usage";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub protocol_version: u32,
    pub message_type: String,
    pub dashboard_id: String,
    /// When the hub built this snapshot, by the hub's clock. Displays measure ages from here.
    pub generated_at: Timestamp,
    /// How long the display should wait before polling again (before its own jitter).
    pub refresh_after_seconds: u32,
    /// In display order. The display shows exactly these pages, in this order.
    pub pages: Vec<Page>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Page {
    /// Stable across snapshots, so a display can keep its place when pages are reordered.
    pub id: String,
    pub title: String,
    pub data_quality: DataQuality,
    /// When the figures were collected. `null` when there have never been any.
    pub updated_at: Option<Timestamp>,
    /// The page is stale once `updated_at` is older than this.
    pub stale_after_seconds: u32,
    /// A short explanation, such as why a page is unavailable. Plain text.
    pub note: Option<String>,
    pub metrics: Vec<Metric>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Metric {
    pub id: String,
    pub label: String,
    /// Share of the allowance **used**, 0 to 100. `null` when unknown: never shown as zero.
    pub used_percent: Option<f64>,
    /// When the allowance window resets, if the source says.
    pub resets_at: Option<Timestamp>,
}

/// Where a page's figures came from. These are not interchangeable, and a display must say which
/// one it is showing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DataQuality {
    /// Sample figures for trying the system out. Not anyone's real usage.
    Demo,
    /// Reported by the provider through a documented interface.
    ProviderReported,
    /// Worked out locally, for example from logs. Approximate.
    Estimated,
    /// Typed in by the owner.
    Manual,
    /// No supported source. The page shows why, not numbers.
    Unavailable,
    /// A value from a newer protocol revision. Displays treat it as unavailable. Never sent.
    #[serde(other, skip_serializing)]
    Unrecognised,
}

impl DataQuality {
    pub const ALL: [DataQuality; 5] = [
        DataQuality::Demo,
        DataQuality::ProviderReported,
        DataQuality::Estimated,
        DataQuality::Manual,
        DataQuality::Unavailable,
    ];

    /// The wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            DataQuality::Demo => "demo",
            DataQuality::ProviderReported => "provider_reported",
            DataQuality::Estimated => "estimated",
            DataQuality::Manual => "manual",
            DataQuality::Unavailable => "unavailable",
            DataQuality::Unrecognised => "unrecognised",
        }
    }

    /// A short upper-case label for a screen.
    pub fn label(self) -> &'static str {
        match self {
            DataQuality::Demo => "DEMO",
            DataQuality::ProviderReported => "REPORTED",
            DataQuality::Estimated => "ESTIMATE",
            DataQuality::Manual => "MANUAL",
            DataQuality::Unavailable | DataQuality::Unrecognised => "UNAVAILABLE",
        }
    }

    /// Whether figures of this quality may be shown as numbers at all.
    pub fn shows_figures(self) -> bool {
        !matches!(self, DataQuality::Unavailable | DataQuality::Unrecognised)
    }
}

/// One rule a snapshot broke, with where.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
#[error("{path}: {problem}")]
pub struct Violation {
    pub path: String,
    pub problem: String,
}

fn v(path: impl Into<String>, problem: impl Into<String>) -> Violation {
    Violation {
        path: path.into(),
        problem: problem.into(),
    }
}

fn check_id(path: &str, id: &str, out: &mut Vec<Violation>) {
    let ok = !id.is_empty()
        && id.len() <= MAX_ID_LEN
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !id.starts_with('-');
    if !ok {
        out.push(v(
            path,
            format!("must be 1-{MAX_ID_LEN} characters of a-z, 0-9 and '-', not starting with '-'"),
        ));
    }
}

fn check_str(path: &str, s: &str, max: usize, out: &mut Vec<Violation>) {
    if let Err(p) = check_text(s, max) {
        out.push(v(
            path,
            match p {
                TextProblem::Empty => "must not be empty".to_string(),
                TextProblem::TooLong => format!("must be at most {max} characters"),
                TextProblem::ControlCharacter => "must not contain control characters".to_string(),
                TextProblem::NotNormalised => "must be Unicode NFC, without surrounding spaces".to_string(),
            },
        ));
    }
}

impl Snapshot {
    /// Every rule a hub must follow before sending. Empty means valid.
    pub fn violations(&self) -> Vec<Violation> {
        let mut out = Vec::new();
        if self.protocol_version != PROTOCOL_VERSION {
            out.push(v("protocol_version", format!("must be {PROTOCOL_VERSION}")));
        }
        if self.message_type != MESSAGE_TYPE {
            out.push(v("message_type", format!("must be {MESSAGE_TYPE:?}")));
        }
        if self.dashboard_id != DASHBOARD_LLM_USAGE {
            out.push(v("dashboard_id", format!("must be {DASHBOARD_LLM_USAGE:?}")));
        }
        if !REFRESH_RANGE.contains(&self.refresh_after_seconds) {
            out.push(v(
                "refresh_after_seconds",
                format!("must be {}-{}", REFRESH_RANGE.start(), REFRESH_RANGE.end()),
            ));
        }
        if self.pages.len() > MAX_PAGES {
            out.push(v("pages", format!("at most {MAX_PAGES} pages")));
        }
        let mut seen = std::collections::HashSet::new();
        for (i, page) in self.pages.iter().enumerate() {
            let p = format!("pages[{i}]");
            check_id(&format!("{p}.id"), &page.id, &mut out);
            if !seen.insert(page.id.as_str()) {
                out.push(v(format!("{p}.id"), "duplicate page id"));
            }
            check_str(&format!("{p}.title"), &page.title, MAX_TITLE_LEN, &mut out);
            if page.data_quality == DataQuality::Unrecognised {
                out.push(v(format!("{p}.data_quality"), "unknown value"));
            }
            if !STALE_AFTER_RANGE.contains(&page.stale_after_seconds) {
                out.push(v(
                    format!("{p}.stale_after_seconds"),
                    format!(
                        "must be {}-{}",
                        STALE_AFTER_RANGE.start(),
                        STALE_AFTER_RANGE.end()
                    ),
                ));
            }
            if let Some(note) = &page.note {
                check_str(&format!("{p}.note"), note, MAX_NOTE_LEN, &mut out);
            }
            if let Some(u) = page.updated_at {
                if u > self.generated_at {
                    out.push(v(format!("{p}.updated_at"), "must not be after generated_at"));
                }
            }
            // Rules that depend on the quality apply only to values this revision knows.
            if page.data_quality.shows_figures() && page.updated_at.is_none() {
                out.push(v(
                    format!("{p}.updated_at"),
                    "required unless the page is unavailable",
                ));
            }
            if page.data_quality == DataQuality::Unavailable && !page.metrics.is_empty() {
                out.push(v(
                    format!("{p}.metrics"),
                    "an unavailable page carries no metrics",
                ));
            }
            if page.metrics.len() > MAX_METRICS {
                out.push(v(
                    format!("{p}.metrics"),
                    format!("at most {MAX_METRICS} metrics"),
                ));
            }
            let mut seen_m = std::collections::HashSet::new();
            for (j, m) in page.metrics.iter().enumerate() {
                let mp = format!("{p}.metrics[{j}]");
                check_id(&format!("{mp}.id"), &m.id, &mut out);
                if !seen_m.insert(m.id.as_str()) {
                    out.push(v(format!("{mp}.id"), "duplicate metric id"));
                }
                check_str(&format!("{mp}.label"), &m.label, MAX_LABEL_LEN, &mut out);
                if let Some(pct) = m.used_percent {
                    if !(pct.is_finite() && (0.0..=100.0).contains(&pct)) {
                        out.push(v(format!("{mp}.used_percent"), "must be 0-100 or null"));
                    }
                }
            }
        }
        if out.is_empty() {
            match serde_json::to_vec(self) {
                Ok(bytes) if bytes.len() > MAX_SNAPSHOT_BYTES => out.push(v(
                    "",
                    format!(
                        "serialised size {} exceeds {MAX_SNAPSHOT_BYTES} bytes",
                        bytes.len()
                    ),
                )),
                Ok(_) => {}
                Err(e) => out.push(v("", format!("does not serialise: {e}"))),
            }
        }
        out
    }

    pub fn validate(&self) -> Result<(), Vec<Violation>> {
        let out = self.violations();
        if out.is_empty() {
            Ok(())
        } else {
            Err(out)
        }
    }

    /// Serialises a snapshot the hub is about to send, refusing one that breaks the contract.
    pub fn to_wire(&self) -> Result<Vec<u8>, Vec<Violation>> {
        self.validate()?;
        Ok(serde_json::to_vec(self).expect("checked by validate"))
    }
}

/// Why a display could not use a response body.
#[derive(Debug, thiserror::Error)]
pub enum ReceiveError {
    #[error("the response was larger than {MAX_SNAPSHOT_BYTES} bytes")]
    TooLarge,
    #[error("the response was not valid snapshot JSON: {0}")]
    Malformed(String),
    #[error(
        "the hub speaks protocol version {0}, and this display understands only version {PROTOCOL_VERSION}"
    )]
    IncompatibleVersion(u32),
    #[error("the response was not a {MESSAGE_TYPE} message")]
    WrongMessageType,
    #[error("the snapshot broke the protocol: {0}")]
    Invalid(String),
}

/// How a display reads a snapshot: size first, then the version, then the body. Unknown fields
/// are ignored; anything that would make the display show wrong figures is refused.
pub fn receive(body: &[u8]) -> Result<Snapshot, ReceiveError> {
    if body.len() > MAX_SNAPSHOT_BYTES {
        return Err(ReceiveError::TooLarge);
    }
    #[derive(Deserialize)]
    struct Head {
        protocol_version: u32,
        message_type: Option<String>,
    }
    let head: Head = serde_json::from_slice(body).map_err(|e| ReceiveError::Malformed(e.to_string()))?;
    if head.protocol_version != PROTOCOL_VERSION {
        return Err(ReceiveError::IncompatibleVersion(head.protocol_version));
    }
    if head.message_type.as_deref() != Some(MESSAGE_TYPE) {
        return Err(ReceiveError::WrongMessageType);
    }
    let snap: Snapshot = serde_json::from_slice(body).map_err(|e| ReceiveError::Malformed(e.to_string()))?;
    // A display tolerates values from a newer revision, but not figures that break the rules.
    let serious: Vec<String> = snap
        .violations()
        .into_iter()
        .filter(|x| !x.path.ends_with(".data_quality"))
        .map(|x| x.to_string())
        .collect();
    if !serious.is_empty() {
        return Err(ReceiveError::Invalid(serious.join("; ")));
    }
    Ok(snap)
}
