//! What a page holds, in words and figures for the manager to draw, without any GPUI.
//!
//! The Pages list and the Overview preview both show a page's content: bars for a meter, the number
//! for a big number, the words for text, and the lines of a list. A custom kind is drawn by its
//! display's own design, so the manager only says so.

use hub_core::freshness::short_duration;
use hub_core::Source;
use hub_protocol::shapes::{classify_kind, KindClass, KIND_BIG_NUMBER, KIND_LIST, KIND_METER, KIND_TEXT};
use hub_protocol::{BigNumber, List, Meter, PageContent, TextBlock, Timestamp};
use serde_json::Value;

/// What the manager says for a page it can't draw itself.
pub const CUSTOM_CARD: &str = "This page is drawn by its display's own design.";

/// One bar of a meter.
#[derive(Clone, Debug, PartialEq)]
pub struct MetricRow {
    pub label: String,
    pub used_percent: Option<f64>,
    pub resets: String,
}

/// One line of a list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListLine {
    pub text: String,
    pub detail: Option<String>,
}

/// A page's content, ready to draw.
#[derive(Clone, Debug, PartialEq)]
pub enum Shown {
    /// Nothing stored yet.
    Empty,
    Meter(Vec<MetricRow>),
    BigNumber {
        /// The number with its unit, or a dash when it isn't known.
        value: String,
        caption: Option<String>,
    },
    Text(String),
    List(Vec<ListLine>),
    /// A custom kind: only the display's own design knows how to draw it.
    Custom,
    /// Stored content that doesn't read as its kind.
    Unreadable,
}

impl Shown {
    /// Content already read as its kind. `now` is the hub's clock, for how long until a meter resets.
    pub fn from_content(content: &PageContent, now: Option<Timestamp>) -> Shown {
        match content {
            PageContent::Meter(m) => Shown::meter(m, now),
            PageContent::BigNumber(b) => Shown::BigNumber {
                value: big_number_text(b),
                caption: b.caption.clone(),
            },
            PageContent::Text(t) => Shown::Text(t.text.clone()),
            PageContent::List(l) => Shown::list(l),
            PageContent::Custom { .. } => Shown::Custom,
        }
    }

    /// Stored `data` of a page of this kind. Read leniently, as a display does: nothing here checks
    /// the data, because the hub already did.
    pub fn of_data(kind: &str, data: Option<&Value>, now: Option<Timestamp>) -> Shown {
        let Some(data) = data.filter(|d| !d.is_null()) else {
            return Shown::Empty;
        };
        match classify_kind(kind) {
            KindClass::Custom | KindClass::Invalid => Shown::Custom,
            KindClass::Shape => {
                let read = |kind: &str| -> Option<PageContent> {
                    let v = data.clone();
                    Some(match kind {
                        KIND_METER => PageContent::Meter(serde_json::from_value::<Meter>(v).ok()?),
                        KIND_BIG_NUMBER => {
                            PageContent::BigNumber(serde_json::from_value::<BigNumber>(v).ok()?)
                        }
                        KIND_TEXT => PageContent::Text(serde_json::from_value::<TextBlock>(v).ok()?),
                        KIND_LIST => PageContent::List(serde_json::from_value::<List>(v).ok()?),
                        _ => return None,
                    })
                };
                read(kind).map_or(Shown::Unreadable, |c| Shown::from_content(&c, now))
            }
        }
    }

    fn meter(m: &Meter, now: Option<Timestamp>) -> Shown {
        if m.items.is_empty() {
            return Shown::Empty;
        }
        Shown::Meter(
            m.items
                .iter()
                .map(|it| MetricRow {
                    label: it.label.clone(),
                    used_percent: it.percent,
                    resets: resets_text(it.resets_at, now),
                })
                .collect(),
        )
    }

    fn list(l: &List) -> Shown {
        if l.items.is_empty() {
            return Shown::Empty;
        }
        Shown::List(
            l.items
                .iter()
                .map(|it| ListLine {
                    text: it.text.clone(),
                    detail: it.detail.clone(),
                })
                .collect(),
        )
    }
}

