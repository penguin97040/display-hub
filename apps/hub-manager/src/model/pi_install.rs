//! The one line that downloads the hub's files onto a Raspberry Pi, checks them and installs them.
//!
//! Only a release build has it. The release workflow (`.github/workflows/release.yml`) builds the Pi
//! download first, then builds this app with its file name, address and SHA-256 in three variables.
//! A build with none of them (a developer's) has no line, and the guide says to copy the folder
//! over by hand instead. A build with some but not all of them, or with one that isn't right, fails
//! to compile: a release must never quietly lose the line or show a wrong one.

use std::sync::LazyLock;

/// The only place a release build downloads the Pi files from.
const GITHUB: &str = "https://github.com/";

/// The Pi download as this build was given it: (file name, address, SHA-256).
const BUILT_IN: Option<(&str, &str, &str)> = match (
    option_env!("DISPLAY_HUB_PI_TARBALL"),
    option_env!("DISPLAY_HUB_PI_URL"),
    option_env!("DISPLAY_HUB_PI_SHA256"),
) {
    (None, None, None) => None,
    (Some(name), Some(url), Some(sha256)) => {
        assert!(
            valid_name(name),
            "DISPLAY_HUB_PI_TARBALL must be a plain file name ending in .tar.gz"
        );
        assert!(
            valid_url(url, name),
            "DISPLAY_HUB_PI_URL must be an https://github.com/ address ending in the file name"
        );
        assert!(
            valid_sha256(sha256),
            "DISPLAY_HUB_PI_SHA256 must be 64 lower-case hex digits"
        );
        Some((name, url, sha256))
    }
    _ => panic!(
        "set all three of DISPLAY_HUB_PI_TARBALL, DISPLAY_HUB_PI_URL and DISPLAY_HUB_PI_SHA256, or none"
    ),
};

/// This build's line, or `None` for a developer build.
pub fn built_in() -> Option<&'static str> {
    static LINE: LazyLock<Option<String>> =
        LazyLock::new(|| BUILT_IN.and_then(|(name, url, sha256)| one_liner(name, url, sha256)));
    LINE.as_deref()
}

/// The line for a download, or `None` if any part of it isn't what a release makes: so nothing
/// odd ever reaches a terminal.
pub fn one_liner(name: &str, url: &str, sha256: &str) -> Option<String> {
    (valid_name(name) && valid_url(url, name) && valid_sha256(sha256)).then(|| line(name, url, sha256))
}

/// Each step runs only if the one before it worked, so nothing is unpacked or run unless the
/// download matches the checksum. It works in a fresh folder from `mktemp -d`, never the home
/// folder, where an older copy might sit. Every value is in single quotes, and is checked to hold
/// none.
fn line(name: &str, url: &str, sha256: &str) -> String {
    format!(
        "d=$(mktemp -d) && cd \"$d\" && curl -fsSL -o '{name}' '{url}' && echo '{sha256}  {name}' | sha256sum -c - \
         && tar -xzf '{name}' && sudo bash display-hub-pi/install.sh"
    )
}

/// A release's file name: letters, digits, dots, hyphens and underscores, ending in `.tar.gz`, and
/// not starting with a dot or a hyphen.
const fn valid_name(name: &str) -> bool {
    let b = name.as_bytes();
    if b.len() <= ".tar.gz".len() || b.len() > 100 || b[0] == b'.' || b[0] == b'-' {
        return false;
    }
    if !ends_with(b, b".tar.gz") {
        return false;
    }
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if !(c.is_ascii_alphanumeric() || c == b'.' || c == b'-' || c == b'_') {
            return false;
        }
        i += 1;
    }
    true
}

/// A download address on GitHub, made of plain characters, with no `..`, ending in `/<name>`.
const fn valid_url(url: &str, name: &str) -> bool {
    let b = url.as_bytes();
    let n = name.as_bytes();
    if b.len() > 300 || !starts_with(b, GITHUB.as_bytes()) || b.len() <= GITHUB.len() + n.len() {
        return false;
    }
    if !ends_with(b, n) || b[b.len() - n.len() - 1] != b'/' {
        return false;
    }
    let mut i = GITHUB.len();
    while i < b.len() {
        let c = b[i];
        if !(c.is_ascii_alphanumeric() || c == b'.' || c == b'-' || c == b'_' || c == b'/') {
            return false;
        }
        if c == b'.' && i + 1 < b.len() && b[i + 1] == b'.' {
            return false;
        }
        i += 1;
    }
    true
}

/// A SHA-256 as `sha256sum` prints it: 64 lower-case hex digits.
const fn valid_sha256(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() != 64 {
        return false;
    }
    let mut i = 0;
    while i < b.len() {
        if !matches!(b[i], b'0'..=b'9' | b'a'..=b'f') {
            return false;
        }
        i += 1;
    }
    true
}

const fn starts_with(s: &[u8], prefix: &[u8]) -> bool {
    if s.len() < prefix.len() {
        return false;
    }
    let mut i = 0;
    while i < prefix.len() {
        if s[i] != prefix[i] {
            return false;
        }
        i += 1;
    }
    true
}

