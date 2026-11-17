//! A light look at an image before signing, to catch the wrong file: does it start like an ESP-IDF
//! app image, and does its own version match `--version`? Only ever warnings; the display and
//! ESP-IDF check the image properly.
//!
//! An app image starts with the 24-byte image header (its first byte `0xE9`), then the first
//! segment's 8-byte header, then that segment's data, which begins with the app description block
//! (`esp_app_desc_t`): a magic word, then `secure_version` and two reserved words, then `version`
//! as 32 bytes of text ended by a zero byte.

/// The first byte of an ESP-IDF image.
const IMAGE_MAGIC: u8 = 0xE9;
/// The image header and the first segment header.
const APP_DESC_OFFSET: usize = 24 + 8;
/// `ESP_APP_DESC_MAGIC_WORD`, stored little-endian.
const APP_DESC_MAGIC: u32 = 0xABCD_5432;
/// `version`'s place and size inside `esp_app_desc_t`.
const VERSION_OFFSET: usize = 16;
const VERSION_LEN: usize = 32;

pub fn warnings(image: &[u8], version: &str) -> Vec<String> {
    let desc_end = APP_DESC_OFFSET + VERSION_OFFSET + VERSION_LEN;
    let magic = image
        .get(APP_DESC_OFFSET..APP_DESC_OFFSET + 4)
        .map(|b| u32::from_le_bytes(b.try_into().expect("four bytes")));
    if image.first() != Some(&IMAGE_MAGIC) || image.len() < desc_end || magic != Some(APP_DESC_MAGIC) {
        return vec![
            "the image doesn't look like an ESP-IDF app image (the application .bin from your build). Check you chose the right file."
                .into(),
        ];
    }
    let field = &image[APP_DESC_OFFSET + VERSION_OFFSET..desc_end];
    let text = field.split(|&b| b == 0).next().unwrap_or_default();
    let found = String::from_utf8_lossy(text);
    if found != version {
        return vec![format!(
            "the image says its version is {found:?}, but --version is {version:?}. Check they're the same release."
        )];
    }
    Vec::new()
}

#[cfg(test)]
pub(crate) fn fake_image(version: &str, len: usize) -> Vec<u8> {
    let mut img = vec![0u8; len.max(APP_DESC_OFFSET + 256)];
    img[0] = IMAGE_MAGIC;
    img[APP_DESC_OFFSET..APP_DESC_OFFSET + 4].copy_from_slice(&APP_DESC_MAGIC.to_le_bytes());
    let v = APP_DESC_OFFSET + VERSION_OFFSET;
    img[v..v + version.len()].copy_from_slice(version.as_bytes());
    img
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_matching_image_has_no_warnings() {
        assert!(warnings(&fake_image("1.2.0", 4096), "1.2.0").is_empty());
        // A version that fills the whole field, with no zero byte.
        let full = "1.0.0-".to_string() + &"a".repeat(VERSION_LEN - 6);
        assert!(warnings(&fake_image(&full, 4096), &full).is_empty());
    }

    #[test]
    fn a_different_version_warns() {
        let w = warnings(&fake_image("1.1.0", 4096), "1.2.0");
        assert_eq!(w.len(), 1);
        assert!(w[0].contains("\"1.1.0\""), "{}", w[0]);
    }

    #[test]
    fn something_else_warns() {
        assert!(warnings(b"hello", "1.2.0")[0].contains("ESP-IDF"));
        let mut no_desc = fake_image("1.2.0", 4096);
        no_desc[APP_DESC_OFFSET] = 0;
        assert!(warnings(&no_desc, "1.2.0")[0].contains("ESP-IDF"));
        let mut no_magic = fake_image("1.2.0", 4096);
        no_magic[0] = 0;
        assert!(warnings(&no_magic, "1.2.0")[0].contains("ESP-IDF"));
        assert!(warnings(&[], "1.2.0")[0].contains("ESP-IDF"));
    }
}
