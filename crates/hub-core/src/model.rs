//! Pages as the hub stores them, where their content comes from, and the text rules for what owners
//! type in.

use hub_protocol::limits::{MAX_LABEL_LEN, MAX_NOTE_LEN, MAX_TITLE_LEN};
use hub_protocol::text::sanitise_text;
use hub_protocol::{DataQuality, Timestamp};
use serde::{Deserialize, Serialize};

/// Where a page's content comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Source {
    /// Sample content, set through the manager. Always labelled demo, and never stale.
    Demo,
    /// Typed in by the owner through the manager.
    Manual,
    /// Sent by a paired helper, such as the PC helper, to the page's feed.
    Helper,
    /// Nothing yet. The page shows as unavailable, with the reason.
    None,
}

impl Source {
    pub const ALL: [Source; 4] = [Source::Demo, Source::Manual, Source::Helper, Source::None];

    pub fn slug(self) -> &'static str {
        match self {
            Source::Demo => "demo",
            Source::Manual => "manual",
            Source::Helper => "helper",
            Source::None => "none",
        }
    }

    pub fn from_slug(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|x| x.slug() == s)
    }

    /// How long content from this source stays fresh.
    pub fn stale_after_seconds(self) -> u32 {
        match self {
            Source::Demo => 3600,
            Source::Manual => 86_400,
            Source::Helper => 1800,
            Source::None => 3600,
        }
    }
}

/// One page as the hub stores it: any kind, with whatever was last stored for it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StoredPage {
    /// The page id: lower-case letters, digits and hyphens.
    pub id: String,
    pub title: String,
    /// A ready-made shape (`meter`, `big_number`, `text`, `list`) or a custom `name/version` kind.
    pub kind: String,
    pub source: Source,
    /// The named feed that fills the page ([`valid_feed`]). A helper may write only to pages whose
    /// feed it is allowed and whose source is `helper`.
    pub feed: Option<String>,
    pub stale_after_seconds: u32,
    /// What was last stored for the page. `None` until something is.
    pub content: Option<StoredContent>,
}

/// What was last stored for a page.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StoredContent {
    pub quality: DataQuality,
    pub updated_at: Option<Timestamp>,
    /// The page's `data`, in the shape its kind names. `None` when there is nothing to show.
    pub data: Option<serde_json::Value>,
    pub note: Option<String>,
}

/// Owner-supplied page titles and notes, made safe for the wire.
pub fn clean_title(s: &str) -> Option<String> {
    sanitise_text(s, MAX_TITLE_LEN)
}

pub fn clean_note(s: &str) -> Option<String> {
    sanitise_text(s, MAX_NOTE_LEN)
}

pub fn clean_label(s: &str) -> Option<String> {
    sanitise_text(s, MAX_LABEL_LEN)
}

/// A page id is valid wire text and short.
pub fn valid_page_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 48
        && !id.starts_with('-')
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// The longest feed name.
pub const MAX_FEED_LEN: usize = 48;

/// A feed name is a lower-case slug: words of `a-z` and `0-9` joined by single hyphens, such as
/// `garden-weather`, up to [`MAX_FEED_LEN`] characters.
pub fn valid_feed(name: &str) -> bool {
    name.len() <= MAX_FEED_LEN
        && name.split('-').all(|word| {
            !word.is_empty() && word.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feed_names_are_short_lower_case_slugs() {
        for ok in [
            "claude-max",
            "chatgpt-plus",
            "opencode-go-plus",
            "a",
            "garden2",
            &"x".repeat(48),
        ] {
            assert!(valid_feed(ok), "{ok}");
        }
        for bad in [
            "",
            "-x",
            "x-",
            "a--b",
            "Garden",
            "a_b",
            "a.b",
            "a b",
            "kāpiti",
            &"x".repeat(49),
        ] {
            assert!(!valid_feed(bad), "{bad}");
        }
    }

    #[test]
    fn sources_keep_their_stored_names() {
        for s in Source::ALL {
            assert_eq!(Source::from_slug(s.slug()), Some(s));
        }
        assert_eq!(Source::Helper.slug(), "helper");
        assert_eq!(serde_json::to_value(Source::Helper).unwrap(), "helper");
    }
}
