//! Page templates: a short settings form and the pages its answers make, written as declarative
//! JSON (`docs/templates.md`). The desktop manager shows the form, checks the answers with
//! [`Template::settings_from`] and sends the pages [`Template::make_pages`] gives it. Each page
//! records the template's id and the answers, so the page's helper can read them later.
//!
//! The hub never reads templates. A template holds no code: only fields, options, simple conditions
//! and `{key}` placeholders.

mod check;
mod fill;
mod placeholder;

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub use hub_core::Settings;

/// The largest template file read.
pub const MAX_TEMPLATE_BYTES: usize = 16 * 1024;
/// The most settings in one form.
pub const MAX_FIELDS: usize = 20;
/// The most options a choice offers.
pub const MAX_OPTIONS: usize = 20;
/// The most pages one template makes. A display shows at most this many pages.
pub const MAX_TEMPLATE_PAGES: usize = hub_protocol::limits::MAX_PAGES;
/// The longest setting key.
pub const MAX_KEY_LEN: usize = 32;
/// The longest a text setting may allow, and what it allows when the template doesn't say.
pub const MAX_TEXT_SETTING_LEN: usize = 500;
pub const DEFAULT_TEXT_SETTING_LEN: usize = 100;
/// Lengths of a template's own words.
pub const MAX_NAME_LEN: usize = 48;
pub const MAX_LABEL_LEN: usize = 48;
pub const MAX_DESCRIPTION_LEN: usize = 280;
pub const MAX_HELP_LEN: usize = 280;
pub const MAX_COMMAND_LEN: usize = 500;

/// A page template, as read from its JSON.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Template {
    /// `name/version`, written like a custom kind, such as `usage-limits/1`. A changed template
    /// gets a new version, so pages made from the old one can still be told apart.
    pub id: String,
    /// What the manager calls it, such as "Usage limits".
    pub name: String,
    /// One or two plain sentences saying what the pages show.
    pub description: String,
    /// What the owner must do once, by hand, for the pages to fill, such as start a helper.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setup: Option<Setup>,
    /// The settings form, in the order it is shown.
    #[serde(default)]
    pub settings: Vec<Field>,
    /// The pages the answers make, in order.
    pub pages: Vec<PageRecipe>,
}

/// A step the owner takes once, such as setting up a helper on the Pi, for the app to show with a
/// Copy button.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Setup {
    /// Where and why to run it, in a sentence.
    pub help: String,
    /// The one command to run, exactly as typed.
    pub command: String,
}

/// One setting on the form.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Field {
    /// The answer's name in the page's settings: lower case, such as `service` or `show_weekly`.
    pub key: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub help: Option<String>,
    #[serde(rename = "type")]
    pub kind: FieldKind,
    /// Whether it must be answered. A yes/no setting always is (it defaults to no).
    #[serde(default)]
    pub required: bool,
    /// The answer when none is given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<serde_json::Value>,
    /// Text only: the most characters, up to [`MAX_TEXT_SETTING_LEN`] (default
    /// [`DEFAULT_TEXT_SETTING_LEN`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_length: Option<usize>,
    /// A number's lowest and highest values, or how few and how many options a multi-choice
    /// setting takes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
    /// Choice and multi-choice only: what may be chosen.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<Choice>,
}

/// What kind of answer a setting takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldKind {
    /// A line of plain text.
    Text,
    /// A number, perhaps with decimals.
    Number,
    /// One of the options.
    Choice,
    /// Any number of the options, as a list.
    MultiChoice,
    /// Yes (`true`) or no (`false`).
    YesNo,
}

/// One option of a choice.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Choice {
    /// What is stored: a lower-case slug, such as `claude-max`.
    pub value: String,
    /// What the owner sees.
    pub label: String,
    /// Offered only when the other answers match this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<Condition>,
}

/// A test of other answers: each named setting must have one of the values given (one value, or a
/// list of them). A multi-choice setting matches when any of its chosen options is listed. Every
/// setting named must match.
pub type Condition = BTreeMap<String, serde_json::Value>;

/// One page a template makes.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PageRecipe {
    /// The page's title. May hold placeholders: `{key}` is a setting's answer, `{key.label}` the
    /// label of the option chosen, and `{a|b}` is `a`'s answer, or `b`'s when `a` is blank.
    pub title: String,
    /// A ready-made shape (`meter`, `big_number`, `text`, `list`) or a custom `name/version` kind.
    pub kind: String,
    /// `demo`, `manual`, `helper` or `none`, or `{key}` for a choice whose options are those.
    pub source: String,
    /// The feed that fills the page. May hold placeholders, like the title. A helper page needs one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feed: Option<String>,
    /// Made only when the answers match this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<Condition>,
}

impl Template {
    /// Reads a template from its JSON and checks it ([`Template::check`]). The error is a sentence
    /// for the template's maker.
    pub fn parse(json: &str) -> Result<Template, String> {
        if json.len() > MAX_TEMPLATE_BYTES {
            return Err(format!(
                "This template is {} bytes; a template may be at most {MAX_TEMPLATE_BYTES}.",
                json.len()
            ));
        }
        let t: Template =
            serde_json::from_str(json).map_err(|e| format!("This template can't be read: {e}."))?;
        t.check()?;
        Ok(t)
    }

    /// The setting called `key`.
    pub fn field(&self, key: &str) -> Option<&Field> {
        self.settings.iter().find(|f| f.key == key)
    }
}

impl Field {
    /// The options to offer, given the answers so far: those without a condition, and those whose
    /// condition the answers match.
    pub fn available_options<'a>(&'a self, answers: &'a Settings) -> impl Iterator<Item = &'a Choice> + 'a {
        self.options
            .iter()
            .filter(|c| c.when.as_ref().is_none_or(|w| fill::matches(w, answers)))
    }

    /// The option whose value is `value`.
    pub fn option(&self, value: &str) -> Option<&Choice> {
        self.options.iter().find(|c| c.value == value)
    }
}

/// The templates that come with Display Hub, as JSON, by file.
const BUILT_IN: [(&str, &str); 6] = [
    (
        "usage-limits.json",
        include_str!("../templates/usage-limits.json"),
    ),
    ("pi-status.json", include_str!("../templates/pi-status.json")),
    ("big-number.json", include_str!("../templates/big-number.json")),
    ("short-text.json", include_str!("../templates/short-text.json")),
    ("list.json", include_str!("../templates/list.json")),
    ("meter.json", include_str!("../templates/meter.json")),
];

/// The templates that come with Display Hub, in the order the manager offers them.
pub fn built_in() -> Vec<Template> {
    BUILT_IN
        .iter()
        .map(|(file, json)| {
            Template::parse(json).unwrap_or_else(|e| panic!("built-in template {file} is broken: {e}"))
        })
        .collect()
}

/// The built-in template with this id, such as `pi-status/1`.
pub fn built_in_template(id: &str) -> Option<Template> {
    built_in().into_iter().find(|t| t.id == id)
}
