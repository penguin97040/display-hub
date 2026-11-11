#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]
//! Display Hub manager: pairs with the hub on the Raspberry Pi, and manages its pages and displays.

mod app;
mod assets;
mod field;
mod guidance;
mod model;
mod secrets;
mod settings;
mod theme;
mod ui;
mod worker;

use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use gpui::{px, size, App, AppContext, Bounds, Size, TitlebarOptions, WindowBounds, WindowOptions};
use gpui_platform::application;

use crate::guidance::{Guidance, Next, Tone};
use crate::model::{Model, Op};

/// The PC's offset from UTC, read once at start-up while there is one thread (the `time` crate
/// refuses to read it later on Linux).
pub static LOCAL_OFFSET: OnceLock<time::UtcOffset> = OnceLock::new();

struct Args {
    smoke_test: bool,
    config_dir: Option<PathBuf>,
}

fn args() -> Result<Args, String> {
    let mut out = Args {
        smoke_test: false,
        config_dir: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--smoke-test" => out.smoke_test = true,
            // For testing: keep settings somewhere other than the user's configuration folder.
            "--config-dir" => out.config_dir = Some(it.next().ok_or("--config-dir needs a folder")?.into()),
            "--version" => {
                println!("hub-manager {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    Ok(out)
}

/// A default name for this PC on the hub's list of admins.
fn pc_name() -> String {
    let raw = std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok())
        .unwrap_or_default();
    hub_core::model::clean_title(raw.trim()).unwrap_or_else(|| "This PC".into())
}

fn main() {
    if let Ok(o) = time::UtcOffset::current_local_offset() {
        let _ = LOCAL_OFFSET.set(o);
    }
    let args = match args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("hub-manager: {e}");
            std::process::exit(2);
        }
    };
    let smoke_test = args.smoke_test;
    let dir = settings::folder(args.config_dir.as_deref()).unwrap_or_else(|| PathBuf::from("."));
    // Settings and the credential are read before the window opens, so the first frame is right.
    let (saved, mut problem) = match settings::load(&dir) {
        Ok(s) => (s, None),
        Err(e) => (
            None,
            Some(Guidance {
                tone: Tone::Problem,
                title: "Settings couldn't be read".into(),
                detail: e,
                next: Next::None,
            }),
        ),
    };
    let ctx = worker::Ctx::new(Arc::new(secrets::OsStore), dir);
    let mut model = Model::new(saved, &pc_name());
    let mut start = None;
    if let Some(hub) = model.hub.clone() {
        match ctx.resume(&hub) {
            Ok(true) => start = Some(Op::Refresh),
            Ok(false) => {
                model.needs_pairing = true;
                problem = Some(Guidance {
                    tone: Tone::Problem,
                    title: "This PC's credential is missing".into(),
                    detail: "It isn't in the system's credential store any more. Pair again with a new string from \
                             hubd pair-admin."
                        .into(),
                    next: Next::None,
                });
            }
            Err(model::Failure::Store(e)) => problem = Some(guidance::from_store(&e)),
            Err(_) => model.needs_pairing = true,
        }
    }
    model.notice = problem;
    if start.is_some() {
        model.busy = Some(model::Busy::Refreshing);
    }

    application()
        .with_assets(assets::Assets)
        .run(move |cx: &mut App| {
            // Greyscale smoothing, as the style guide's `-webkit-font-smoothing: antialiased`: subpixel
            // colour fringes look worse with light text on black.
            cx.set_text_rendering_mode(gpui::TextRenderingMode::Grayscale);
            cx.text_system()
                .add_fonts(vec![
                    Cow::Borrowed(assets::GEIST_REGULAR),
                    Cow::Borrowed(assets::GEIST_SEMIBOLD),
                    Cow::Borrowed(assets::GEIST_MONO),
                ])
                .expect("the bundled fonts load");
            let bounds = Bounds::centered(None, size(px(1280.0), px(820.0)), cx);
            let window = cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: Some(TitlebarOptions {
                        title: Some("Display Hub".into()),
                        ..Default::default()
                    }),
                    window_min_size: Some(Size {
                        width: px(720.0),
                        height: px(520.0),
                    }),
                    ..Default::default()
                },
                move |_, cx| cx.new(|cx| app::Manager::new(model, ctx, smoke_test, start, cx)),
            );
            if let Err(e) = window {
                eprintln!("hub-manager: couldn't open a window: {e}");
                cx.quit();
                return;
            }
            if smoke_test {
                println!("launched");
            }
            cx.activate(true);
        });
}
