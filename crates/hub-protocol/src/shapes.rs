//! What a page's `kind` and `data` mean: the four ready-made shapes, and custom kinds.
//!
//! A display reads these leniently (unknown fields are ignored) with [`Page::content`], and a hub
//! checks them strictly before sending. Either way, a page the display can't draw becomes a
//! "can't show this" card ([`CantShow`]) and never costs the display the rest of the snapshot.
//!
//! [`Page::content`]: crate::snapshot::Page::content

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::limits::*;
use crate::text::{check_text, TextProblem};
use crate::timestamp::Timestamp;

/// Shape names. They never contain `/`, so they can't clash with a custom kind.
pub const KIND_METER: &str = "meter";
pub const KIND_BIG_NUMBER: &str = "big_number";
pub const KIND_TEXT: &str = "text";
pub const KIND_LIST: &str = "list";
pub const SHAPES: [&str; 4] = [KIND_METER, KIND_BIG_NUMBER, KIND_TEXT, KIND_LIST];

/// What a display says on a page it can't show, under the page's title.
pub const CANT_SHOW_TEXT: &str = "This display can't show this page.";
/// A second line, for a kind the display doesn't know.
pub const CANT_SHOW_UPDATE_HINT: &str = "It may need a firmware update.";

/// Why a display can't show one page.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CantShow {
    /// A shape name or custom kind this display doesn't support.
    #[error("unknown page kind")]
    UnknownKind,
    /// The `data` breaks its shape's rules, or is there when it must not be.
    #[error("bad page data: {0}")]
    BadData(String),
    /// The page is bigger than [`MAX_PAGE_BYTES`].
    #[error("the page is over {MAX_PAGE_BYTES} bytes")]
    TooLarge,
}

impl CantShow {
    /// The reason as conformance vectors name it: `unknown_kind`, `bad_data` or `too_large`.
    pub fn code(&self) -> &'static str {
        match self {
            CantShow::UnknownKind => "unknown_kind",
            CantShow::BadData(_) => "bad_data",
            CantShow::TooLarge => "too_large",
        }
    }

    /// The lines to show under the page title.
    pub fn card_lines(&self) -> &'static [&'static str] {
        match self {
            CantShow::UnknownKind => &[CANT_SHOW_TEXT, CANT_SHOW_UPDATE_HINT],
            CantShow::BadData(_) | CantShow::TooLarge => &[CANT_SHOW_TEXT],
        }
    }
}

/// `meter`: a few bars, each a share from 0 to 100.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Meter {
    pub items: Vec<MeterItem>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MeterItem {
    /// Stable within the page.
    pub id: String,
    pub label: String,
    /// 0 to 100. `null` is unknown: never drawn as zero.
    #[serde(default)]
    pub percent: Option<f64>,
    /// When the item's window resets, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<Timestamp>,
}

impl MeterItem {
    pub fn new(
        id: impl Into<String>,
        label: impl Into<String>,
        percent: Option<f64>,
        resets_at: Option<Timestamp>,
    ) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            percent,
            resets_at,
        }
    }
}

/// `big_number`: one figure, large.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BigNumber {
    /// `null` is shown as a dash, never as 0.
    #[serde(default)]
    pub value: Option<f64>,
    /// Decimal places to show, 0 to 3. Absent means 0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decimals: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caption: Option<String>,
}

/// `text`: a short paragraph.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TextBlock {
    pub text: String,
}

