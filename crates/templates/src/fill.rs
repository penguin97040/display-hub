//! Checks a filled-in settings form and turns it into the pages to add.

use hub_core::api::NewPage;
use hub_core::model::{clean_title, settings_problem, valid_feed};
use hub_core::Source;
use hub_protocol::text::sanitise_text;
use serde_json::Value;

use crate::placeholder::{self, Piece};
use crate::*;

/// Checks one answer against its setting and tidies it: text is made plain and trimmed, a number
/// given as text is read, and a whole number is kept as one. `None` for a blank answer. The error is
/// a sentence for the owner.
pub(crate) fn answer(f: &Field, v: &Value) -> Result<Option<Value>, String> {
    let label = &f.label;
    match f.kind {
        FieldKind::Text => {
            let s = v.as_str().ok_or_else(|| format!("{label} needs to be text."))?;
            let max = f.max_length.unwrap_or(DEFAULT_TEXT_SETTING_LEN);
            // Cleaned with room to spare, so an answer that is too long is refused, not cut.
            match sanitise_text(s, max + 1) {
                None => Ok(None),
                Some(t) if t.chars().count() > max => {
                    Err(format!("{label} can be at most {max} characters."))
                }
                Some(t) => Ok(Some(Value::String(t))),
            }
        }
        FieldKind::Number => {
            let n = match v {
                Value::Number(n) => n.as_f64(),
                Value::String(s) if s.trim().is_empty() => return Ok(None),
                Value::String(s) => s.trim().parse::<f64>().ok(),
                _ => None,
            }
            .filter(|n| n.is_finite())
            .ok_or_else(|| format!("{label} needs to be a number."))?;
            if let Some(lo) = f.min.filter(|lo| n < *lo) {
                return Err(format!("{label} needs to be at least {}.", number_text(lo)));
            }
            if let Some(hi) = f.max.filter(|hi| n > *hi) {
                return Err(format!("{label} can be at most {}.", number_text(hi)));
            }
            Ok(Some(number_value(n)))
        }
        FieldKind::Choice => {
            let s = v
                .as_str()
                .ok_or_else(|| format!("Choose one of the options for {label}."))?;
            if s.is_empty() {
                return Ok(None);
            }
            match f.option(s) {
                Some(_) => Ok(Some(Value::String(s.to_string()))),
                None => Err(format!("{s:?} isn't one of the options for {label}.")),
            }
        }
        FieldKind::MultiChoice => {
            let list = v
                .as_array()
                .ok_or_else(|| format!("{label} needs a list of the options chosen."))?;
            let mut out: Vec<Value> = Vec::new();
            for item in list {
                let s = item
                    .as_str()
                    .filter(|s| f.option(s).is_some())
                    .ok_or_else(|| format!("{item} isn't one of the options for {label}."))?;
                if out.iter().any(|o| o == s) {
                    return Err(format!(
                        "{} is chosen twice for {label}.",
                        f.option(s).map_or(s, |c| &c.label)
                    ));
                }
                out.push(Value::String(s.to_string()));
            }
            let n = out.len() as f64;
            if let Some(lo) = f.min.filter(|lo| n < *lo) {
                return Err(format!("Choose at least {} for {label}.", count(lo)));
            }
            if let Some(hi) = f.max.filter(|hi| n > *hi) {
                return Err(format!("Choose at most {} for {label}.", count(hi)));
            }
            Ok(Some(Value::Array(out)))
        }
        FieldKind::YesNo => match v {
            Value::Bool(_) => Ok(Some(v.clone())),
            _ => Err(format!("{label} needs to be yes (true) or no (false).")),
        },
    }
}

fn count(n: f64) -> String {
    match n as u64 {
        1 => "1 option".into(),
        n => format!("{n} options"),
    }
}

/// A number as JSON: whole numbers without a decimal point.
fn number_value(n: f64) -> Value {
    if n.fract() == 0.0 && n.abs() < 9.0e15 {
        Value::from(n as i64)
    } else {
        Value::from(n)
    }
}

fn number_text(n: f64) -> String {
    number_value(n).to_string()
}

/// Whether the answers match a condition: each named setting has one of the values wanted.
pub(crate) fn matches(w: &Condition, answers: &Settings) -> bool {
    w.iter().all(|(key, want)| {
        let wants = |v: &Value| match want {
            Value::Array(a) => a.contains(v),
            one => one == v,
        };
        match answers.get(key) {
            Some(Value::Array(chosen)) => chosen.iter().any(wants),
            Some(v) => wants(v),
            None => false,
        }
    })
}

