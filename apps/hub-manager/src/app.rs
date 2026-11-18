//! The window: rail, screens and footer, drawn from the model. Clicks and keys become model calls,
//! and any resulting operation runs on the worker (`run`).

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::accesskit::{Role, Toggled};
use gpui::{
    canvas, div, fill, font, point, prelude::*, px, size, App, Bounds, ClipboardItem, Context, CursorStyle,
    Div, ElementId, FocusHandle, Hsla, KeyDownEvent, MouseButton, MouseDownEvent, Pixels, ShapedLine,
    SharedString, Stateful, TextRun, Window,
};
use hub_core::Source;

use crate::field::Field;
use crate::forms::{Input, Shape, HELPER_PAGE_KINDS};
use crate::guidance::{Guidance, Next, Tone};
use crate::model::{self, Confirm, EnrolStage, Failure, Model, Op, Section, HUB_PAGE_LIMIT};
use crate::shown::{self, Shown};
use crate::theme::*;
use crate::ui::{self, c, Metrics};
use crate::worker::{self, Ctx};
use usage::Service;

mod firmware_view;

const MAC: bool = cfg!(target_os = "macos");

type HoverListener = Box<dyn Fn(&bool, &mut Window, &mut App)>;
const FIELD_TEXT: f32 = 16.0;

/// Which form a template's settings are on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FormSlot {
    Add,
    Edit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FieldId {
    Pairing,
    PcName,
    /// A text or number setting of a template's form.
    Form(FormSlot, usize),
    EditTitle,
    HelperPageTitle,
    HelperPageKind,
    HelperPageFeed,
    MeterLabel(usize),
    MeterPercent(usize),
    MeterResets(usize),
    BigValue,
    BigUnit,
    BigCaption,
    TextBody,
    ListText(usize),
    ListDetail(usize),
    EditNote,
    EnrolName,
    Ssid,
    Password,
    OtherPort,
    InstallName,
    InstallSsid,
    InstallPassword,
    InstallPort,
    InstallMaker,
    SignerLabel,
    PublisherLabel,
    /// Feed names typed in for a helper invite.
    HelperOther,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Primary,
    Secondary,
    Quiet,
}

pub struct Manager {
    pub model: Model,
    ctx: Arc<Ctx>,
    /// Focus handles by element, and whether this frame drew them.
    focus: HashMap<SharedString, (FocusHandle, bool)>,
    /// Takes focus when the focused element goes away, so keys still reach the window.
    root: FocusHandle,
    /// The control under the pointer. Hover colours are decided at render from this, because GPUI
    /// applies a hover style's text colour too late for text inside the element.
    hovered: Option<SharedString>,
    /// Where each field was last drawn, for placing the caret on a click.
    field_bounds: Rc<RefCell<HashMap<FieldId, Bounds<Pixels>>>>,
    smoke_test: bool,
}

impl Manager {
    pub fn new(
        model: Model,
        ctx: Arc<Ctx>,
        smoke_test: bool,
        start: Option<Op>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self {
            model,
            ctx,
            focus: HashMap::new(),
            root: cx.focus_handle(),
            hovered: None,
            field_bounds: Rc::default(),
            smoke_test,
        };
        this.run(start, cx);
        // Checks the hub now and then while the window is open. A timer, not an animation: nothing
        // is drawn unless an answer changes something.
        if !smoke_test {
            cx.spawn(async move |this, cx| loop {
                cx.background_executor().timer(Duration::from_secs(5)).await;
                let alive = this.update(cx, |this, cx| {
                    let op = this.model.refresh_due(Instant::now());
                    if op.is_some() {
                        this.run(op, cx);
                    }
                });
                if alive.is_err() {
                    break;
                }
            })
            .detach();
        }
        this
    }