/// `list`: up to eight lines, each with an optional detail.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct List {
    pub items: Vec<ListItem>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ListItem {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// A page's `data`, read according to its `kind`.
#[derive(Clone, Debug, PartialEq)]
pub enum PageContent {
    Meter(Meter),
    BigNumber(BigNumber),
    Text(TextBlock),
    List(List),
    /// A well-formed custom kind such as `nearby-flights/1`. The hub doesn't look inside `data`
    /// beyond it being an object that nests no deeper than [`MAX_CUSTOM_DEPTH`]. Only a display
    /// whose firmware knows the kind can draw it.
    Custom {
        kind: String,
        data: Value,
    },
}

impl PageContent {
    /// A custom page. `kind` must look like `name/1`; see [`custom_kind_ok`].
    pub fn custom(kind: impl Into<String>, data: Value) -> Self {
        PageContent::Custom {
            kind: kind.into(),
            data,
        }
    }

    /// The wire `kind` and `data`.
    pub fn into_wire(self) -> (String, Value) {
        fn to<T: Serialize>(v: &T) -> Value {
            serde_json::to_value(v).expect("shapes always serialise")
        }
        match self {
            PageContent::Meter(m) => (KIND_METER.into(), to(&m)),
            PageContent::BigNumber(b) => (KIND_BIG_NUMBER.into(), to(&b)),
            PageContent::Text(t) => (KIND_TEXT.into(), to(&t)),
            PageContent::List(l) => (KIND_LIST.into(), to(&l)),
            PageContent::Custom { kind, data } => (kind, data),
        }
    }
}

impl From<Meter> for PageContent {
    fn from(m: Meter) -> Self {
        PageContent::Meter(m)
    }
}
impl From<BigNumber> for PageContent {
    fn from(b: BigNumber) -> Self {
        PageContent::BigNumber(b)
    }
}
impl From<TextBlock> for PageContent {
    fn from(t: TextBlock) -> Self {
        PageContent::Text(t)
    }
}
impl From<List> for PageContent {
    fn from(l: List) -> Self {
        PageContent::List(l)
    }
}

/// How a `kind` reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KindClass {
    /// One of [`SHAPES`].
    Shape,
    /// `name/version`, such as `nearby-flights/1`.
    Custom,
    /// Neither. A hub never sends it, and a display can't know what it means.
    Invalid,
}

pub fn classify_kind(kind: &str) -> KindClass {
    if SHAPES.contains(&kind) {
        KindClass::Shape
    } else if custom_kind_ok(kind) {
        KindClass::Custom
    } else {
        KindClass::Invalid
    }
}

/// `^[a-z0-9][a-z0-9-]*(\.[a-z0-9][a-z0-9-]*)*/[1-9][0-9]{0,3}$`, at most [`MAX_KIND_LEN`] characters.
pub fn custom_kind_ok(kind: &str) -> bool {
    if kind.len() > MAX_KIND_LEN {
        return false;
    }
    let Some((name, version)) = kind.split_once('/') else {
        return false;
    };
    let label_ok = |l: &str| {
        let b = l.as_bytes();
        !b.is_empty()
            && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
            && b.iter()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
    };
    let version_ok = (1..=4).contains(&version.len())
        && version.bytes().all(|c| c.is_ascii_digit())
        && !version.starts_with('0');
    version_ok && name.split('.').all(label_ok)
}

pub(crate) fn id_problem(id: &str) -> Option<String> {
    let ok = !id.is_empty()
        && id.len() <= MAX_ID_LEN
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !id.starts_with('-');
    (!ok).then(|| format!("must be 1-{MAX_ID_LEN} characters of a-z, 0-9 and '-', not starting with '-'"))
}

pub(crate) fn text_problem(s: &str, max: usize) -> Option<String> {
    check_text(s, max).err().map(|p| match p {
        TextProblem::Empty => "must not be empty".to_string(),
        TextProblem::TooLong => format!("must be at most {max} characters"),
        TextProblem::ControlCharacter => "must not contain control characters".to_string(),
        TextProblem::NotNormalised => "must be Unicode NFC, without surrounding spaces".to_string(),
    })
}

fn bad(s: impl Into<String>) -> CantShow {
    CantShow::BadData(s.into())
}

/// Whether `v` nests containers more than `levels` deep.
fn nests_deeper_than(v: &Value, levels: usize) -> bool {
    let mut children: Box<dyn Iterator<Item = &Value>> = match v {
        Value::Object(m) => Box::new(m.values()),
        Value::Array(a) => Box::new(a.iter()),
        _ => return false,
    };
    levels == 0 || children.any(|c| nests_deeper_than(c, levels - 1))
}

/// Checks an object's keys: none outside `allowed` (hubs only) and all of `required`.
fn check_keys(
    v: &Value,
    path: &str,
    allowed: &[&str],
    required: &[&str],
    strict: bool,
) -> Result<(), CantShow> {
    let Some(obj) = v.as_object() else {
        return Err(bad(format!("{path}: must be an object")));
    };
    if strict {
        if let Some(k) = obj.keys().find(|k| !allowed.contains(&k.as_str())) {
            return Err(bad(format!("{path}: unknown field {k:?}")));
        }
        if let Some(k) = required.iter().find(|k| !obj.contains_key(**k)) {
            return Err(bad(format!("{path}: missing {k:?}")));
        }
    }
    Ok(())
}

