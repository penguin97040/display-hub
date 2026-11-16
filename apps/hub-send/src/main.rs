//! `hub-send`: pairs this computer with your Display Hub and sends page content to a feed.
//! See `docs/helpers.md`.

use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};
use hub_core::{Clock, SystemClock};
use hub_protocol::Timestamp;
use hub_send::{FeedContent, Profile, StoreKind};
use zeroize::Zeroize;

const TOOL: &str = "hub-send";
/// The credential store's service name for this tool's token.
const SERVICE: &str = "display-hub-hub-send";
/// The most page data it will read; the hub's own page limit is far smaller.
const MAX_DATA_BYTES: u64 = 1024 * 1024;

/// Sends page content from this computer to your Display Hub.
#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    /// Keep the settings here instead of `<your config folder>/display-hub/hub-send`. Also set by the
    /// HUB_SEND_DIR environment variable.
    #[arg(long, global = true)]
    config_dir: Option<PathBuf>,
    /// Where the helper token is kept: `os` (the system's credential store, the default) or `file`
    /// (an owner-only file named `token` in the settings folder, for headless Linux services with no
    /// keyring). Also set by HUB_SEND_STORE.
    #[arg(long, global = true, value_enum)]
    store: Option<Store>,
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Clone, Copy, ValueEnum)]
enum Store {
    Os,
    File,
}

#[derive(Subcommand)]
enum Cmd {
    /// Pair with the hub. Paste the helper string from the manager's Settings when asked (it is read
    /// from standard input, never from the command line).
    Login,
    /// Show the hub, the feeds this helper may send to, and recent activity.
    Status,
    /// Forget the hub on this computer.
    Logout,
    /// Send a page body to one of this helper's feeds.
    Page {
        /// The feed to send to, as named in the manager.
        feed: String,
        /// The kind of page the data is for: `meter`, `big_number`, `text`, `list` or a custom kind.
        #[arg(long)]
        kind: String,
        /// A file holding the page's data as JSON, or `-` to read it from standard input.
        #[arg(long)]
        data: String,
        /// A short note shown with the page, if its kind has room for one.
        #[arg(long)]
        note: Option<String>,
        /// When the data was collected, like 2026-11-16T09:30:00Z. Defaults to now.
        #[arg(long)]
        collected_at: Option<String>,
    },
}

