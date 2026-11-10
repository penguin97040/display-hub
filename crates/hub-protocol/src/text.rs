//! Free text on the wire: titles, labels and notes. Plain, short, normalised and printable.

use unicode_normalization::{is_nfc, UnicodeNormalization};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextProblem {
    Empty,
    TooLong,
    ControlCharacter,
    NotNormalised,
}

/// Checks text a hub is about to send. `max` counts characters.
pub fn check_text(s: &str, max: usize) -> Result<(), TextProblem> {
    if s.is_empty() {
        return Err(TextProblem::Empty);
    }
    if s.chars().any(is_forbidden) {
        return Err(TextProblem::ControlCharacter);
    }
    if !is_nfc(s) || s.trim() != s {
        return Err(TextProblem::NotNormalised);
    }
    if s.chars().count() > max {
        return Err(TextProblem::TooLong);
    }
    Ok(())
}

/// Turns user or provider text into something [`check_text`] accepts: NFC, control and format
/// characters removed, runs of white space collapsed to one space, trimmed, and cut to `max`
/// characters with an ellipsis. `None` if nothing printable is left.
pub fn sanitise_text(s: &str, max: usize) -> Option<String> {
    let mut out = String::with_capacity(s.len().min(max * 4));
    let mut space = false;
    for c in s.nfc() {
        if c.is_whitespace() {
            space = !out.is_empty();
            continue;
        }
        if is_forbidden(c) {
            continue;
        }
        if space {
            out.push(' ');
            space = false;
        }
        out.push(c);
    }
    if out.is_empty() {
        return None;
    }
    if out.chars().count() > max {
        let mut cut: String = out.chars().take(max.saturating_sub(1)).collect();
        cut.truncate(cut.trim_end().len());
        cut.push('…');
        out = cut;
    }
    // Cutting can't denormalise NFC text that ends at a character boundary, but check anyway.
    let out: String = out.nfc().collect();
    check_text(&out, max).ok().map(|_| out)
}

/// Control characters, and invisible format characters that could disguise text (bidi overrides,
/// zero-width joiners and the like).
fn is_forbidden(c: char) -> bool {
    c.is_control()
        || matches!(c, '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{2069}' | '\u{feff}' | '\u{fff9}'..='\u{fffb}')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_plain_text() {
        assert_eq!(check_text("Claude Max", 48), Ok(()));
        assert_eq!(check_text("Tēnā koe", 48), Ok(()));
    }

    #[test]
    fn rejects_bad_text() {
        assert_eq!(check_text("", 48), Err(TextProblem::Empty));
        assert_eq!(check_text("a\u{7}b", 48), Err(TextProblem::ControlCharacter));
        assert_eq!(check_text("a\u{202e}b", 48), Err(TextProblem::ControlCharacter));
        assert_eq!(check_text(" padded", 48), Err(TextProblem::NotNormalised));
        assert_eq!(check_text("e\u{301}", 48), Err(TextProblem::NotNormalised));
        assert_eq!(check_text(&"x".repeat(49), 48), Err(TextProblem::TooLong));
    }

    #[test]
    fn sanitises_into_valid_text() {
        assert_eq!(
            sanitise_text("  Claude \n\t Max\u{0} ", 48).as_deref(),
            Some("Claude Max")
        );
        assert_eq!(sanitise_text("e\u{301}", 48).as_deref(), Some("é"));
        assert_eq!(sanitise_text("\u{202e}\u{200b}", 48), None);
        let long = sanitise_text(&"word ".repeat(30), 20).unwrap();
        assert_eq!(long.chars().count(), 20);
        assert!(long.ends_with('…'));
        assert_eq!(check_text(&long, 20), Ok(()));
    }
}
