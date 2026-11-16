//! Draws what the display would show, as text. One block per page, in the hub's order.

use hub_core::{short_duration, Freshness};
use hub_protocol::{BigNumber, CantShow, DataQuality, List, Meter, Page, PageContent, Timestamp};

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
        out.push("No pages. Add some in the desktop manager.".into());
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
    if let Err(cant) = drawable(page) {
        // The standard card: the title above, and nothing else about the page.
        out.extend(cant.card_lines().iter().map(|l| format!("  {l}")));
        return out;
    }
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
    let Ok(Some(content)) = drawable(page) else {
        return out; // Handled above.
    };
    // Reset countdowns run on the hub's clock, plus the time this display has held the snapshot.
    let now_on_hub = generated_at.plus_seconds(held_for);
    match &content {
        PageContent::Meter(m) => draw_meter(m, now_on_hub, &mut out),
        PageContent::BigNumber(b) => draw_big_number(b, &mut out),
        PageContent::Text(t) => out.extend(wrap(&t.text, WIDTH - 2).into_iter().map(|l| format!("  {l}"))),
        PageContent::List(l) => draw_list(l, &mut out),
        PageContent::Custom { .. } => unreachable!("drawable turns custom kinds into cards"),
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

/// What this display can draw for a page. It knows the four ready-made shapes and no custom kinds,
/// so a custom kind is a card, as it is on any display whose firmware doesn't know it.
pub fn drawable(page: &Page) -> Result<Option<PageContent>, CantShow> {
    match page.content()? {
        Some(PageContent::Custom { .. }) => Err(CantShow::UnknownKind),
        other => Ok(other),
    }
}

fn draw_meter(m: &Meter, now_on_hub: Timestamp, out: &mut Vec<String>) {
    for it in &m.items {
        out.push(format!("  {}", it.label));
        let value = match it.percent {
            Some(p) => format!("{:>5}% {}", format_percent(p), bar(p)),
            None => format!("{:>6} {}", "--", "unknown"),
        };
        let reset = match it.resets_at {
            Some(r) if r > now_on_hub => {
                format!("  resets in {}", short_duration(now_on_hub.seconds_until(r)))
            }
            Some(_) => "  reset due".into(),
            None => String::new(),
        };
        out.push(format!("    {value}{reset}"));
    }
}

/// The figure as drawn: `decimals` places, or a dash when unknown. Never 0 for unknown.
pub fn big_number_text(b: &BigNumber) -> String {
    match b.value {
        Some(v) => format!("{:.*}", b.decimals.unwrap_or(0) as usize, v),
        None => "-".into(),
    }
}

fn draw_big_number(b: &BigNumber, out: &mut Vec<String>) {
    if let Some(c) = &b.caption {
        out.push(format!("  {c}"));
    }
    let unit = b.unit.as_deref().map(|u| format!(" {u}")).unwrap_or_default();
    out.push(format!("    {}{unit}", big_number_text(b)));
}

fn draw_list(l: &List, out: &mut Vec<String>) {
    if l.items.is_empty() {
        out.push("  (nothing listed)".into());
    }
    for it in &l.items {
        match &it.detail {
            Some(d) => out.push(format!("  - {} · {d}", it.text)),
            None => out.push(format!("  - {}", it.text)),
        }
    }
}

/// Breaks text into lines of at most `width` characters, at spaces where it can.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split(' ') {
        let (have, add) = (line.chars().count(), word.chars().count());
        if have > 0 && have + 1 + add > width {
            lines.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
        while line.chars().count() > width {
            let cut: String = line.chars().take(width).collect();
            let rest: String = line.chars().skip(width).collect();
            lines.push(cut);
            line = rest;
        }
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
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