    /// Hands an operation to the worker and applies its answer when it comes.
    fn run(&mut self, op: Option<Op>, cx: &mut Context<Self>) {
        cx.notify();
        let Some(op) = op else { return };
        let installing = matches!(op, Op::Install(_));
        let handle = worker::spawn(worker::execute(self.ctx.clone(), op));
        if installing {
            self.watch_install(cx);
        }
        cx.spawn(async move |this, cx| {
            let result = handle
                .await
                .unwrap_or_else(|e| Err(Failure::File(format!("the worker stopped: {e}"))));
            let _ = this.update(cx, |this, cx| {
                this.model.finish(result, Instant::now());
                // Setup details are ready: look for the display's USB port straight away.
                let op = this
                    .model
                    .first_scan()
                    .or_else(|| this.model.first_install_scan())
                    .or_else(|| this.model.save_guide_if_pending());
                this.run(op, cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn is_hot(&self, id: &SharedString) -> bool {
        self.hovered.as_ref() == Some(id)
    }

    fn track_hover(id: SharedString, cx: &mut Context<Self>) -> HoverListener {
        Box::new(cx.listener(move |this, over: &bool, _, cx| {
            if *over {
                this.hovered = Some(id.clone());
            } else if this.hovered.as_ref() == Some(&id) {
                this.hovered = None;
            }
            cx.notify();
        }))
    }

    fn fh(&mut self, key: impl Into<SharedString>, cx: &mut Context<Self>) -> FocusHandle {
        let e = self
            .focus
            .entry(key.into())
            .or_insert_with(|| (cx.focus_handle().tab_stop(true), false));
        e.1 = true;
        e.0.clone()
    }

    /// Drops handles this frame didn't draw. If one had focus, the window keeps it at the root.
    fn prune_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut lost = false;
        self.focus.retain(|_, (h, used)| {
            let keep = std::mem::replace(used, false);
            lost |= !keep && h.is_focused(window);
            keep
        });
        if lost || window.focused(cx).is_none() {
            window.focus(&self.root, cx);
        }
    }

    fn field_mut(&mut self, id: FieldId) -> Option<&mut Field> {
        let m = &mut self.model;
        Some(match id {
            FieldId::Pairing => &mut m.pairing,
            FieldId::PcName => &mut m.pc_name,
            FieldId::Form(FormSlot::Add, i) => m.add.form.as_mut()?.text_mut(i)?,
            FieldId::Form(FormSlot::Edit, i) => m.edit.as_mut()?.form.as_mut()?.text_mut(i)?,
            FieldId::EditTitle => &mut m.edit.as_mut()?.title,
            FieldId::HelperPageTitle => &mut m.add.helper_page.as_mut()?.title,
            FieldId::HelperPageKind => &mut m.add.helper_page.as_mut()?.custom_kind,
            FieldId::HelperPageFeed => &mut m.add.helper_page.as_mut()?.feed,
            FieldId::HelperOther => &mut m.helper.other,
            FieldId::MeterLabel(i) => {
                &mut m
                    .edit
                    .as_mut()?
                    .content
                    .as_mut()?
                    .meter_rows_mut()?
                    .get_mut(i)?
                    .label
            }
            FieldId::MeterPercent(i) => {
                &mut m
                    .edit
                    .as_mut()?
                    .content
                    .as_mut()?
                    .meter_rows_mut()?
                    .get_mut(i)?
                    .percent
            }
            FieldId::MeterResets(i) => {
                &mut m
                    .edit
                    .as_mut()?
                    .content
                    .as_mut()?
                    .meter_rows_mut()?
                    .get_mut(i)?
                    .resets_in
            }
            FieldId::BigValue => m.edit.as_mut()?.content.as_mut()?.big_number_mut()?.0,
            FieldId::BigUnit => m.edit.as_mut()?.content.as_mut()?.big_number_mut()?.1,
            FieldId::BigCaption => m.edit.as_mut()?.content.as_mut()?.big_number_mut()?.2,
            FieldId::TextBody => m.edit.as_mut()?.content.as_mut()?.text_mut()?,
            FieldId::ListText(i) => {
                &mut m
                    .edit
                    .as_mut()?
                    .content
                    .as_mut()?
                    .list_rows_mut()?
                    .get_mut(i)?
                    .text
            }
            FieldId::ListDetail(i) => {
                &mut m
                    .edit
                    .as_mut()?
                    .content
                    .as_mut()?
                    .list_rows_mut()?
                    .get_mut(i)?
                    .detail
            }
            FieldId::EditNote => &mut m.edit.as_mut()?.content.as_mut()?.note,
            FieldId::EnrolName => &mut m.enrol.name,
            FieldId::Ssid => &mut m.enrol.ssid,
            FieldId::Password => &mut m.enrol.password,
            FieldId::OtherPort => &mut m.enrol.other_port,
            FieldId::InstallName => &mut m.fw.install.name,
            FieldId::InstallSsid => &mut m.fw.install.ssid,
            FieldId::InstallPassword => &mut m.fw.install.password,
            FieldId::InstallPort => &mut m.fw.install.other_port,
            FieldId::InstallMaker => &mut m.fw.install.maker,
            FieldId::SignerLabel => &mut m.fw.label_edit.as_mut()?.field,
            FieldId::PublisherLabel => &mut m.fw.publisher_label,
        })
    }

    /// Enter in a field submits its form.
    fn submit(&mut self, id: FieldId, cx: &mut Context<Self>) {
        let now = Instant::now();
        let op = match id {
            FieldId::Pairing | FieldId::PcName => self.model.submit_pairing(),
            FieldId::Form(FormSlot::Add, _) => self.model.submit_add(),
            FieldId::HelperPageTitle | FieldId::HelperPageKind | FieldId::HelperPageFeed => {
                self.model.submit_helper_page()
            }
            FieldId::HelperOther => self.model.create_helper(),
            FieldId::Form(FormSlot::Edit, _)
            | FieldId::EditTitle
            | FieldId::MeterLabel(_)
            | FieldId::MeterPercent(_)
            | FieldId::MeterResets(_)
            | FieldId::BigValue
            | FieldId::BigUnit
            | FieldId::BigCaption
            | FieldId::TextBody
            | FieldId::ListText(_)
            | FieldId::ListDetail(_)
            | FieldId::EditNote => self.model.save_edit(now),
            FieldId::EnrolName | FieldId::Ssid | FieldId::Password => self.model.create_setup(),
            FieldId::OtherPort => {
                let port = self.model.enrol.other_port.text.clone();
                self.model.send_usb(&port)
            }
            FieldId::InstallName
            | FieldId::InstallSsid
            | FieldId::InstallPassword
            | FieldId::InstallMaker
            | FieldId::InstallPort => {
                let port = self.model.fw.install.other_port.text.clone();
                self.model.ask_install(&port)
            }
            FieldId::SignerLabel => self.model.save_label(),
            FieldId::PublisherLabel => self.model.create_publisher(),
        };
        self.run(op, cx);
    }

    fn go(&mut self, s: Section, cx: &mut Context<Self>) {
        self.model.section = s;
        self.model.confirm = None;
        cx.notify();
    }

    fn key_down(&mut self, e: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let k = e.keystroke.key.as_str();
        let md = &e.keystroke.modifiers;
        let command = if MAC { md.platform } else { md.control };
        if command && !md.shift {
            if let Some(i) = ["1", "2", "3", "4", "5"].iter().position(|n| *n == k) {
                if self.model.connected() {
                    self.go(Section::ALL[i], cx);
                }
                return;
            }
            if k == "r" {
                let op = self.model.refresh();
                return self.run(op, cx);
            }
        }
        match k {
            "tab" if md.shift => window.focus_prev(cx),
            "tab" => window.focus_next(cx),
            "f5" => {
                let op = self.model.refresh();
                self.run(op, cx);
            }
            "escape" => {
                self.model.escape();
                cx.notify();
            }
            _ => {}
        }
    }

    // ---- interactive parts ------------------------------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    fn button(
        &mut self,
        id: impl Into<SharedString>,
        text: &str,
        kind: Kind,
        enabled: bool,
        cx: &mut Context<Self>,
        on: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> Stateful<Div> {
        let id: SharedString = id.into();
        let fh = self.fh(id.clone(), cx);
        let hot = enabled && self.is_hot(&id);
        let (fg, bg, border) = match (kind, enabled, hot) {
            (_, false, _) => (MUTED, BLACK, LINE),
            (Kind::Primary, _, false) => (BLACK, BEAK, BEAK),
            (Kind::Primary, _, true) => (BLACK, TEXT, TEXT),
            (Kind::Secondary, _, false) => (BEAK, BLACK, BEAK),
            (Kind::Secondary, _, true) => (BLACK, BEAK, BEAK),
            (Kind::Quiet, _, false) => (MUTED, BLACK, MUTED),
            (Kind::Quiet, _, true) => (TEXT, BLACK, TEXT),
        };
        let face = div()
            .flex()
            .items_center()
            .justify_center()
            .min_h(px(TARGET_MIN - 4.0))
            .px(px(18.0))
            .bg(c(bg))
            .border_1()
            .border_color(c(border))
            .text_color(c(fg))
            .font_family(FONT_MONO)
            .text_size(px(BUTTON_SIZE))
            .child(ui::spaced(&ui::caps(text), BUTTON_SIZE, BUTTON_TRACKING_EM));
        div()
            .id(ElementId::Name(id.clone()))
            .on_hover(Self::track_hover(id, cx))
            .role(Role::Button)
            .aria_label(SharedString::from(ui::plain(text)))
            .when(!enabled, |d| {
                d.a11y_synthetic_children(|b| b.parent_node().set_disabled())
            })
            // Disabled controls aren't tab stops.
            .when(enabled, |d| d.track_focus(&fh))
            .flex_none()
            // The 2 px focus frame sits just outside the button, like `outline-offset`.
            .p(px(2.0))
            .border_2()
            .border_color(c(BLACK))
            .focus_visible(|s| s.border_color(c(BEAK)))
            .when(enabled, |d| d.cursor_pointer())
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(move |this, _, window, cx| {
                if enabled {
                    on(this, window, cx);
                    cx.notify();
                }
            }))
            .child(face)
    }

    /// A choice among a few options, as a row of toggle buttons (a radio group).
    fn choice<T: Copy + PartialEq + 'static>(
        &mut self,
        id: &str,
        group_label: &str,
        options: &[(T, &str)],
        current: T,
        cx: &mut Context<Self>,
        set: impl Fn(&mut Self, T) + Copy + 'static,
    ) -> impl IntoElement {
        let mut row = div().flex().flex_wrap().gap(px(8.0));
        for (i, (value, text)) in options.iter().enumerate() {
            let key: SharedString = format!("{id}-{i}").into();
            let fh = self.fh(key.clone(), cx);
            let on = *value == current;
            let hot = self.is_hot(&key);
            let value = *value;
            row = row.child(
                div()
                    .id(ElementId::Name(key.clone()))
                    .on_hover(Self::track_hover(key, cx))
                    .role(Role::RadioButton)
                    .aria_label(SharedString::from(text.to_string()))
                    .aria_toggled(if on { Toggled::True } else { Toggled::False })
                    .track_focus(&fh)
                    .p(px(2.0))
                    .border_2()
                    .border_color(c(BLACK))
                    .focus_visible(|s| s.border_color(c(BEAK)))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        set(this, value);
                        cx.notify();
                    }))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .min_h(px(TARGET_MIN - 4.0))
                            .px(px(14.0))
                            .border_1()
                            .border_color(c(if on {
                                BEAK
                            } else if hot {
                                TEXT
                            } else {
                                MUTED
                            }))
                            .text_color(c(if on {
                                BEAK
                            } else if hot {
                                TEXT
                            } else {
                                MUTED
                            }))
                            .font_family(FONT_MONO)
                            .text_size(px(BUTTON_SIZE))
                            .child(ui::spaced(&text.to_uppercase(), BUTTON_SIZE, BUTTON_TRACKING_EM)),
                    ),
            );
        }
        div()
            .id(ElementId::Name(format!("{id}-group").into()))
            .role(Role::RadioGroup)
            .aria_label(SharedString::from(group_label.to_string()))
            .flex()
            .flex_col()
            .gap(px(8.0))
            .child(ui::label(group_label))
            .child(row)
    }

    /// A labelled single-line text field.
    fn field(
        &mut self,
        id: FieldId,
        label: &str,
        hint: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Div {
        let key: SharedString = format!("field-{id:?}").into();
        let fh = self.fh(key.clone(), cx);
        let focused = fh.is_focused(window);
        let Some(f) = self.field_mut(id).cloned() else {
            return div();
        };
        let shown = f.shown();
        let bounds_store = self.field_bounds.clone();
        let hit_store = self.field_bounds.clone();
        let fh_click = fh.clone();
        let input = div()
            .id(ElementId::Name(key))
            .role(Role::TextInput)
            .aria_label(SharedString::from(label.to_string()))
            .when(!f.secret, |d| d.aria_value(SharedString::from(f.text.clone())))
            .when(!hint.is_empty(), |d| {
                d.aria_description(SharedString::from(hint.to_string()))
            })
            .track_focus(&fh)
            .w_full()
            .p(px(1.0))
            .border_1()
            .border_color(c(if focused { BEAK } else { BLACK }))
            .cursor(CursorStyle::IBeam)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, e: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    window.focus(&fh_click, cx);
                    let origin = hit_store.borrow().get(&id).map(|b| f32::from(b.origin.x));
                    if let Some(f) = this.field_mut(id) {
                        if e.click_count >= 3 {
                            f.select_all();
                        } else {
                            let x = origin.map_or(0.0, |o| f32::from(e.position.x) - o);
                            let shown = f.shown();
                            let at = f.text_offset(caret_index(&shown, x, window));
                            if e.click_count == 2 {
                                f.select_word(at);
                            } else {
                                f.caret = at;
                                if !e.modifiers.shift {
                                    f.anchor = at;
                                }
                            }
                        }
                    }
                    cx.notify();
                }),
            )
            .on_key_down(cx.listener(move |this, e: &KeyDownEvent, _, cx| {
                let k = e.keystroke.key.as_str();
                if matches!(k, "tab" | "escape" | "f5") {
                    return;
                }
                if k == "enter" {
                    cx.stop_propagation();
                    return this.submit(id, cx);
                }
                let md = e.keystroke.modifiers;
                let command = if MAC { md.platform } else { md.control };
                let word = if MAC { md.alt } else { md.control };
                let typed = e.keystroke.key_char.clone();
                let mut pasted = None;
                let edit = {
                    let Some(f) = this.field_mut(id) else { return };
                    let mut clip = |w: Option<String>| match w {
                        Some(t) => {
                            cx.write_to_clipboard(ClipboardItem::new_string(t));
                            None
                        }
                        None => {
                            let t = cx.read_from_clipboard().and_then(|c| c.text());
                            pasted.clone_from(&t);
                            t
                        }
                    };
                    f.key(k, md.shift, word, command, typed.as_deref(), &mut clip)
                };
                // A pairing string is secret: once it's in the field, clear it from the clipboard.
                if id == FieldId::Pairing && pasted.is_some_and(|t| t.trim().starts_with("dh1.")) {
                    cx.write_to_clipboard(ClipboardItem::new_string(String::new()));
                }
                if edit.changed {
                    this.model.typing();
                }
                if edit.handled {
                    cx.stop_propagation();
                }
                cx.notify();
            }))
            .child(
                div()
                    .h(px(TARGET_MIN - 4.0))
                    .px(px(12.0))
                    .flex()
                    .items_center()
                    .border_1()
                    .border_color(c(if focused { BEAK } else { MUTED }))
                    .bg(c(BLACK))
                    .child(field_canvas(f, shown, focused, id, bounds_store)),
            );
        div()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .w_full()
            .child(ui::label(label))
            .child(input)
            .when(!hint.is_empty(), |d| {
                d.child(ui::p_muted(hint.to_string()).text_size(px(14.0)))
            })
    }

    // ---- frame --------------------------------------------------------------------------------------

    fn rail(&mut self, m: Metrics, cx: &mut Context<Self>) -> impl IntoElement {
        let connected = self.model.connected();
        let fh = self.fh("brand", cx);
        let brand_hot = self.is_hot(&"brand".into());
        let brand = div()
            .id("brand")
            .on_hover(Self::track_hover("brand".into(), cx))
            .role(Role::Link)
            .aria_label("Display Hub, overview")
            .track_focus(&fh)
            .flex()
            .items_center()
            .min_h(px(TARGET_MIN))
            .px(px(m.gutter))
            .bg(c(if brand_hot { BEAK } else { TEXT }))
            .text_color(c(BLACK))
            .font_family(FONT_BODY)
            .font_weight(gpui::FontWeight::SEMIBOLD)
            .text_size(px(16.0))
            .focus_visible(|s| s.bg(c(BEAK)))
            .cursor_pointer()
            .on_click(cx.listener(|this, _, _, cx| {
                if this.model.connected() {
                    this.go(Section::Overview, cx)
                }
            }))
            .child(ui::spaced("DISPLAY HUB", 16.0, 0.02));
        let mut nav = div()
            .id("nav")
            .role(Role::TabList)
            .aria_label("Sections")
            .flex()
            .flex_wrap();
        if connected {
            for (i, s) in Section::ALL.into_iter().enumerate() {
                let key: SharedString = format!("nav-{}", s.label()).into();
                let fh = self.fh(key.clone(), cx);
                let on = self.model.section == s && self.model.confirm.is_none();
                let hot = !on && self.is_hot(&key);
                nav = nav.child(
                    div()
                        .id(ElementId::Name(key.clone()))
                        .on_hover(Self::track_hover(key, cx))
                        .role(Role::Tab)
                        .aria_label(SharedString::from(s.label()))
                        .aria_selected(on)
                        .aria_keyshortcuts(SharedString::from(format!(
                            "{}+{}",
                            if MAC { "Meta" } else { "Control" },
                            i + 1
                        )))
                        .track_focus(&fh)
                        .flex()
                        .items_center()
                        .min_h(px(TARGET_MIN))
                        .px(px((m.width * 0.014).clamp(10.0, 20.0)))
                        .border_r_1()
                        .border_color(c(LINE))
                        .font_family(FONT_MONO)
                        .text_size(px(12.0))
                        .cursor_pointer()
                        .when(on, |d| d.text_color(c(BEAK)).border_2().border_color(c(BEAK)))
                        .when(!on && !hot, |d| d.text_color(c(MUTED)))
                        .when(hot, |d| d.bg(c(TEXT)).text_color(c(BLACK)))
                        .focus_visible(|s| s.border_2().border_color(c(BEAK)))
                        .on_click(cx.listener(move |this, _, _, cx| this.go(s, cx)))
                        .child(ui::spaced(&s.label().to_uppercase(), 12.0, 0.08)),
                );
            }
        }
        // What is true right now: connection and any work in flight. Never decoration.
        let state = match (&self.model.busy, self.model.online, connected) {
            (Some(b), _, _) => b.label().to_string(),
            (None, _, false) => "Not paired".into(),
            (None, Some(true), true) => "Online".into(),
            (None, Some(false), true) => "Can't reach hub".into(),
            (None, None, true) => "Connecting".into(),
        };
        let meta = div()
            .id("rail-meta")
            .role(Role::Status)
            .aria_label(SharedString::from(format!("{state}. {}", self.model.address())))
            .ml_auto()
            .flex()
            .items_center()
            .gap(px(16.0))
            .px(px(m.gutter))
            .min_h(px(TARGET_MIN))
            .child(ui::label_in(
                &state,
                if self.model.online == Some(false) {
                    TEXT
                } else {
                    MUTED
                },
            ))
            .when(connected && m.width > 900.0, |d| {
                d.child(ui::label(&self.model.address()))
            });
        div()
            .flex()
            .flex_wrap()
            .items_stretch()
            .bg(c(BLACK))
            .border_b_1()
            .border_color(c(LINE))
            .child(brand)
            .child(nav)
            .child(meta)
    }

    fn footer(&self, m: Metrics) -> impl IntoElement {
        div()
            .flex()
            .flex_wrap()
            .items_center()
            .justify_between()
            .gap(px(12.0))
            .px(px(m.gutter))
            .py(px(14.0))
            .bg(c(BLACK))
            .border_t_1()
            .border_color(c(LINE))
            .child(
                div()
                    .id("nz")
                    .role(Role::Label)
                    .aria_label("Aotearoa New Zealand")
                    .child(ui::nz_mark()),
            )
            .child(ui::label(&format!("Display Hub {}", env!("CARGO_PKG_VERSION"))))
    }

    /// The app head: mark, title, one sentence and the page's actions.
    fn head(&mut self, title: &str, lead: &str, m: Metrics, cx: &mut Context<Self>) -> Div {
        let can_refresh = self.model.connected() && self.model.busy.is_none();
        let refresh = self.button(
            "refresh",
            "Refresh",
            Kind::Secondary,
            can_refresh,
            cx,
            |this, _, cx| {
                let op = this.model.refresh();
                this.run(op, cx);
            },
        );
        ui::with_ticks(
            div()
                .relative()
                .flex()
                .flex_wrap()
                .items_center()
                .gap(px(20.0))
                .p(px(m.pad))
                .bg(c(BLACK))
                .border_1()
                .border_color(c(LINE))
                .child(ui::mark("mark", (m.h1 * 1.05).clamp(40.0, 80.0), BEAK))
                .child(
                    div()
                        .id(ElementId::Name(format!("h1-{title}").into()))
                        .role(Role::Heading)
                        .aria_level(1)
                        .aria_label(SharedString::from(title.to_string()))
                        .child(ui::h1(title.to_string(), m)),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(260.0))
                        .max_w(px(640.0))
                        .child(ui::p_muted(lead.to_string())),
                )
                .when(self.model.connected(), |d| {
                    d.child(div().ml_auto().child(refresh))
                }),
        )
    }

    fn notice(&mut self, n: &Guidance, m: Metrics, cx: &mut Context<Self>) -> impl IntoElement {
        let (tag, colour) = match n.tone {
            Tone::Done => ("Done", MUTED),
            Tone::Problem => ("Problem", TEXT),
            Tone::Danger => ("Stop", BEAK),
        };
        let retry = (n.next == Next::Retry && self.model.connected()).then(|| {
            self.button(
                "notice-retry",
                "Try again",
                Kind::Secondary,
                self.model.busy.is_none(),
                cx,
                |this, _, cx| {
                    let op = this.model.refresh();
                    this.run(op, cx);
                },
            )
        });
        let dismiss = self.button(
            "notice-dismiss",
            "Dismiss",
            Kind::Quiet,
            true,
            cx,
            |this, _, _| {
                this.model.notice = None;
            },
        );
        let text = format!("{}. {}", n.title, n.detail);
        div()
            .id("notice")
            .role(if n.tone == Tone::Done {
                Role::Status
            } else {
                Role::Alert
            })
            .aria_label(SharedString::from(text))
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(16.0))
            .px(px(m.gutter))
            .py(px(12.0))
            .bg(c(BLACK))
            .border_t(px(if n.tone == Tone::Danger { 2.0 } else { 1.0 }))
            .border_color(c(if n.tone == Tone::Danger { BEAK } else { LINE }))
            .child(ui::tag(tag, colour))
            .child(
                div()
                    .flex_1()
                    .min_w(px(240.0))
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .child(ui::h3(n.title.clone()))
                    .when(!n.detail.is_empty(), |d| {
                        d.child(ui::p_muted(n.detail.clone()).text_size(px(15.0)))
                    }),
            )
            .child(div().flex().gap(px(8.0)).children(retry).child(dismiss))
    }

    // ---- screens --------------------------------------------------------------------------------------

    fn connect(&mut self, m: Metrics, window: &mut Window, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        let busy = self.model.busy.is_some();
        let pairing = self.field(FieldId::Pairing, "Pairing string", "", window, cx);
        let name = self.field(FieldId::PcName, "This PC's name on the hub", "", window, cx);
        let paste = self.button(
            "paste",
            "Paste",
            Kind::Secondary,
            !busy,
            cx,
            |this, window, cx| {
                if let Some(t) = cx.read_from_clipboard().and_then(|c| c.text()) {
                    this.model.pairing.set(t.trim());
                    if t.trim().starts_with("dh1.") {
                        cx.write_to_clipboard(ClipboardItem::new_string(String::new()));
                    }
                }
                let fh = this.fh("pair", cx);
                window.focus(&fh, cx);
            },
        );
        let pair = self.button("pair", "Pair", Kind::Primary, !busy, cx, |this, _, cx| {
            let op = this.model.submit_pairing();
            this.run(op, cx);
        });
        let first = self.model.first_run();
        let lead = if self.model.needs_pairing {
            "This PC's credential no longer works. Pair again with a new string from the hub."
        } else if first {
            "Three short steps get your hub going. On the Pi there's just one command to run."
        } else {
            "Connect this PC to the hub on your Raspberry Pi. You need its pairing string, which you make on the Pi."
        };
        let form_title = if first {
            "3. Connect this app to your hub"
        } else {
            "Paste the string"
        };
        let form = ui::with_ticks(
            ui::panel(Some((form_title, "Works once")), m).child(
                ui::body(m)
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(6.0))
                            .child(div().flex().items_end().gap(px(8.0)).child(div().flex_1().min_w_0().child(pairing)).child(paste))
                            .child(ui::p_muted(
                                "It starts with dh1. and works once, for 10 minutes. The clipboard is cleared once \
                                 it's pasted.",
                            ).text_size(px(14.0)))
                            .when(first, |d| {
                                d.child(ui::p(
                                    "The pairing string is a one-time code that lets this app manage your hub. The \
                                     installer on the Pi printed it at the end. Copy that whole line, starting with \
                                     dh1., and paste it here.",
                                ))
                            }),
                    )
                    .child(name)
                    .child(div().flex().child(pair)),
            ),
        );
        let steps = [
            (
                "1",
                "On the Pi, over SSH, run:",
                Some("sudo -u display-hub hubd pair-admin"),
            ),
            ("2", "Copy the whole line it prints, starting with dh1.", None),
            ("3", "Paste it here and choose Pair.", None),
        ];
        if first {
            let mut out = vec![self.head("Set up Display Hub", lead, m, cx).into_any_element()];
            let guide = self.first_run_steps(m, cx);
            out.extend(guide.into_iter().map(|d| d.into_any_element()));
            out.push(div().w_full().max_w(px(860.0)).child(form).into_any_element());
            let again = ui::with_ticks(
                ui::panel(Some(("If the string has gone", "On the Pi")), m).child(
                    ui::body(m)
                        .child(ui::p(
                            "A string only lasts 10 minutes and works once. If yours has run out or the screen \
                             has cleared, make a new one by running this on the Pi:",
                        ))
                        .child(
                            ui::data(model::NEW_PAIRING_COMMAND)
                                .px(px(10.0))
                                .py(px(6.0))
                                .border_1()
                                .border_color(c(LINE)),
                        )
                        .child(ui::p_muted(
                            "The string carries the hub's key, so this app only ever talks to that hub. Anything \
                             else answering at that address is refused before anything is sent.",
                        )),
                ),
            );
            out.push(div().w_full().max_w(px(860.0)).child(again).into_any_element());
            return out;
        }
        let help = ui::with_ticks(
            ui::panel(Some(("How to get one", "On the Pi")), m).child(
                ui::body(m)
                    .children(steps.iter().map(|(n, t, code)| {
                        div()
                            .flex()
                            .gap(px(16.0))
                            .child(ui::data(n.to_string()).text_color(c(BEAK)).pt(px(3.0)))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .flex()
                                    .flex_col()
                                    .gap(px(6.0))
                                    .child(ui::p(t.to_string()))
                                    .when_some(*code, |d, code| {
                                        d.child(ui::data(code).px(px(10.0)).py(px(6.0)).border_1().border_color(c(LINE)))
                                    }),
                            )
                    }))
                    .child(ui::p_muted(
                        "The string carries the hub's key, so this app only ever talks to that hub. Anything else \
                         answering at that address is refused before anything is sent.",
                    )),
            ),
        );
        let row = if m.wide {
            div()
                .flex()
                .gap(px(m.gap))
                .child(div().flex_1().min_w_0().child(form))
                .child(div().w(px(460.0)).flex_none().child(help))
        } else {
            div().flex().flex_col().gap(px(m.gap)).child(form).child(help)
        };
        vec![
            self.head("Pair with your hub", lead, m, cx).into_any_element(),
            row.into_any_element(),
        ]
    }

    /// A command in its own box, with a Copy button under it and any `extra` buttons beside that.
    fn command_box(
        &mut self,
        id: &str,
        command: &str,
        copied: bool,
        extra: Option<gpui::AnyElement>,
        cx: &mut Context<Self>,
        on_copy: impl Fn(&mut Self) + 'static,
    ) -> Div {
        let text = command.to_string();
        let copy = self.button(
            id.to_string(),
            if copied { "Copied" } else { "Copy command" },
            Kind::Primary,
            true,
            cx,
            move |this, _, cx| {
                on_copy(this);
                cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
            },
        );
        div()
            .flex()
            .flex_col()
            .gap(px(10.0))
            .child(
                div()
                    .id(ElementId::Name(format!("{id}-text").into()))
                    .role(Role::Group)
                    .aria_label(SharedString::from(format!("Command to run: {command}")))
                    .p(px(12.0))
                    .border_1()
                    .border_color(c(MUTED))
                    .bg(c(BLACK))
                    .child(ui::command(command)),
            )
            .child(div().flex().flex_wrap().gap(px(8.0)).child(copy).children(extra))
    }

    /// Steps 1 and 2 of the first-run guide. Step 3, the pairing form, follows them.
    fn first_run_steps(&mut self, m: Metrics, cx: &mut Context<Self>) -> Vec<Div> {
        let need = |text: &'static str| {
            div()
                .flex()
                .gap(px(12.0))
                .child(ui::data("-").text_color(c(BEAK)))
                .child(div().flex_1().min_w_0().child(ui::p(text)))
        };
        let needs = ui::with_ticks(
            ui::panel(Some(("1. What you need", "")), m).child(
                ui::body(m)
                    .child(ui::p(
                        "Display Hub has a small hub that runs all day, and this app that manages it. For that you need:",
                    ))
                    .child(need(
                        "A computer that stays on all the time. A Raspberry Pi is the best choice, and it's what \
                         this guide uses.",
                    ))
                    .child(need("A Display Hub screen. You'll add it a little later, from this app."))
                    .child(need("This PC, on the same home network as the Pi.")),
            ),
        );
        let copied = self.model.install_copied;
        let command = self.command_box("install-copy", model::INSTALL_COMMAND, copied, None, cx, |this| {
            this.model.copy_install();
        });
        let install = ui::with_ticks(
            ui::panel(Some(("2. Set up the hub on the Pi", "")), m).child(
                ui::body(m)
                    .child(ui::p(
                        "The hub keeps your pages and sends them to your displays. First copy the display-hub-pi \
                         folder onto the Pi (the full guide says how). Then open a terminal on the Pi and run this \
                         one command:",
                    ))
                    .child(command)
                    .child(ui::p(
                        "It takes about a minute, and ends by printing a pairing string for the next step.",
                    ))
                    .child(ui::p_muted(
                        "Every step in detail, including getting the files: docs/deploy-raspberry-pi.md.",
                    )),
            ),
        );
        vec![
            div().w_full().max_w(px(860.0)).child(needs),
            div().w_full().max_w(px(860.0)).child(install),
        ]
    }

    /// What's left to do after pairing, ticked off from what the hub really has.
    fn guide_panel(&mut self, m: Metrics, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let items = self.model.guide_items()?;
        let next = self.model.guide_next();
        let total = items.len();
        let done = items.iter().filter(|i| i.done).count();
        let all_done = done == total;
        let note = if all_done {
            "All done".to_string()
        } else {
            format!("{done} of {total} done")
        };
        let mut rows = Vec::new();
        for (i, item) in items.iter().enumerate() {
            let go = item.go;
            let button = (!item.done && Some(i) == next).then(|| {
                self.button(
                    format!("guide-go-{i}"),
                    "Go there",
                    Kind::Secondary,
                    true,
                    cx,
                    move |this, _, cx| this.go(go, cx),
                )
            });
            rows.push(
                div()
                    .id(ElementId::Name(format!("guide-item-{i}").into()))
                    .role(Role::ListItem)
                    .aria_label(SharedString::from(format!(
                        "{}: {}",
                        item.title,
                        if item.done { "done" } else { "to do" }
                    )))
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap(px(12.0))
                    .py(px(8.0))
                    .border_t_1()
                    .border_color(c(LINE))
                    .child(div().w(px(64.0)).flex_none().child(if item.done {
                        ui::tag("Done", BEAK)
                    } else {
                        ui::tag("To do", MUTED)
                    }))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(220.0))
                            .flex()
                            .flex_col()
                            .gap(px(4.0))
                            .child(ui::h3(item.title))
                            .child(ui::p_muted(item.help)),
                    )
                    .children(button),
            );
        }
        let close = self.button(
            "guide-close",
            if all_done { "Finish" } else { "Hide this checklist" },
            if all_done { Kind::Primary } else { Kind::Quiet },
            true,
            cx,
            |this, _, cx| {
                let op = this.model.dismiss_guide();
                this.run(op, cx);
            },
        );
        Some(
            ui::with_ticks(
                ui::panel(Some(("Finish setting up", &note)), m).child(
                    ui::body(m)
                        .child(ui::p(if all_done {
                            "Everything is set up. Your displays will show your pages on their next check."
                        } else {
                            "Your hub is connected. Three things are left, and this list ticks itself off as you go."
                        }))
                        .child(div().flex().flex_col().children(rows))
                        .child(div().flex().child(close))
                        .child(ui::p_muted("You can bring this list back from Settings.")),
                ),
            )
            .into_any_element(),
        )
    }

    fn overview(&mut self, m: Metrics, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        let head = self
            .head(
                "Overview",
                "What your displays show now, and how the hub is doing.",
                m,
                cx,
            )
            .into_any_element();
        let Some(r) = self.model.last.clone() else {
            return vec![head, waiting(m).into_any_element()];
        };
        let s = &r.status;
        let clock = match s.clock_synchronised {
            Some(true) => "Synchronised",
            Some(false) => "Not synchronised",
            None => "Unknown",
        };
        let stats = div()
            .flex()
            .flex_wrap()
            .gap(px(m.gap))
            .child(stat_panel(s.pages.to_string(), "Pages", m))
            .child(stat_panel(s.displays.to_string(), "Displays", m))
            .child(stat_panel(
                format!("{} min", r.preview.refresh_after_seconds / 60),
                "Displays check every",
                m,
            ))
            .child(stat_panel(s.version.clone(), "Hub version", m));
        let mut out = vec![head];
        out.extend(self.guide_panel(m, cx));
        out.push(stats.into_any_element());
        if s.clock_synchronised == Some(false) {
            out.push(
                self.notice_static(
                    "The hub's clock isn't synchronised",
                    "Ages and reset times come from the Pi's clock. Check the Pi can reach a time server \
                     (timedatectl status), or the figures' ages may be wrong.",
                    m,
                )
                .into_any_element(),
            );
        }
        let cards: Vec<_> = r
            .preview
            .pages
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let (fresh, stale) = model::preview_freshness(p, &r.preview);
                ui::with_ticks(
                    ui::panel(None, m).flex_1().min_w(px(300.0)).child(
                        ui::body(m)
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .gap(px(8.0))
                                    .child(ui::label(&format!("{:02}", i + 1)))
                                    .child(
                                        div()
                                            .flex()
                                            .gap(px(6.0))
                                            .when(stale, |d| d.child(ui::tag("Stale", TEXT)))
                                            .child(ui::tag(model::quality_tag(p.data_quality), MUTED)),
                                    ),
                            )
                            .child(ui::h2(p.title.clone()))
                            .children(preview_body(p, r.preview.generated_at))
                            .when_some(self.model.preview_note(p), |d, n| d.child(ui::p_muted(n)))
                            .child(ui::label(&fresh)),
                    ),
                )
                .into_any_element()
            })
            .collect();
        let shows = ui::with_ticks(ui::panel(Some(("What displays show", "In order")), m).child(
            if cards.is_empty() {
                ui::body(m).child(ui::p_muted(
                    "No pages yet. Add one under Pages, and displays show it on their next check.",
                ))
            } else {
                div()
                    .flex()
                    .flex_wrap()
                    .gap(px(m.gap))
                    .p(px(m.gap))
                    .children(cards)
            },
        ));
        out.push(shows.into_any_element());
        let hub = ui::with_ticks(
            ui::panel(Some(("Hub", clock)), m).child(
                ui::body(m)
                    .child(kv("Address", self.model.address()))
                    .child(kv(
                        "Looks for displays at",
                        format!("{} (port {})", s.hosts.join(", "), s.port),
                    ))
                    .child(kv("Paired PCs", s.admins.to_string()))
                    .child(kv("Started", nz_date_time(s.started_at))),
            ),
        );
        out.push(hub.into_any_element());
        out
    }

    fn notice_static(&self, title: &str, detail: &str, m: Metrics) -> impl IntoElement {
        ui::with_ticks(
            div()
                .id(ElementId::Name(format!("warn-{title}").into()))
                .role(Role::Alert)
                .aria_label(SharedString::from(format!("{title}. {detail}")))
                .relative()
                .flex()
                .gap(px(16.0))
                .items_center()
                .p(px(m.pad))
                .bg(c(BLACK))
                .border_1()
                .border_color(c(LINE))
                .child(ui::tag("Check", TEXT))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap(px(4.0))
                        .child(ui::h3(title.to_string()))
                        .child(ui::p_muted(detail.to_string())),
                ),
        )
    }

    fn pages(&mut self, m: Metrics, window: &mut Window, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        let head = self
            .head(
                "Pages",
                "Every page on the hub, in the order displays show them. Changes reach displays on their next check.",
                m,
                cx,
            )
            .into_any_element();
        if self.model.last.is_none() {
            return vec![head, waiting(m).into_any_element()];
        }
        let now = Instant::now();
        let rows = self.model.page_rows(now);
        let n = rows.len();
        let busy = self.model.busy.is_some();
        let mut list = Vec::new();
        for (i, r) in rows.into_iter().enumerate() {
            let id = r.id.clone();
            let (a, b, e, d) = (id.clone(), id.clone(), id.clone(), id.clone());
            let up = self.icon_button(
                format!("up-{id}"),
                "up",
                &format!("Move {} up", r.title),
                !busy && i > 0,
                cx,
                move |this, cx| {
                    let op = this.model.move_page(&a, -1);
                    this.run(op, cx);
                },
            );
            let down = self.icon_button(
                format!("down-{id}"),
                "down",
                &format!("Move {} down", r.title),
                !busy && i + 1 < n,
                cx,
                move |this, cx| {
                    let op = this.model.move_page(&b, 1);
                    this.run(op, cx);
                },
            );
            let edit = self.button(
                format!("edit-{id}"),
                "Edit",
                Kind::Secondary,
                !busy,
                cx,
                move |this, _, cx| {
                    this.model.begin_edit(&e, Instant::now());
                    let fh = this.fh(format!("field-{:?}", FieldId::EditTitle), cx);
                    let _ = fh;
                },
            );
            let remove = self.button(
                format!("remove-{id}"),
                "Remove",
                Kind::Quiet,
                !busy,
                cx,
                move |this, window, cx| {
                    this.model.ask_remove_page(&d);
                    let fh = this.fh("confirm-no", cx);
                    window.focus(&fh, cx);
                },
            );
            let editing = self.model.edit.as_ref().is_some_and(|x| x.id == id);
            let summary = format!(
                "Page {} of {n}: {}, {}, {}. {}.{}",
                i + 1,
                r.title,
                r.quality,
                r.origin,
                r.freshness,
                r.on_displays.as_ref().map_or(String::new(), |t| format!(" {t}."))
            );
            let key_id = id.clone();
            list.push(
                div()
                    .id(ElementId::Name(format!("row-{id}").into()))
                    .role(Role::Group)
                    .aria_label(SharedString::from(summary))
                    .aria_description("Alt and the up or down arrow move this page")
                    .on_key_down(cx.listener(move |this, e: &KeyDownEvent, _, cx| {
                        let md = e.keystroke.modifiers;
                        let delta = match e.keystroke.key.as_str() {
                            "up" if md.alt => -1,
                            "down" if md.alt => 1,
                            _ => return,
                        };
                        cx.stop_propagation();
                        let op = this.model.move_page(&key_id, delta);
                        this.run(op, cx);
                    }))
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap(px(16.0))
                    .px(px(m.pad))
                    .py(px(14.0))
                    .border_b_1()
                    .border_color(c(LINE))
                    .when(editing, |d| d.border_l_2().border_color(c(BEAK)))
                    .child(ui::data(format!("{:02}", i + 1)).text_color(c(MUTED)).w(px(28.0)))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(220.0))
                            .flex()
                            .flex_col()
                            .gap(px(6.0))
                            .child(
                                div()
                                    .flex()
                                    .flex_wrap()
                                    .items_center()
                                    .gap(px(10.0))
                                    .child(ui::h3(r.title.clone()))
                                    .child(ui::tag(r.quality, MUTED))
                                    .when(r.stale, |d| d.child(ui::tag("Stale", TEXT))),
                            )
                            .child(ui::label(&format!("{} · {} · {}", r.kind, r.origin, r.freshness)))
                            .when_some(r.on_displays.clone(), |d, t| d.child(ui::label_in(&t, TEXT)))
                            .when_some(r.note.clone(), |d, note| {
                                d.child(ui::p_muted(note).text_size(px(14.0)))
                            })
                            .when(r.shown != Shown::Empty, |d| {
                                let wide = matches!(r.shown, Shown::Meter(_));
                                d.child(
                                    div()
                                        .flex()
                                        .when(wide, |d| d.flex_row().flex_wrap().gap(px(20.0)))
                                        .when(!wide, |d| d.flex_col().gap(px(4.0)))
                                        .pt(px(4.0))
                                        .children(shown_view(&r.shown).into_iter().map(|bar| {
                                            if wide {
                                                div().min_w(px(240.0)).flex_1().child(bar)
                                            } else {
                                                bar
                                            }
                                        })),
                                )
                            }),
                    )
                    .child(
                        div()
                            .flex()
                            .gap(px(4.0))
                            .child(up)
                            .child(down)
                            .child(edit)
                            .child(remove),
                    )
                    .into_any_element(),
            );
        }
        let count = format!("{n} of {HUB_PAGE_LIMIT}");
        let list_panel = ui::with_ticks(ui::panel(Some(("All pages", &count)), m).child(
            if list.is_empty() {
                ui::body(m).child(ui::p_muted("No pages yet. Add your first one from a template."))
            } else {
                div().flex().flex_col().children(list)
            },
        ));
        let side = if self.model.edit.is_some() {
            self.edit_panel(m, window, cx)
        } else {
            self.add_panel(m, window, cx)
        };
        let row = if m.wide {
            div()
                .flex()
                .items_start()
                .gap(px(m.gap))
                .child(div().flex_1().min_w_0().child(list_panel))
                .child(div().w(px(460.0)).flex_none().child(side))
        } else {
            div()
                .flex()
                .flex_col()
                .gap(px(m.gap))
                .child(list_panel)
                .child(side)
        };
        vec![head, row.into_any_element()]
    }

    fn icon_button(
        &mut self,
        id: String,
        icon: &str,
        name: &str,
        enabled: bool,
        cx: &mut Context<Self>,
        on: impl Fn(&mut Self, &mut Context<Self>) + 'static,
    ) -> Stateful<Div> {
        let key: SharedString = id.into();
        let fh = self.fh(key.clone(), cx);
        let hot = enabled && self.is_hot(&key);
        div()
            .id(ElementId::Name(key.clone()))
            .on_hover(Self::track_hover(key, cx))
            .role(Role::Button)
            .aria_label(SharedString::from(name.to_string()))
            .when(!enabled, |d| {
                d.a11y_synthetic_children(|b| b.parent_node().set_disabled())
            })
            // Disabled controls aren't tab stops.
            .when(enabled, |d| d.track_focus(&fh))
            .p(px(2.0))
            .border_2()
            .border_color(c(BLACK))
            .focus_visible(|s| s.border_color(c(BEAK)))
            .when(enabled, |d| d.cursor_pointer())
            .on_click(cx.listener(move |this, _, _, cx| {
                if enabled {
                    on(this, cx);
                }
            }))
            .child(
                div()
                    .size(px(TARGET_MIN - 4.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .border_1()
                    .border_color(c(if hot {
                        TEXT
                    } else if enabled {
                        MUTED
                    } else {
                        LINE
                    }))
                    .child(ui::mark(
                        icon,
                        20.0,
                        if hot {
                            TEXT
                        } else if enabled {
                            MUTED
                        } else {
                            LINE
                        },
                    )),
            )
    }

    /// Runs `f` on the template form in one of the two places a form can be.
    fn with_form(&mut self, slot: FormSlot, f: impl FnOnce(&mut crate::forms::TemplateForm)) {
        let form = match slot {
            FormSlot::Add => self.model.add.form.as_mut(),
            FormSlot::Edit => self.model.edit.as_mut().and_then(|e| e.form.as_mut()),
        };
        if let Some(form) = form {
            f(form);
        }
    }

    fn add_panel(&mut self, m: Metrics, window: &mut Window, cx: &mut Context<Self>) -> Div {
        if let Some(setup) = self.model.add.setup.clone() {
            return self.setup_panel(&setup, m, cx);
        }
        if let Some(steps) = self.model.add.feed.clone() {
            return self.feed_panel(&steps, m, cx);
        }
        if self.model.add.form.is_some() {
            return self.add_form_panel(m, window, cx);
        }
        if self.model.add.helper_page.is_some() {
            return self.helper_page_panel(m, window, cx);
        }
        self.template_picker(m, cx)
    }

    /// The templates to choose from.
    fn template_picker(&mut self, m: Metrics, cx: &mut Context<Self>) -> Div {
        let enabled = self.model.busy.is_none() && self.model.can_add_page();
        let templates: Vec<(String, String)> = self
            .model
            .add
            .templates
            .iter()
            .map(|t| (t.name.clone(), t.description.clone()))
            .collect();
        let mut body = ui::body(m).child(ui::p_muted(
            "Pick the kind of page to add. A short form follows, and you can change the answers later.",
        ));
        for (i, (name, description)) in templates.into_iter().enumerate() {
            let pick = self.button(
                format!("template-{i}"),
                &name,
                Kind::Secondary,
                enabled,
                cx,
                move |this, _, _| this.model.pick_template(i),
            );
            body = body.child(
                div()
                    .flex()
                    .flex_col()
                    .items_start()
                    .gap(px(6.0))
                    .pt(px(10.0))
                    .border_t_1()
                    .border_color(c(LINE))
                    .child(pick)
                    .child(ui::p_muted(description).text_size(px(14.0))),
            );
        }
        let own = self.button(
            "helper-page",
            "A page for your own helper",
            Kind::Secondary,
            enabled,
            cx,
            |this, _, _| this.model.open_helper_page(),
        );
        body = body.child(
            div()
                .flex()
                .flex_col()
                .items_start()
                .gap(px(6.0))
                .pt(px(10.0))
                .border_t_1()
                .border_color(c(LINE))
                .child(own)
                .child(
                    ui::p_muted(
                        "A page filled by a helper you wrote: a small program of yours that sends it with hub-send. \
                         You choose its kind and the feed it takes.",
                    )
                    .text_size(px(14.0)),
                ),
        );
        if !self.model.can_add_page() {
            body = body.child(ui::p_muted(format!(
                "The hub holds at most {HUB_PAGE_LIMIT} pages. Remove one to add another."
            )));
        }
        ui::with_ticks(ui::panel(Some(("Add from template", "")), m).child(body))
    }

    /// The form for a page the maker's own helper fills.
    fn helper_page_panel(&mut self, m: Metrics, window: &mut Window, cx: &mut Context<Self>) -> Div {
        let Some(form) = self.model.add.helper_page.clone() else {
            return div();
        };
        let busy = self.model.busy.is_some();
        let title = self.field(
            FieldId::HelperPageTitle,
            "Title",
            &format!("Up to {} characters.", hub_protocol::limits::MAX_TITLE_LEN),
            window,
            cx,
        );
        let names: Vec<(Option<&'static str>, String)> = HELPER_PAGE_KINDS
            .iter()
            .map(|k| {
                (
                    *k,
                    k.map_or_else(|| "Your own kind".to_string(), shown::kind_label),
                )
            })
            .collect();
        let options: Vec<(Option<&'static str>, &str)> =
            names.iter().map(|(k, t)| (*k, t.as_str())).collect();
        let kind = self.choice("helper-page-kind", "Kind", &options, form.shape, cx, |this, k| {
            if let Some(f) = this.model.add.helper_page.as_mut() {
                f.choose_kind(k);
            }
        });
        let custom = form.shape.is_none().then(|| {
            self.field(
                FieldId::HelperPageKind,
                "Your kind",
                "A name, a slash and a version, such as nearby-flights/1. Only a display whose firmware knows \
                 this kind can draw it.",
                window,
                cx,
            )
        });
        let feed = self.field(
            FieldId::HelperPageFeed,
            "Feed",
            "The name your helper sends to, such as bus-times: lower-case letters, digits and single hyphens.",
            window,
            cx,
        );
        let add = self.button(
            "helper-page-add",
            "Add page",
            Kind::Primary,
            !busy,
            cx,
            |this, _, cx| {
                let op = this.model.submit_helper_page();
                this.run(op, cx);
            },
        );
        let back = self.button(
            "helper-page-back",
            "Back",
            Kind::Quiet,
            !busy,
            cx,
            |this, _, _| this.model.leave_form(),
        );
        let mut body = ui::body(m)
            .child(ui::p_muted(
                "A page your own helper fills. Choose the kind of page it sends, and the feed it sends to. Meter, \
                 big number, short text and list are the shapes every display draws.",
            ))
            .child(title)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.0))
                    .child(kind)
                    .children(custom),
            )
            .child(feed);
        if let Some(e) = form.error.clone() {
            body = body.child(form_error(&e));
        }
        ui::with_ticks(
            ui::panel(Some(("A page for your own helper", "Esc to go back")), m)
                .child(body.child(div().flex().gap(px(8.0)).child(add).child(back))),
        )
    }

    /// How to feed a page just made for the maker's own helper: on the Pi, or on another computer.
    fn feed_panel(&mut self, steps: &model::FeedSteps, m: Metrics, cx: &mut Context<Self>) -> Div {
        let done = self
            .button("feed-done", "Done", Kind::Secondary, true, cx, |this, _, _| {
                this.model.finish_setup()
            })
            .into_any_element();
        let pi = self.command_box(
            "feed-pi-copy",
            &steps.pi_command,
            steps.pi_copied,
            None,
            cx,
            |this| this.model.copy_feed_command(true),
        );
        let feed = steps.feed.clone();
        let invite = self.button(
            "feed-invite",
            "Invite a helper",
            Kind::Secondary,
            true,
            cx,
            move |this, _, _| this.model.invite_for_feed(&feed),
        );
        let send = self.command_box(
            "feed-send-copy",
            &steps.send_command,
            steps.send_copied,
            None,
            cx,
            |this| this.model.copy_feed_command(false),
        );
        let kind = shown::kind_label(&steps.kind).to_lowercase();
        let example = match steps.example {
            Some(e) => format!("Data for a {kind} page looks like this: {e}"),
            None => "The data is in the shape your kind needs, which your display's firmware draws.".into(),
        };
        let mut pi_help = format!("Copy your helper program to the Pi, named {}.", steps.pi_name);
        if steps.pi_name != steps.feed {
            pi_help.push_str(
                " A helper's name on the Pi is 20 characters at most, so this one has a shorter name and the \
                 command names its feed with --feed.",
            );
        }
        pi_help.push_str(" Then, in the folder that holds it, run:");
        ui::with_ticks(
            ui::panel(Some(("Feed this page", "")), m).child(
                ui::body(m)
                    .child(ui::p(format!(
                        "The page shows what a helper sends to the feed {}, as {} pages. Set the helper up in one \
                         of these two places.",
                        steps.feed,
                        if steps.example.is_some() { kind.clone() } else { steps.kind.clone() }
                    )))
                    .child(ui::h3("On the hub's Raspberry Pi"))
                    .child(ui::p_muted(pi_help))
                    .child(pi)
                    .child(ui::p_muted(
                        "That pairs the helper, runs it once and then every 5 minutes, in a sandbox of its own. It \
                         needs no invite.",
                    ))
                    .child(ui::h3("On another computer"))
                    .child(ui::p_muted(format!(
                        "1. Invite a helper for {} in Settings. On that computer, run hub-send login and paste the \
                         helper string.",
                        steps.feed
                    )))
                    .child(div().flex().child(invite))
                    .child(ui::p_muted(
                        "2. Your helper sends each page with this command, giving the page's data as JSON on \
                         standard input:",
                    ))
                    .child(send)
                    .child(ui::p_muted(example).text_size(px(14.0)))
                    .child(div().flex().pt(px(8.0)).child(done)),
            ),
        )
    }

    /// The chosen template's form, and the buttons to add the pages or go back.
    fn add_form_panel(&mut self, m: Metrics, window: &mut Window, cx: &mut Context<Self>) -> Div {
        let Some(form) = self.model.add.form.clone() else {
            return div();
        };
        let fields = self.form_fields(FormSlot::Add, window, cx);
        let busy = self.model.busy.is_some();
        let add = self.button(
            "add-pages",
            "Add page",
            Kind::Primary,
            !busy,
            cx,
            |this, _, cx| {
                let op = this.model.submit_add();
                this.run(op, cx);
            },
        );
        let back = self.button("add-back", "Back", Kind::Quiet, !busy, cx, |this, _, _| {
            this.model.leave_form()
        });
        let mut body = ui::body(m)
            .child(ui::p_muted(form.template.description.clone()))
            .child(fields);
        if let Some(e) = form.error.clone() {
            body = body.child(form_error(&e));
        }
        ui::with_ticks(
            ui::panel(Some((&form.template.name, "Esc to go back")), m)
                .child(body.child(div().flex().gap(px(8.0)).child(add).child(back))),
        )
    }

    /// The fields of a template's form: a box for text and numbers, buttons for choices.
    fn form_fields(&mut self, slot: FormSlot, window: &mut Window, cx: &mut Context<Self>) -> Div {
        let form = match slot {
            FormSlot::Add => self.model.add.form.clone(),
            FormSlot::Edit => self.model.edit.as_ref().and_then(|e| e.form.clone()),
        };
        let mut out = div().flex().flex_col().gap(px(16.0));
        let Some(form) = form else { return out };
        for (i, setting) in form.template.settings.iter().enumerate() {
            let hint = form.hint(i);
            let optional = !form.required(i) && setting.default.is_none();
            let label = if optional && matches!(form.inputs[i], Input::Text(_) | Input::Number(_)) {
                format!("{} (optional)", setting.label)
            } else {
                setting.label.clone()
            };
            let id = format!("{slot:?}-s{i}");
            let element = match &form.inputs[i] {
                Input::Text(_) | Input::Number(_) => {
                    self.field(FieldId::Form(slot, i), &label, &hint, window, cx)
                }
                Input::Choice(_) => {
                    let options = form.options(i);
                    let labels: Vec<(usize, &str)> = options
                        .iter()
                        .enumerate()
                        .map(|(n, o)| (n, o.label.as_str()))
                        .collect();
                    let current = options.iter().position(|o| o.chosen).unwrap_or(usize::MAX);
                    let group = self.choice(&id, &label, &labels, current, cx, move |this, n| {
                        this.with_form(slot, |f| {
                            if let Some(o) = f.options(i).get(n) {
                                let value = o.value.clone();
                                f.pick(i, &value);
                            }
                        })
                    });
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(6.0))
                        .child(group)
                        .when(!hint.is_empty(), |d| {
                            d.child(ui::p_muted(hint.clone()).text_size(px(14.0)))
                        })
                }
                Input::Multi(_) => {
                    let mut row = div().flex().flex_wrap().gap(px(8.0));
                    for (n, o) in form.options(i).into_iter().enumerate() {
                        let toggle = self.toggle(&format!("{id}-{n}"), &o.label, o.chosen, cx, move |this| {
                            this.with_form(slot, |f| {
                                if let Some(o) = f.options(i).get(n) {
                                    let value = o.value.clone();
                                    f.toggle(i, &value);
                                }
                            })
                        });
                        row = row.child(toggle);
                    }
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(8.0))
                        .child(ui::label(&label))
                        .child(row)
                        .when(!hint.is_empty(), |d| {
                            d.child(ui::p_muted(hint.clone()).text_size(px(14.0)))
                        })
                }
                Input::YesNo(yes) => {
                    let group = self.choice(
                        &id,
                        &label,
                        &[(true, "Yes"), (false, "No")],
                        *yes,
                        cx,
                        move |this, v| this.with_form(slot, |f| f.set_yes_no(i, v)),
                    );
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(6.0))
                        .child(group)
                        .when(!hint.is_empty(), |d| {
                            d.child(ui::p_muted(hint.clone()).text_size(px(14.0)))
                        })
                }
            };
            out = out.child(element);
        }
        out
    }

    /// What to do once by hand after adding: the command, with a Copy button.
    fn setup_panel(&mut self, setup: &model::SetupNote, m: Metrics, cx: &mut Context<Self>) -> Div {
        let done = self
            .button("setup-done", "Done", Kind::Secondary, true, cx, |this, _, _| {
                this.model.finish_setup()
            })
            .into_any_element();
        let command = self.command_box(
            "setup-copy",
            &setup.command,
            setup.copied,
            Some(done),
            cx,
            |this| {
                this.model.copy_setup();
            },
        );
        ui::with_ticks(
            ui::panel(Some(("One more step", "")), m).child(
                ui::body(m)
                    .child(ui::p(setup.help.clone()))
                    .child(command)
                    .child(ui::p_muted(
                        "Open a terminal on the Pi (for example over SSH), paste the command and press Enter. The \
                         page stays empty until the helper has run.",
                    )),
            ),
        )
    }

    fn edit_panel(&mut self, m: Metrics, window: &mut Window, cx: &mut Context<Self>) -> Div {
        let Some(e) = self.model.edit.as_ref() else {
            return div();
        };
        let (has_title, has_form, source, typed_in, template_id, choices, kind, feed, can_edit) = (
            e.has_title_box(),
            e.form.is_some(),
            e.source,
            e.effective_source() == Source::Manual,
            e.template_id.clone(),
            e.source_choices(),
            e.kind.clone(),
            e.feed.clone(),
            e.content.is_some(),
        );
        let effective = e.effective_source();
        let mut body = ui::body(m).child(ui::label(&format!(
            "{} · {}",
            shown::kind_label(&kind),
            shown::origin_text(effective, feed.as_deref())
        )));
        if has_title {
            body = body.child(self.field(FieldId::EditTitle, "Title on the display", "", window, cx));
        }
        if has_form {
            let fields = self.form_fields(FormSlot::Edit, window, cx);
            body = body.child(fields);
            if let Some(err) = self
                .model
                .edit
                .as_ref()
                .and_then(|e| e.form.as_ref())
                .and_then(|f| f.error.clone())
            {
                body = body.child(form_error(&err));
            }
        } else {
            if let Some(t) = template_id {
                body = body.child(ui::p_muted(format!(
                    "This page was made from the template {t}, which this app doesn't have, so its settings can't \
                     be changed here."
                )));
            }
            let labels: Vec<(Source, &str)> = choices.iter().map(|s| (*s, model::source_label(*s))).collect();
            let pick = self.choice(
                "edit-src",
                "Content comes from",
                &labels,
                source,
                cx,
                |this, v| {
                    if let Some(e) = this.model.edit.as_mut() {
                        e.source = v;
                    }
                },
            );
            body = body.child(pick);
        }
        if typed_in && can_edit {
            body = body.child(self.content_editor(window, cx));
            if let Some(err) = self.model.edit.as_ref().and_then(|e| e.content_error.clone()) {
                body = body.child(form_error(&err));
            }
        } else {
            let why = match (effective, can_edit) {
                (Source::Helper, _) => Some("A helper fills this page, so there is nothing to type in here."),
                (Source::Demo, _) => Some("This page shows sample content, so there is nothing to type in here."),
                (Source::Manual, false) => Some(
                    "This page's own display design draws it, and only its helper can fill it, so there is nothing \
                     to type in here.",
                ),
                _ => None,
            };
            if let Some(why) = why {
                body = body.child(ui::p_muted(why));
            }
        }
        let busy = self.model.busy.is_some();
        let save = self.button("edit-save", "Save", Kind::Primary, !busy, cx, |this, _, cx| {
            let op = this.model.save_edit(Instant::now());
            this.run(op, cx);
        });
        let cancel = self.button("edit-cancel", "Cancel", Kind::Quiet, true, cx, |this, _, _| {
            this.model.edit = None
        });
        ui::with_ticks(
            ui::panel(Some(("Edit page", "Esc to cancel")), m)
                .child(body.child(div().flex().gap(px(8.0)).child(save).child(cancel))),
        )
    }

    /// The boxes for typing in a page's content, by its shape.
    fn content_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Div {
        let Some(shape) = self.model.edit.as_ref().and_then(|e| e.content.clone()) else {
            return div();
        };
        let mut out = div().flex().flex_col().gap(px(12.0));
        match &shape.shape {
            Shape::UsageMeter { rows, .. } => {
                for (i, row) in rows.iter().enumerate() {
                    let pct = self.field(FieldId::MeterPercent(i), "Used, %", "", window, cx);
                    let resets = self.field(FieldId::MeterResets(i), "Resets in", "", window, cx);
                    out = out.child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(8.0))
                            .pt(px(8.0))
                            .border_t_1()
                            .border_color(c(LINE))
                            .child(ui::h3(row.window.map_or("", |w| w.label())))
                            .child(
                                div()
                                    .flex()
                                    .gap(px(8.0))
                                    .child(div().w(px(120.0)).child(pct))
                                    .child(div().flex_1().min_w_0().child(resets)),
                            ),
                    );
                }
                out = out.child(ui::p_muted(
                    "Leave a share empty if you don't know it: displays show it as unknown, never as zero. Reset \
                     times look like 3 h 20 min.",
                ));
            }
            Shape::Meter { rows } => {
                let n = rows.len();
                for i in 0..n {
                    let name = self.field(FieldId::MeterLabel(i), "Bar name", "", window, cx);
                    let pct = self.field(FieldId::MeterPercent(i), "Used, %", "", window, cx);
                    let resets = self.field(FieldId::MeterResets(i), "Resets in (optional)", "", window, cx);
                    let remove = self.button(
                        format!("meter-remove-{i}"),
                        "Remove bar",
                        Kind::Quiet,
                        n > 1,
                        cx,
                        move |this, _, _| {
                            if let Some(c) = this.model.edit.as_mut().and_then(|e| e.content.as_mut()) {
                                c.remove_row(i);
                            }
                        },
                    );
                    out = out.child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(8.0))
                            .pt(px(8.0))
                            .border_t_1()
                            .border_color(c(LINE))
                            .child(name)
                            .child(
                                div()
                                    .flex()
                                    .gap(px(8.0))
                                    .child(div().w(px(120.0)).child(pct))
                                    .child(div().flex_1().min_w_0().child(resets)),
                            )
                            .child(div().flex().child(remove)),
                    );
                }
                let add = self.button(
                    "meter-add",
                    "Add a bar",
                    Kind::Secondary,
                    shape.can_add_row(),
                    cx,
                    |this, _, _| {
                        if let Some(c) = this.model.edit.as_mut().and_then(|e| e.content.as_mut()) {
                            c.add_row();
                        }
                    },
                );
                out = out.child(div().flex().child(add)).child(ui::p_muted(
                    "Leave a share empty if you don't know it: displays show it as unknown, never as zero.",
                ));
            }
            Shape::BigNumber { .. } => {
                out = out
                    .child(self.field(
                        FieldId::BigValue,
                        "Value",
                        "Leave empty if you don't know: displays show a dash.",
                        window,
                        cx,
                    ))
                    .child(self.field(
                        FieldId::BigUnit,
                        "Unit (optional)",
                        "Such as °C or kW. Up to 8 characters.",
                        window,
                        cx,
                    ))
                    .child(self.field(FieldId::BigCaption, "Caption (optional)", "", window, cx));
            }
            Shape::Text { .. } => {
                out = out.child(self.field(FieldId::TextBody, "Text", "Up to 280 characters.", window, cx));
            }
            Shape::List { rows } => {
                let n = rows.len();
                for i in 0..n {
                    let text = self.field(FieldId::ListText(i), "Item", "", window, cx);
                    let detail = self.field(FieldId::ListDetail(i), "Detail (optional)", "", window, cx);
                    let remove = self.button(
                        format!("list-remove-{i}"),
                        "Remove item",
                        Kind::Quiet,
                        true,
                        cx,
                        move |this, _, _| {
                            if let Some(c) = this.model.edit.as_mut().and_then(|e| e.content.as_mut()) {
                                c.remove_row(i);
                            }
                        },
                    );
                    out = out.child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(8.0))
                            .pt(px(8.0))
                            .border_t_1()
                            .border_color(c(LINE))
                            .child(text)
                            .child(detail)
                            .child(div().flex().child(remove)),
                    );
                }
                let add = self.button(
                    "list-add",
                    "Add an item",
                    Kind::Secondary,
                    shape.can_add_row(),
                    cx,
                    |this, _, _| {
                        if let Some(c) = this.model.edit.as_mut().and_then(|e| e.content.as_mut()) {
                            c.add_row();
                        }
                    },
                );
                out = out.child(div().flex().child(add)).child(ui::p_muted(
                    "Up to eight items, each up to 48 characters. A display shows them in this order.",
                ));
            }
        }
        out.child(self.field(
            FieldId::EditNote,
            "Note on the display (optional)",
            "",
            window,
            cx,
        ))
    }

    fn displays(&mut self, m: Metrics, window: &mut Window, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        let head = self
            .head(
                "Displays",
                "Every display set up with this hub. Removing one stops its credential working straight away.",
                m,
                cx,
            )
            .into_any_element();
        if self.model.last.is_none() {
            return vec![head, waiting(m).into_any_element()];
        }
        let now = Instant::now();
        let busy = self.model.busy.is_some();
        let displays = self.model.displays().to_vec();
        let mut rows = Vec::new();
        for d in &displays {
            let id = d.id.clone();
            let seen = self.model.seen_text(d, now);
            let what = d
                .device
                .as_ref()
                .map_or("Hasn't reported its software yet".to_string(), |x| {
                    // The display's own words, in their own case: versions and hardware names are
                    // case-sensitive.
                    format!(
                        "{} {} · {}",
                        ui::keep_case(&x.device_type),
                        ui::keep_case(&x.firmware_version),
                        ui::keep_case(&x.hardware_revision)
                    )
                });
            let remove = self.button(
                format!("revoke-{id}"),
                "Remove",
                Kind::Quiet,
                !busy,
                cx,
                move |this, window, cx| {
                    this.model.ask_revoke(&id);
                    let fh = this.fh("confirm-no", cx);
                    window.focus(&fh, cx);
                },
            );
            // A hub that doesn't say what a display shows can't have it changed from here: sending
            // ticks it never reported could untick everything.
            let ticks = self
                .tick_panel(&d.id, &d.name, cx)
                .unwrap_or_else(|| div().child(ui::p_muted("Update the hub to choose pages here.")));
            let firmware = self.display_firmware_block(&d.id, cx);
            rows.push(
                div()
                    .id(ElementId::Name(format!("display-{}", d.id).into()))
                    .role(Role::Group)
                    .aria_label(SharedString::from(format!(
                        "{}. {seen}. {}",
                        d.name,
                        ui::plain(&what)
                    )))
                    .flex()
                    .flex_col()
                    .gap(px(14.0))
                    .px(px(m.pad))
                    .py(px(14.0))
                    .border_b_1()
                    .border_color(c(LINE))
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .gap(px(16.0))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(220.0))
                                    .flex()
                                    .flex_col()
                                    .gap(px(6.0))
                                    .child(ui::h3(d.name.clone()))
                                    .child(ui::label(&format!("{seen} · {what}"))),
                            )
                            .child(remove),
                    )
                    .child(ticks)
                    .children(firmware)
                    .into_any_element(),
            );
        }
        let count = displays.len().to_string();
        let list = ui::with_ticks(
            ui::panel(Some(("Your displays", &count)), m).child(if rows.is_empty() {
                ui::body(m).child(ui::p_muted("No displays yet. Add one with its USB cable."))
            } else {
                div().flex().flex_col().children(rows)
            }),
        );
        let side = self.enrol_panel(m, window, cx);
        let row = if m.wide {
            div()
                .flex()
                .items_start()
                .gap(px(m.gap))
                .child(div().flex_1().min_w_0().child(list))
                .child(div().w(px(500.0)).flex_none().child(side))
        } else {
            div().flex().flex_col().gap(px(m.gap)).child(list).child(side)
        };
        vec![head, row.into_any_element()]
    }

    /// "Pages on this display": a tick box for every page, with Save and Cancel while there are
    /// unsaved changes. Nothing is sent until Save, so one tap never changes what a display shows.
    fn tick_panel(&mut self, display: &str, name: &str, cx: &mut Context<Self>) -> Option<Div> {
        let view = self.model.tick_view(display)?;
        let busy = self.model.busy.is_some();
        let mut boxes = div().flex().flex_col().gap(px(2.0));
        if view.rows.is_empty() {
            boxes = boxes.child(ui::p_muted("No pages yet. Add one under Pages."));
        }
        for r in &view.rows {
            let (d, p) = (display.to_string(), r.id.clone());
            let label = if r.waiting {
                format!("{} (not shown yet)", r.title)
            } else {
                r.title.clone()
            };
            // The kind tells same-named pages apart, but says nothing new when it is the title.
            let kind = if r.kind.eq_ignore_ascii_case(&r.title) {
                ""
            } else {
                r.kind.as_str()
            };
            boxes = boxes.child(self.tick_box(
                format!("tick-{display}-{}", r.id),
                &label,
                kind,
                &format!("{} on {name}", r.title),
                r.ticked,
                r.can_change,
                cx,
                move |this| this.model.toggle_tick(&d, &p),
            ));
        }
        let (a, b) = (display.to_string(), display.to_string());
        let save = self.button(
            format!("ticks-save-{display}"),
            "Save",
            Kind::Primary,
            view.dirty && view.can_save && !busy,
            cx,
            move |this, _, cx| {
                let op = this.model.save_ticks(&a);
                this.run(op, cx);
            },
        );
        let cancel = self.button(
            format!("ticks-cancel-{display}"),
            "Cancel",
            Kind::Quiet,
            view.dirty,
            cx,
            move |this, _, _| this.model.cancel_ticks(&b),
        );
        Some(
            div()
                .flex()
                .flex_col()
                .gap(px(8.0))
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .justify_between()
                        .gap(px(8.0))
                        .child(ui::label("Pages on this display"))
                        .child(
                            div()
                                .flex()
                                .gap(px(6.0))
                                .when(view.dirty, |d| d.child(ui::tag("Unsaved changes", BEAK)))
                                .child(ui::tag(&view.count_text(), MUTED)),
                        ),
                )
                .child(boxes)
                .when_some(view.note(), |d, n| d.child(ui::p_muted(n).text_size(px(14.0))))
                .when(view.dirty, |d| {
                    d.child(div().flex().flex_wrap().gap(px(8.0)).child(save).child(cancel))
                }),
        )
    }

    /// One tick box. A box that can't be changed isn't a tab stop, and says why in its description.
    #[allow(clippy::too_many_arguments)]
    fn tick_box(
        &mut self,
        id: String,
        text: &str,
        kind: &str,
        aria: &str,
        on: bool,
        enabled: bool,
        cx: &mut Context<Self>,
        set: impl Fn(&mut Self) + 'static,
    ) -> Stateful<Div> {
        let key: SharedString = id.into();
        let fh = self.fh(key.clone(), cx);
        let hot = enabled && self.is_hot(&key);
        let colour = if !enabled {
            MUTED
        } else if on || hot {
            TEXT
        } else {
            MUTED
        };
        // A box that can't be ticked is dimmed itself, not only its label.
        let edge = if on {
            BEAK
        } else if !enabled {
            FIELD_NODE
        } else {
            colour
        };
        div()
            .id(ElementId::Name(key.clone()))
            .on_hover(Self::track_hover(key, cx))
            .role(Role::CheckBox)
            .aria_label(SharedString::from(aria.to_string()))
            .aria_toggled(if on { Toggled::True } else { Toggled::False })
            .when(!enabled, |d| {
                d.aria_description("Ten pages are ticked already. Untick one to choose another.")
                    .a11y_synthetic_children(|b| b.parent_node().set_disabled())
            })
            .when(enabled, |d| d.track_focus(&fh))
            .p(px(2.0))
            .border_2()
            .border_color(c(BLACK))
            .focus_visible(|s| s.border_color(c(BEAK)))
            .when(enabled, |d| d.cursor_pointer())
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(move |this, _, _, cx| {
                if enabled {
                    set(this);
                    cx.notify();
                }
            }))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(12.0))
                    .min_h(px(TARGET_MIN - 4.0))
                    .px(px(10.0))
                    .child(
                        div()
                            .flex_none()
                            .size(px(18.0))
                            .border_1()
                            .border_color(c(edge))
                            .p(px(3.0))
                            .child(div().size_full().when(on, |d| d.bg(c(BEAK)))),
                    )
                    .child(
                        div()
                            .text_color(c(colour))
                            .flex()
                            .items_center()
                            .gap(px(8.0))
                            .child(ui::p(text.to_string()).text_color(c(colour)))
                            .child(
                                div()
                                    .text_size(px(13.0))
                                    .text_color(c(MUTED))
                                    .child(kind.to_string()),
                            ),
                    ),
            )
    }

    fn enrol_panel(&mut self, m: Metrics, window: &mut Window, cx: &mut Context<Self>) -> Div {
        let busy = self.model.busy.is_some();
        if !self.model.enrol.open {
            let start = self.button(
                "enrol-open",
                "Add a display",
                Kind::Primary,
                !busy,
                cx,
                |this, window, cx| {
                    this.model.open_enrol();
                    let fh = this.fh(format!("field-{:?}", FieldId::EnrolName), cx);
                    window.focus(&fh, cx);
                },
            );
            return ui::with_ticks(
                ui::panel(Some(("Add a display", "USB")), m).child(
                    ui::body(m)
                        .child(ui::p_muted(
                            "Plug the display into this PC with a USB cable. The app sends it the hub's details and \
                             a one-time secret, and optionally your Wi-Fi settings. Nothing else is stored on it.",
                        ))
                        .child(div().flex().child(start)),
                ),
            );
        }
        let cancel = self.button("enrol-cancel", "Cancel", Kind::Quiet, !busy, cx, |this, _, _| {
            this.model.close_enrol()
        });
        let body = match self.model.enrol.stage.clone() {
            EnrolStage::Form => {
                let name = self.field(FieldId::EnrolName, "Display name", "", window, cx);
                let ssid = self.field(FieldId::Ssid, "Wi-Fi network (2.4 GHz)", "Leave empty if the display already has Wi-Fi.", window, cx);
                let pw = self.field(FieldId::Password, "Wi-Fi password", "Empty for an open network.", window, cx);
                let create = self.button("enrol-create", "Create setup details", Kind::Primary, !busy, cx, |this, _, cx| {
                    let op = this.model.create_setup();
                    this.run(op, cx);
                });
                let keys = self.pending_keys_block(cx);
                ui::body(m)
                    .child(name)
                    .child(ssid)
                    .child(pw)
                    .children(keys)
                    .child(div().flex().gap(px(8.0)).child(create).child(cancel))
            }
            EnrolStage::Ready => {
                let left = self.model.setup_expires_in(Instant::now()).unwrap_or(0);
                let ports = self.model.enrol.ports.clone();
                let mut list = div().flex().flex_col().gap(px(8.0));
                if ports.is_empty() {
                    list = list.child(ui::p_muted("No serial ports found. Check the cable is a data cable, then look again."));
                }
                for (i, p) in ports.iter().enumerate() {
                    let name = p.name.clone();
                    let send = self.button(format!("send-{i}"), &format!("Send to {}", p.name), Kind::Secondary, !busy, cx, move |this, _, cx| {
                        let op = this.model.send_usb(&name);
                        this.run(op, cx);
                    });
                    list = list.child(div().flex().items_center().gap(px(12.0)).child(send).child(ui::label(&p.description)));
                }
                let rescan = self.button("ports-rescan", "Look again", Kind::Quiet, !busy, cx, |this, _, cx| {
                    let op = this.model.list_ports();
                    this.run(op, cx);
                });
                let other = self.field(FieldId::OtherPort, "Or type a port", "Such as COM5, /dev/ttyACM0 or /dev/cu.usbmodem1101. Enter sends.", window, cx);
                let save = self.button("bundle-save", "Save as a file", Kind::Quiet, !busy, cx, |this, _, cx| this.prompt_save(cx));
                let keys = self.pending_keys_block(cx);
                ui::body(m)
                    .child(ui::p(format!(
                        "Setup details are ready, valid for about {} more. Choose the display's port.",
                        hub_core::short_duration(left.max(0))
                    )))
                    .children(keys)
                    .child(list)
                    .child(other)
                    .child(div().flex().flex_wrap().gap(px(8.0)).child(rescan).child(save).child(cancel))
            }
            EnrolStage::Waiting { how } => ui::body(m)
                .child(ui::h3(how))
                .child(ui::p_muted(
                    "Waiting for the display to join Wi-Fi and check in with the hub. This list updates by itself.",
                ))
                .child(div().flex().child(cancel)),
            EnrolStage::Joined { name } => {
                let done = self.button("enrol-done", "Done", Kind::Secondary, true, cx, |this, _, _| this.model.close_enrol());
                ui::body(m)
                    .child(ui::h3(format!("{name} is set up")))
                    .child(ui::p_muted("It has checked in with the hub, and shows your pages on its next check."))
                    .child(div().flex().child(done))
            }
        };
        ui::with_ticks(ui::panel(Some(("Add a display", "USB")), m).child(body))
    }

    fn prompt_save(&mut self, cx: &mut Context<Self>) {
        let dir = dirs::download_dir().or_else(dirs::home_dir).unwrap_or_default();
        let answer = cx.prompt_for_new_path(&dir, Some("display-setup.dhbundle.json"));
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(path))) = answer.await {
                let _ = this.update(cx, |this, cx| {
                    let op = this.model.save_bundle(path);
                    this.run(op, cx);
                });
            }
        })
        .detach();
    }

    fn settings(&mut self, m: Metrics, window: &mut Window, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        let head = self
            .head("Settings", "This PC's connection to the hub.", m, cx)
            .into_any_element();
        let Some(hub) = self.model.hub.clone() else {
            return vec![head];
        };
        let fingerprint = hub.fingerprint();
        let copy_fp = self.button(
            "copy-fp",
            "Copy key fingerprint",
            Kind::Secondary,
            true,
            cx,
            move |this, _, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(fingerprint.clone()));
                this.model.notice = Some(Guidance::done(
                    "Copied the key fingerprint",
                    "Compare it with hubd fingerprint on the Pi.",
                ));
            },
        );
        let copy_diag = self.button(
            "copy-diag",
            "Copy diagnostics",
            Kind::Secondary,
            true,
            cx,
            |this, _, cx| {
                let d = this
                    .model
                    .diagnostics(env!("CARGO_PKG_VERSION"), std::env::consts::OS);
                cx.write_to_clipboard(ClipboardItem::new_string(d));
                this.model.notice = Some(Guidance::done(
                    "Copied diagnostics",
                    "They hold no credentials, secrets or Wi-Fi details.",
                ));
            },
        );
        let forget = self.button(
            "forget",
            "Forget this hub",
            Kind::Quiet,
            self.model.busy.is_none(),
            cx,
            |this, window, cx| {
                this.model.forget();
                let fh = this.fh("confirm-no", cx);
                window.focus(&fh, cx);
            },
        );
        let hub_panel = ui::with_ticks(
            ui::panel(Some(("Hub", "Paired")), m).child(
                ui::body(m)
                    .child(kv("Address", format!("{}:{}", hub.host, hub.port)))
                    .child(kv("Key fingerprint (SHA-256)", hub.fingerprint()))
                    .child(kv("This PC's name on the hub", hub.label.clone()))
                    .child(kv("Paired", nz_date(hub.paired_at)))
                    .child(ui::p_muted(
                        "This app accepts only this key from the hub. The credential is in the system's credential \
                         store, not in a file.",
                    ))
                    .child(div().flex().flex_wrap().gap(px(8.0)).child(copy_fp).child(copy_diag).child(forget)),
            ),
        );
        let again = self.button(
            "guide-again",
            "Show the setup guide again",
            Kind::Secondary,
            true,
            cx,
            |this, _, cx| {
                let op = this.model.show_guide_again();
                this.run(op, cx);
            },
        );
        let guide_panel = ui::with_ticks(
            ui::panel(Some(("Setup guide", "")), m).child(
                ui::body(m)
                    .child(ui::p_muted(
                        "The checklist on the Overview shows what's left to set up: a display, a page, and what the \
                         display shows.",
                    ))
                    .child(div().flex().child(again)),
            ),
        );
        let helper_panel = self.helper_panel(m, window, cx);
        let about = ui::with_ticks(
            ui::panel(Some(("About", "")), m).child(
                ui::body(m)
                    .child(kv("Version", env!("CARGO_PKG_VERSION").to_string()))
                    .child(ui::p_muted(
                        "Display Hub is MIT licensed. Geist and Geist Mono are under the SIL Open Font License. \
                         The app makes no requests except to your hub, and collects nothing.",
                    ))
                    .child(div().flex().flex_col().gap(px(4.0)).child(ui::label("Keys")).child(ui::p_muted(
                        "Tab moves between controls. Enter or Space activates. Ctrl+1 to 5 switch sections, Ctrl+R \
                         refreshes, Alt+Up and Alt+Down move a page, Esc cancels.",
                    ))),
            ),
        );
        let row = if m.wide {
            div()
                .flex()
                .items_start()
                .gap(px(m.gap))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap(px(m.gap))
                        .child(hub_panel)
                        .child(guide_panel)
                        .child(helper_panel),
                )
                .child(div().w(px(460.0)).flex_none().child(about))
        } else {
            div()
                .flex()
                .flex_col()
                .gap(px(m.gap))
                .child(hub_panel)
                .child(guide_panel)
                .child(helper_panel)
                .child(about)
        };
        vec![head, row.into_any_element()]
    }

    /// An on/off control, for choices that aren't exclusive.
    fn toggle(
        &mut self,
        id: &str,
        text: &str,
        on: bool,
        cx: &mut Context<Self>,
        set: impl Fn(&mut Self) + 'static,
    ) -> impl IntoElement {
        let key: SharedString = id.to_string().into();
        let fh = self.fh(key.clone(), cx);
        let hot = self.is_hot(&key);
        div()
            .id(ElementId::Name(key.clone()))
            .on_hover(Self::track_hover(key, cx))
            .role(Role::CheckBox)
            .aria_label(SharedString::from(text.to_string()))
            .aria_toggled(if on { Toggled::True } else { Toggled::False })
            .track_focus(&fh)
            .p(px(2.0))
            .border_2()
            .border_color(c(BLACK))
            .focus_visible(|s| s.border_color(c(BEAK)))
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| {
                set(this);
                cx.notify();
            }))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.0))
                    .min_h(px(TARGET_MIN - 4.0))
                    .px(px(14.0))
                    .border_1()
                    .border_color(c(if on {
                        BEAK
                    } else if hot {
                        TEXT
                    } else {
                        MUTED
                    }))
                    .text_color(c(if on {
                        BEAK
                    } else if hot {
                        TEXT
                    } else {
                        MUTED
                    }))
                    .font_family(FONT_MONO)
                    .text_size(px(BUTTON_SIZE))
                    .child(
                        div()
                            .size(px(10.0))
                            .when(on, |d| d.bg(c(BEAK)))
                            .when(!on, |d| d.border_1().border_color(c(MUTED))),
                    )
                    .child(ui::spaced(&text.to_uppercase(), BUTTON_SIZE, BUTTON_TRACKING_EM)),
            )
    }

    fn helper_panel(&mut self, m: Metrics, window: &mut Window, cx: &mut Context<Self>) -> Div {
        let busy = self.model.busy.is_some();
        let claude = self.toggle(
            "helper-claude",
            "Claude Max (Claude Code)",
            self.model.helper.claude,
            cx,
            |this| this.model.helper.claude = !this.model.helper.claude,
        );
        let chatgpt = self.toggle(
            "helper-chatgpt",
            "ChatGPT Plus (Codex)",
            self.model.helper.chatgpt,
            cx,
            |this| this.model.helper.chatgpt = !this.model.helper.chatgpt,
        );
        let create = self.button(
            "helper-create",
            "Create helper string",
            Kind::Secondary,
            !busy,
            cx,
            |this, _, cx| {
                let op = this.model.create_helper();
                this.run(op, cx);
            },
        );
        let mut own = div().flex().flex_wrap().gap(px(8.0));
        let choices = self.model.helper_feed_choices();
        for choice in &choices {
            let feed = choice.feed.clone();
            let label = if choice.pages.is_empty() {
                choice.feed.clone()
            } else {
                format!("{} (fills: {})", choice.feed, choice.pages.join(", "))
            };
            own = own.child(self.toggle(
                &format!("helper-feed-{}", choice.feed),
                &label,
                choice.ticked,
                cx,
                move |this| this.model.toggle_helper_feed(&feed),
            ));
        }
        let other = self.field(
            FieldId::HelperOther,
            "Other feeds (optional)",
            "Feeds no page has yet, separated by spaces, such as garden-weather.",
            window,
            cx,
        );
        let mut body = ui::body(m)
            .child(ui::p_muted(
                "A helper is a small program that sends figures to your pages, and you can do without it. Pages whose \
                 figures are set to Helper show what it sends, marked REPORTED. Choose the feeds it may send to, then \
                 make a helper string for it here. (Helpers on the hub's Pi are set up there, with \
                 display-hub-helper, and need no string.)",
            ))
            .child(ui::label("Usage limits, sent by usage-helper"))
            .child(ui::p_muted(
                "usage-helper runs on a computer where Claude Code or Codex is signed in, and passes on the plan limits \
                 they report there. It reads nothing else: no browser data, passwords or other apps' tokens.",
            ))
            .child(div().flex().flex_wrap().gap(px(8.0)).child(claude).child(chatgpt))
            .child(ui::label("Your own helper, sending with hub-send"))
            .when(choices.is_empty(), |d| {
                d.child(ui::p_muted(
                    "No page takes a feed of your own yet. Make one in Pages, with A page for your own helper, or type \
                     its feed below.",
                ))
            })
            .child(own)
            .child(other)
            .child(div().flex().child(create));
        if let Some(s) = self.model.helper.string.clone() {
            let shown: String = s
                .as_bytes()
                .chunks(24)
                .map(|c| String::from_utf8_lossy(c).into_owned())
                .collect::<Vec<_>>()
                .join(" ");
            let full = s.to_string();
            let copy = self.button(
                "helper-copy",
                "Copy helper string",
                Kind::Primary,
                true,
                cx,
                move |this, _, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(full.clone()));
                    this.model.notice = Some(Guidance::done(
                        "Copied the helper string",
                        "Paste it where usage-helper login or hub-send login asks. It works once, within 10 minutes.",
                    ));
                },
            );
            let (usage, own) = self.model.invite_programs();
            let own_line = own.then(|| {
                format!(
                    "For your own helper (feeds: {}), run hub-send login on that computer and paste it. Your \
                     helper then sends with hub-send page: docs/helpers.md has the steps.",
                    self.model.invited_own_feeds().join(", ")
                )
            });
            let mut how: Vec<&str> = Vec::new();
            if usage {
                how.push(
                    "For the usage limits, run usage-helper login on that computer and paste it. Then add usage-helper \
                     claude-statusline as Claude Code's status line, or schedule usage-helper-sync for Codex: \
                     docs/providers.md has the steps.",
                );
            }
            if let Some(line) = &own_line {
                how.push(line);
            }
            if usage && own {
                how.push("A string works once, for one program: to set up both, make a string for each.");
            }
            body = body.child(
                div()
                    .id("helper-string")
                    .role(Role::Group)
                    .aria_label("Helper string")
                    .flex()
                    .flex_col()
                    .gap(px(10.0))
                    .p(px(14.0))
                    .border_1()
                    .border_color(c(LINE))
                    .child(ui::label("Helper string: works once, within 10 minutes"))
                    .child(ui::data(shown))
                    .child(div().flex().child(copy))
                    .child(ui::p_muted(how.join(" "))),
            );
        }
        let helpers = self.model.helpers().to_vec();
        if !helpers.is_empty() {
            let mut list = div().flex().flex_col().gap(px(8.0)).child(ui::label("Set up"));
            for h in helpers {
                // A service's feed shows as the service; any other feed by its name.
                let services = h
                    .feeds
                    .iter()
                    .map(|f| Service::from_slug(f).map_or(f.as_str(), |s| s.default_title()))
                    .collect::<Vec<_>>()
                    .join(", ");
                let id = h.id.clone();
                let row = div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap(px(12.0))
                    .py(px(6.0))
                    .border_t_1()
                    .border_color(c(LINE))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(200.0))
                            .flex()
                            .flex_col()
                            .gap(px(4.0))
                            .child(ui::h3(h.label.clone()))
                            .child(ui::label(&format!(
                                "{services} · paired {}",
                                nz_date(h.created_at)
                            ))),
                    );
                list = list.child(if h.revoked {
                    row.child(ui::tag("Removed", MUTED))
                } else {
                    row.child(self.button(
                        format!("helper-revoke-{id}"),
                        "Remove",
                        Kind::Quiet,
                        !busy,
                        cx,
                        move |this, window, cx| {
                            this.model.ask_revoke_helper(&id);
                            let fh = this.fh("confirm-no", cx);
                            window.focus(&fh, cx);
                        },
                    ))
                });
            }
            body = body.child(list);
        }
        ui::with_ticks(ui::panel(Some(("Helper", "On this computer")), m).child(body))
    }

    fn confirm_view(&mut self, c: &Confirm, m: Metrics, cx: &mut Context<Self>) -> impl IntoElement {
        let (q, detail, action) = c.question();
        let says = c.package_says().map(firmware_view::package_says);
        let yes = self.button(
            "confirm-yes",
            action,
            Kind::Quiet,
            self.model.busy.is_none(),
            cx,
            |this, _, cx| {
                let op = this.model.confirm_yes();
                this.run(op, cx);
            },
        );
        let no = self.button(
            "confirm-no",
            "Keep it",
            Kind::Secondary,
            true,
            cx,
            |this, _, _| this.model.confirm_no(),
        );
        div()
            .flex()
            .justify_center()
            .pt(px(m.gap * 4.0))
            .child(ui::with_ticks(
                ui::panel(Some(("Check", "Esc keeps it")), m)
                    .w_full()
                    .max_w(px(620.0))
                    .child(
                        div()
                            .id("confirm")
                            .role(Role::AlertDialog)
                            .aria_label(SharedString::from(format!("{q} {detail}")))
                            .flex()
                            .flex_col()
                            .gap(px(16.0))
                            .p(px(m.pad))
                            .child(ui::h2(q))
                            .child(ui::p_muted(detail))
                            .children(says)
                            .child(div().flex().flex_wrap().gap(px(8.0)).child(no).child(yes)),
                    ),
            ))
    }
}

