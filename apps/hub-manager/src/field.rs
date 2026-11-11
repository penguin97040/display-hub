//! A single-line text field's state: text, caret, selection, clipboard and word movement.
//!
//! Plain data, so it is tested without a window. Adapted from md-viewer's find field. The caret is
//! drawn steady: the style guide allows no blinking (`ui::field`).

use unicode_segmentation::UnicodeSegmentation;
use zeroize::Zeroize;

#[derive(Default, Debug, PartialEq, Eq)]
pub struct FieldEdit {
    pub changed: bool,
    pub handled: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Field {
    pub text: String,
    pub caret: usize,
    pub anchor: usize,
    /// The most bytes the field holds. 0 means no limit.
    pub max_len: usize,
    /// Shown as dots, and never copied out.
    pub secret: bool,
}

impl Field {
    pub fn new(max_len: usize) -> Self {
        Self {
            text: String::new(),
            caret: 0,
            anchor: 0,
            max_len,
            secret: false,
        }
    }

    pub fn secret(max_len: usize) -> Self {
        Self {
            text: String::new(),
            caret: 0,
            anchor: 0,
            max_len,
            secret: true,
        }
    }

    pub fn selection(&self) -> (usize, usize) {
        (self.caret.min(self.anchor), self.caret.max(self.anchor))
    }

    pub fn selected(&self) -> &str {
        let (a, b) = self.selection();
        &self.text[a..b]
    }

    pub fn select_all(&mut self) {
        self.anchor = 0;
        self.caret = self.text.len();
    }

    /// Replaces the text, with the caret at the end.
    pub fn set(&mut self, t: &str) {
        self.clear();
        self.insert(t);
    }

    /// Empties the field, wiping the old text from memory.
    pub fn clear(&mut self) {
        self.text.zeroize();
        self.caret = 0;
        self.anchor = 0;
    }

    pub fn trimmed(&self) -> &str {
        self.text.trim()
    }

    /// What the field shows: the text, or one dot per character for a secret.
    pub fn shown(&self) -> String {
        if self.secret {
            "•".repeat(self.text.chars().count())
        } else {
            self.text.clone()
        }
    }

    /// Maps a byte offset in the text to one in [`Field::shown`].
    pub fn shown_offset(&self, at: usize) -> usize {
        if self.secret {
            self.text[..at].chars().count() * '•'.len_utf8()
        } else {
            at
        }
    }

    /// Maps a byte offset in [`Field::shown`] back to the text.
    pub fn text_offset(&self, shown_at: usize) -> usize {
        if self.secret {
            let n = shown_at / '•'.len_utf8();
            self.text
                .char_indices()
                .nth(n)
                .map_or(self.text.len(), |(i, _)| i)
        } else {
            shown_at.min(self.text.len())
        }
    }

    pub fn insert(&mut self, t: &str) {
        let (a, b) = self.selection();
        // One line: pasted line breaks and tabs become spaces, and control characters go.
        let mut clean: String = t
            .chars()
            .map(|c| if matches!(c, '\n' | '\r' | '\t') { ' ' } else { c })
            .filter(|c| !c.is_control())
            .collect();
        if self.max_len > 0 {
            let room = self.max_len.saturating_sub(self.text.len() - (b - a));
            if clean.len() > room {
                let mut cut = room;
                while !clean.is_char_boundary(cut) {
                    cut -= 1;
                }
                clean.truncate(cut);
            }
        }
        self.text.replace_range(a..b, &clean);
        self.caret = a + clean.len();
        self.anchor = self.caret;
    }

    fn prev(&self, at: usize) -> usize {
        self.text[..at]
            .grapheme_indices(true)
            .next_back()
            .map_or(0, |(i, _)| i)
    }

    fn next(&self, at: usize) -> usize {
        self.text[at..]
            .graphemes(true)
            .next()
            .map_or(at, |g| at + g.len())
    }

    fn prev_word(&self, at: usize) -> usize {
        let mut last = 0;
        for (i, w) in self.text[..at].split_word_bound_indices() {
            if !w.trim().is_empty() {
                last = i;
            }
        }
        last
    }

    fn next_word(&self, at: usize) -> usize {
        let mut seen = false;
        for (i, w) in self.text[at..].split_word_bound_indices() {
            if w.trim().is_empty() {
                if seen {
                    return at + i;
                }
            } else {
                seen = true;
            }
        }
        self.text.len()
    }

    /// Selects the word around a position, for a double-click.
    pub fn select_word(&mut self, at: usize) {
        if self.secret {
            return self.select_all();
        }
        for (i, w) in self.text.split_word_bound_indices() {
            if at >= i && at <= i + w.len() {
                self.anchor = i;
                self.caret = i + w.len();
                return;
            }
        }
    }

