//! The forms on the Pages screen, without any GPUI: a template's settings form, and the editors for
//! content someone types in. Each checks what was entered and says what is wrong in plain words.

use hub_core::api::{NewPage, PageContentInput, PageView};
use hub_core::model::clean_note;
use hub_protocol::limits::{
    MAX_BIG_NUMBER_ABS, MAX_CAPTION_LEN, MAX_LABEL_LEN, MAX_LIST_ITEMS, MAX_LIST_TEXT_LEN, MAX_METER_ITEMS,
    MAX_NOTE_LEN, MAX_TEXT_LEN, MAX_UNIT_LEN,
};
use hub_protocol::shapes::{classify_kind, KindClass, KIND_BIG_NUMBER, KIND_LIST, KIND_METER, KIND_TEXT};
use hub_protocol::text::sanitise_text;
use hub_protocol::{BigNumber, DataQuality, List, ListItem, Meter, MeterItem, TextBlock, Timestamp};
use hub_templates::{number_text, Field as Setting, FieldKind, Settings, Template, DEFAULT_TEXT_SETTING_LEN};
use serde_json::Value;
use usage::{ManualFigures, ManualMetric, Service, Window};

use crate::field::Field;
use crate::model::{format_percent, parse_duration, parse_percent};

// ---- a template's settings form -------------------------------------------------------------------

/// What has been entered for one setting.
#[derive(Clone, Debug)]
pub enum Input {
    Text(Field),
    /// Typed as text and read when checked, so "3.5" and a typing slip both get a plain message.
    Number(Field),
    Choice(Option<String>),
    Multi(Vec<String>),
    YesNo(bool),
}

/// One option a choice offers right now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OptionView {
    pub value: String,
    pub label: String,
    pub chosen: bool,
}

/// A template's form, with what has been entered so far.
#[derive(Clone, Debug)]
pub struct TemplateForm {
    pub template: Template,
    pub inputs: Vec<Input>,
    /// What the last check found wrong, in words for the owner.
    pub error: Option<String>,
}

impl TemplateForm {
    /// A form with the template's defaults, or with a page's stored settings when editing it.
    pub fn new(template: Template, stored: Option<&Settings>) -> Self {
        let inputs = template
            .settings
            .iter()
            .map(|s| Input::for_setting(s, stored))
            .collect();
        let mut form = Self {
            template,
            inputs,
            error: None,
        };
        form.settle();
        form
    }

    /// The answers so far, as the template's checker takes them. Blank text and numbers and unchosen
    /// options are left out, so defaults apply.
    pub fn answers(&self) -> Settings {
        let mut out = Settings::new();
        for (s, input) in self.template.settings.iter().zip(&self.inputs) {
            let value = match input {
                Input::Text(f) | Input::Number(f) if !f.trimmed().is_empty() => Value::String(f.text.clone()),
                Input::Choice(Some(v)) => Value::String(v.clone()),
                Input::Multi(v) if !v.is_empty() => {
                    Value::Array(v.iter().cloned().map(Value::String).collect())
                }
                Input::YesNo(b) => Value::Bool(*b),
                _ => continue,
            };
            out.insert(s.key.clone(), value);
        }
        out
    }

    /// The pages these answers make, or what is wrong with them.
    pub fn check(&self) -> Result<Vec<NewPage>, String> {
        self.template.make_pages(&self.answers())
    }

    /// The options a choice offers now: only those the other answers allow.
    pub fn options(&self, i: usize) -> Vec<OptionView> {
        let Some(setting) = self.template.settings.get(i) else {
            return Vec::new();
        };
        let answers = self.answers();
        setting
            .available_options(&answers)
            .map(|c| OptionView {
                value: c.value.clone(),
                label: c.label.clone(),
                chosen: match self.inputs.get(i) {
                    Some(Input::Choice(Some(v))) => *v == c.value,
                    Some(Input::Multi(v)) => v.contains(&c.value),
                    _ => false,
                },
            })
            .collect()
    }

    /// Chooses an option of a single-choice setting. Choosing the one already chosen clears it, for
    /// a setting that may be left unanswered.
    pub fn pick(&mut self, i: usize, value: &str) {
        let required = self.template.settings.get(i).is_some_and(|s| s.required);
        if let Some(Input::Choice(current)) = self.inputs.get_mut(i) {
            *current = if current.as_deref() == Some(value) && !required {
                None
            } else {
                Some(value.to_string())
            };
        }
        self.changed();
    }

    /// Ticks or unticks an option of a multi-choice setting.
    pub fn toggle(&mut self, i: usize, value: &str) {
        if let Some(Input::Multi(chosen)) = self.inputs.get_mut(i) {
            match chosen.iter().position(|v| v == value) {
                Some(at) => {
                    chosen.remove(at);
                }
                None => chosen.push(value.to_string()),
            }
        }
        self.changed();
    }

    pub fn set_yes_no(&mut self, i: usize, yes: bool) {
        if let Some(Input::YesNo(b)) = self.inputs.get_mut(i) {
            *b = yes;
        }
        self.changed();
    }

    /// The text box of a text or number setting.
    pub fn text_mut(&mut self, i: usize) -> Option<&mut Field> {
        match self.inputs.get_mut(i)? {
            Input::Text(f) | Input::Number(f) => Some(f),
            _ => None,
        }
    }

    /// An answer changed: a message about the old answers no longer applies, and an option another
    /// answer ruled out is dropped.
    fn changed(&mut self) {
        self.error = None;
        self.settle();
    }

