//! Comparing firmware versions, as the hub and the app do ("Update available" in
//! `docs/firmware-updates.md`): `MAJOR.MINOR.PATCH` as three numbers, and with those equal, a version
//! with a pre-release tag is older than one without. Version text never decides what a display
//! accepts; the security counter does. This only orders what people are shown.

use std::cmp::Ordering;

/// A `MAJOR.MINOR.PATCH[-pre]` version, as the manifest rules allow.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Version<'a> {
    pub numbers: [u64; 3],
    /// The pre-release tag, without its `-`.
    pub pre: Option<&'a str>,
}

/// Reads a version. `None` if `s` isn't three numbers, optionally followed by `-` and a tag.
pub fn parse(s: &str) -> Option<Version<'_>> {
    let (core, pre) = match s.split_once('-') {
        Some((c, p)) if !p.is_empty() => (c, Some(p)),
        Some(_) => return None,
        None => (s, None),
    };
    let mut parts = core.split('.');
    let mut numbers = [0u64; 3];
    for n in &mut numbers {
        let p = parts.next()?;
        if p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        *n = p.parse().ok()?;
    }
    parts.next().is_none().then_some(Version { numbers, pre })
}

/// Whether `s` is a version with a pre-release tag.
pub fn is_pre_release(s: &str) -> bool {
    parse(s).is_some_and(|v| v.pre.is_some())
}

/// Compares two versions: three numbers, and with those equal, a version with a pre-release tag is
/// older than one without. Two tags compare part by part (split at `.`), numbers as numbers and
/// anything else as text, a number before text, and a shorter tag before a longer one it begins.
/// `None` if either isn't a version.
pub fn compare(a: &str, b: &str) -> Option<Ordering> {
    let (a, b) = (parse(a)?, parse(b)?);
    Some(a.numbers.cmp(&b.numbers).then_with(|| match (a.pre, b.pre) {
        (None, None) => Ordering::Equal,
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (Some(x), Some(y)) => {
            let mut xs = x.split('.');
            let mut ys = y.split('.');
            loop {
                match (xs.next(), ys.next()) {
                    (None, None) => break Ordering::Equal,
                    (None, Some(_)) => break Ordering::Less,
                    (Some(_), None) => break Ordering::Greater,
                    (Some(p), Some(q)) => {
                        let o = match (p.parse::<u64>(), q.parse::<u64>()) {
                            (Ok(m), Ok(n)) => m.cmp(&n),
                            (Ok(_), Err(_)) => Ordering::Less,
                            (Err(_), Ok(_)) => Ordering::Greater,
                            _ => p.cmp(q),
                        };
                        if o != Ordering::Equal {
                            break o;
                        }
                    }
                }
            }
        }
    }))
}

/// A total order for sorting or choosing the lowest: real versions by [`compare`], anything
/// unreadable before them, and unreadable ones by their text.
pub fn order(a: &str, b: &str) -> Ordering {
    match (parse(a).is_some(), parse(b).is_some()) {
        (true, true) => compare(a, b).unwrap_or(Ordering::Equal),
        (false, true) => Ordering::Less,
        (true, false) => Ordering::Greater,
        (false, false) => a.cmp(b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_as_three_numbers_with_pre_releases_older() {
        use Ordering::*;
        for (a, b, o) in [
            ("1.2.0", "1.10.0", Less),
            ("1.2.0", "1.2.0", Equal),
            ("2.0.0", "1.99.99", Greater),
            ("1.3.0-beta.1", "1.3.0", Less),
            ("1.3.0-beta.2", "1.3.0-beta.10", Less),
            ("1.3.0-beta", "1.3.0-beta.1", Less),
            ("1.3.0-1", "1.3.0-alpha", Less),
            ("0.0.0-simulator", "0.0.1", Less),
        ] {
            assert_eq!(compare(a, b), Some(o), "{a} {b}");
            assert_eq!(compare(b, a), Some(o.reverse()), "{b} {a}");
        }
        for bad in ["1.2", "1.2.3.4", "v1.2.3", "1.2.3-", "", "1..2"] {
            assert_eq!(compare(bad, "1.0.0"), None, "{bad}");
        }
    }

    #[test]
    fn unreadable_versions_sort_first_and_tags_are_spotted() {
        let mut v = vec!["1.0.0", "junk", "0.9.0", "1.0.0-rc.1", "also junk"];
        v.sort_by(|a, b| order(a, b));
        assert_eq!(v, ["also junk", "junk", "0.9.0", "1.0.0-rc.1", "1.0.0"]);
        assert!(is_pre_release("1.3.0-beta.1"));
        assert!(!is_pre_release("1.3.0"));
        assert!(!is_pre_release("not-a-version"));
    }
}
