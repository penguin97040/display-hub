//! What each `hubctl` command does. Both binaries call these.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use hub_core::api::UsageReport;
use hub_core::{Clock, SystemClock};
use hub_protocol::pairing::PairingString;
use os_secrets::{OsStore, TokenStore};

use crate::claude::{self, Sent};
use crate::codex;
use crate::helper::{self, Helper, HelperError};

const PENDING: &str = "claude-pending.json";
const SENT: &str = "claude-sent.json";
/// A queued Claude report older than this isn't sent: newer figures will come.
const PENDING_MAX_AGE_SECONDS: i64 = 600;

pub fn store() -> OsStore {
    OsStore {
        service: helper::SERVICE,
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime starts")
}

fn service_names(cfg: &helper::HelperConfig) -> String {
    cfg.services
        .iter()
        .map(|s| s.default_title())
        .collect::<Vec<_>>()
        .join(" and ")
}

/// `hubctl login`: the helper string comes on stdin, never as an argument, so it stays out of shell
/// history and process lists.
pub fn login(dir: &Path, store: &dyn TokenStore, input: &str) -> Result<String, String> {
    let pairing = PairingString::decode(input.trim()).map_err(|e| e.to_string())?;
    let cfg = runtime()
        .block_on(helper::login(dir, store, &pairing, SystemClock.now()))
        .map_err(|e| match e {
            HelperError::Hub(c) if c.code() == Some(hub_protocol::ErrorCode::EnrolmentUsed) => {
                "this helper string was already used. Make a new one in the manager's Settings.".into()
            }
            HelperError::Hub(c) if c.code() == Some(hub_protocol::ErrorCode::EnrolmentExpired) => {
                "this helper string has expired (they last 10 minutes). Make a new one in the manager's Settings.".into()
            }
            other => other.to_string(),
        })?;
    Ok(format!(
        "Paired with the hub at {}:{}. This PC can now report {}.",
        cfg.host,
        cfg.port,
        service_names(&cfg)
    ))
}

pub fn logout(dir: &Path, store: &dyn TokenStore) -> Result<String, String> {
    match helper::logout(dir, store).map_err(|e| e.to_string())? {
        Some(_) => Ok(
            "Forgot the hub on this PC. Remove the helper in the manager's Settings too, to cut it off."
                .into(),
        ),
        None => Ok("This PC helper wasn't set up.".into()),
    }
}

pub fn status(dir: &Path, store: &dyn TokenStore) -> Result<String, String> {
    let h = Helper::open(dir, store).map_err(|e| e.to_string())?;
    let mut out = vec![
        format!("Hub: {}:{}", h.config.host, h.config.port),
        format!("Reports: {}", service_names(&h.config)),
    ];
    out.push(match runtime().block_on(h.reachable()) {
        Ok(()) => "Reachable: yes".into(),
        Err(e) => format!("Reachable: no ({e})"),
    });
    let tail = helper::log_tail(dir, 5);
    if !tail.is_empty() {
        out.push("Recent activity:".into());
        out.extend(tail.into_iter().map(|l| format!("  {l}")));
    }
    Ok(out.join("\n"))
}

/// `hubctl claude-statusline`: prints the status line straight away, then, at most once a minute,
/// starts a separate `send-pending` so Claude Code never waits on the network.
pub fn claude_statusline(
    dir: &Path,
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
    if helper::load_config(dir).is_err() {
        return;
    }
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
    if helper::write_private(&dir.join(PENDING), &pending).is_err()
        || helper::write_private(&dir.join(SENT), &sent).is_err()
    {
        return;
    }
    if spawn {
        if let Err(e) = spawn_sender(dir) {
            helper::log(dir, now, &format!("couldn't start the sender: {e}"));
        }
    }
}

/// Starts `hubctl send-pending` on its own, detached, with no window and no inherited streams.
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

/// `hubctl send-pending`: sends the queued Claude Code figures.
pub fn send_pending(dir: &Path, store: &dyn TokenStore) -> Result<String, String> {
    let now = SystemClock.now();
    let path = dir.join(PENDING);
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
    let result = send(dir, store, &report);
    helper::log(
        dir,
        now,
        &format!("Claude Code: {}", result.as_ref().unwrap_or_else(|e| e)),
    );
    result
}

fn send(dir: &Path, store: &dyn TokenStore, report: &UsageReport) -> Result<String, String> {
    let h = Helper::open(dir, store).map_err(|e| e.to_string())?;
    let accepted = runtime().block_on(h.send(report)).map_err(|e| e.to_string())?;
    Ok(if accepted.updated.is_empty() {
        format!(
            "sent, but no page uses it yet: in the manager, set a {} page's figures to \"PC helper\"",
            report.service.default_title()
        )
    } else {
        format!("sent to {}", accepted.updated.join(", "))
    })
}

/// `hubctl codex-sync`: asks Codex for its plan limits and sends them. For Task Scheduler.
pub fn codex_sync(
    dir: &Path,
    store: &dyn TokenStore,
    codex_path: Option<&Path>,
    dry_run: bool,
) -> Result<String, String> {
    let now = SystemClock.now();
    if !dry_run {
        // Fail early, before starting Codex, if there's nowhere to send.
        Helper::open(dir, store).map_err(|e| e.to_string())?;
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
            send(dir, store, &report).map(|s| format!("Codex reports {shown}: {s}"))
        });
    if !dry_run {
        helper::log(
            dir,
            now,
            &format!("Codex: {}", result.as_ref().unwrap_or_else(|e| e)),
        );
    }
    result
}

pub fn dir(arg: Option<PathBuf>) -> PathBuf {
    helper::folder(arg.as_deref())
}
