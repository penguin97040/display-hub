//! The zvx.nz components in GPUI: panels with corner ticks, labels, meters, the static pattern and
//! the marks. Stateless; the interactive parts (buttons, fields, navigation) are in `app.rs`.

use std::sync::Arc;

use gpui::{
    canvas, div, fill, point, prelude::*, px, rgb, size, svg, Bounds, Div, FontFeatures, FontWeight, Hsla,
    IntoElement, Pixels, SharedString,
};

use crate::assets::Assets;
use crate::theme::*;

pub fn c(v: u32) -> Hsla {
    rgb(v).into()
}

/// Sizes that follow the window, as the CSS `clamp()`s do.
#[derive(Clone, Copy, Debug)]
pub struct Metrics {
    pub width: f32,
    pub gutter: f32,
    pub gap: f32,
    pub pad: f32,
    pub h1: f32,
    /// Two columns fit.
    pub wide: bool,
}

impl Metrics {
    pub fn for_width(width: f32) -> Self {
        let clamp = |v: f32, lo: f32, hi: f32| v.clamp(lo, hi);
        Self {
            width,
            gutter: clamp(width * 0.025, 12.0, 48.0),
            gap: clamp(width * 0.01, 8.0, PANEL_GAP),
            pad: clamp(width * 0.015, 16.0, 28.0),
            h1: clamp(width * 0.045, 36.0, 72.0),
            wide: width >= 1100.0,
        }
    }
}

/// `font-variant-numeric: tabular-nums`.
pub fn tabular() -> FontFeatures {
    FontFeatures(Arc::new(vec![("tnum".to_string(), 1)]))
}

/// CSS `letter-spacing`: GPUI has none, so each character is its own item with a trailing gap.
/// Kerning is lost, which a monospaced face doesn't need. Words stay whole and wrap as words.
pub fn spaced(text: &str, size: f32, em: f32) -> Div {
    let gap = size * em;
    div()
        .flex()
        .flex_wrap()
        .gap_x(px(size * 0.6 + 2.0 * gap))
        .gap_y(px(2.0))
        .children(text.split(' ').filter(|w| !w.is_empty()).map(move |word| {
            div()
                .flex()
                .flex_none()
                .gap(px(gap))
                .children(word.chars().map(|ch| div().flex_none().child(ch.to_string())))
        }))
}

/// `.label`: mono, upper case, 0.1em tracking, muted.
pub fn label(text: &str) -> Div {
    div()
        .font_family(FONT_MONO)
        .text_size(px(LABEL_SIZE))
        .line_height(px(LABEL_SIZE * 1.4))
        .text_color(c(MUTED))
        .child(spaced(&text.to_uppercase(), LABEL_SIZE, LABEL_TRACKING_EM))
}

/// A label in another colour, for state words such as STALE.
pub fn label_in(text: &str, colour: u32) -> Div {
    label(text).text_color(c(colour))
}

pub fn h1(text: impl Into<SharedString>, m: Metrics) -> Div {
    div()
        .font_family(FONT_BODY)
        .font_weight(FontWeight::SEMIBOLD)
        .text_size(px(m.h1))
        .line_height(px(m.h1))
        .text_color(c(TEXT))
        .child(text.into())
}

pub fn h2(text: impl Into<SharedString>) -> Div {
    div()
        .font_family(FONT_BODY)
        .font_weight(FontWeight::SEMIBOLD)
        .text_size(px(H2_SIZE))
        .line_height(px(H2_SIZE * 1.2))
        .text_color(c(TEXT))
        .child(text.into())
}

pub fn h3(text: impl Into<SharedString>) -> Div {
    div()
        .font_family(FONT_BODY)
        .font_weight(FontWeight::SEMIBOLD)
        .text_size(px(18.0))
        .line_height(px(18.0 * 1.3))
        .text_color(c(TEXT))
        .child(text.into())
}

/// Body copy in Geist, never mono.
pub fn p(text: impl Into<SharedString>) -> Div {
    div()
        .font_family(FONT_BODY)
        .text_size(px(BODY_SIZE))
        .line_height(px(BODY_SIZE * 1.6))
        .text_color(c(TEXT))
        .child(text.into())
}

pub fn p_muted(text: impl Into<SharedString>) -> Div {
    p(text).text_color(c(MUTED))
}

/// Mono data, such as an address or a fingerprint.
pub fn data(text: impl Into<SharedString>) -> Div {
    div()
        .font_family(FONT_MONO)
        .text_size(px(15.0))
        .line_height(px(15.0 * 1.4))
        .font_features(tabular())
        .text_color(c(TEXT))
        .child(text.into())
}

/// `.stat-big` with its label underneath.
pub fn stat(value: impl Into<SharedString>, caption: &str) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(px(6.0))
        .child(
            div()
                .font_family(FONT_MONO)
                .text_size(px(48.0))
                .line_height(px(48.0 * 1.05))
                .font_features(tabular())
                .text_color(c(TEXT))
                .child(value.into()),
        )
        .child(label(caption))
}

/// The white tick top left and the orange tick bottom right, on the panel's own border.
fn ticks() -> [Div; 2] {
    [
        div()
            .absolute()
            .top(px(-1.0))
            .left(px(-1.0))
            .w(px(TICK))
            .h(px(TICK))
            .border_t(px(TICK_STROKE))
            .border_l(px(TICK_STROKE))
            .border_color(c(TEXT)),
        div()
            .absolute()
            .bottom(px(-1.0))
            .right(px(-1.0))
            .w(px(TICK))
            .h(px(TICK))
            .border_b(px(TICK_STROKE))
            .border_r(px(TICK_STROKE))
            .border_color(c(BEAK)),
    ]
}

