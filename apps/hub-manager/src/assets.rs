//! Embedded assets: fonts and the few SVG marks. GPUI draws an SVG as a mask tinted with the
//! element's text colour, so the documents are drawn in black.

use std::borrow::Cow;

use gpui::{AssetSource, SharedString};

pub const GEIST_REGULAR: &[u8] = include_bytes!("../../../assets/fonts/Geist-Regular.ttf");
pub const GEIST_SEMIBOLD: &[u8] = include_bytes!("../../../assets/fonts/Geist-SemiBold.ttf");
pub const GEIST_MONO: &[u8] = include_bytes!("../../../assets/fonts/GeistMono-Regular.ttf");

/// The four stars of the Southern Cross, placed as on the New Zealand flag (zvx.nz footer mark).
const NZ_STARS: &str = r#"<polygon points="12.0,0.6 12.8,2.9 15.2,2.9 13.2,4.4 14.0,6.8 12.0,5.3 10.0,6.8 10.8,4.4 8.8,2.9 11.2,2.9"/>
<polygon points="4.5,10.1 5.3,12.4 7.7,12.4 5.7,13.9 6.5,16.3 4.5,14.8 2.5,16.3 3.3,13.9 1.3,12.4 3.7,12.4"/>
<polygon points="19.0,8.8 19.6,10.7 21.6,10.7 20.0,11.8 20.6,13.7 19.0,12.5 17.4,13.7 18.0,11.8 16.4,10.7 18.4,10.7"/>
<polygon points="12.0,21.7 12.9,24.3 15.6,24.3 13.4,25.9 14.2,28.6 12.0,27.0 9.8,28.6 10.6,25.9 8.4,24.3 11.1,24.3"/>"#;

/// (name, stroked path data). Square caps and mitred joins, as `.brand-mark` asks.
const STROKED: &[(&str, &str)] = &[
    // The app's mark: a small display on a stand, with a reading across it.
    ("mark", "M3 4h18v12H3zM12 16v4M8 20h8M6 12h4M6 9h8"),
    ("up", "m6 15 6-6 6 6"),
    ("down", "m6 9 6 6 6-6"),
];

pub struct Assets;

impl Assets {
    pub fn path(name: &str) -> SharedString {
        format!("marks/{name}.svg").into()
    }
}

impl AssetSource for Assets {
    fn load(&self, path: &str) -> gpui::Result<Option<Cow<'static, [u8]>>> {
        let Some(name) = path.strip_prefix("marks/").and_then(|p| p.strip_suffix(".svg")) else {
            return Ok(None);
        };
        let svg = if name == "nz" {
            format!(
                r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 30" fill="#000">{NZ_STARS}</svg>"##
            )
        } else {
            let Some((_, d)) = STROKED.iter().find(|(n, _)| *n == name) else {
                return Ok(None);
            };
            format!(
                r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#000" stroke-width="1.6" stroke-linecap="square" stroke-linejoin="miter"><path d="{d}"/></svg>"##
            )
        };
        Ok(Some(Cow::Owned(svg.into_bytes())))
    }

    fn list(&self, _path: &str) -> gpui::Result<Vec<SharedString>> {
        Ok(Vec::new())
    }
}
