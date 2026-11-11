//! Codex's app server (https://learn.chatgpt.com/docs/app-server; `codex-rs/app-server-protocol` in
//! github.com/openai/codex).
//!
//! `codex app-server` speaks JSON-RPC 2.0 over stdio, one JSON object per line, without the
//! `"jsonrpc"` member. After `initialize` and the `initialized` notification, `account/rateLimits/read`
//! returns `rateLimits` with optional `primary` and `secondary` windows: `usedPercent` (an integer),
//! `windowDurationMins` and `resetsAt` (Unix seconds). It needs Codex to be signed in with ChatGPT.
//! Windows are matched to the hub's by their length only; one whose length isn't given or doesn't match
//! is left out rather than guessed.

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use hub_core::api::{ManualMetric, UsageReport};
use hub_core::{Service, Window};
use hub_protocol::Timestamp;
use serde::Deserialize;
use serde_json::{json, Value};

/// How long the whole exchange may take, including Codex starting up.
pub const TIMEOUT: Duration = Duration::from_secs(30);
const MAX_LINE_BYTES: usize = 1024 * 1024;

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum CodexError {
    #[error("couldn't start Codex ({0}). Is the Codex CLI installed? Pass --codex with its path if it isn't on PATH.")]
    Start(String),
    #[error("Codex didn't answer within {0} seconds")]
    Timeout(u64),
    #[error("Codex closed the connection")]
    Closed,
    #[error("Codex isn't signed in with ChatGPT, so it has no plan limits to report. Run `codex login`.")]
    NotSignedIn,
    #[error("Codex refused: {0}")]
    Refused(String),
    #[error("Codex answered something this helper doesn't understand: {0}")]
    Unexpected(String),
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct LimitWindow {
    pub used_percent: f64,
    pub window_duration_mins: Option<i64>,
    pub resets_at: Option<i64>,
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub primary: Option<LimitWindow>,
    pub secondary: Option<LimitWindow>,
    pub plan_type: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReadResponse {
    rate_limits: Snapshot,
}

/// The hub's window for a Codex window length, if it is one the hub knows.
fn window_for(minutes: i64) -> Option<Window> {
    match minutes {
        300 => Some(Window::FiveHour),
        10_080 => Some(Window::Weekly),
        40_320..=44_640 => Some(Window::Monthly),
        _ => None,
    }
}

/// The report for the hub. ChatGPT plan limits go to the ChatGPT pages.
pub fn report(s: &Snapshot, now: Timestamp) -> Option<UsageReport> {
    let mut metrics: Vec<ManualMetric> = Vec::new();
    for w in [&s.primary, &s.secondary].into_iter().flatten() {
        let Some(window) = w.window_duration_mins.and_then(window_for) else {
            continue;
        };
        if !Service::ChatgptPlus.windows().contains(&window) || metrics.iter().any(|m| m.window == window) {
            continue;
        }
        let used_percent = Some(w.used_percent).filter(|p| p.is_finite() && (0.0..=100.0).contains(p));
        metrics.push(ManualMetric {
            window,
            used_percent,
            resets_at: w.resets_at.and_then(Timestamp::from_unix),
        });
    }
    (!metrics.is_empty()).then_some(UsageReport {
        service: Service::ChatgptPlus,
        collected_at: now,
        metrics,
    })
}

/// Reads lines on a thread, so waiting for one can time out.
fn lines<R: Read + Send + 'static>(reader: R) -> mpsc::Receiver<std::io::Result<String>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut r = BufReader::new(reader);
        loop {
            let mut line = String::new();
            match r.by_ref().take(MAX_LINE_BYTES as u64).read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {
                    if tx.send(Ok(line)).is_err() {
                        break;
                    }
                }
                Err(e) => {
                    let _ = tx.send(Err(e));
                    break;
                }
            }
        }
    });
    rx
}