    /// Drops chosen options the current answers don't offer, going back to the default where it is
    /// still offered. A choice can depend on another, so this repeats until nothing changes.
    fn settle(&mut self) {
        for _ in 0..=self.inputs.len() {
            let mut moved = false;
            for i in 0..self.inputs.len() {
                if !matches!(self.inputs[i], Input::Choice(_) | Input::Multi(_)) {
                    continue;
                }
                let offered: Vec<String> = self.options(i).into_iter().map(|o| o.value).collect();
                let default = self.template.settings[i].default.clone();
                let replacement = match &self.inputs[i] {
                    Input::Choice(Some(v)) if !offered.contains(v) => Some(Input::Choice(
                        default
                            .as_ref()
                            .and_then(Value::as_str)
                            .filter(|d| offered.iter().any(|o| o == d))
                            .map(str::to_string),
                    )),
                    Input::Multi(v) if v.iter().any(|x| !offered.contains(x)) => Some(Input::Multi(
                        v.iter().filter(|x| offered.contains(x)).cloned().collect(),
                    )),
                    _ => None,
                };
                if let Some(r) = replacement {
                    self.inputs[i] = r;
                    moved = true;
                }
            }
            if !moved {
                break;
            }
        }
    }

    /// The line under a setting: its own help, and what a number or text may be.
    pub fn hint(&self, i: usize) -> String {
        let Some(s) = self.template.settings.get(i) else {
            return String::new();
        };
        let mut parts: Vec<String> = s.help.iter().cloned().collect();
        match s.kind {
            FieldKind::Number => match (s.min, s.max) {
                (Some(lo), Some(hi)) => {
                    parts.push(format!("From {} to {}.", number_text(lo), number_text(hi)))
                }
                (Some(lo), None) => parts.push(format!("At least {}.", number_text(lo))),
                (None, Some(hi)) => parts.push(format!("At most {}.", number_text(hi))),
                (None, None) => {}
            },
            FieldKind::Text => parts.push(format!(
                "Up to {} characters.",
                s.max_length.unwrap_or(DEFAULT_TEXT_SETTING_LEN)
            )),
            FieldKind::MultiChoice => match (s.min, s.max) {
                (Some(lo), Some(hi)) => {
                    parts.push(format!("Choose {} to {}.", number_text(lo), number_text(hi)))
                }
                (Some(lo), None) => parts.push(format!("Choose at least {}.", number_text(lo))),
                (None, Some(hi)) => parts.push(format!("Choose at most {}.", number_text(hi))),
                (None, None) => {}
            },
            _ => {}
        }
        parts.join(" ")
    }

    /// Whether the setting must be answered, for marking it on the form.
    pub fn required(&self, i: usize) -> bool {
        self.template
            .settings
            .get(i)
            .is_some_and(|s| s.required && s.kind != FieldKind::YesNo)
    }
}

impl Input {
    /// An empty input for the setting, filled from the page's stored settings when editing, or from
    /// the setting's default when making a new page.
    fn for_setting(s: &Setting, stored: Option<&Settings>) -> Input {
        let value: Option<&Value> = match stored {
            Some(st) => st.get(&s.key),
            None => s.default.as_ref(),
        };
        let text_of = |v: Option<&Value>| match v {
            Some(Value::String(t)) => t.clone(),
            Some(Value::Number(n)) => n.to_string(),
            _ => String::new(),
        };
        match s.kind {
            FieldKind::Text => {
                let max = s.max_length.unwrap_or(DEFAULT_TEXT_SETTING_LEN);
                // Room for any characters, so a too-long answer is refused with a message instead
                // of the box silently stopping.
                let mut f = Field::new(max * 4 + 16);
                f.set(&text_of(value));
                Input::Text(f)
            }
            FieldKind::Number => {
                let mut f = Field::new(24);
                f.set(&text_of(value));
                Input::Number(f)
            }
            FieldKind::Choice => Input::Choice(value.and_then(Value::as_str).map(str::to_string)),
            FieldKind::MultiChoice => Input::Multi(
                value
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
                    .unwrap_or_default(),
            ),
            FieldKind::YesNo => Input::YesNo(value.and_then(Value::as_bool).unwrap_or(false)),
        }
    }
}

// ---- editing typed-in content ---------------------------------------------------------------------------

/// Something wrong with what was typed, as a title and a sentence of detail.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EditProblem {
    pub title: String,
    pub detail: String,
}

fn problem(title: impl Into<String>, detail: impl Into<String>) -> EditProblem {
    EditProblem {
        title: title.into(),
        detail: detail.into(),
    }
}

/// One bar being edited. A usage page's bars are its service's windows, with fixed names.
#[derive(Clone, Debug)]
pub struct MeterRow {
    /// The bar's id on the page; empty for a new one.
    pub id: String,
    pub window: Option<Window>,
    pub label: Field,
    pub percent: Field,
    pub resets_in: Field,
    /// The reset time stored on the hub and how it was written in the box, so an untouched box keeps
    /// the stored time exactly (rounded text can't be turned back, and a time that couldn't be shown
    /// must not be lost).
    kept_reset: Option<(String, Timestamp)>,
}

impl MeterRow {
    fn blank() -> Self {
        Self {
            id: String::new(),
            window: None,
            label: Field::new(MAX_LABEL_LEN * 4),
            percent: Field::new(6),
            resets_in: Field::new(24),
            kept_reset: None,
        }
    }

    fn is_blank(&self) -> bool {
        self.label.trimmed().is_empty()
            && self.percent.trimmed().is_empty()
            && self.resets_in.trimmed().is_empty()
    }