    /// Applies a key. `word` is the word-movement modifier, `command` the shortcut modifier.
    /// `clipboard(Some(text))` writes; `clipboard(None)` reads.
    pub fn key(
        &mut self,
        key: &str,
        shift: bool,
        word: bool,
        command: bool,
        typed: Option<&str>,
        clipboard: &mut dyn FnMut(Option<String>) -> Option<String>,
    ) -> FieldEdit {
        let mut e = FieldEdit {
            changed: false,
            handled: true,
        };
        let has_sel = self.caret != self.anchor;
        // A secret moves by whole words as if it were one.
        let word = word && !self.secret;
        let target = match key {
            "left" if !shift && has_sel && !word => Some(self.selection().0),
            "right" if !shift && has_sel && !word => Some(self.selection().1),
            "left" => Some(if word {
                self.prev_word(self.caret)
            } else {
                self.prev(self.caret)
            }),
            "right" => Some(if word {
                self.next_word(self.caret)
            } else {
                self.next(self.caret)
            }),
            "home" => Some(0),
            "end" => Some(self.text.len()),
            _ => None,
        };
        if let Some(t) = target {
            self.caret = t;
            if !shift {
                self.anchor = t;
            }
            return e;
        }
        match key {
            "backspace" | "delete" => {
                if has_sel {
                    self.insert("");
                } else if key == "backspace" && self.caret > 0 {
                    let p = if word {
                        self.prev_word(self.caret)
                    } else {
                        self.prev(self.caret)
                    };
                    self.text.replace_range(p..self.caret, "");
                    self.caret = p;
                    self.anchor = p;
                } else if key == "delete" && self.caret < self.text.len() {
                    let n = if word {
                        self.next_word(self.caret)
                    } else {
                        self.next(self.caret)
                    };
                    self.text.replace_range(self.caret..n, "");
                }
                e.changed = true;
            }
            "a" if command => self.select_all(),
            "c" if command => {
                if has_sel && !self.secret {
                    clipboard(Some(self.selected().to_string()));
                }
            }
            "x" if command => {
                if has_sel && !self.secret {
                    clipboard(Some(self.selected().to_string()));
                    self.insert("");
                    e.changed = true;
                }
            }
            "v" if command => {
                if let Some(t) = clipboard(None) {
                    self.insert(&t);
                    e.changed = true;
                }
            }
            _ => match typed {
                Some(t) if !command && !t.is_empty() => {
                    self.insert(t);
                    e.changed = true;
                }
                _ => e.handled = false,
            },
        }
        e
    }
}

impl Drop for Field {
    fn drop(&mut self) {
        if self.secret {
            self.text.zeroize();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(
        f: &mut Field,
        key: &str,
        shift: bool,
        word: bool,
        command: bool,
        typed: Option<&str>,
    ) -> FieldEdit {
        let mut clip = |w: Option<String>| -> Option<String> {
            assert!(
                w.is_none() || !f_is_secret_marker(&w),
                "secrets never reach the clipboard"
            );
            Some("pasted\ntext".to_string())
        };
        f.key(key, shift, word, command, typed, &mut clip)
    }

    fn f_is_secret_marker(w: &Option<String>) -> bool {
        w.as_deref() == Some("hunter22")
    }

    #[test]
    fn typing_and_caret_movement() {
        let mut f = Field::default();
        for c in ["a", "b", "c"] {
            assert!(press(&mut f, c, false, false, false, Some(c)).changed);
        }
        assert_eq!((f.text.as_str(), f.caret), ("abc", 3));
        press(&mut f, "left", false, false, false, None);
        press(&mut f, "x", false, false, false, Some("x"));
        assert_eq!((f.text.as_str(), f.caret), ("abxc", 3));
        press(&mut f, "home", false, false, false, None);
        assert_eq!(f.caret, 0);
    }

    #[test]
    fn shift_selects_and_typing_replaces() {
        let mut f = Field::default();
        f.set("hello world");
        press(&mut f, "home", false, false, false, None);
        press(&mut f, "right", true, true, false, None);
        assert_eq!(f.selected(), "hello");
        press(&mut f, "J", false, false, false, Some("J"));
        assert_eq!(f.text, "J world");
    }

    #[test]
    fn graphemes_move_as_one_and_backspace_keeps_utf8_valid() {
        let mut f = Field::default();
        f.set("ae\u{301}😀");
        press(&mut f, "backspace", false, false, false, None);
        assert_eq!(f.text, "ae\u{301}");
        press(&mut f, "backspace", false, false, false, None);
        assert_eq!(f.text, "a");
    }

    #[test]
    fn paste_flattens_lines_and_respects_the_limit() {
        let mut f = Field::new(8);
        assert!(press(&mut f, "v", false, false, true, None).changed);
        assert_eq!(f.text, "pasted t");
        f.set("ā".repeat(10).as_str());
        assert_eq!(f.text, "āāāā", "cut on a character boundary");
    }

    #[test]
    fn secrets_show_as_dots_and_are_never_copied() {
        let mut f = Field::secret(63);
        f.set("hunter22");
        assert_eq!(f.shown(), "••••••••");
        f.select_all();
        press(&mut f, "c", false, false, true, None);
        press(&mut f, "x", false, false, true, None);
        assert_eq!(f.text, "hunter22", "cut does nothing");
        assert_eq!(f.text_offset(f.shown_offset(3)), 3);
        f.clear();
        assert!(f.text.is_empty());
    }

    #[test]
    fn word_keys() {
        let mut f = Field::default();
        f.set("one two three");
        press(&mut f, "backspace", false, true, false, None);
        assert_eq!(f.text, "one two ");
        f.select_word(1);
        assert_eq!(f.selected(), "one");
    }
}