impl Render for Manager {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.smoke_test {
            self.smoke_test = false;
            window.on_next_frame(|_, cx| {
                println!("rendered");
                cx.quit();
            });
        }
        let m = Metrics::for_width(f32::from(window.viewport_size().width));
        let title = match (self.model.connected(), self.model.section) {
            (false, _) => "Display Hub".to_string(),
            (true, s) => format!("{} · Display Hub", s.label()),
        };
        window.set_window_title(&title);
        let mut content: Vec<gpui::AnyElement> = Vec::new();
        // Messages dock above the footer, so nothing moves under the pointer when one appears.
        let notice = self.model.notice.clone().map(|n| self.notice(&n, m, cx));
        if let Some(confirm) = self.model.confirm.clone() {
            content.push(self.confirm_view(&confirm, m, cx).into_any_element());
        } else if !self.model.connected() {
            content.extend(self.connect(m, window, cx));
        } else {
            let screen = match self.model.section {
                Section::Overview => self.overview(m, cx),
                Section::Pages => self.pages(m, window, cx),
                Section::Displays => self.displays(m, window, cx),
                Section::Firmware => self.firmware(m, window, cx),
                Section::Settings => self.settings(m, window, cx),
            };
            content.extend(screen);
        }
        let main = div()
            .id("main")
            .role(Role::Main)
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .overflow_y_scroll()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(m.gap))
                    .px(px(m.gutter))
                    .py(px(m.gap))
                    .w_full()
                    .max_w(px(2400.0))
                    .children(content),
            );
        let root = div()
            .id("root")
            .track_focus(&self.root)
            .size_full()
            .flex()
            .flex_col()
            .bg(c(BLACK))
            .text_color(c(TEXT))
            .font_family(FONT_MONO)
            .on_key_down(cx.listener(|this, e: &KeyDownEvent, w, cx| this.key_down(e, w, cx)))
            .child(self.rail(m, cx))
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .child(ui::pattern())
                    .child(main),
            )
            .when_some(notice, |d, n| d.child(n))
            .child(self.footer(m));
        self.prune_focus(window, cx);
        root
    }
}

