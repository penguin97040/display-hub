//! What each `usage-helper` command does. Both binaries call these.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use hub_core::{Clock, SystemClock};
use hub_send::{write_private, Config, Error, Profile};
use os_secrets::OsStore;
use usage::{Service, UsageReport};

use crate::claude::{self, Sent};
use crate::codex;

/// The name the helper's messages use for its command.
pub const TOOL: &str = "usage-helper";
/// The credential store's service name for the helper's token.
pub const SERVICE: &str = "display-hub-usage-helper";

const PENDING: &str = "claude-pending.json";
const SENT: &str = "claude-sent.json";
/// A queued Claude report older than this isn't sent: newer figures will come.
const PENDING_MAX_AGE_SECONDS: i64 = 600;

/// This helper's place on the computer: its folder (`dir`, or the user's configuration folder) and
/// the system's credential store.
pub fn profile(dir: Option<PathBuf>) -> Profile {
    let dir = dir.unwrap_or_else(|| hub_send::default_dir(TOOL));
    Profile::new(TOOL, dir, Box::new(OsStore { service: SERVICE }))
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime starts")
}

/// What the helper reports, by service name where its feed is a service's.
fn service_names(cfg: &Config) -> String {
    cfg.feeds
        .iter()
        .map(|f| Service::from_slug(f).map_or(f.as_str(), |s| s.default_title()))
        .collect::<Vec<_>>()
        .join(" and ")
}

/// `usage-helper login`: the helper string comes on stdin, never as an argument, so it stays out of
/// shell history and process lists.
pub fn login(p: &Profile, input: &str) -> Result<String, String> {
    let cfg = runtime()
        .block_on(p.login(input, SystemClock.now()))
        .map_err(|e| e.to_string())?;
    Ok(format!(
        "Paired with the hub at {}:{}. This PC can now report {}.",
        cfg.host,
        cfg.port,
        service_names(&cfg)
    ))
}

pub fn logout(p: &Profile) -> Result<String, String> {
    match p.logout().map_err(|e| e.to_string())? {
        Some(_) => Ok(
            "Forgot the hub on this PC. Remove the helper in the manager's Settings too, to cut it off."
                .into(),
        ),
        None => Ok("This PC helper wasn't set up.".into()),
    }
}

pub fn status(p: &Profile) -> Result<String, String> {
    let h = p.open().map_err(|e| e.to_string())?;
    let mut out = vec![
        format!("Hub: {}:{}", h.config.host, h.config.port),
        format!("Reports: {}", service_names(&h.config)),
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

/// `usage-helper claude-statusline`: prints the status line straight away, then, at most once a minute,
/// starts a separate `send-pending` so Claude Code never waits on the network.
pub fn claude_statusline(
    p: &Profile,
    stdin: &mut impl Read,
    stdout: &mut impl Write,
    quiet: bool,
    spawn: bool,
) {
    let mut buf = Vec::new();
    let _ = stdin.take(claude::MAX_INPUT_BYTES as u64).read_to_end(&mut buf);
    let update = claude::parse(&String::from_utf8_lossy(&buf));
    if !quiet {
        let _ = writeln!(stdout, "{}", claude::line(&update));
        let _ = stdout.flush();
    }
    let now = SystemClock.now();
    let Some(report) = claude::report(&update, now) else {
        return;
    };
    // Only a PC that has been set up sends anything.
    if p.config().is_err() {
        return;
    }
    let dir = p.dir();
    let digest = claude::digest(&report);
    let last: Option<Sent> = std::fs::read(dir.join(SENT))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok());
    if !claude::due(last.as_ref(), &digest, now.unix()) {
        return;
    }
    let pending = serde_json::to_vec(&report).expect("serialises");
    let sent = serde_json::to_vec(&Sent {
        at: now.unix(),
        digest,
    })
    .expect("serialises");
    if write_private(&dir.join(PENDING), &pending).is_err() || write_private(&dir.join(SENT), &sent).is_err()
    {
        return;
    }
    if spawn {
        if let Err(e) = spawn_sender(dir) {
            p.log(now, &format!("couldn't start the sender: {e}"));
        }
    }
}

/// Starts `usage-helper send-pending` on its own, detached, with no window and no inherited streams.
fn spawn_sender(dir: &Path) -> std::io::Result<()> {
    let exe = std::env::current_exe()?;
    let make = || {
        let mut c = Command::new(&exe);
        c.arg("send-pending").arg("--config-dir").arg(dir);
        c.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        c
    };
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Its own process group, so Claude Code cancelling the status line doesn't stop it.
        make().process_group(0).spawn().map(drop)
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
        let base = CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP;
        // Leave Claude Code's job object if it allows that, so cancelling the status line doesn't
        // end the send; otherwise run inside it.
        make()
            .creation_flags(base | CREATE_BREAKAWAY_FROM_JOB)
            .spawn()
            .or_else(|_| make().creation_flags(base).spawn())
            .map(drop)
    }
}

/// `usage-helper send-pending`: sends the queued Claude Code figures.
pub fn send_pending(p: &Profile) -> Result<String, String> {
    let now = SystemClock.now();
    let path = p.dir().join(PENDING);
    let Some(report) = std::fs::read(&path)
        .ok()
        .and_then(|b| serde_json::from_slice::<UsageReport>(&b).ok())
    else {
        return Ok("nothing to send".into());
    };
    let _ = std::fs::remove_file(&path);
    if report.collected_at.seconds_until(now) > PENDING_MAX_AGE_SECONDS {
        return Ok("the queued figures were too old to send".into());
    }
    let result = send(p, &report);
    p.log(
        now,
        &format!("Claude Code: {}", result.as_ref().unwrap_or_else(|e| e)),
    );
    result
}

fn send(p: &Profile, report: &UsageReport) -> Result<String, String> {
    let h = p.open().map_err(|e| e.to_string())?;
    // Only figures that pass the usage checks are sent, as a meter page to the service's feed.
    let (feed, body) = report
        .to_feed_content()
        .map_err(|e| Error::Hub(hub_send::ClientError::Invalid(e)).to_string())?;
    let accepted = runtime()
        .block_on(h.send_page(feed, &body))
        .map_err(|e| e.to_string())?;
    Ok(if accepted.updated.is_empty() {
        format!(
            "sent, but no page uses it yet: in the manager, set a {} page's figures to \"PC helper\"",
            report.service.default_title()
        )
    } else {
        format!("sent to {}", accepted.updated.join(", "))
    })
}

/// `usage-helper codex-sync`: asks Codex for its plan limits and sends them. For Task Scheduler.
pub fn codex_sync(p: &Profile, codex_path: Option<&Path>, dry_run: bool) -> Result<String, String> {
    let now = SystemClock.now();
    if !dry_run {
        // Fail early, before starting Codex, if there's nowhere to send.
        p.open().map_err(|e| e.to_string())?;
    }
    let result = codex::run(codex_path)
        .map_err(|e| e.to_string())
        .and_then(|snapshot| {
            let report = codex::report(&snapshot, now)
                .ok_or_else(|| "Codex reported no five-hour or weekly limits".to_string())?;
            let shown = report
                .metrics
                .iter()
                .map(|m| {
                    format!(
                        "{} {}",
                        m.window.id(),
                        m.used_percent.map_or("unknown".into(), |p| format!("{p:.0}%"))
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            if dry_run {
                return Ok(format!("Codex reports {shown} (not sent)"));
            }
            send(p, &report).map(|s| format!("Codex reports {shown}: {s}"))
        });
    if !dry_run {
        p.log(now, &format!("Codex: {}", result.as_ref().unwrap_or_else(|e| e)));
    }
    result
}