impl Template {
    /// Checks a filled-in form and gives the settings to record on each page: every answer tidied,
    /// defaults filled in, and blank answers to optional settings left out. A yes/no setting not
    /// answered is no. The error is a sentence for the owner.
    pub fn settings_from(&self, answers: &Settings) -> Result<Settings, String> {
        if let Some(key) = answers.keys().find(|k| self.field(k).is_none()) {
            return Err(format!("This template has no setting called {key:?}."));
        }
        let mut out = Settings::new();
        for f in &self.settings {
            let given = match answers.get(&f.key).filter(|v| !v.is_null()) {
                Some(v) => answer(f, v)?,
                None => None,
            };
            let value = match given {
                Some(v) => Some(v),
                None => f.default.as_ref().map(|d| answer(f, d)).transpose()?.flatten(),
            };
            let value = match (value, f.kind) {
                (Some(v), _) => v,
                (None, FieldKind::YesNo) => Value::Bool(false),
                (None, _) if !f.required => continue,
                (None, FieldKind::Choice | FieldKind::MultiChoice) => {
                    return Err(format!("Choose an option for {}.", f.label))
                }
                (None, _) => return Err(format!("Fill in {}.", f.label)),
            };
            out.insert(f.key.clone(), value);
        }
        // An option offered only with some other answers can't be chosen with others.
        for f in &self.settings {
            let chosen: Vec<&str> = match out.get(&f.key) {
                Some(Value::String(s)) => vec![s.as_str()],
                Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).collect(),
                _ => continue,
            };
            for value in chosen {
                let Some(choice) = f.option(value) else { continue };
                if let Some(w) = choice.when.as_ref().filter(|w| !matches(w, &out)) {
                    return Err(format!(
                        "{} isn't available {}.",
                        choice.label,
                        self.describe(w, &out)
                    ));
                }
            }
        }
        if let Some(why) = settings_problem(&out) {
            return Err(why);
        }
        Ok(out)
    }

    /// The answers a condition tests, as words: "when Service is OpenCode Go Plus".
    fn describe(&self, w: &Condition, answers: &Settings) -> String {
        let parts: Vec<String> = w
            .keys()
            .filter_map(|k| {
                let f = self.field(k)?;
                let shown = match answers.get(k) {
                    Some(Value::String(s)) => f.option(s).map_or(s.clone(), |c| c.label.clone()),
                    Some(Value::Bool(b)) => if *b { "yes" } else { "no" }.into(),
                    Some(Value::Array(a)) if a.is_empty() => "none chosen".into(),
                    Some(v) => text_of(f, v, false),
                    None => "not answered".into(),
                };
                Some(format!("{} is {shown}", f.label))
            })
            .collect();
        if parts.is_empty() {
            "with these settings".into()
        } else {
            format!("when {}", parts.join(" and "))
        }
    }

    /// Checks a filled-in form ([`Template::settings_from`]) and gives the pages to add, in order,
    /// each recording this template's id and the settings. The error is a sentence for the owner.
    pub fn make_pages(&self, answers: &Settings) -> Result<Vec<NewPage>, String> {
        let settings = self.settings_from(answers)?;
        let mut pages = Vec::new();
        for p in &self.pages {
            if p.when.as_ref().is_some_and(|w| !matches(w, &settings)) {
                continue;
            }
            let title = self.fill(&p.title, &settings);
            let title = clean_title(&title).ok_or_else(|| {
                "A page would have no title. Fill in the settings it takes its title from.".to_string()
            })?;
            let source_text = self.fill(&p.source, &settings);
            let source = Source::from_slug(&source_text)
                .ok_or_else(|| format!("{source_text:?} isn't a page source."))?;
            let feed = match &p.feed {
                Some(f) => {
                    let feed = self.fill(f, &settings);
                    if !valid_feed(&feed) {
                        return Err(format!(
                            "These settings would give the feed {feed:?}, which isn't a feed name: lower-case letters and digits, in words joined by single hyphens."
                        ));
                    }
                    Some(feed)
                }
                None => None,
            };
            pages.push(NewPage {
                title,
                kind: p.kind.clone(),
                source,
                feed,
                id: None,
                stale_after_seconds: None,
                template: Some(self.id.clone()),
                settings: Some(settings.clone()),
            });
        }
        if pages.is_empty() {
            return Err("These settings make no pages. Change them so at least one page is made.".into());
        }
        Ok(pages)
    }

    /// Text with its placeholders filled from the settings. A template is checked before use, so
    /// its placeholders parse.
    fn fill(&self, text: &str, settings: &Settings) -> String {
        let pieces = placeholder::parse(text).unwrap_or_default();
        let mut out = String::new();
        for piece in pieces {
            match piece {
                Piece::Text(t) => out.push_str(&t),
                Piece::Slot(refs) => {
                    let first = refs.iter().find_map(|r| {
                        let f = self.field(&r.key)?;
                        let text = text_of(f, settings.get(&r.key)?, r.label);
                        (!text.trim().is_empty()).then_some(text)
                    });
                    out.push_str(&first.unwrap_or_default());
                }
            }
        }
        out
    }
}

/// An answer as text in a title or feed: the value (or option label), several joined by ", ", and
/// yes or no.
fn text_of(f: &Field, v: &Value, label: bool) -> String {
    let one = |s: &str| {
        if label {
            f.option(s).map_or(s.to_string(), |c| c.label.clone())
        } else {
            s.to_string()
        }
    };
    match v {
        Value::String(s) => one(s),
        Value::Array(a) => a
            .iter()
            .filter_map(Value::as_str)
            .map(one)
            .collect::<Vec<_>>()
            .join(", "),
        Value::Bool(b) => if *b { "yes" } else { "no" }.into(),
        other => other.to_string(),
    }
}