fn items_of(data: &Value, max: usize, min: usize) -> Result<&Vec<Value>, CantShow> {
    let items = data
        .get("items")
        .and_then(Value::as_array)
        .ok_or_else(|| bad("items: must be a list"))?;
    if items.len() < min || items.len() > max {
        return Err(bad(format!("items: must have {min}-{max} entries")));
    }
    Ok(items)
}

fn field<T: serde::de::DeserializeOwned>(data: &Value) -> Result<T, CantShow> {
    serde_json::from_value(data.clone()).map_err(|e| bad(e.to_string()))
}

fn text_field(path: &str, s: &str, max: usize) -> Result<(), CantShow> {
    match text_problem(s, max) {
        Some(p) => Err(bad(format!("{path}: {p}"))),
        None => Ok(()),
    }
}

fn opt_text_field(path: &str, s: &Option<String>, max: usize) -> Result<(), CantShow> {
    s.as_deref().map_or(Ok(()), |s| text_field(path, s, max))
}

/// Reads and checks `data` for a page that shows figures. `strict` is what a hub does: it also
/// refuses fields the shape doesn't have and keys that must be present (`percent`, `value`), which
/// a display tolerates.
pub(crate) fn read(kind: &str, data: Option<&Value>, strict: bool) -> Result<PageContent, CantShow> {
    let class = classify_kind(kind);
    if class == KindClass::Invalid {
        return Err(CantShow::UnknownKind);
    }
    let data = match data {
        Some(d) if !d.is_null() => d,
        _ => return Err(bad("data: required")),
    };
    if class == KindClass::Custom {
        if !data.is_object() {
            return Err(bad("data: must be an object"));
        }
        if nests_deeper_than(data, MAX_CUSTOM_DEPTH) {
            return Err(bad(format!("data: nests deeper than {MAX_CUSTOM_DEPTH} levels")));
        }
        return Ok(PageContent::custom(kind, data.clone()));
    }
    match kind {
        KIND_METER => {
            check_keys(data, "data", &["items"], &["items"], strict)?;
            let raw = items_of(data, MAX_METER_ITEMS, 1)?;
            for (i, it) in raw.iter().enumerate() {
                let p = format!("items[{i}]");
                check_keys(
                    it,
                    &p,
                    &["id", "label", "percent", "resets_at"],
                    &["id", "label", "percent"],
                    strict,
                )?;
            }
            let m: Meter = field(data)?;
            let mut seen = HashSet::new();
            for (i, it) in m.items.iter().enumerate() {
                let p = format!("items[{i}]");
                if let Some(e) = id_problem(&it.id) {
                    return Err(bad(format!("{p}.id: {e}")));
                }
                if !seen.insert(it.id.as_str()) {
                    return Err(bad(format!("{p}.id: duplicate item id")));
                }
                text_field(&format!("{p}.label"), &it.label, MAX_LABEL_LEN)?;
                if it
                    .percent
                    .is_some_and(|x| !(x.is_finite() && (0.0..=100.0).contains(&x)))
                {
                    return Err(bad(format!("{p}.percent: must be 0-100 or null")));
                }
            }
            Ok(PageContent::Meter(m))
        }
        KIND_BIG_NUMBER => {
            check_keys(
                data,
                "data",
                &["value", "decimals", "unit", "caption"],
                &["value"],
                strict,
            )?;
            let b: BigNumber = field(data)?;
            if b.value.is_some_and(|x| !x.is_finite()) {
                return Err(bad("value: must be a finite number or null"));
            }
            if b.decimals.is_some_and(|d| d > MAX_DECIMALS) {
                return Err(bad(format!("decimals: must be 0-{MAX_DECIMALS}")));
            }
            opt_text_field("unit", &b.unit, MAX_UNIT_LEN)?;
            opt_text_field("caption", &b.caption, MAX_CAPTION_LEN)?;
            Ok(PageContent::BigNumber(b))
        }
        KIND_TEXT => {
            check_keys(data, "data", &["text"], &["text"], strict)?;
            let t: TextBlock = field(data)?;
            text_field("text", &t.text, MAX_TEXT_LEN)?;
            Ok(PageContent::Text(t))
        }
        KIND_LIST => {
            check_keys(data, "data", &["items"], &["items"], strict)?;
            let raw = items_of(data, MAX_LIST_ITEMS, 0)?;
            for (i, it) in raw.iter().enumerate() {
                check_keys(it, &format!("items[{i}]"), &["text", "detail"], &["text"], strict)?;
            }
            let l: List = field(data)?;
            for (i, it) in l.items.iter().enumerate() {
                text_field(&format!("items[{i}].text"), &it.text, MAX_LIST_TEXT_LEN)?;
                opt_text_field(&format!("items[{i}].detail"), &it.detail, MAX_LIST_TEXT_LEN)?;
            }
            Ok(PageContent::List(l))
        }
        _ => unreachable!("classify_kind only allows the shapes above"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn kinds_are_classified() {
        for k in SHAPES {
            assert_eq!(classify_kind(k), KindClass::Shape);
        }
        for k in ["nearby-flights/1", "a/9999", "org.example.board-2/12", "0x/1"] {
            assert_eq!(classify_kind(k), KindClass::Custom, "{k}");
        }
        for k in [
            "", "gauge", "Meter", "meter/1x", "x/0", "x/10000", "x/01", "x/", "/1", "-x/1", "x..y/1",
            "x.-y/1", "x_y/1", "x/1/2", "X/1", "é/1",
        ] {
            assert_eq!(classify_kind(k), KindClass::Invalid, "{k}");
        }
        let long = format!("{}/1", "a".repeat(MAX_KIND_LEN - 1));
        assert_eq!(classify_kind(&long), KindClass::Invalid);
        let longest = format!("{}/1", "a".repeat(MAX_KIND_LEN - 2));
        assert_eq!(classify_kind(&longest), KindClass::Custom);
    }

    #[test]
    fn nesting_depth_counts_the_data_object() {
        let mut v = json!({"x": 1});
        for _ in 1..MAX_CUSTOM_DEPTH {
            v = json!({ "n": v });
        }
        assert!(read("a/1", Some(&v), true).is_ok());
        assert!(read("a/1", Some(&json!({ "n": v })), true).is_err());
        assert!(read("a/1", Some(&json!([1])), false).is_err());
    }

    #[test]
    fn shapes_are_checked() {
        let ok = |k, d: Value, strict| read(k, Some(&d), strict);
        assert!(ok(
            KIND_METER,
            json!({"items":[{"id":"a","label":"A","percent":null}]}),
            true
        )
        .is_ok());
        assert!(ok(KIND_METER, json!({"items":[]}), false).is_err());
        assert!(ok(
            KIND_METER,
            json!({"items":[{"id":"a","label":"A","percent":101}]}),
            false
        )
        .is_err());
        assert!(ok(KIND_METER, json!({"items":[{"id":"a","label":"A"}]}), false).is_ok());
        assert!(ok(KIND_METER, json!({"items":[{"id":"a","label":"A"}]}), true).is_err());
        assert!(ok(
            KIND_METER,
            json!({"items":[{"id":"a","label":"A","percent":1,"x":1}]}),
            false
        )
        .is_ok());
        assert!(ok(
            KIND_METER,
            json!({"items":[{"id":"a","label":"A","percent":1,"x":1}]}),
            true
        )
        .is_err());
        assert!(ok(KIND_BIG_NUMBER, json!({"value":null}), true).is_ok());
        assert!(ok(KIND_BIG_NUMBER, json!({"value":1,"decimals":4}), false).is_err());
        assert!(ok(KIND_BIG_NUMBER, json!({"value":1,"unit":"123456789"}), false).is_err());
        assert!(ok(KIND_TEXT, json!({"text":""}), false).is_err());
        assert!(ok(KIND_LIST, json!({"items":[]}), true).is_ok());
        assert!(ok(KIND_LIST, json!({"items":[{"text":"x","detail":" y"}]}), false).is_err());
        assert_eq!(read("nope", Some(&json!({})), false), Err(CantShow::UnknownKind));
    }
}
