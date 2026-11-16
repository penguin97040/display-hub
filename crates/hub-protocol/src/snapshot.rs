//! The `dashboard.snapshot` message a display polls for.
//!
//! Parsing is lenient where the contract says displays must be (unknown fields are ignored and an
//! unrecognised `data_quality` reads as [`DataQuality::Unrecognised`]), and [`Snapshot::validate`]
//! is strict, for what a hub sends. A page whose `kind` or `data` a display can't use is not a
//! reason to refuse the snapshot: see [`Page::content`] and [`CantShow`].

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::limits::*;
use crate::shapes::{self, classify_kind, id_problem, text_problem, CantShow, KindClass, PageContent};
use crate::timestamp::Timestamp;
use crate::PROTOCOL_VERSION;

pub const MESSAGE_TYPE: &str = "dashboard.snapshot";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub protocol_version: u32,
    pub message_type: String,
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
    /// A ready-made shape (`meter`, `big_number`, `text` or `list`) or a custom kind such as
    /// `nearby-flights/1`. Read it with [`Page::content`].
    pub kind: String,
    pub data_quality: DataQuality,
    /// When the figures were collected. `null` when there have never been any.
    pub updated_at: Option<Timestamp>,
    /// The page is stale once `updated_at` is older than this.
    pub stale_after_seconds: u32,
    /// A short explanation, such as why a page is unavailable. Plain text.
    pub note: Option<String>,
    /// What the page shows, in the shape `kind` names. Absent on an unavailable page, and required
    /// on every other.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl Page {
    /// A page with figures, built from typed content.
    pub fn with_content(
        id: impl Into<String>,
        title: impl Into<String>,
        data_quality: DataQuality,
        updated_at: Option<Timestamp>,
        stale_after_seconds: u32,
        note: Option<String>,
        content: impl Into<PageContent>,
    ) -> Self {
        let (kind, data) = content.into().into_wire();
        Self {
            id: id.into(),
            title: title.into(),
            kind,
            data_quality,
            updated_at,
            stale_after_seconds,
            note,
            data: Some(data),
        }
    }

    /// A page with no figures, and so no `data`. `kind` is what it would show if it had any.
    pub fn unavailable(
        id: impl Into<String>,
        title: impl Into<String>,
        kind: impl Into<String>,
        stale_after_seconds: u32,
        note: Option<String>,
    ) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            kind: kind.into(),
            data_quality: DataQuality::Unavailable,
            updated_at: None,
            stale_after_seconds,
            note,
            data: None,
        }
    }

    /// What a display draws for this page, read leniently (unknown fields in `data` are ignored).
    ///
    /// `Ok(None)` means the page shows no figures (unavailable, or a quality from a newer
    /// revision): draw the title and note only, whatever `kind` says. An `Err` is a page to draw as
    /// a "can't show this" card; the rest of the snapshot is unaffected. A custom kind comes back
    /// as [`PageContent::Custom`], and a display whose firmware doesn't know it draws that card too.
    pub fn content(&self) -> Result<Option<PageContent>, CantShow> {
        self.read(false)
    }

    fn read(&self, strict: bool) -> Result<Option<PageContent>, CantShow> {
        if self.size() > MAX_PAGE_BYTES {
            return Err(CantShow::TooLarge);
        }
        match self.data_quality {
            DataQuality::Unavailable if self.data.as_ref().is_some_and(|d| !d.is_null()) => {
                Err(CantShow::BadData("an unavailable page carries no data".into()))
            }
            q if !q.shows_figures() => Ok(None),
            _ => shapes::read(&self.kind, self.data.as_ref(), strict).map(Some),
        }
    }

    /// Whether a hub may send this page: the page-level rules (size, kind and data), checked
    /// strictly. The common fields are checked by [`Snapshot::violations`].
    pub fn check(&self) -> Result<(), CantShow> {
        self.read(true).map(|_| ())
    }

    /// The serialised size of the page object.
    pub fn size(&self) -> usize {
        serde_json::to_vec(self).map_or(usize::MAX, |b| b.len())
    }
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
    /// A value from a newer protocol revision. Displays treat it as unavailable.
    /// A hub never sends it (validation refuses it). It serialises as `"unrecognised"` only so a
    /// display can save what it received and read it back the same way.
    #[serde(other)]
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
    if let Some(p) = id_problem(id) {
        out.push(v(path, p));
    }
}

fn check_str(path: &str, s: &str, max: usize, out: &mut Vec<Violation>) {
    if let Some(p) = text_problem(s, max) {
        out.push(v(path, p));
    }
}

impl Snapshot {
    /// Every rule a hub must follow before sending. Empty means valid.
    pub fn violations(&self) -> Vec<Violation> {
        let mut out = self.envelope_violations();
        for (i, page) in self.pages.iter().enumerate() {
            let p = format!("pages[{i}]");
            if classify_kind(&page.kind) == KindClass::Invalid {
                out.push(v(
                    format!("{p}.kind"),
                    format!(
                        "must be meter, big_number, text, list or a custom kind like \"name/1\" ({MAX_KIND_LEN} characters at most)"
                    ),
                ));
            }
            match page.read(true) {
                // Reported above, with a better message.
                Err(CantShow::UnknownKind) => {}
                Err(CantShow::BadData(why)) => out.push(v(format!("{p}.data"), why)),
                Err(CantShow::TooLarge) => out.push(v(
                    p,
                    format!("serialised page is {} bytes, over {MAX_PAGE_BYTES}", page.size()),
                )),
                Ok(_) => {}
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

    /// The rules about the envelope and the fields every page shares, but not a page's `kind`,
    /// `data` or size. A display refuses the whole snapshot for these; a page it can't draw only
    /// costs it that page.
    fn envelope_violations(&self) -> Vec<Violation> {
        let mut out = Vec::new();
        if self.protocol_version != PROTOCOL_VERSION {
            out.push(v("protocol_version", format!("must be {PROTOCOL_VERSION}")));
        }
        if self.message_type != MESSAGE_TYPE {
            out.push(v("message_type", format!("must be {MESSAGE_TYPE:?}")));
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
    reread(body)
}

/// [`receive`] without the size cap, for a display reading back its own saved copy of a snapshot
/// it already accepted. Writing a parsed snapshot out again can make it a little longer (an absent
/// `note` becomes `null`, `1E5` becomes `100000.0`), and that must not cost the display its copy.
pub fn reread(body: &[u8]) -> Result<Snapshot, ReceiveError> {
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
    // A display tolerates values from a newer revision, and draws a page it can't use as a card
    // (see `Page::content`). Anything else that breaks the rules makes the snapshot unusable.
    let serious: Vec<String> = snap
        .envelope_violations()
        .into_iter()
        .filter(|x| !x.path.ends_with(".data_quality"))
        .map(|x| x.to_string())
        .collect();
    if !serious.is_empty() {
        return Err(ReceiveError::Invalid(serious.join("; ")));
    }
    Ok(snap)
}
