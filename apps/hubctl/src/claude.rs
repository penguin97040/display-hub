//! Claude Code's status line (https://code.claude.com/docs/en/statusline).
//!
//! Claude Code runs a status line command after each assistant message, passing JSON on stdin. For
//! claude.ai Pro and Max subscribers, after the first response in a session, that JSON includes
//! `rate_limits.five_hour` and `rate_limits.seven_day`, each with `used_percentage` (0-100) and
//! `resets_at` (Unix seconds). Either window may be missing. Claude Code cancels a status line command
//! that is still running when the next update comes, so `hubctl claude-statusline` prints at once and
//! leaves sending to a separate process (`send-pending`), at most once a minute.

use hub_core::api::{ManualMetric, UsageReport};
use hub_core::{Service, Window};
use hub_protocol::Timestamp;
use serde::{Deserialize, Serialize};

/// The most stdin the helper reads from Claude Code.
pub const MAX_INPUT_BYTES: usize = 256 * 1024;
/// At most one send a minute, and one every five minutes even when nothing changed (so the page
/// doesn't go stale while Claude Code is in use).
pub const MIN_INTERVAL_SECONDS: i64 = 60;
pub const REFRESH_SECONDS: i64 = 300;

#[derive(Debug, Default, Deserialize)]
struct Input {
    model: Option<Model>,
    rate_limits: Option<RateLimits>,
}

#[derive(Debug, Deserialize)]
struct Model {
    display_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RateLimits {
    five_hour: Option<LimitWindow>,
    seven_day: Option<LimitWindow>,
}

#[derive(Debug, Deserialize)]
struct LimitWindow {
    used_percentage: Option<f64>,
    resets_at: Option<i64>,
}

/// What a status line update said.
#[derive(Debug, Default, PartialEq)]
pub struct StatusUpdate {
    pub model: Option<String>,
    pub metrics: Vec<ManualMetric>,
}

/// Reads Claude Code's JSON. Anything unreadable gives an empty update, never an error: the status
/// line must always print something.
pub fn parse(json: &str) -> StatusUpdate {
    let input: Input = serde_json::from_str(json).unwrap_or_default();
    let model = input
        .model
        .and_then(|m| m.display_name)
        .map(|n| clean(&n))
        .filter(|n| !n.is_empty());
    let mut metrics = Vec::new();
    if let Some(r) = input.rate_limits {
        for (window, w) in [(Window::FiveHour, r.five_hour), (Window::Weekly, r.seven_day)] {
            let Some(w) = w else { continue };
            // A share outside 0-100 isn't trusted; it becomes unknown rather than being guessed at.
            let used_percent = w
                .used_percentage
                .filter(|p| p.is_finite() && (0.0..=100.0).contains(p));
            let resets_at = w.resets_at.and_then(Timestamp::from_unix);
            if used_percent.is_some() || resets_at.is_some() {
                metrics.push(ManualMetric {
                    window,
                    used_percent,
                    resets_at,
                });
            }
        }
    }
    StatusUpdate { model, metrics }
}

fn clean(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_control())
        .take(40)
        .collect::<String>()
        .trim()
        .to_string()
}

/// The status line itself: `Opus 4.7 · 5h 42% · 7d 18%`.
pub fn line(update: &StatusUpdate) -> String {
    let mut parts: Vec<String> = update.model.iter().cloned().collect();
    for m in &update.metrics {
        let name = match m.window {
            Window::FiveHour => "5h",
            Window::Weekly => "7d",
            Window::Monthly => "30d",
        };
        if let Some(p) = m.used_percent {
            parts.push(format!("{name} {p:.0}%"));
        }
    }
    parts.join(" · ")
}

/// The report for the hub, if Claude Code gave any figures.
pub fn report(update: &StatusUpdate, now: Timestamp) -> Option<UsageReport> {
    (!update.metrics.is_empty()).then(|| UsageReport {
        service: Service::ClaudeMax,
        collected_at: now,
        metrics: update.metrics.clone(),
    })
}

/// When figures were last handed to the sender, and what they were.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sent {
    pub at: i64,
    pub digest: String,
}

/// What identifies a set of figures, ignoring the time they were read.
pub fn digest(report: &UsageReport) -> String {
    serde_json::to_string(&report.metrics).expect("serialises")
}

/// Whether to send now: never twice within a minute; when the figures changed; or every five
/// minutes so the page stays fresh while Claude Code is in use.
pub fn due(last: Option<&Sent>, digest: &str, now: i64) -> bool {
    match last {
        None => true,
        Some(s) => {
            let since = now - s.at;
            since >= MIN_INTERVAL_SECONDS && (s.digest != digest || since >= REFRESH_SECONDS) || since < 0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The shape of the documented example, trimmed.
    const SAMPLE: &str = r#"{
      "session_id": "abc", "version": "2.1.90",
      "model": { "id": "claude-opus-4-7", "display_name": "Opus 4.7" },
      "context_window": { "used_percentage": 12 },
      "rate_limits": {
        "five_hour": { "used_percentage": 23.5, "resets_at": 1738425600 },
        "seven_day": { "used_percentage": 41.2, "resets_at": 1738857600 }
      }
    }"#;

    #[test]
    fn documented_fields_become_a_report() {
        let u = parse(SAMPLE);
        assert_eq!(u.model.as_deref(), Some("Opus 4.7"));
        assert_eq!(line(&u), "Opus 4.7 · 5h 24% · 7d 41%");
        let now = Timestamp::parse("2026-11-11T01:00:00Z").unwrap();
        let r = report(&u, now).unwrap();
        assert_eq!(r.service, Service::ClaudeMax);
        assert_eq!(r.metrics[0].window, Window::FiveHour);
        assert_eq!(r.metrics[0].used_percent, Some(23.5));
        assert_eq!(r.metrics[0].resets_at.unwrap().unix(), 1_738_425_600);
        assert_eq!(r.metrics[1].window, Window::Weekly);
    }

    #[test]
    fn missing_or_odd_figures_are_left_out_never_guessed() {
        // API-key users and the start of a session: no rate_limits at all.
        let u = parse(r#"{"model":{"display_name":"Sonnet"}}"#);
        assert_eq!(
            (line(&u).as_str(), report(&u, Timestamp::from_unix(0).unwrap())),
            ("Sonnet", None)
        );
        // One window only, and an impossible share.
        let u = parse(r#"{"rate_limits":{"seven_day":{"used_percentage":140,"resets_at":1738857600}}}"#);
        assert_eq!(u.metrics.len(), 1);
        assert_eq!(u.metrics[0].used_percent, None);
        assert_eq!(line(&u), "");
        // Not JSON at all: still a (blank) status line, no panic.
        assert_eq!(parse("\u{1b}[31mnope"), StatusUpdate::default());
    }

    #[test]
    fn sending_is_throttled() {
        let d = "x";
        assert!(due(None, d, 1000));
        let s = Sent {
            at: 1000,
            digest: d.into(),
        };
        assert!(!due(Some(&s), "y", 1030), "within a minute");
        assert!(due(Some(&s), "y", 1060), "changed, a minute later");
        assert!(!due(Some(&s), d, 1200), "unchanged");
        assert!(due(Some(&s), d, 1300), "unchanged, but five minutes on");
        assert!(due(Some(&s), d, 900), "the clock went backwards");
    }
}