/// "Resets in 3 h 20 min", "Reset due", or nothing when it isn't known.
pub fn resets_text(at: Option<Timestamp>, now: Option<Timestamp>) -> String {
    match (at, now) {
        (Some(at), Some(now)) => {
            let left = now.seconds_until(at);
            if left <= 0 {
                "Reset due".into()
            } else {
                format!("Resets in {}", short_duration(left))
            }
        }
        _ => String::new(),
    }
}

/// A big number as a display writes it: its decimal places, then its unit. A dash when unknown,
/// never 0.
pub fn big_number_text(b: &BigNumber) -> String {
    let Some(v) = b.value else {
        return "–".into();
    };
    let places = usize::from(b.decimals.unwrap_or(0).min(3));
    let number = format!("{v:.places$}");
    match b.unit.as_deref().filter(|u| !u.is_empty()) {
        Some(u) => format!("{number} {u}"),
        None => number,
    }
}

/// A page's kind in plain words.
pub fn kind_label(kind: &str) -> String {
    match kind {
        KIND_METER => "Meter".into(),
        KIND_BIG_NUMBER => "Big number".into(),
        KIND_TEXT => "Short text".into(),
        KIND_LIST => "List".into(),
        other => format!("Custom: {other}"),
    }
}

/// Where a page's content comes from, in plain words.
pub fn origin_text(source: Source, feed: Option<&str>) -> String {
    match (source, feed) {
        (Source::Manual, _) => "Typed in".into(),
        (Source::Demo, _) => "Demo".into(),
        (Source::Helper, Some(feed)) => format!("Filled by a helper (feed {feed})"),
        (Source::Helper, None) => "Filled by a helper".into(),
        (Source::None, _) => "Nothing yet".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn kinds_read_in_plain_words() {
        assert_eq!(kind_label("meter"), "Meter");
        assert_eq!(kind_label("big_number"), "Big number");
        assert_eq!(kind_label("text"), "Short text");
        assert_eq!(kind_label("list"), "List");
        assert_eq!(kind_label("nearby-flights/1"), "Custom: nearby-flights/1");
    }

    #[test]
    fn origins_say_where_the_content_comes_from() {
        assert_eq!(origin_text(Source::Manual, None), "Typed in");
        assert_eq!(origin_text(Source::Demo, Some("x")), "Demo");
        assert_eq!(
            origin_text(Source::Helper, Some("pi-status")),
            "Filled by a helper (feed pi-status)"
        );
    }

    #[test]
    fn each_shape_reads_back_and_unknown_stays_unknown() {
        let meter = json!({"items":[{"id":"a","label":"Tank","percent":null},{"id":"b","label":"Bath","percent":42.5}]});
        let Shown::Meter(rows) = Shown::of_data("meter", Some(&meter), None) else {
            panic!("meter")
        };
        assert_eq!(rows[0].used_percent, None, "unknown is not zero");
        assert_eq!(rows[1].used_percent, Some(42.5));

        let big = json!({"value": 21.456, "decimals": 1, "unit": "°C"});
        assert_eq!(
            Shown::of_data("big_number", Some(&big), None),
            Shown::BigNumber {
                value: "21.5 °C".into(),
                caption: None
            }
        );
        let unknown = json!({"value": null});
        assert_eq!(
            Shown::of_data("big_number", Some(&unknown), None),
            Shown::BigNumber {
                value: "–".into(),
                caption: None
            }
        );
        assert_eq!(
            Shown::of_data("text", Some(&json!({"text": "Back at 3"})), None),
            Shown::Text("Back at 3".into())
        );
        let list = json!({"items":[{"text":"Bins","detail":"Tuesday"},{"text":"Post"}]});
        let Shown::List(lines) = Shown::of_data("list", Some(&list), None) else {
            panic!("list")
        };
        assert_eq!(lines[0].detail.as_deref(), Some("Tuesday"));
        assert_eq!(lines[1].detail, None);

        assert_eq!(Shown::of_data("meter", None, None), Shown::Empty);
        assert_eq!(
            Shown::of_data("meter", Some(&json!({"items": []})), None),
            Shown::Empty
        );
        assert_eq!(
            Shown::of_data("text", Some(&json!({"nope": 1})), None),
            Shown::Unreadable
        );
        assert_eq!(
            Shown::of_data("nearby-flights/1", Some(&json!({"x": 1})), None),
            Shown::Custom
        );
    }
}