    fn fill(&mut self, percent: Option<f64>, resets_at: Option<Timestamp>, now: Option<Timestamp>) {
        if let Some(p) = percent {
            self.percent.set(&format_percent(p));
        }
        if let Some(at) = resets_at {
            if let Some(now) = now {
                let left = now.seconds_until(at);
                if left > 0 {
                    // Under a minute reads as "1 min": the box takes d, h and min, not seconds.
                    let shown = hub_core::freshness::short_duration(left.max(60));
                    self.resets_in.set(&shown);
                }
            }
            // Kept while it still means something: a reset already past is just dropped, so it can
            // be cleared. With no hub clock there is no telling, so it is kept.
            if now.is_none_or(|n| n.seconds_until(at) > 0) {
                self.kept_reset = Some((self.resets_in.trimmed().to_string(), at));
            }
        }
    }

    /// The name shown for the bar's reading, for messages.
    fn name(&self) -> String {
        match self.window {
            Some(w) => w.label().to_string(),
            None => self.label.trimmed().to_string(),
        }
    }

    /// The share and reset time typed in, or what is wrong with them.
    fn read(&self, now: Option<Timestamp>) -> Result<(Option<f64>, Option<Timestamp>), EditProblem> {
        let percent = parse_percent(self.percent.trimmed()).map_err(|()| {
            problem(
                format!("{} isn't a share from 0 to 100", self.name()),
                "Enter a number such as 42 or 42.5, or leave it empty if you don't know. An empty box shows as \
                 unknown, never as zero.",
            )
        })?;
        let resets_at = match parse_duration(self.resets_in.trimmed()) {
            Ok(_)
                if self
                    .kept_reset
                    .as_ref()
                    .is_some_and(|(t, _)| t == self.resets_in.trimmed()) =>
            {
                self.kept_reset.as_ref().map(|(_, at)| *at)
            }
            Ok(None) => None,
            Ok(Some(secs)) => now.map(|n| n.plus_seconds(secs)),
            Err(()) => return Err(problem(
                "Reset times look like 3 h 20 min",
                "Enter how long until the allowance resets, using d, h and min, such as 2 d 4 h or 45 min. \
                     Leave it empty if you don't know.",
            )),
        };
        Ok((percent, resets_at))
    }
}

/// One list line being edited.
#[derive(Clone, Debug)]
pub struct ListRow {
    pub text: Field,
    pub detail: Field,
}

impl ListRow {
    fn blank() -> Self {
        Self {
            text: Field::new(MAX_LIST_TEXT_LEN * 4),
            detail: Field::new(MAX_LIST_TEXT_LEN * 4),
        }
    }
}

/// The part of the editor that depends on the page's shape.
#[derive(Clone, Debug)]
pub enum Shape {
    /// A usage page's meter: one bar for each of its service's windows.
    UsageMeter {
        service: Service,
        rows: Vec<MeterRow>,
    },
    Meter {
        rows: Vec<MeterRow>,
    },
    BigNumber {
        value: Field,
        unit: Field,
        caption: Field,
    },
    Text {
        text: Field,
    },
    List {
        rows: Vec<ListRow>,
    },
}

/// An editor for the content of a typed-in page of one of the four shapes.
#[derive(Clone, Debug)]
pub struct ContentEdit {
    pub shape: Shape,
    pub note: Field,
    /// A big number's stored value and places, with the text it loaded as: kept unless that box is
    /// typed over, so editing the unit or caption never changes them.
    kept_number: Option<(String, f64, Option<u8>)>,
    /// The boxes' text as first filled; see [`ContentEdit::is_edited`].
    original: Vec<String>,
}

