//! `{key}` placeholders in page titles and feeds.

/// A piece of a title or feed: literal text, or a placeholder.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Piece {
    Text(String),
    /// The first of these answers that isn't blank, as `{a|b.label}`.
    Slot(Vec<Ref>),
}

/// One answer a placeholder names: `key`, or `key.label` for the chosen option's label.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Ref {
    pub key: String,
    pub label: bool,
}

/// Splits text into literal text and placeholders. Braces are only for placeholders.
pub(crate) fn parse(s: &str) -> Result<Vec<Piece>, String> {
    let mut out = Vec::new();
    let mut rest = s;
    while let Some(open) = rest.find(['{', '}']) {
        if rest.as_bytes()[open] == b'}' {
            return Err(format!("{s:?} has a }} with no {{ before it."));
        }
        if open > 0 {
            out.push(Piece::Text(rest[..open].to_string()));
        }
        let after = &rest[open + 1..];
        let close = after
            .find('}')
            .ok_or_else(|| format!("{s:?} has a {{ with no }} after it."))?;
        let inner = &after[..close];
        if inner.contains('{') {
            return Err(format!("{s:?} has a {{ inside a placeholder."));
        }
        let refs = inner
            .split('|')
            .map(|r| {
                let (key, label) = match r.trim().strip_suffix(".label") {
                    Some(k) => (k, true),
                    None => (r.trim(), false),
                };
                if key.is_empty() || key.contains('.') {
                    return Err(format!(
                        "{s:?} has a placeholder {{{inner}}} that doesn't name a setting. Write {{key}}, {{key.label}} or {{key|other}}."
                    ));
                }
                Ok(Ref {
                    key: key.to_string(),
                    label,
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        out.push(Piece::Slot(refs));
        rest = &after[close + 1..];
    }
    if !rest.is_empty() {
        out.push(Piece::Text(rest.to_string()));
    }
    Ok(out)
}

/// Whether text holds any placeholder.
pub(crate) fn has_slots(pieces: &[Piece]) -> bool {
    pieces.iter().any(|p| matches!(p, Piece::Slot(_)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(key: &str, label: bool) -> Ref {
        Ref {
            key: key.into(),
            label,
        }
    }

    #[test]
    fn placeholders_name_answers_labels_and_fallbacks() {
        assert_eq!(parse("Plain").unwrap(), [Piece::Text("Plain".into())]);
        assert_eq!(
            parse("Garden {zone} - {title|service.label}").unwrap(),
            [
                Piece::Text("Garden ".into()),
                Piece::Slot(vec![r("zone", false)]),
                Piece::Text(" - ".into()),
                Piece::Slot(vec![r("title", false), r("service", true)]),
            ]
        );
        assert_eq!(parse("").unwrap(), []);
        for bad in ["{", "}", "a{b", "a}b", "{}", "{a|}", "{{a}}", "{a.b}", "{.label}"] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }
}
