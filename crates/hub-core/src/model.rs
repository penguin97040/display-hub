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
    /// The template the page was made from, as `name/version` ([`valid_template_id`]), if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template: Option<String>,
    /// The answers given to the template's settings form, if any ([`settings_problem`]). Not
    /// secret: the page's helper can read them, so API keys and passwords never go here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings: Option<Settings>,
}

/// A page's template settings: a JSON object of answers, such as `{"service": "claude-max"}`.
pub type Settings = serde_json::Map<String, serde_json::Value>;

/// The most a page's settings may take, written out as JSON.
pub const MAX_SETTINGS_BYTES: usize = 2048;
/// How deeply a page's settings may nest: the object itself, and lists or objects in it.
pub const MAX_SETTINGS_DEPTH: usize = 3;
/// The longest name of a setting.
pub const MAX_SETTING_KEY_LEN: usize = 64;

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

/// A template id is written like a custom kind, `name/version`, such as `usage-limits/1`.
pub fn valid_template_id(id: &str) -> bool {
    hub_protocol::shapes::custom_kind_ok(id)
}

/// Why a page's settings can't be kept, as a sentence, or `None` when they can: they must be at most
/// [`MAX_SETTINGS_BYTES`] as JSON, nest at most [`MAX_SETTINGS_DEPTH`] deep, and name each setting
/// in 1 to [`MAX_SETTING_KEY_LEN`] bytes.
pub fn settings_problem(settings: &Settings) -> Option<String> {
    fn depth(v: &serde_json::Value) -> usize {
        match v {
            serde_json::Value::Array(a) => 1 + a.iter().map(depth).max().unwrap_or(0),
            serde_json::Value::Object(o) => 1 + o.values().map(depth).max().unwrap_or(0),
            _ => 0,
        }
    }
    fn keys_ok(v: &serde_json::Value) -> bool {
        match v {
            serde_json::Value::Array(a) => a.iter().all(keys_ok),
            serde_json::Value::Object(o) => o
                .iter()
                .all(|(k, v)| !k.is_empty() && k.len() <= MAX_SETTING_KEY_LEN && keys_ok(v)),
            _ => true,
        }
    }
    let size = serde_json::to_string(settings).map_or(usize::MAX, |s| s.len());
    if size > MAX_SETTINGS_BYTES {
        return Some(format!(
            "These settings take {size} bytes; a page's settings may take at most {MAX_SETTINGS_BYTES}. Shorten the longer answers."
        ));
    }
    if 1 + settings.values().map(depth).max().unwrap_or(0) > MAX_SETTINGS_DEPTH {
        return Some(format!(
            "These settings nest too deeply: at most {MAX_SETTINGS_DEPTH} levels, counting the settings themselves."
        ));
    }
    let object = serde_json::Value::Object(settings.clone());
    if !keys_ok(&object) {
        return Some(format!(
            "Each setting needs a name of 1 to {MAX_SETTING_KEY_LEN} characters."
        ));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn settings(v: serde_json::Value) -> Settings {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn settings_are_small_shallow_and_named() {
        assert_eq!(settings_problem(&Settings::new()), None);
        let ok = settings(json!({"service": "claude-max", "days": ["mon", "tue"], "level": 3, "on": true}));
        assert_eq!(settings_problem(&ok), None);
        let ok = settings(json!({"a": [{"b": 1}]}));
        assert_eq!(settings_problem(&ok), None);
        let deep = settings(json!({"a": [[[1]]]}));
        assert!(settings_problem(&deep).unwrap().contains("nest too deeply"));
        let big = settings(json!({"note": "x".repeat(MAX_SETTINGS_BYTES)}));
        assert!(settings_problem(&big).unwrap().contains("at most 2048"));
        let fits = settings(json!({"n": "x".repeat(MAX_SETTINGS_BYTES - 8)}));
        assert_eq!(serde_json::to_string(&fits).unwrap().len(), MAX_SETTINGS_BYTES);
        assert_eq!(settings_problem(&fits), None);
        for bad in [json!({"": 1}), json!({"a": {"": 1}}), json!({"k".repeat(65): 1})] {
            assert!(settings_problem(&settings(bad)).unwrap().contains("a name"));
        }
    }

    #[test]
    fn template_ids_are_written_like_custom_kinds() {
        for ok in ["usage-limits/1", "pi-status/12", "garden.weather/3"] {
            assert!(valid_template_id(ok), "{ok}");
        }
        for bad in ["usage-limits", "usage-limits/0", "Usage/1", "a/1.0", ""] {
            assert!(!valid_template_id(bad), "{bad}");
        }
    }

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
