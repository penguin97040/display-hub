//! Checks a template itself: its words, its form and its page recipes. Every message names the
//! setting or page it is about, for the template's maker.

use hub_core::model::{valid_feed, valid_template_id};
use hub_core::Source;
use hub_protocol::shapes::{classify_kind, KindClass};
use hub_protocol::text::sanitise_text;
use serde_json::Value;

use crate::placeholder::{self, Piece};
use crate::*;

/// Plain text that is already tidy (no control characters, no runs of spaces, trimmed) and at most
/// `max` characters.
fn tidy(s: &str, max: usize) -> bool {
    sanitise_text(s, max).as_deref() == Some(s)
}

fn need_tidy(what: &str, s: &str, max: usize) -> Result<(), String> {
    if tidy(s, max) {
        Ok(())
    } else {
        Err(format!(
            "{what} needs 1 to {max} characters of plain text, with no line breaks or spaces at the ends."
        ))
    }
}

/// A setting key: a lower-case letter, then lower-case letters, digits and underscores.
fn valid_key(k: &str) -> bool {
    k.len() <= MAX_KEY_LEN
        && k.bytes().next().is_some_and(|b| b.is_ascii_lowercase())
        && k.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

impl Template {
    /// Checks the template is complete and makes sense: [`Template::parse`] does this. The error is a
    /// sentence for its maker.
    pub fn check(&self) -> Result<(), String> {
        if !valid_template_id(&self.id) {
            return Err(format!(
                "The template id {:?} needs a name and a version, such as usage-limits/1: lower-case letters, digits and hyphens, then / and a number.",
                self.id
            ));
        }
        need_tidy("The template's name", &self.name, MAX_NAME_LEN)?;
        need_tidy(
            "The template's description",
            &self.description,
            MAX_DESCRIPTION_LEN,
        )?;
        if let Some(setup) = &self.setup {
            need_tidy("The setup help", &setup.help, MAX_HELP_LEN)?;
            need_tidy("The setup command", &setup.command, MAX_COMMAND_LEN)?;
        }
        if self.settings.len() > MAX_FIELDS {
            return Err(format!("A template has at most {MAX_FIELDS} settings."));
        }
        for (i, f) in self.settings.iter().enumerate() {
            if self.settings[..i].iter().any(|g| g.key == f.key) {
                return Err(format!("Two settings are called {:?}.", f.key));
            }
            self.check_field(f)
                .map_err(|e| format!("Setting {:?}: {e}", f.key))?;
        }
        if self.pages.is_empty() || self.pages.len() > MAX_TEMPLATE_PAGES {
            return Err(format!("A template makes 1 to {MAX_TEMPLATE_PAGES} pages."));
        }
        for (i, p) in self.pages.iter().enumerate() {
            self.check_page(p).map_err(|e| format!("Page {}: {e}", i + 1))?;
        }
        Ok(())
    }

    fn check_field(&self, f: &Field) -> Result<(), String> {
        if !valid_key(&f.key) {
            return Err(format!(
                "a key is a lower-case letter, then lower-case letters, digits and underscores, up to {MAX_KEY_LEN} characters."
            ));
        }
        need_tidy("Its label", &f.label, MAX_LABEL_LEN)?;
        if let Some(h) = &f.help {
            need_tidy("Its help", h, MAX_HELP_LEN)?;
        }
        let not_for = |what: &str| format!("{what} isn't used by a {} setting.", kind_name(f.kind));
        let choices = matches!(f.kind, FieldKind::Choice | FieldKind::MultiChoice);
        if f.kind != FieldKind::Text && f.max_length.is_some() {
            return Err(not_for("max_length"));
        }
        if !choices && !f.options.is_empty() {
            return Err(not_for("options"));
        }
        if !matches!(f.kind, FieldKind::Number | FieldKind::MultiChoice)
            && (f.min.is_some() || f.max.is_some())
        {
            return Err(not_for("min and max"));
        }
        for bound in [f.min, f.max].into_iter().flatten() {
            if !bound.is_finite() {
                return Err("min and max are numbers.".into());
            }
        }
        if let (Some(lo), Some(hi)) = (f.min, f.max) {
            if lo > hi {
                return Err("min is more than max.".into());
            }
        }
        match f.kind {
            FieldKind::Text => {
                if f.max_length.is_some_and(|n| n == 0 || n > MAX_TEXT_SETTING_LEN) {
                    return Err(format!("max_length is 1 to {MAX_TEXT_SETTING_LEN}."));
                }
            }
            FieldKind::Choice | FieldKind::MultiChoice => {
                if f.options.is_empty() || f.options.len() > MAX_OPTIONS {
                    return Err(format!("it needs 1 to {MAX_OPTIONS} options."));
                }
                for (i, c) in f.options.iter().enumerate() {
                    if !valid_feed(&c.value) {
                        return Err(format!(
                            "the option value {:?} needs lower-case letters and digits, in words joined by single hyphens, such as claude-max.",
                            c.value
                        ));
                    }
                    if f.options[..i].iter().any(|d| d.value == c.value) {
                        return Err(format!("two options are {:?}.", c.value));
                    }
                    need_tidy(
                        &format!("The label of option {:?}", c.value),
                        &c.label,
                        MAX_LABEL_LEN,
                    )?;
                    if let Some(w) = &c.when {
                        if w.contains_key(&f.key) {
                            return Err(format!("option {:?} depends on its own setting.", c.value));
                        }
                        self.check_condition(w)
                            .map_err(|e| format!("option {:?}: {e}", c.value))?;
                    }
                }
                if f.kind == FieldKind::MultiChoice {
                    let n = f.options.len() as f64;
                    for bound in [f.min, f.max].into_iter().flatten() {
                        if bound.fract() != 0.0 || !(0.0..=n).contains(&bound) {
                            return Err(format!(
                                "for a multi-choice setting, min and max are how many options may be chosen: whole numbers from 0 to {n}."
                            ));
                        }
                    }
                }
            }
            FieldKind::Number | FieldKind::YesNo => {}
        }
        if let Some(d) = &f.default {
            crate::fill::answer(f, d).map_err(|e| format!("its default doesn't fit: {e}"))?;
        }
        Ok(())
    }

    /// Checks a condition names choice or yes/no settings and only values they can have.
    fn check_condition(&self, w: &Condition) -> Result<(), String> {
        if w.is_empty() {
            return Err("its condition names no setting.".into());
        }
        for (key, want) in w {
            let Some(f) = self.field(key) else {
                return Err(format!("its condition names {key:?}, which isn't a setting."));
            };
            let wants: Vec<&Value> = match want {
                Value::Array(a) if !a.is_empty() => a.iter().collect(),
                Value::Array(_) => return Err(format!("its condition lists no values for {key:?}.")),
                v => vec![v],
            };
            for v in wants {
                let ok = match f.kind {
                    FieldKind::Choice | FieldKind::MultiChoice => {
                        v.as_str().is_some_and(|s| f.option(s).is_some())
                    }
                    FieldKind::YesNo => v.is_boolean(),
                    FieldKind::Text | FieldKind::Number => {
                        return Err(format!(
                            "its condition tests {key:?}, but only choice, multi-choice and yes/no settings can be tested."
                        ))
                    }
                };
                if !ok {
                    return Err(format!("its condition wants {v} for {key:?}, which it can't be."));
                }
            }
        }
        Ok(())
    }

    /// Checks placeholders name settings, and `.label` only choices.
    fn check_slots(&self, what: &str, text: &str) -> Result<Vec<Piece>, String> {
        let pieces = placeholder::parse(text).map_err(|e| format!("its {what}: {e}"))?;
        for p in &pieces {
            if let Piece::Slot(refs) = p {
                for r in refs {
                    let Some(f) = self.field(&r.key) else {
                        return Err(format!("its {what} names {:?}, which isn't a setting.", r.key));
                    };
                    if r.label && !matches!(f.kind, FieldKind::Choice | FieldKind::MultiChoice) {
                        return Err(format!(
                            "its {what} asks for {}.label, but only choices have labels.",
                            r.key
                        ));
                    }
                }
            }
        }
        Ok(pieces)
    }

    fn check_page(&self, p: &PageRecipe) -> Result<(), String> {
        let title = self.check_slots("title", &p.title)?;
        if !placeholder::has_slots(&title) {
            need_tidy("Its title", &p.title, hub_protocol::limits::MAX_TITLE_LEN)?;
        }
        if classify_kind(&p.kind) == KindClass::Invalid {
            return Err(format!(
                "its kind {:?} isn't meter, big_number, text, list or a custom kind such as nearby-flights/1.",
                p.kind
            ));
        }
        // The sources it can have: one, or every option of the choice it names.
        let sources: Vec<String> = match placeholder::parse(&p.source) {
            Ok(pieces) if !placeholder::has_slots(&pieces) => vec![p.source.clone()],
            Ok(pieces) => match pieces.as_slice() {
                [Piece::Slot(refs)] if refs.len() == 1 && !refs[0].label => {
                    let key = &refs[0].key;
                    match self.field(key) {
                        Some(f) if f.kind == FieldKind::Choice && (f.required || f.default.is_some()) => {
                            f.options.iter().map(|c| c.value.clone()).collect()
                        }
                        _ => {
                            return Err(format!(
                                "its source {{{key}}} needs to name a choice setting that is required or has a default."
                            ))
                        }
                    }
                }
                _ => return Err("its source is one source, or a single {key} placeholder.".into()),
            },
            Err(e) => return Err(format!("its source: {e}")),
        };
        for s in &sources {
            if Source::from_slug(s).is_none() {
                return Err(format!(
                    "its source could be {s:?}, which isn't demo, manual, helper or none."
                ));
            }
        }
        match &p.feed {
            Some(feed) => {
                let pieces = self.check_slots("feed", feed)?;
                if !placeholder::has_slots(&pieces) && !valid_feed(feed) {
                    return Err(format!(
                        "its feed {feed:?} needs lower-case letters and digits, in words joined by single hyphens, such as garden-weather."
                    ));
                }
            }
            None if sources.iter().any(|s| s == "helper") => {
                return Err("it can take content from a helper, so it needs a feed.".into())
            }
            None => {}
        }
        if let Some(w) = &p.when {
            self.check_condition(w)?;
        }
        Ok(())
    }
}

fn kind_name(k: FieldKind) -> &'static str {
    match k {
        FieldKind::Text => "text",
        FieldKind::Number => "number",
        FieldKind::Choice => "choice",
        FieldKind::MultiChoice => "multi-choice",
        FieldKind::YesNo => "yes/no",
    }
}