/// A message about the form's answers, under the form.
fn form_error(text: &str) -> Stateful<Div> {
    div()
        .id("form-error")
        .role(Role::Alert)
        .aria_label(SharedString::from(text.to_string()))
        .flex()
        .gap(px(10.0))
        .items_start()
        .p(px(12.0))
        .border_1()
        .border_color(c(BEAK))
        .child(ui::tag("Check", BEAK))
        .child(div().flex_1().min_w_0().child(ui::p(text.to_string())))
}

fn stat_panel(value: String, caption: &str, m: Metrics) -> impl IntoElement {
    ui::with_ticks(
        ui::panel(None, m)
            .flex_1()
            .min_w(px(200.0))
            .child(ui::body(m).child(ui::stat(value, caption))),
    )
}

fn kv(k: &str, v: String) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(px(4.0))
        .child(ui::label(k))
        .child(ui::data(v))
}

fn waiting(m: Metrics) -> impl IntoElement {
    ui::with_ticks(ui::panel(None, m).child(ui::body(m).child(ui::p_muted("Checking the hub."))))
}

/// DD/MM/YYYY and a 24-hour time in the PC's time zone, or UTC when it isn't known.
fn nz_date_time(t: hub_protocol::Timestamp) -> String {
    let utc = time::OffsetDateTime::from_unix_timestamp(t.unix()).unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
    let local = crate::LOCAL_OFFSET.get().map_or(utc, |o| utc.to_offset(*o));
    format!("{} {:02}:{:02}", nz_date(t), local.hour(), local.minute())
}