/// Waits for the response to request `id`, skipping notifications and anything else.
fn response(
    rx: &mpsc::Receiver<std::io::Result<String>>,
    id: i64,
    deadline: Instant,
) -> Result<Value, CodexError> {
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let line = match rx.recv_timeout(left) {
            Ok(Ok(l)) => l,
            Ok(Err(e)) => return Err(CodexError::Unexpected(e.to_string())),
            Err(mpsc::RecvTimeoutError::Timeout) => return Err(CodexError::Timeout(TIMEOUT.as_secs())),
            Err(mpsc::RecvTimeoutError::Disconnected) => return Err(CodexError::Closed),
        };
        let Ok(msg) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        if msg.get("id").and_then(Value::as_i64) != Some(id) || msg.get("method").is_some() {
            continue;
        }
        if let Some(err) = msg.get("error") {
            let text = err.get("message").and_then(Value::as_str).unwrap_or("no details");
            let text: String = text.chars().filter(|c| !c.is_control()).take(200).collect();
            return Err(if text.contains("authentication required") {
                CodexError::NotSignedIn
            } else {
                CodexError::Refused(text)
            });
        }
        return msg
            .get("result")
            .cloned()
            .ok_or_else(|| CodexError::Unexpected("a response with no result".into()));
    }
}

fn send(w: &mut impl Write, msg: &Value) -> Result<(), CodexError> {
    let mut line = serde_json::to_vec(msg).expect("serialises");
    line.push(b'\n');
    w.write_all(&line)
        .and_then(|_| w.flush())
        .map_err(|_| CodexError::Closed)
}

/// The exchange itself, over any pair of pipes.
pub fn read_limits<R: Read + Send + 'static, W: Write>(
    from: R,
    to: &mut W,
    timeout: Duration,
) -> Result<Snapshot, CodexError> {
    let deadline = Instant::now() + timeout;
    let rx = lines(from);
    send(
        to,
        &json!({"method": "initialize", "id": 0, "params": {"clientInfo": {
            "name": "display_hub_hubctl", "title": "Display Hub PC helper", "version": env!("CARGO_PKG_VERSION")
        }}}),
    )?;
    response(&rx, 0, deadline)?;
    send(to, &json!({"method": "initialized", "params": {}}))?;
    send(to, &json!({"method": "account/rateLimits/read", "id": 1}))?;
    let result = response(&rx, 1, deadline)?;
    serde_json::from_value::<ReadResponse>(result)
        .map(|r| r.rate_limits)
        .map_err(|e| CodexError::Unexpected(e.to_string()))
}

/// How to start Codex: `--codex <path>`, or `codex` on PATH. On Windows the npm install is a
/// `codex.cmd` shim, which only `cmd.exe` can run, with fixed arguments.
fn command(path: Option<&std::path::Path>) -> Command {
    if let Some(p) = path {
        let mut c = Command::new(p);
        c.arg("app-server");
        return c;
    }
    #[cfg(windows)]
    {
        let on_path = std::env::var_os("PATH")
            .map(|p| std::env::split_paths(&p).any(|d| d.join("codex.exe").is_file()))
            .unwrap_or(false);
        if !on_path {
            let mut c = Command::new("cmd.exe");
            c.args(["/d", "/c", "codex", "app-server"]);
            return c;
        }
    }
    let mut c = Command::new("codex");
    c.arg("app-server");
    c
}