/// `.panel` with an optional `.panel-head` strip: a title and a note.
pub fn panel(head: Option<(&str, &str)>, m: Metrics) -> Div {
    let mut d = div()
        .relative()
        .flex()
        .flex_col()
        .min_w_0()
        .bg(c(BLACK))
        .border_1()
        .border_color(c(LINE));
    if let Some((title, note)) = head {
        d = d.child(
            div()
                .flex()
                .flex_wrap()
                .items_center()
                .justify_between()
                .gap(px(8.0))
                .px(px(m.pad))
                .py(px(12.0))
                .border_b_1()
                .border_color(c(LINE))
                .child(h2(title.to_string()))
                .when(!note.is_empty(), |d| d.child(label(note))),
        );
    }
    d
}

/// The panel's content area, padded. Ticks go last so they draw over the edge.
pub fn body(m: Metrics) -> Div {
    div().flex().flex_col().gap(px(16.0)).p(px(m.pad))
}

pub fn with_ticks<E: ParentElement>(panel: E) -> E {
    let [a, b] = ticks();
    panel.child(a).child(b)
}

/// A share of an allowance as square cells, with the number. `None` is unknown and drawn as words,
/// never as an empty meter that would read as zero.
pub fn meter(name: &str, used: Option<f64>, resets: &str) -> Div {
    const CELLS: usize = 20;
    let reading = match used {
        Some(p) => {
            let filled = ((p / 100.0) * CELLS as f64).round() as usize;
            let colour = if p >= 90.0 { BEAK } else { TEXT };
            div()
                .flex()
                .items_center()
                .gap(px(12.0))
                .child(
                    div()
                        .flex()
                        .flex_1()
                        .gap(px(2.0))
                        .children((0..CELLS).map(move |i| {
                            let cell = div().flex_1().h(px(12.0));
                            if i < filled {
                                cell.bg(c(colour))
                            } else {
                                cell.border_1().border_color(c(LINE))
                            }
                        })),
                )
                .child(
                    div()
                        .w(px(64.0))
                        .flex()
                        .justify_end()
                        .font_family(FONT_MONO)
                        .text_size(px(20.0))
                        .font_features(tabular())
                        .text_color(c(TEXT))
                        .child(format!(
                            "{}%",
                            if p.fract() == 0.0 {
                                format!("{p:.0}")
                            } else {
                                format!("{p:.1}")
                            }
                        )),
                )
        }
        None => div()
            .flex()
            .items_center()
            .h(px(24.0))
            .child(label_in("Not known", MUTED)),
    };
    div()
        .flex()
        .flex_col()
        .gap(px(6.0))
        .child(label(name))
        .child(reading)
        .when(!resets.is_empty(), |d| d.child(label(resets)))
}

/// A small tag such as DEMO or STALE: mono upper case in a muted frame.
pub fn tag(text: &str, colour: u32) -> Div {
    div()
        .flex_none()
        .px(px(6.0))
        .py(px(2.0))
        .border_1()
        .border_color(c(colour))
        .child(label_in(text, colour))
}

pub fn mark(name: &str, size: f32, colour: u32) -> gpui::Svg {
    svg()
        .path(Assets::path(name))
        .size(px(size))
        .flex_none()
        .text_color(c(colour))
}

/// The NZ mark: the Southern Cross in orange, and the words that carry its meaning.
pub fn nz_mark() -> Div {
    div()
        .flex()
        .items_center()
        .gap(px(11.0))
        .child(
            svg()
                .path(Assets::path("nz"))
                .w(px(17.6))
                .h(px(22.0))
                .flex_none()
                .text_color(c(BEAK)),
        )
        .child(label("Aotearoa New Zealand"))
}

/// The static pattern: a 2 px square every 14 px, and a 4 px square every 112 px, from the window's
/// top left so it stays still as content scrolls over it.
pub fn pattern() -> impl IntoElement {
    canvas(
        |_, _, _| {},
        |bounds: Bounds<Pixels>, _, window, _| {
            let (w, h) = (f32::from(bounds.size.width), f32::from(bounds.size.height));
            let (ox, oy) = (f32::from(bounds.origin.x), f32::from(bounds.origin.y));
            let start = |o: f32, pitch: f32| (o / pitch).ceil() * pitch - o;
            let rest = c(FIELD_REST);
            let node = c(FIELD_NODE);
            let mut y = start(oy, PATTERN_PITCH);
            while y < h {
                let mut x = start(ox, PATTERN_PITCH);
                while x < w {
                    let (ax, ay) = (x + ox, y + oy);
                    let is_node =
                        (ax % PATTERN_NODE_PITCH).abs() < 0.5 && (ay % PATTERN_NODE_PITCH).abs() < 0.5;
                    let (s, colour) = if is_node { (4.0, node) } else { (2.0, rest) };
                    window.paint_quad(fill(
                        Bounds::new(point(px(ax), px(ay)), size(px(s), px(s))),
                        colour,
                    ));
                    x += PATTERN_PITCH;
                }
                y += PATTERN_PITCH;
            }
        },
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full()
}
