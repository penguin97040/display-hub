//! zvx.nz design tokens (AGENT_STYLE_GUIDE.md v3.1), as 0xRRGGBB and px at 1x.
//!
//! The values are written out here so the app has no build step, and a test checks every one
//! against `design/tokens.json`, a verbatim copy of zvx.nz's `brand/tokens.json`.

/// Page and panel fill.
pub const BLACK: u32 = 0x000000;
/// 1px rules and panel edges. Never text (1.5:1).
pub const LINE: u32 = 0x2c2c2c;
/// Labels and secondary text (6.5:1 on black).
pub const MUTED: u32 = 0x8f8f8f;
/// Primary text (18.8:1 on black).
pub const TEXT: u32 = 0xf2f2f2;
/// The single accent. At most 5% of any view at rest; black text on it, never white.
pub const BEAK: u32 = 0xdb7700;
/// Static pattern: idle cells and every eighth cell. Never text.
pub const FIELD_REST: u32 = 0x2c2c2c;
pub const FIELD_NODE: u32 = 0x4d4d4d;

pub const FONT_BODY: &str = "Geist";
pub const FONT_MONO: &str = "Geist Mono";

/// Static pattern pitch: a 2px square every 14px, a 4px square every 112px.
pub const PATTERN_PITCH: f32 = 14.0;
pub const PATTERN_NODE_PITCH: f32 = 112.0;

/// Label: 0.7rem mono, 0.1em tracking, upper case.
pub const LABEL_SIZE: f32 = 11.2;
pub const LABEL_TRACKING_EM: f32 = 0.1;
/// Button text: 0.8rem mono, 0.08em tracking, upper case.
pub const BUTTON_SIZE: f32 = 12.8;
pub const BUTTON_TRACKING_EM: f32 = 0.08;
pub const BODY_SIZE: f32 = 17.0;
pub const H2_SIZE: f32 = 24.0;
/// Minimum touch target.
pub const TARGET_MIN: f32 = 44.0;
pub const PANEL_GAP: f32 = 16.0;
/// The nominal value; layouts use the guide's `clamp()` (`ui::Metrics`). Checked against the tokens.
#[allow(dead_code)]
pub const PANEL_PAD: f32 = 24.0;
/// Corner tick length on panels (0.75rem) and its stroke.
pub const TICK: f32 = 12.0;
pub const TICK_STROKE: f32 = 2.0;

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens() -> serde_json::Value {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../design/tokens.json");
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    fn colour(t: &serde_json::Value, path: &[&str]) -> u32 {
        let mut v = &t["color"];
        for p in path {
            v = &v[p];
        }
        let hex = v["$value"].as_str().unwrap().trim_start_matches('#');
        u32::from_str_radix(hex, 16).unwrap()
    }

    fn px(t: &serde_json::Value, group: &str, name: &str) -> f64 {
        t[group][name]["$value"]["value"].as_f64().unwrap()
    }

    #[test]
    fn colours_match_the_brand_tokens() {
        let t = tokens();
        assert_eq!(colour(&t, &["black"]), BLACK);
        assert_eq!(colour(&t, &["line"]), LINE);
        assert_eq!(colour(&t, &["muted"]), MUTED);
        assert_eq!(colour(&t, &["text"]), TEXT);
        assert_eq!(colour(&t, &["beak"]), BEAK);
        assert_eq!(colour(&t, &["field", "rest"]), FIELD_REST);
        assert_eq!(colour(&t, &["field", "node"]), FIELD_NODE);
    }

    #[test]
    fn type_and_sizes_match_the_brand_tokens() {
        let t = tokens();
        assert_eq!(t["font"]["family"]["body"]["$value"][0], FONT_BODY);
        assert_eq!(t["font"]["family"]["mono"]["$value"][0], FONT_MONO);
        let ty = &t["typography"];
        assert_eq!(
            ty["label"]["$value"]["fontSize"]["value"].as_f64().unwrap() as f32,
            LABEL_SIZE
        );
        assert_eq!(
            ty["button"]["$value"]["fontSize"]["value"].as_f64().unwrap() as f32,
            BUTTON_SIZE
        );
        assert_eq!(
            ty["body"]["$value"]["fontSize"]["value"].as_f64().unwrap() as f32,
            BODY_SIZE
        );
        assert_eq!(
            ty["h2"]["$value"]["fontSize"]["value"].as_f64().unwrap() as f32,
            H2_SIZE
        );
        assert_eq!(px(&t, "size", "fieldPitch") as f32, PATTERN_PITCH);
        assert_eq!(px(&t, "size", "panelGap") as f32, PANEL_GAP);
        assert_eq!(px(&t, "size", "panelPad") as f32, PANEL_PAD);
        assert_eq!(px(&t, "size", "touchTargetIOS") as f32, TARGET_MIN);
        assert_eq!(px(&t, "size", "cornerRadius"), 0.0);
    }
}