const fn ends_with(s: &[u8], suffix: &[u8]) -> bool {
    if s.len() < suffix.len() {
        return false;
    }
    let start = s.len() - suffix.len();
    let mut i = 0;
    while i < suffix.len() {
        if s[start + i] != suffix[i] {
            return false;
        }
        i += 1;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    const NAME: &str = "display-hub-pi-arm64-0.1.0.tar.gz";
    const URL: &str =
        "https://github.com/penguin97040/display-hub/releases/download/v0.1.0/display-hub-pi-arm64-0.1.0.tar.gz";
    const SHA: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn the_line_for_a_release() {
        assert_eq!(
            one_liner(NAME, URL, SHA).as_deref(),
            Some(
                "d=$(mktemp -d) && cd \"$d\" && curl -fsSL -o 'display-hub-pi-arm64-0.1.0.tar.gz' \
                 'https://github.com/penguin97040/display-hub/releases/download/v0.1.0/display-hub-pi-arm64-0.1.0.tar.gz' \
                 && echo '0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef  \
                 display-hub-pi-arm64-0.1.0.tar.gz' | sha256sum -c - && tar -xzf 'display-hub-pi-arm64-0.1.0.tar.gz' \
                 && sudo bash display-hub-pi/install.sh"
            )
        );
    }

    #[test]
    fn a_release_candidate_works_too() {
        let name = "display-hub-pi-arm64-0.2.0-rc.1.tar.gz";
        let url = format!("https://github.com/penguin97040/display-hub/releases/download/v0.2.0-rc.1/{name}");
        assert!(one_liner(name, &url, SHA).is_some_and(|l| l.contains(&url)));
    }

    #[test]
    fn a_malformed_checksum_is_refused() {
        for sha in [
            String::new(),
            SHA[1..].to_string(),
            format!("{SHA}0"),
            SHA.to_uppercase(),
            SHA.replace('a', "g"),
            format!("{}'", &SHA[1..]),
        ] {
            assert_eq!(one_liner(NAME, URL, &sha), None, "{sha:?}");
        }
    }

    #[test]
    fn an_address_off_github_or_odd_is_refused() {
        for url in [
            "http://github.com/penguin97040/display-hub/releases/download/v0.1.0/display-hub-pi-arm64-0.1.0.tar.gz",
            "https://github.com.example.nz/x/display-hub-pi-arm64-0.1.0.tar.gz",
            "https://example.nz/https://github.com/display-hub-pi-arm64-0.1.0.tar.gz",
            "https://github.com/display-hub-pi-arm64-0.1.0.tar.gz.evil",
            "https://github.com/x/other-display-hub-pi-arm64-0.1.0.tar.gz",
            "https://github.com/x/../display-hub-pi-arm64-0.1.0.tar.gz",
            "https://github.com/x/'$(reboot)'/display-hub-pi-arm64-0.1.0.tar.gz",
            "https://github.com/x/a b/display-hub-pi-arm64-0.1.0.tar.gz",
            "https://github.com/x?y=/display-hub-pi-arm64-0.1.0.tar.gz",
            "https://github.com/display-hub-pi-arm64-0.1.0.tar.gz/",
        ] {
            assert_eq!(one_liner(NAME, url, SHA), None, "{url}");
        }
    }

    #[test]
    fn an_odd_file_name_is_refused() {
        for name in [
            "",
            ".tar.gz",
            "-rf.tar.gz",
            ".hidden.tar.gz",
            "display-hub-pi.zip",
            "a b.tar.gz",
            "a'b.tar.gz",
            "a/b.tar.gz",
        ] {
            let url = format!("https://github.com/x/{name}");
            assert_eq!(one_liner(name, &url, SHA), None, "{name:?}");
        }
    }

    /// A developer build (the usual test run) has no line; one given the release variables has
    /// the line for them.
    #[test]
    fn this_builds_line_comes_from_its_variables() {
        match BUILT_IN {
            None => assert_eq!(built_in(), None),
            Some((name, url, sha256)) => {
                assert_eq!(built_in(), one_liner(name, url, sha256).as_deref());
                assert!(built_in().is_some_and(|l| l.contains(sha256)));
            }
        }
    }

    /// The docs and the release notes show the same line, with stand-ins for what each release fills in.
    #[test]
    fn the_docs_and_release_notes_show_this_line() {
        let example = line(NAME, URL, "<checksum>");
        for (path, text) in [
            ("docs/install.md", include_str!("../../../../docs/install.md")),
            (
                "docs/deploy-raspberry-pi.md",
                include_str!("../../../../docs/deploy-raspberry-pi.md"),
            ),
        ] {
            assert!(text.contains(&example), "{path} doesn't show: {example}");
        }
        let template = line("{{PI_TARBALL}}", "{{PI_URL}}", "{{PI_SHA256}}");
        assert!(include_str!("../../../../.github/release-notes.md").contains(&template));
    }
}
