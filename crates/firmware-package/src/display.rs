//! What a display checks before it writes anything to flash ("Checks before installing", steps 1 to
//! 7), for the reference simulator. Each failure is one of the document's reason codes.
//!
//! The display's own stored keys decide. The key in a package's header is only ever compared with
//! them; the signature is verified with the display's stored copy.

use crate::check::update_form_len;
use crate::header::{verify_signature, Header};
use crate::key::PublicKey;
use crate::limits::{HEADER_LEN, MAX_MANIFEST_LEN};
use crate::manifest::Manifest;

/// Why a display refused an offer, before anything was written.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Refusal {
    NoTrustedKeys,
    TooLarge,
    BadFormat,
    UntrustedKey,
    BadSignature,
    WrongDeviceType,
    WrongHardware,
    Downgrade,
}

impl Refusal {
    /// The `reason` a display reports.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NoTrustedKeys => "no_trusted_keys",
            Self::TooLarge => "too_large",
            Self::BadFormat => "bad_format",
            Self::UntrustedKey => "untrusted_key",
            Self::BadSignature => "bad_signature",
            Self::WrongDeviceType => "wrong_device_type",
            Self::WrongHardware => "wrong_hardware",
            Self::Downgrade => "downgrade",
        }
    }
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The dropped-key rule of step 7: a `key_set` that leaves out any key the display trusts retires
/// that key, and a maker must raise the security counter above the display's floor to do it.
/// Without the rule, a replay of an older release at the same counter could bring back a key a later
/// release removed. True when a package breaks the rule, so a display refuses it as a `downgrade`.
///
/// The keys can be anything that names them one to one: a display compares its stored public keys,
/// and the hub and the app the key ids a display reports. The hub and the app use this to avoid
/// offering a package the display would refuse; the display's own check is the one that counts.
pub fn drops_key_unraised<K: PartialEq>(
    key_set: Option<&[K]>,
    trusted: &[K],
    security_counter: u32,
    security_floor: u32,
) -> bool {
    key_set.is_some_and(|set| trusted.iter().any(|k| !set.contains(k)) && security_counter <= security_floor)
}

/// What a display knows about itself.
#[derive(Clone, Copy, Debug)]
pub struct Display<'a> {
    /// Its stored trusted keys (0 to 3).
    pub trusted_keys: &'a [PublicKey],
    pub device_type: &'a str,
    pub hardware_revision: &'a str,
    pub security_floor: u32,
    /// The size of its spare app partition.
    pub slot_bytes: u64,
}

/// A header that passed steps 3 and 4, with the display's own copy of the key that matched.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DisplayHeader {
    pub header: Header,
    /// The display's stored key equal to the header's. Verification uses this one.
    pub stored_key: PublicKey,
}

impl Display<'_> {
    /// Steps 1 and 2, before any of the body is read: a display with no trusted keys refuses every
    /// offer, and the offer's `size` and the response's `Content-Length` must be equal and no more
    /// than a header, the largest manifest and the spare partition.
    pub fn check_offer(&self, offer_size: u64, content_length: u64) -> Result<(), Refusal> {
        if self.trusted_keys.is_empty() {
            return Err(Refusal::NoTrustedKeys);
        }
        let limit = (HEADER_LEN + MAX_MANIFEST_LEN) as u64 + self.slot_bytes;
        if offer_size != content_length || offer_size > limit {
            return Err(Refusal::TooLarge);
        }
        Ok(())
    }

    /// Steps 3 and 4: the header's fixed fields, then its key must be one of the display's own, all
    /// 65 bytes. A display with no keys refuses here too, in case step 1 was skipped.
    pub fn check_header(&self, header: &[u8; HEADER_LEN]) -> Result<DisplayHeader, Refusal> {
        if self.trusted_keys.is_empty() {
            return Err(Refusal::NoTrustedKeys);
        }
        let header = Header::parse(header).map_err(|_| Refusal::BadFormat)?;
        let stored_key = *self
            .trusted_keys
            .iter()
            .find(|k| k.as_bytes() == header.public_key.as_bytes())
            .ok_or(Refusal::UntrustedKey)?;
        Ok(DisplayHeader { header, stored_key })
    }

    /// Steps 5 to 7: the signature, verified with the stored key before the manifest is parsed; then
    /// the manifest; then whether it is for this display, not a downgrade, and fits. A `key_set`
    /// that leaves out any key the display trusts now counts as a downgrade unless its security
    /// counter is above the display's floor.
    /// `manifest` is the `manifest_len` bytes after the header, and `content_length` the response's.
    pub fn check_manifest(
        &self,
        header: &DisplayHeader,
        manifest: &[u8],
        content_length: u64,
    ) -> Result<Manifest, Refusal> {
        if manifest.len() != header.header.manifest_len as usize {
            return Err(Refusal::BadFormat);
        }
        // 5.
        if !verify_signature(&header.stored_key, &header.header, manifest) {
            return Err(Refusal::BadSignature);
        }
        // 6.
        let m = Manifest::parse_for_display(manifest).map_err(|_| Refusal::BadFormat)?;
        // 7.
        if m.device_type.as_bytes() != self.device_type.as_bytes() {
            return Err(Refusal::WrongDeviceType);
        }
        if !m
            .hardware_revisions
            .iter()
            .any(|r| r.as_bytes() == self.hardware_revision.as_bytes())
        {
            return Err(Refusal::WrongHardware);
        }
        if m.security_counter < self.security_floor {
            return Err(Refusal::Downgrade);
        }
        if drops_key_unraised(
            m.key_set.as_deref(),
            self.trusted_keys,
            m.security_counter,
            self.security_floor,
        ) {
            return Err(Refusal::Downgrade);
        }
        if m.image_size > self.slot_bytes {
            return Err(Refusal::TooLarge);
        }
        if update_form_len(header.header.manifest_len, m.image_size) != content_length {
            return Err(Refusal::BadFormat);
        }
        Ok(m)
    }

    /// Steps 3 to 7 together.
    pub fn check(
        &self,
        header: &[u8; HEADER_LEN],
        manifest: &[u8],
        content_length: u64,
    ) -> Result<Manifest, Refusal> {
        let h = self.check_header(header)?;
        self.check_manifest(&h, manifest, content_length)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_set_that_drops_a_trusted_key_needs_a_higher_counter() {
        let trusted = ["a", "b"];
        // No key_set changes nothing.
        assert!(!drops_key_unraised(None, &trusted, 3, 3));
        // Keeping every trusted key, with or without more, is fine at the floor.
        assert!(!drops_key_unraised(Some(&["a", "b"][..]), &trusted, 3, 3));
        assert!(!drops_key_unraised(Some(&["b", "a", "c"][..]), &trusted, 3, 3));
        // Dropping one needs a counter above the floor.
        assert!(drops_key_unraised(Some(&["a"][..]), &trusted, 3, 3));
        assert!(drops_key_unraised(Some(&["a", "c"][..]), &trusted, 2, 3));
        assert!(!drops_key_unraised(Some(&["a"][..]), &trusted, 4, 3));
        // A display that trusts nothing drops nothing.
        assert!(!drops_key_unraised(Some(&["a"][..]), &[] as &[&str], 0, 0));
    }
}
