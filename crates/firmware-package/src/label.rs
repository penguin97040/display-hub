//! The owner's labels for signing keys ("Who signed it" in `docs/firmware-updates.md`). A label is
//! kept on the hub and shown by the app; neither lets one pass itself off as a key built into the
//! app, so both refuse the same names with [`reserved_label`].

/// Whether the owner may not call a key this: the name the app keeps for keys built into it
/// (*Official Display Hub firmware*), or anything claiming to be official Display Hub firmware.
/// Compared ignoring case, spacing and punctuation.
pub fn reserved_label(label: &str) -> bool {
    let plain: String = label
        .chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect();
    plain == "officialdisplayhubfirmware" || (plain.contains("official") && plain.contains("displayhub"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_official_name_is_reserved() {
        for bad in [
            "Official Display Hub firmware",
            "official display hub firmware",
            "official display-hub firmware",
            "OFFICIAL-DISPLAY-HUB-FIRMWARE",
            "OFFICIAL  DISPLAY HUB",
            "Official  Display Hub  Firmware.",
            "officialdisplayhubfirmware",
            "Display Hub (official)",
            "The official DisplayHub build",
        ] {
            assert!(reserved_label(bad), "{bad}");
        }
        for ok in [
            "Kiwi Displays",
            "Kea Displays",
            "Display Hub fan",
            "Display Hub fan club",
            "Officially Kiwi",
            "Official Kea firmware",
            "Hub firmware",
        ] {
            assert!(!reserved_label(ok), "{ok}");
        }
    }
}
