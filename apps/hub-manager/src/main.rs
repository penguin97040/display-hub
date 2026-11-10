#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

// The spike uses few tokens; the full manager (M3) uses them all.
#[allow(dead_code)]
mod theme;

use std::borrow::Cow;

use gpui::{div, prelude::*, px, rgb, size, App, Bounds, Context, Window, WindowBounds, WindowOptions};
use gpui_platform::application;

const GEIST_REGULAR: &[u8] = include_bytes!("../../../assets/fonts/Geist-Regular.ttf");
const GEIST_SEMIBOLD: &[u8] = include_bytes!("../../../assets/fonts/Geist-SemiBold.ttf");
const GEIST_MONO: &[u8] = include_bytes!("../../../assets/fonts/GeistMono-Regular.ttf");

struct Spike {
    smoke_test: bool,
}

impl Render for Spike {
    fn render(&mut self, window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        if self.smoke_test {
            window.on_next_frame(|_, cx| {
                println!("rendered");
                cx.quit();
            });
        }
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(theme::BLACK))
            .text_color(rgb(theme::TEXT))
            .font_family(theme::FONT_MONO)
            .p(px(theme::PANEL_GAP))
            .child(
                div()
                    .p(px(theme::PANEL_PAD))
                    .border_1()
                    .border_color(rgb(theme::LINE))
                    .child(
                        div()
                            .font_family(theme::FONT_BODY)
                            .text_size(px(36.0))
                            .child("Display Hub"),
                    )
                    .child(
                        div()
                            .text_size(px(theme::LABEL_SIZE))
                            .text_color(rgb(theme::MUTED))
                            .child("GPUI SPIKE"),
                    ),
            )
    }
}

fn main() {
    let smoke_test = std::env::args().any(|a| a == "--smoke-test");
    application().run(move |cx: &mut App| {
        cx.text_system()
            .add_fonts(vec![
                Cow::Borrowed(GEIST_REGULAR),
                Cow::Borrowed(GEIST_SEMIBOLD),
                Cow::Borrowed(GEIST_MONO),
            ])
            .expect("the bundled fonts load");
        let bounds = Bounds::centered(None, size(px(1024.0), px(700.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_, cx| cx.new(|_| Spike { smoke_test }),
        )
        .expect("the window opens");
        if smoke_test {
            println!("launched");
        }
        cx.activate(true);
    });
}