impl ContentEdit {
    /// An editor for the page, filled with what was typed in before. `None` for a custom kind, which
    /// only its own helper can fill. `now` is the hub's clock, for turning reset times into "in 2 h".
    pub fn for_page(page: &PageView, now: Option<Timestamp>) -> Option<ContentEdit> {
        let stored = page.content.as_ref().filter(|c| c.quality == DataQuality::Manual);
        let data = stored.and_then(|c| c.data.as_ref());
        let mut note = Field::new(MAX_NOTE_LEN);
        if let Some(n) = stored.and_then(|c| c.note.as_deref()) {
            note.set(n);
        }
        if classify_kind(&page.kind) != KindClass::Shape {
            return None;
        }
        let mut big: Option<BigNumber> = None;
        let shape = match page.kind.as_str() {
            KIND_METER => {
                let items = data
                    .and_then(|d| serde_json::from_value::<Meter>(d.clone()).ok())
                    .map(|m| m.items)
                    .unwrap_or_default();
                match usage::usage_service(page) {
                    Some(service) => {
                        let rows = service
                            .windows()
                            .iter()
                            .map(|&w| {
                                let mut row = MeterRow::blank();
                                row.window = Some(w);
                                row.id = w.id().into();
                                if let Some(it) = items.iter().find(|it| it.id == w.id()) {
                                    row.fill(it.percent, it.resets_at, now);
                                }
                                row
                            })
                            .collect();
                        Shape::UsageMeter { service, rows }
                    }
                    None => {
                        let mut rows: Vec<MeterRow> = items
                            .iter()
                            .map(|it| {
                                let mut row = MeterRow::blank();
                                row.id.clone_from(&it.id);
                                row.label.set(&it.label);
                                row.fill(it.percent, it.resets_at, now);
                                row
                            })
                            .collect();
                        if rows.is_empty() {
                            rows.push(MeterRow::blank());
                        }
                        Shape::Meter { rows }
                    }
                }
            }
            KIND_BIG_NUMBER => {
                let b = data.and_then(|d| serde_json::from_value::<BigNumber>(d.clone()).ok());
                // Long enough for any value as written (the smallest numbers run to over 300 digits).
                let (mut value, mut unit, mut caption) = (
                    Field::new(400),
                    Field::new(MAX_UNIT_LEN * 4),
                    Field::new(MAX_CAPTION_LEN * 4),
                );
                big = b.clone();
                if let Some(b) = b {
                    if let Some(v) = b.value {
                        value.set(&number_as_written(v, b.decimals));
                    }
                    unit.set(b.unit.as_deref().unwrap_or(""));
                    caption.set(b.caption.as_deref().unwrap_or(""));
                }
                Shape::BigNumber { value, unit, caption }
            }
            KIND_TEXT => {
                let mut text = Field::new(MAX_TEXT_LEN * 4);
                if let Some(t) = data.and_then(|d| serde_json::from_value::<TextBlock>(d.clone()).ok()) {
                    text.set(&t.text);
                }
                Shape::Text { text }
            }
            KIND_LIST => {
                let rows = data
                    .and_then(|d| serde_json::from_value::<List>(d.clone()).ok())
                    .map(|l| l.items)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|it| {
                        let mut row = ListRow::blank();
                        row.text.set(&it.text);
                        row.detail.set(it.detail.as_deref().unwrap_or(""));
                        row
                    })
                    .collect();
                Shape::List { rows }
            }
            _ => return None,
        };
        let mut edit = ContentEdit {
            shape,
            note,
            kept_number: None,
            original: Vec::new(),
        };
        if let (Shape::BigNumber { value, .. }, Some(b)) = (&edit.shape, &big) {
            edit.kept_number = b.value.map(|v| (value.trimmed().to_string(), v, b.decimals));
        }
        edit.original = edit.texts();
        Some(edit)
    }

    /// Every box's text, in a fixed order, for telling whether anything was typed.
    fn texts(&self) -> Vec<String> {
        let mut out = vec![self.note.text.clone()];
        match &self.shape {
            Shape::UsageMeter { rows, .. } | Shape::Meter { rows } => {
                for r in rows {
                    out.extend([
                        r.label.text.clone(),
                        r.percent.text.clone(),
                        r.resets_in.text.clone(),
                    ]);
                }
            }
            Shape::BigNumber { value, unit, caption } => {
                out.extend([value.text.clone(), unit.text.clone(), caption.text.clone()]);
            }
            Shape::Text { text } => out.push(text.text.clone()),
            Shape::List { rows } => {
                for r in rows {
                    out.extend([r.text.text.clone(), r.detail.text.clone()]);
                }
            }
        }
        out
    }

    /// Whether anything was changed since the editor was filled. If not, the page's content is left
    /// as it is, never rebuilt from the text shown (which may be rounded).
    pub fn is_edited(&self) -> bool {
        self.texts() != self.original
    }

    /// Whether nothing at all has been entered. A page with no content yet and a blank editor keeps
    /// having none, so a title change doesn't need figures.
    pub fn is_blank(&self) -> bool {
        if !self.note.trimmed().is_empty() {
            return false;
        }
        match &self.shape {
            Shape::UsageMeter { rows, .. } | Shape::Meter { rows } => rows.iter().all(MeterRow::is_blank),
            Shape::BigNumber { value, unit, caption } => {
                [value, unit, caption].iter().all(|f| f.trimmed().is_empty())
            }
            Shape::Text { text } => text.trimmed().is_empty(),
            Shape::List { rows } => rows
                .iter()
                .all(|r| r.text.trimmed().is_empty() && r.detail.trimmed().is_empty()),
        }
    }

    pub fn meter_rows_mut(&mut self) -> Option<&mut Vec<MeterRow>> {
        match &mut self.shape {
            Shape::UsageMeter { rows, .. } | Shape::Meter { rows } => Some(rows),
            _ => None,
        }
    }

    pub fn list_rows_mut(&mut self) -> Option<&mut Vec<ListRow>> {
        match &mut self.shape {
            Shape::List { rows } => Some(rows),
            _ => None,
        }
    }

    /// The value, unit and caption boxes of a big number.
    pub fn big_number_mut(&mut self) -> Option<(&mut Field, &mut Field, &mut Field)> {
        match &mut self.shape {
            Shape::BigNumber { value, unit, caption } => Some((value, unit, caption)),
            _ => None,
        }
    }

    pub fn text_mut(&mut self) -> Option<&mut Field> {
        match &mut self.shape {
            Shape::Text { text } => Some(text),
            _ => None,
        }
    }

    /// Whether a bar or line can be added.
    pub fn can_add_row(&self) -> bool {
        match &self.shape {
            Shape::Meter { rows } => rows.len() < MAX_METER_ITEMS,
            Shape::List { rows } => rows.len() < MAX_LIST_ITEMS,
            _ => false,
        }
    }

    pub fn add_row(&mut self) {
        if !self.can_add_row() {
            return;
        }
        match &mut self.shape {
            Shape::Meter { rows } => rows.push(MeterRow::blank()),
            Shape::List { rows } => rows.push(ListRow::blank()),
            _ => {}
        }
    }

    pub fn remove_row(&mut self, i: usize) {
        match &mut self.shape {
            Shape::Meter { rows } if i < rows.len() && rows.len() > 1 => {
                rows.remove(i);
            }
            Shape::List { rows } if i < rows.len() => {
                rows.remove(i);
            }
            _ => {}
        }
    }

    /// The content to send, checked, or what is wrong with it. `now` is the hub's clock.
    pub fn build(&self, now: Option<Timestamp>) -> Result<PageContentInput, EditProblem> {
        let note = match self.note.trimmed() {
            "" => None,
            n => clean_note(n),
        };
        let data = match &self.shape {
            Shape::UsageMeter { service, rows } => {
                let mut metrics = Vec::new();
                for row in rows {
                    let (used_percent, resets_at) = row.read(now)?;
                    metrics.push(ManualMetric {
                        window: row.window.expect("a usage bar is a window"),
                        used_percent,
                        resets_at,
                    });
                }
                return ManualFigures { metrics, note }
                    .to_content(*service)
                    .map_err(|e| problem("Those figures can't be saved", e));
            }
            Shape::Meter { rows } => meter_data(rows, now)?,
            Shape::BigNumber { value, unit, caption } => {
                big_number_data(value, unit, caption, self.kept_number.as_ref())?
            }
            Shape::Text { text } => {
                let t = checked_text(text.trimmed(), MAX_TEXT_LEN, "The text")?
                    .ok_or_else(|| problem("Type some text", "A text page needs something to say."))?;
                serde_json::to_value(TextBlock { text: t }).expect("serialises")
            }
            Shape::List { rows } => list_data(rows)?,
        };
        Ok(PageContentInput {
            data: Some(data),
            note,
        })
    }
}