/// DD/MM/YYYY in the PC's time zone, or UTC when it isn't known.
fn nz_date(t: hub_protocol::Timestamp) -> String {
    let utc = time::OffsetDateTime::from_unix_timestamp(t.unix()).unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
    let local = crate::LOCAL_OFFSET.get().map_or(utc, |o| utc.to_offset(*o));
    format!(
        "{:02}/{:02}/{}",
        local.day(),
        u8::from(local.month()),
        local.year()
    )
}

// ---- drawing a field -----------------------------------------------------------------------------------

fn shape(text: &str, colour: Hsla, sel: Option<(usize, usize)>, window: &Window) -> ShapedLine {
    let f = font(FONT_MONO);
    let run = |len: usize, c: Hsla| TextRun {
        len,
        font: f.clone(),
        color: c,
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let runs: Vec<TextRun> = match sel {
        Some((a, b)) if a < b => [(a, colour), (b - a, c(BLACK)), (text.len() - b, colour)]
            .into_iter()
            .filter(|r| r.0 > 0)
            .map(|(l, c)| run(l, c))
            .collect(),
        _ => vec![run(text.len(), colour)],
    };
    window
        .text_system()
        .shape_line(text.to_string().into(), px(FIELD_TEXT), &runs, None)
}

fn caret_index(shown: &str, x: f32, window: &Window) -> usize {
    if shown.is_empty() {
        return 0;
    }
    shape(shown, c(TEXT), None, window).closest_index_for_x(px(x.max(0.0)))
}

/// The field's text, selection and a steady caret, scrolled so the caret stays in view.
fn field_canvas(
    f: Field,
    shown: String,
    focused: bool,
    id: FieldId,
    store: Rc<RefCell<HashMap<FieldId, Bounds<Pixels>>>>,
) -> impl IntoElement {
    canvas(
        |_, _, _| {},
        move |bounds: Bounds<Pixels>, _, window, cx: &mut App| {
            store.borrow_mut().insert(id, bounds);
            let line_h = px(FIELD_TEXT * 1.3);
            let y = bounds.origin.y + (bounds.size.height - line_h) / 2.0;
            let (a, b) = f.selection();
            let (a, b, caret) = (f.shown_offset(a), f.shown_offset(b), f.shown_offset(f.caret));
            let line = shape(&shown, c(TEXT), (focused && a < b).then_some((a, b)), window);
            let caret_x = f32::from(line.x_for_index(caret));
            let width = f32::from(bounds.size.width);
            let scroll = (caret_x - width + 2.0).max(0.0);
            let ox = bounds.origin.x - px(scroll);
            window.with_content_mask(Some(gpui::ContentMask { bounds }), |window| {
                if focused && a < b {
                    let (xa, xb) = (f32::from(line.x_for_index(a)), f32::from(line.x_for_index(b)));
                    window.paint_quad(fill(
                        Bounds::new(point(ox + px(xa), y), size(px(xb - xa), line_h)),
                        c(BEAK),
                    ));
                }
                let _ = line.paint(point(ox, y), line_h, gpui::TextAlign::Left, None, window, cx);
                if focused && a == b {
                    window.paint_quad(fill(
                        Bounds::new(point(ox + px(caret_x), y), size(px(1.5), line_h)),
                        c(BEAK),
                    ));
                }
            });
        },
    )
    .flex_1()
    .h_full()
}

/// The page's content in the manager's preview: what a display would show, as far as the manager
/// can draw it. A custom kind is drawn by its display's own design, so the manager only says so.
fn preview_body(page: &hub_protocol::Page, generated_at: hub_protocol::Timestamp) -> Vec<gpui::Div> {
    match page.content() {
        Ok(Some(content)) => shown_view(&Shown::from_content(&content, Some(generated_at))),
        Ok(None) => Vec::new(),
        Err(cant) => cant.card_lines().iter().map(|l| ui::p_muted(*l)).collect(),
    }
}

/// A page's content as the Pages list and the Overview draw it.
fn shown_view(content: &Shown) -> Vec<gpui::Div> {
    match content {
        Shown::Empty => Vec::new(),
        Shown::Meter(rows) => rows
            .iter()
            .map(|r| ui::meter(&r.label, r.used_percent, &r.resets))
            .collect(),
        Shown::BigNumber { value, caption } => {
            let mut out = vec![ui::data(value.clone()).text_size(px(34.0)).line_height(px(40.0))];
            if let Some(c) = caption {
                out.push(ui::p_muted(c.clone()).text_size(px(14.0)));
            }
            out
        }
        Shown::Text(t) => vec![ui::p(t.clone())],
        Shown::List(lines) => lines
            .iter()
            .map(|l| {
                div()
                    .flex()
                    .flex_wrap()
                    .justify_between()
                    .gap(px(12.0))
                    .child(ui::p(l.text.clone()))
                    .when_some(l.detail.clone(), |d, x| d.child(ui::p_muted(x)))
            })
            .collect(),
        Shown::Custom => vec![ui::p_muted(shown::CUSTOM_CARD)],
        Shown::Unreadable => vec![ui::p_muted("What this page holds can't be read.")],
    }
}