fn finish(r: Result<String, String>) -> ExitCode {
    match r {
        Ok(m) => {
            println!("{m}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("{TOOL}: {e}");
            ExitCode::FAILURE
        }
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime starts")
}

/// The folder, from `--config-dir`, then HUB_SEND_DIR, then the default.
fn folder(arg: Option<PathBuf>) -> PathBuf {
    arg.or_else(|| {
        std::env::var_os("HUB_SEND_DIR")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    })
    .unwrap_or_else(|| hub_send::default_dir(TOOL))
}

/// The token store, from `--store`, then HUB_SEND_STORE, then the system's credential store.
fn store_kind(arg: Option<Store>) -> Result<StoreKind, String> {
    if let Some(s) = arg {
        return Ok(match s {
            Store::Os => StoreKind::Os,
            Store::File => StoreKind::File,
        });
    }
    match std::env::var("HUB_SEND_STORE") {
        Ok(v) if !v.trim().is_empty() => StoreKind::parse(&v).map_err(|e| format!("HUB_SEND_STORE: {e}")),
        _ => Ok(StoreKind::Os),
    }
}

fn profile(cli: &Cli) -> Result<Profile, String> {
    let dir = folder(cli.config_dir.clone());
    let store = hub_send::token_store(store_kind(cli.store)?, SERVICE, &dir)?;
    Ok(Profile::new(TOOL, dir, store))
}

fn login(p: &Profile) -> Result<String, String> {
    if std::io::stdin().is_terminal() {
        eprint!("Paste the helper string from the manager's Settings, then press Enter: ");
        let _ = std::io::stderr().flush();
    }
    let mut line = String::new();
    let read = std::io::stdin().read_line(&mut line);
    if read.is_err() || line.trim().is_empty() {
        line.zeroize();
        return Err("no helper string given".into());
    }
    let r = runtime().block_on(p.login(&line, SystemClock.now()));
    line.zeroize();
    let cfg = r.map_err(|e| e.to_string())?;
    Ok(format!(
        "Paired with the hub at {}:{}. This helper can send to: {}.",
        cfg.host,
        cfg.port,
        cfg.feeds.join(", ")
    ))
}

fn status(p: &Profile) -> Result<String, String> {
    let h = p.open().map_err(|e| e.to_string())?;
    let mut out = vec![
        format!("Hub: {}:{}", h.config.host, h.config.port),
        format!("Feeds: {}", h.config.feeds.join(", ")),
    ];
    out.push(match runtime().block_on(h.reachable()) {
        Ok(()) => "Reachable: yes".into(),
        Err(e) => format!("Reachable: no ({e})"),
    });
    let tail = p.log_tail(5);
    if !tail.is_empty() {
        out.push("Recent activity:".into());
        out.extend(tail.into_iter().map(|l| format!("  {l}")));
    }
    Ok(out.join("\n"))
}

fn logout(p: &Profile) -> Result<String, String> {
    Ok(match p.logout().map_err(|e| e.to_string())? {
        Some(_) => {
            "Forgot the hub on this computer. Remove the helper in the manager's Settings too, to cut it off."
                .into()
        }
        None => "This helper wasn't set up.".into(),
    })
}

fn read_data(source: &str) -> Result<serde_json::Value, String> {
    let mut bytes = Vec::new();
    let result = if source == "-" {
        std::io::stdin().take(MAX_DATA_BYTES + 1).read_to_end(&mut bytes)
    } else {
        std::fs::File::open(Path::new(source))
            .and_then(|f| f.take(MAX_DATA_BYTES + 1).read_to_end(&mut bytes))
    };
    result.map_err(|e| {
        format!(
            "couldn't read the data from {}: {e}",
            if source == "-" { "standard input" } else { source }
        )
    })?;
    if bytes.len() as u64 > MAX_DATA_BYTES {
        return Err("the data is far too large for a page".into());
    }
    serde_json::from_slice(&bytes).map_err(|e| format!("the data isn't valid JSON: {e}"))
}

fn page(
    p: &Profile,
    feed: &str,
    kind: &str,
    data: &str,
    note: Option<String>,
    collected_at: Option<&str>,
) -> Result<String, String> {
    let collected_at = collected_at
        .map(|t| {
            Timestamp::parse(t).map_err(|_| format!("{t:?} isn't a time like 2026-11-16T09:30:00Z (UTC)"))
        })
        .transpose()?;
    let content = FeedContent {
        kind: kind.to_string(),
        data: read_data(data)?,
        collected_at,
        note,
    };
    let h = p.open().map_err(|e| e.to_string())?;
    let now = SystemClock.now();
    let result = runtime()
        .block_on(h.send_page(feed, &content))
        .map_err(|e| e.to_string());
    p.log(
        now,
        &match &result {
            Ok(a) => format!(
                "{feed}: sent ({} updated, {} skipped)",
                a.updated.len(),
                a.skipped.len()
            ),
            Err(e) => format!("{feed}: {e}"),
        },
    );
    let accepted = result?;
    let mut lines = Vec::new();
    if !accepted.updated.is_empty() {
        lines.push(format!(
            "Updated {}: {}",
            plural(accepted.updated.len()),
            accepted.updated.join(", ")
        ));
    }
    if !accepted.skipped.is_empty() {
        lines.push(format!(
            "Left alone because they take another kind of page than {kind}: {}",
            accepted.skipped.join(", ")
        ));
    }
    if lines.is_empty() {
        lines.push(format!(
            "The hub accepted it, but no page takes the {feed} feed from a helper yet. See \"Pages for a helper\" in docs/helpers.md."
        ));
    }
    Ok(lines.join("\n"))
}

fn plural(n: usize) -> String {
    if n == 1 {
        "1 page".into()
    } else {
        format!("{n} pages")
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let p = match profile(&cli) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{TOOL}: {e}");
            return ExitCode::from(2);
        }
    };
    match cli.command {
        Cmd::Login => finish(login(&p)),
        Cmd::Status => finish(status(&p)),
        Cmd::Logout => finish(logout(&p)),
        Cmd::Page {
            feed,
            kind,
            data,
            note,
            collected_at,
        } => finish(page(&p, &feed, &kind, &data, note, collected_at.as_deref())),
    }
}