/// Plain text that must fit: tidied, but never cut. `None` when nothing is left.
fn checked_text(s: &str, max: usize, what: &str) -> Result<Option<String>, EditProblem> {
    // Tidied with room for the whole text (NFC can at most triple it), so a text that is too long is
    // refused, never silently shortened.
    let room = s.chars().count().saturating_mul(3).saturating_add(1);
    match sanitise_text(s, room) {
        None => Ok(None),
        Some(t) if t.chars().count() > max => Err(problem(
            format!("{what} is too long"),
            format!("It can be at most {max} characters."),
        )),
        Some(t) => Ok(Some(t)),
    }
}

fn meter_data(rows: &[MeterRow], now: Option<Timestamp>) -> Result<Value, EditProblem> {
    let mut items: Vec<MeterItem> = Vec::new();
    for row in rows.iter().filter(|r| !r.is_blank()) {
        let label = checked_text(row.label.trimmed(), MAX_LABEL_LEN, "A bar's name")?.ok_or_else(|| {
            problem(
                "Give each bar a name",
                "A bar with a share or a reset time needs a name.",
            )
        })?;
        let (percent, resets_at) = row.read(now)?;
        let id = if row.id.is_empty() {
            fresh_id(&label, &items)
        } else {
            row.id.clone()
        };
        items.push(MeterItem::new(id, label, percent, resets_at));
    }
    if items.is_empty() {
        return Err(problem(
            "Add at least one bar",
            "A meter needs a bar with a name.",
        ));
    }
    Ok(serde_json::to_value(Meter { items }).expect("serialises"))
}

/// An id for a new bar from its name, not used by another bar.
fn fresh_id(label: &str, taken: &[MeterItem]) -> String {
    let mut slug = String::new();
    for c in label.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() {
            slug.push(c);
        } else if !slug.ends_with('-') && !slug.is_empty() {
            slug.push('-');
        }
    }
    let slug = slug.trim_matches('-');
    let base: String = if slug.is_empty() {
        "bar".into()
    } else {
        slug.chars().take(24).collect()
    };
    let base = base.trim_end_matches('-').to_string();
    let mut id = base.clone();
    let mut n = 2;
    while taken.iter().any(|it| it.id == id) {
        id = format!("{base}-{n}");
        n += 1;
    }
    id
}

fn big_number_data(
    value: &Field,
    unit: &Field,
    caption: &Field,
    kept: Option<&(String, f64, Option<u8>)>,
) -> Result<Value, EditProblem> {
    let typed = value.trimmed();
    let (number, decimals) = if typed.is_empty() {
        (None, None)
    } else if let Some((_, v, places)) = kept.filter(|(text, ..)| text == typed) {
        (Some(*v), *places)
    } else {
        if typed.contains(['e', 'E']) {
            return Err(problem(
                "Write the number out in full",
                "Enter it with digits only, such as 1500 or 0.25, without an e.",
            ));
        }
        let n = typed
            .parse::<f64>()
            .ok()
            .filter(|n| n.is_finite() && n.abs() <= MAX_BIG_NUMBER_ABS)
            .ok_or_else(|| {
                problem(
                    "The value isn't a number",
                    "Enter a number such as 21 or 21.5, or leave it empty if you don't know. An empty box shows as a \
                     dash, never as zero.",
                )
            })?;
        // Plain notation only (exponents are refused above), so what follows the point is the places.
        let places = typed.split_once('.').map_or(0, |(_, frac)| frac.len());
        (Some(n), Some(places.min(3) as u8))
    };
    let unit = checked_text(unit.trimmed(), MAX_UNIT_LEN, "The unit")?;
    let caption = checked_text(caption.trimmed(), MAX_CAPTION_LEN, "The caption")?;
    Ok(serde_json::to_value(BigNumber {
        value: number,
        decimals,
        unit,
        caption,
    })
    .expect("serialises"))
}

/// A stored value as it was written: all its digits, padded with zeros up to the stored places.
fn number_as_written(v: f64, decimals: Option<u8>) -> String {
    let mut s = format!("{v}");
    let have = s.split_once('.').map_or(0, |(_, f)| f.len());
    let want = usize::from(decimals.unwrap_or(0).min(3));
    if want > have {
        if have == 0 {
            s.push('.');
        }
        s.push_str(&"0".repeat(want - have));
    }
    s
}

