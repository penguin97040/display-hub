//! Draws what the display would show, as text. One block per page, in the hub's order.

use hub_core::{short_duration, Freshness};
use hub_protocol::{DataQuality, Page, Timestamp};

use crate::device::{Device, Link};

pub const WIDTH: usize = 48;
const BAR_CELLS: usize = 20;

/// `10/11/2026 20:00 UTC`.
pub fn nz_date_time(t: Timestamp) -> String {
    let d = t.get();
    format!(
        "{:02}/{:02}/{} {:02}:{:02} UTC",
        d.day(),
        u8::from(d.month()),
        d.year(),
        d.hour(),
        d.minute()
    )
}

fn rule(left: &str, right: &str) -> String {
    let used = left.chars().count() + right.chars().count();
    let fill = WIDTH.saturating_sub(used + 2).max(1);
    format!("{left} {} {right}", "-".repeat(fill))
}

fn bar(pct: f64) -> String {
    let filled = ((pct / 100.0) * BAR_CELLS as f64)
        .round()
        .clamp(0.0, BAR_CELLS as f64) as usize;
    format!("[{}{}]", "#".repeat(filled), ".".repeat(BAR_CELLS - filled))
}

/// The whole screen at `now`, by the display's clock.
pub fn screen(device: &Device, now: Timestamp) -> String {
    let mut out = Vec::new();
    out.push(rule("== DISPLAY", "368 x 448 =="));
    match &device.link {
        Link::NeverConnected => out.push("[STARTING] Connecting to the hub...".into()),
        Link::Online { last_sync } => out.push(format!("[ONLINE] Last sync {}", nz_date_time(*last_sync))),
        Link::Trouble {
            problem,
            since,
            failures,
        } => {
            out.push(format!("[OFFLINE] {}", problem.headline()));
            out.push(format!("  {}", problem.advice()));
            out.push(format!(
                "  Since {} ({} failed {})",
                nz_date_time(*since),
                failures,
                if *failures == 1 { "attempt" } else { "attempts" }
            ));
        }
    }
    let Some(held) = &device.held else {
        out.push("No data yet. The display shows pages once it has synced with the hub.".into());
        return out.join("\n") + "\n";
    };
    let held_for = held.received_at.seconds_until(now).max(0);
    if !matches!(device.link, Link::Online { .. }) {
        out.push(format!(
            "Showing saved data from {} ({} old)",
            nz_date_time(held.received_at),
            short_duration(held_for)
        ));
    }
    let snap = &held.snapshot;
    if snap.pages.is_empty() {
        out.push("No pages. Add subscriptions in the desktop manager.".into());
    }
    let total = snap.pages.len();
    for (i, page) in snap.pages.iter().enumerate() {
        out.extend(render_page(page, i + 1, total, snap.generated_at, held_for));
    }
    out.join("\n") + "\n"
}

fn render_page(page: &Page, n: usize, total: usize, generated_at: Timestamp, held_for: i64) -> Vec<String> {
    let mut out = Vec::new();
    let fresh = Freshness::of(page, generated_at, held_for);
    let mut tag = page.data_quality.label().to_string();
    if fresh.is_stale() && page.data_quality.shows_figures() {
        tag = format!("STALE {tag}");
    }
    out.push(rule(
        &format!("-- Page {n}/{total}: {}", page.title),
        &format!("{tag} --"),
    ));
    if !page.data_quality.shows_figures() {
        out.push(format!(
            "  {}",
            page.note.as_deref().unwrap_or("No data available")
        ));
        if page.data_quality == DataQuality::Unrecognised {
            out.push("  (unrecognised data quality from a newer hub, shown as unavailable)".into());
        }
        return out;
    }
    // Reset countdowns run on the hub's clock, plus the time this display has held the snapshot.
    let now_on_hub = generated_at.plus_seconds(held_for);
    for m in &page.metrics {
        out.push(format!("  {}", m.label));
        let value = match m.used_percent {
            Some(p) => format!("{:>5}% {}", format_percent(p), bar(p)),
            None => format!("{:>6} {}", "--", "unknown"),
        };
        let reset = match m.resets_at {
            Some(r) if r > now_on_hub => {
                format!("  resets in {}", short_duration(now_on_hub.seconds_until(r)))
            }
            Some(_) => "  reset due".into(),
            None => String::new(),
        };
        out.push(format!("    {value}{reset}"));
    }
    let age = match fresh {
        Freshness::Never => "Never updated".to_string(),
        Freshness::Fresh { age_seconds } => format!("Updated {} ago", short_duration(age_seconds)),
        Freshness::Stale { age_seconds } => format!("STALE: updated {} ago", short_duration(age_seconds)),
    };
    match &page.note {
        Some(note) => out.push(format!("  {age} · {note}")),
        None => out.push(format!("  {age}")),
    }
    out
}

fn format_percent(p: f64) -> String {
    if (p - p.round()).abs() < 0.05 {
        format!("{:.0}", p)
    } else {
        format!("{:.1}", p)
    }
}

/// How many pages a rendered screen shows.
pub fn count_pages(screen: &str) -> usize {
    screen.lines().filter(|l| l.starts_with("-- Page ")).count()
}