/// Starts `codex app-server`, reads the limits and stops it.
pub fn run(path: Option<&std::path::Path>) -> Result<Snapshot, CodexError> {
    let mut cmd = command(path);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = cmd.spawn().map_err(|e| CodexError::Start(e.to_string()))?;
    let stdout = child.stdout.take().expect("piped");
    let mut stdin = child.stdin.take().expect("piped");
    let result = read_limits(stdout, &mut stdin, TIMEOUT);
    drop(stdin);
    // It should exit once its input closes; make sure.
    let stop_by = Instant::now() + Duration::from_secs(3);
    while Instant::now() < stop_by {
        if let Ok(Some(_)) = child.try_wait() {
            return result;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in for `codex app-server`: answers on a pipe as the documented protocol does.
    fn fake(
        answer: Value,
    ) -> (
        std::io::PipeReader,
        std::io::PipeWriter,
        std::thread::JoinHandle<Vec<Value>>,
    ) {
        let (from_server, mut server_out) = std::io::pipe().unwrap();
        let (server_in, to_server) = std::io::pipe().unwrap();
        let t = std::thread::spawn(move || {
            let mut seen = Vec::new();
            for line in BufReader::new(server_in).lines() {
                let msg: Value = serde_json::from_str(&line.unwrap()).unwrap();
                let reply = match (msg.get("method").and_then(Value::as_str), msg.get("id")) {
                    (Some("initialize"), Some(id)) => {
                        // A notification first, which must be skipped.
                        writeln!(server_out, "{}", json!({"method": "codex/event", "params": {}})).unwrap();
                        Some(json!({"id": id, "result": {"userAgent": "codex/1"}}))
                    }
                    (Some("account/rateLimits/read"), Some(id)) => {
                        let mut a = answer.clone();
                        a["id"] = id.clone();
                        Some(a)
                    }
                    _ => None,
                };
                seen.push(msg);
                if let Some(r) = reply {
                    writeln!(server_out, "{r}").unwrap();
                }
            }
            seen
        });
        (from_server, to_server, t)
    }

    #[test]
    fn the_documented_exchange_gives_the_limits() {
        let (from, mut to, server) = fake(json!({"result": {"rateLimits": {
            "limitId": "codex",
            "primary": {"usedPercent": 25, "windowDurationMins": 300, "resetsAt": 1730947200},
            "secondary": {"usedPercent": 61, "windowDurationMins": 10080, "resetsAt": 1731384000},
            "planType": "plus", "credits": null, "somethingNew": 1
        }, "rateLimitsByLimitId": {}}}));
        let s = read_limits(from, &mut to, Duration::from_secs(5)).unwrap();
        drop(to);
        let seen = server.join().unwrap();
        assert_eq!(
            seen.iter()
                .filter_map(|m| m.get("method").and_then(Value::as_str))
                .collect::<Vec<_>>(),
            ["initialize", "initialized", "account/rateLimits/read"]
        );
        assert_eq!(s.plan_type.as_deref(), Some("plus"));
        let r = report(&s, Timestamp::parse("2026-11-11T01:00:00Z").unwrap()).unwrap();
        assert_eq!(r.service, Service::ChatgptPlus);
        assert_eq!(
            (r.metrics[0].window, r.metrics[0].used_percent),
            (Window::FiveHour, Some(25.0))
        );
        assert_eq!(
            (r.metrics[1].window, r.metrics[1].used_percent),
            (Window::Weekly, Some(61.0))
        );
        assert_eq!(r.metrics[1].resets_at.unwrap().unix(), 1_731_384_000);
    }

    #[test]
    fn windows_of_unknown_length_are_left_out() {
        let s = Snapshot {
            primary: Some(LimitWindow {
                used_percent: 25.0,
                window_duration_mins: Some(15),
                resets_at: None,
            }),
            secondary: Some(LimitWindow {
                used_percent: 9.0,
                window_duration_mins: None,
                resets_at: None,
            }),
            plan_type: None,
        };
        assert_eq!(report(&s, Timestamp::from_unix(0).unwrap()), None);
    }

    #[test]
    fn errors_are_explained() {
        let (from, mut to, _s) = fake(
            json!({"error": {"code": -32600, "message": "chatgpt authentication required to read rate limits"}}),
        );
        assert_eq!(
            read_limits(from, &mut to, Duration::from_secs(5)).unwrap_err(),
            CodexError::NotSignedIn
        );

        // Nothing answers.
        let (from, _keep) = std::io::pipe().unwrap();
        let (_r, mut to) = std::io::pipe().unwrap();
        assert_eq!(
            read_limits(from, &mut to, Duration::from_millis(200)).unwrap_err(),
            CodexError::Timeout(30)
        );
    }
}