fn list_data(rows: &[ListRow]) -> Result<Value, EditProblem> {
    let mut items = Vec::new();
    for row in rows {
        let text = checked_text(row.text.trimmed(), MAX_LIST_TEXT_LEN, "A list item")?;
        let detail = checked_text(row.detail.trimmed(), MAX_LIST_TEXT_LEN, "A list item's detail")?;
        match (text, detail) {
            (None, None) => {}
            (None, Some(_)) => {
                return Err(problem(
                    "Give each detail an item",
                    "A detail goes with an item's text.",
                ))
            }
            (Some(text), detail) => items.push(ListItem { text, detail }),
        }
    }
    Ok(serde_json::to_value(List { items }).expect("serialises"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use hub_core::api::PageView;
    use hub_core::{Source, StoredContent};
    use serde_json::json;

    fn ts(s: &str) -> Timestamp {
        Timestamp::parse(s).unwrap()
    }

    fn page(kind: &str, feed: Option<&str>, data: Option<Value>) -> PageView {
        PageView {
            id: "p".into(),
            title: "P".into(),
            kind: kind.into(),
            source: Source::Manual,
            feed: feed.map(str::to_string),
            stale_after_seconds: 86_400,
            content: data.map(|d| StoredContent {
                quality: DataQuality::Manual,
                updated_at: Some(ts("2026-11-10T19:00:00Z")),
                data: Some(d),
                note: Some("From the website".into()),
            }),
            template: None,
            settings: None,
        }
    }

    fn form(id: &str) -> TemplateForm {
        TemplateForm::new(hub_templates::built_in_template(id).unwrap(), None)
    }

    fn setting_index(f: &TemplateForm, key: &str) -> usize {
        f.template.settings.iter().position(|s| s.key == key).unwrap()
    }

    #[test]
    fn the_form_starts_with_the_defaults_and_makes_the_pages() {
        let f = form("usage-limits/1");
        let pages = f.check().unwrap();
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].title, "Claude Max");
        assert_eq!(pages[0].template.as_deref(), Some("usage-limits/1"));
        assert_eq!(pages[0].settings.as_ref().unwrap()["service"], "claude-max");
    }

    #[test]
    fn choices_offer_only_what_the_other_answers_allow() {
        let mut f = form("usage-limits/1");
        let (svc, src) = (setting_index(&f, "service"), setting_index(&f, "source"));
        let labels = |f: &TemplateForm| f.options(src).into_iter().map(|o| o.label).collect::<Vec<_>>();
        f.pick(src, "helper");
        assert!(labels(&f).contains(&"Helper".to_string()));
        // OpenCode has no helper: the option goes away, and the answer that used it is dropped.
        f.pick(svc, "opencode-go-plus");
        assert!(!labels(&f).contains(&"Helper".to_string()));
        let Input::Choice(chosen) = &f.inputs[src] else {
            panic!()
        };
        assert_eq!(chosen.as_deref(), Some("manual"), "falls back to the default");
        assert!(f.check().is_ok());
    }

    #[test]
    fn mistakes_come_back_in_plain_words_and_clear_on_the_next_change() {
        let mut f = form("short-text/1");
        let title = setting_index(&f, "title");
        f.text_mut(title).unwrap().set(&"x".repeat(60));
        let msg = f.check().unwrap_err();
        assert!(msg.contains("at most 48 characters"), "{msg}");
        f.error = Some(msg);
        f.set_yes_no(title, true); // not a yes/no setting: nothing to do, but it counts as a change
        assert!(f.error.is_none());
    }

    #[test]
    fn editing_fills_the_form_from_the_stored_settings() {
        let t = hub_templates::built_in_template("usage-limits/1").unwrap();
        let stored = t
            .settings_from(
                &json!({"service": "chatgpt-plus", "source": "demo", "title": "Work"})
                    .as_object()
                    .unwrap()
                    .clone(),
            )
            .unwrap();
        let f = TemplateForm::new(t, Some(&stored));
        let pages = f.check().unwrap();
        assert_eq!(
            (pages[0].title.as_str(), pages[0].feed.as_deref(), pages[0].source),
            ("Work", Some("chatgpt-plus"), Source::Demo)
        );
    }

    #[test]
    fn a_usage_meter_keeps_its_windows_checks_and_reset_times() {
        let data = json!({"items":[{"id":"five-hour","label":"5 hours","percent":42.0,"resets_at":"2026-11-10T22:00:00Z"}]});
        let p = page("meter", Some("claude-max"), Some(data));
        let now = Some(ts("2026-11-10T20:00:00Z"));
        let mut e = ContentEdit::for_page(&p, now).unwrap();
        let Shape::UsageMeter { rows, .. } = &mut e.shape else {
            panic!("usage")
        };
        assert_eq!(rows[0].percent.text, "42");
        assert_eq!(rows[0].resets_in.text, "2 h");
        assert_eq!(rows[1].percent.text, "", "weekly was never entered");
        rows[1].percent.set("abc");
        let err = e.build(now).unwrap_err();
        assert!(err.title.contains("0 to 100"), "{}", err.title);
        let Shape::UsageMeter { rows, .. } = &mut e.shape else {
            panic!()
        };
        rows[1].percent.set("");
        rows[1].resets_in.set("2 d 4 h");
        let built = e.build(now).unwrap();
        let items = built.data.unwrap()["items"].clone();
        assert_eq!(items[0]["percent"], 42.0);
        assert!(items[1]["percent"].is_null(), "empty is unknown, not zero");
        assert_eq!(items[1]["resets_at"], "2026-11-13T00:00:00Z");
        assert_eq!(built.note.as_deref(), Some("From the website"));
    }

    #[test]
    fn a_plain_meter_takes_named_bars_with_their_own_ids() {
        let p = page("meter", None, None);
        let mut e = ContentEdit::for_page(&p, None).unwrap();
        assert!(e.is_blank());
        assert_eq!(e.build(None).unwrap_err().title, "Add at least one bar");
        e.add_row();
        let Shape::Meter { rows } = &mut e.shape else {
            panic!()
        };
        rows[0].label.set("Water tank");
        rows[0].percent.set("80");
        rows[1].label.set("Water tank");
        rows[1].percent.set("101");
        assert!(e.build(None).unwrap_err().title.contains("0 to 100"));
        let Shape::Meter { rows } = &mut e.shape else {
            panic!()
        };
        rows[1].percent.set("");
        rows[1].resets_in.set("");
        let items = e.build(None).unwrap().data.unwrap()["items"].clone();
        assert_eq!(
            (items[0]["id"].as_str(), items[1]["id"].as_str()),
            (Some("water-tank"), Some("water-tank-2"))
        );
        assert!(items[1]["percent"].is_null());
        for _ in 0..10 {
            e.add_row();
        }
        let Shape::Meter { rows } = &e.shape else { panic!() };
        assert_eq!(rows.len(), MAX_METER_ITEMS);
    }

    #[test]
    fn a_big_number_keeps_its_decimals_and_unknown_stays_a_dash() {
        let p = page(
            "big_number",
            None,
            Some(json!({"value": 21.5, "decimals": 1, "unit": "°C"})),
        );
        let mut e = ContentEdit::for_page(&p, None).unwrap();
        let Shape::BigNumber { value, unit, .. } = &mut e.shape else {
            panic!()
        };
        assert_eq!((value.text.as_str(), unit.text.as_str()), ("21.5", "°C"));
        value.set("30.25");
        let d = e.build(None).unwrap().data.unwrap();
        assert_eq!(
            (d["value"].as_f64(), d["decimals"].as_u64()),
            (Some(30.25), Some(2))
        );
        let Shape::BigNumber { value, unit, .. } = &mut e.shape else {
            panic!()
        };
        value.set("");
        unit.set("far too long a unit");
        assert!(e.build(None).unwrap_err().title.contains("unit"));
        let Shape::BigNumber { value, unit, .. } = &mut e.shape else {
            panic!()
        };
        unit.set("");
        value.set("1e99");
        assert!(e.build(None).is_err());
        let Shape::BigNumber { value, .. } = &mut e.shape else {
            panic!()
        };
        value.set("");
        assert!(e.build(None).unwrap().data.unwrap()["value"].is_null());
    }

    #[test]
    fn a_stored_value_loads_as_written_and_places_round_trip() {
        let load = |d: Value| {
            let e = ContentEdit::for_page(&page("big_number", None, Some(d)), None).unwrap();
            let Shape::BigNumber { value, .. } = &e.shape else {
                panic!()
            };
            (value.text.clone(), e)
        };
        let (text, e) = load(json!({"value": 1234.56789, "decimals": 2}));
        assert_eq!(text, "1234.56789");
        assert!(!e.is_edited());
        let (text, e) = load(json!({"value": 30.0, "decimals": 1}));
        assert_eq!(text, "30.0");
        assert_eq!(e.build(None).unwrap().data.unwrap()["decimals"], 1);
        let (text, e) = load(json!({"value": 5.0, "decimals": 0}));
        assert_eq!(text, "5");
        assert_eq!(
            e.build(None).unwrap().data.unwrap()["decimals"],
            0,
            "Some(0) stays Some(0)"
        );
    }

    #[test]
    fn editing_the_unit_keeps_the_stored_value_and_places() {
        let p = page(
            "big_number",
            None,
            Some(json!({"value": 1234.56789, "decimals": 2, "unit": "m"})),
        );
        let mut e = ContentEdit::for_page(&p, None).unwrap();
        e.big_number_mut().unwrap().1.set("km");
        let d = e.build(None).unwrap().data.unwrap();
        assert_eq!(
            (d["value"].as_f64(), d["decimals"].as_u64()),
            (Some(1234.56789), Some(2))
        );
        assert_eq!(d["unit"], "km");
        e.big_number_mut().unwrap().0.set("5.5");
        assert_eq!(
            e.build(None).unwrap().data.unwrap()["decimals"],
            1,
            "typed over: read afresh"
        );
    }

    #[test]
    fn a_very_small_value_loads_whole_and_a_past_reset_can_be_cleared() {
        let tiny = 1.2345678901234567e-10_f64;
        let p = page("big_number", None, Some(json!({"value": tiny, "decimals": 3})));
        let e = ContentEdit::for_page(&p, None).unwrap();
        let Shape::BigNumber { value, .. } = &e.shape else {
            panic!()
        };
        assert_eq!(
            value.text.parse::<f64>().unwrap(),
            tiny,
            "nothing cut off: {}",
            value.text
        );

        let data = json!({"items":[{"id":"five-hour","label":"5 hours","percent":42.0,"resets_at":"2026-11-10T19:00:00Z"}]});
        let p = page("meter", Some("claude-max"), Some(data));
        let now = Some(ts("2026-11-10T20:00:00Z"));
        let mut e = ContentEdit::for_page(&p, now).unwrap();
        e.note.set("x");
        let items = e.build(now).unwrap().data.unwrap()["items"].clone();
        assert!(
            items[0]["resets_at"].is_null(),
            "a reset already past means nothing"
        );
    }

    #[test]
    fn an_exponent_is_refused_and_places_count_only_plain_digits() {
        let mut e = ContentEdit::for_page(&page("big_number", None, None), None).unwrap();
        e.big_number_mut().unwrap().0.set("1.5e3");
        let err = e.build(None).unwrap_err();
        assert!(err.title.contains("in full"), "{}", err.title);
        e.big_number_mut().unwrap().0.set("1500");
        assert_eq!(e.build(None).unwrap().data.unwrap()["decimals"], 0);
    }

    #[test]
    fn text_that_grows_when_tidied_is_refused_not_cut() {
        // U+0958 becomes two characters in NFC, so this is over the limit once tidied.
        let long: String = "\u{0958}".repeat(MAX_UNIT_LEN);
        let err = checked_text(&long, MAX_UNIT_LEN, "The unit").unwrap_err();
        assert!(err.title.contains("too long"), "{}", err.title);
        assert_eq!(
            checked_text("kW", MAX_UNIT_LEN, "The unit").unwrap().as_deref(),
            Some("kW")
        );
    }

    #[test]
    fn a_reset_that_cant_be_shown_is_kept_unless_typed_over() {
        let data = json!({"items":[{"id":"five-hour","label":"5 hours","percent":42.0,"resets_at":"2026-11-10T22:00:00Z"}]});
        let p = page("meter", Some("claude-max"), Some(data));
        // No hub clock: the box is empty but the stored time survives a change to something else.
        let mut e = ContentEdit::for_page(&p, None).unwrap();
        let Shape::UsageMeter { rows, .. } = &mut e.shape else {
            panic!()
        };
        assert_eq!(rows[0].resets_in.text, "");
        rows[0].percent.set("50");
        let items = e.build(None).unwrap().data.unwrap()["items"].clone();
        assert_eq!(items[0]["resets_at"], "2026-11-10T22:00:00Z");
        let Shape::UsageMeter { rows, .. } = &mut e.shape else {
            panic!()
        };
        rows[0].resets_in.set("1 h");
        let now = Some(ts("2026-11-10T20:00:00Z"));
        let items = e.build(now).unwrap().data.unwrap()["items"].clone();
        assert_eq!(items[0]["resets_at"], "2026-11-10T21:00:00Z");
    }

    #[test]
    fn text_and_list_are_tidied_but_never_cut() {
        let mut e = ContentEdit::for_page(&page("text", None, None), None).unwrap();
        assert_eq!(e.build(None).unwrap_err().title, "Type some text");
        let Shape::Text { text } = &mut e.shape else {
            panic!()
        };
        text.set(&"a".repeat(MAX_TEXT_LEN + 1));
        assert!(e.build(None).unwrap_err().title.contains("too long"));
        let Shape::Text { text } = &mut e.shape else {
            panic!()
        };
        text.set("  Back at\n 3  ");
        assert_eq!(e.build(None).unwrap().data.unwrap(), json!({"text": "Back at 3"}));

        let mut l = ContentEdit::for_page(
            &page(
                "list",
                None,
                Some(json!({"items":[{"text":"Bins","detail":"Tue"}]})),
            ),
            None,
        )
        .unwrap();
        l.add_row();
        let Shape::List { rows } = &mut l.shape else {
            panic!()
        };
        assert_eq!(rows[0].detail.text, "Tue");
        rows[1].text.set("Post");
        assert_eq!(
            l.build(None).unwrap().data.unwrap(),
            json!({"items":[{"text":"Bins","detail":"Tue"},{"text":"Post"}]})
        );
        l.remove_row(0);
        l.remove_row(0);
        assert_eq!(l.build(None).unwrap().data.unwrap(), json!({"items": []}));
    }

    #[test]
    fn a_custom_kind_has_no_editor() {
        assert!(ContentEdit::for_page(&page("nearby-flights/1", None, None), None).is_none());
    }

    #[test]
    fn what_the_editors_build_passes_the_hubs_own_checks() {
        // Built the way the hub checks it: strictly, as a stored page.
        let now = Some(ts("2026-11-10T20:00:00Z"));
        let mut metre = ContentEdit::for_page(&page("meter", None, None), now).unwrap();
        let Shape::Meter { rows } = &mut metre.shape else {
            panic!()
        };
        rows[0].label.set("Tank");
        rows[0].percent.set("12.5");
        rows[0].resets_in.set("3 h");
        let mut big = ContentEdit::for_page(&page("big_number", None, None), now).unwrap();
        let Shape::BigNumber { value, unit, caption } = &mut big.shape else {
            panic!()
        };
        value.set("7.25");
        unit.set("kW");
        caption.set("Right now");
        let mut text = ContentEdit::for_page(&page("text", None, None), now).unwrap();
        let Shape::Text { text: t } = &mut text.shape else {
            panic!()
        };
        t.set("Hello");
        let mut list = ContentEdit::for_page(&page("list", None, None), now).unwrap();
        list.add_row();
        let Shape::List { rows } = &mut list.shape else {
            panic!()
        };
        rows[0].text.set("One");
        for (kind, e) in [
            ("meter", metre),
            ("big_number", big),
            ("text", text),
            ("list", list),
        ] {
            let content = e.build(now).unwrap();
            let page = hub_protocol::Page {
                id: "p".into(),
                title: "P".into(),
                kind: kind.into(),
                data_quality: DataQuality::Manual,
                updated_at: now,
                stale_after_seconds: 3600,
                note: None,
                data: content.data,
            };
            assert!(page.check().is_ok(), "{kind}: {:?}", page.check());
        }
    }
}
