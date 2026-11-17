//! Every fixed value and limit of the package format, from "The package" in `docs/firmware-updates.md`.

/// The first four bytes of every package.
pub const MAGIC: [u8; 4] = *b"DHFW";
/// The only format version this crate reads or writes.
pub const FORMAT_VERSION: u8 = 1;
/// ECDSA on P-256 over a SHA-256 digest: the only algorithm there is.
pub const ALGORITHM_ECDSA_P256_SHA256: u8 = 1;

/// Header field offsets.
pub const OFFSET_MAGIC: usize = 0;
pub const OFFSET_FORMAT_VERSION: usize = 4;
pub const OFFSET_ALGORITHM: usize = 5;
pub const OFFSET_MANIFEST_LEN: usize = 6;
pub const OFFSET_PUBLIC_KEY: usize = 8;
pub const OFFSET_SIGNATURE: usize = 73;

/// The uncompressed SEC 1 public key: `0x04`, then X, then Y.
pub const PUBLIC_KEY_LEN: usize = 65;
/// The first byte of an uncompressed point.
pub const PUBLIC_KEY_PREFIX: u8 = 0x04;
/// `r` then `s`, 32 bytes each, big-endian.
pub const SIGNATURE_LEN: usize = 64;
/// The whole header.
pub const HEADER_LEN: usize = 137;
/// The part of the header the signature covers: everything before the signature.
pub const SIGNED_HEADER_LEN: usize = OFFSET_SIGNATURE;
/// The fixed text at the front of the signed bytes, with its zero byte.
pub const SIGNATURE_CONTEXT: &[u8; 24] = b"display-hub.firmware.v1\0";

/// `manifest_len` bounds.
pub const MIN_MANIFEST_LEN: usize = 2;
pub const MAX_MANIFEST_LEN: usize = 4096;
/// How deeply the manifest's JSON may nest. Objects and arrays each count as a level, and the
/// manifest object itself is the first.
pub const MAX_JSON_DEPTH: usize = 4;

/// The shortest file a checker reads past step 1: a header and the shortest manifest.
pub const MIN_PACKAGE_LEN: u64 = (HEADER_LEN + MIN_MANIFEST_LEN) as u64;
/// The longest whole package.
pub const MAX_PACKAGE_LEN: u64 = 16 * 1024 * 1024;
/// `image_size` bounds.
pub const MIN_IMAGE_SIZE: u64 = 1;
pub const MAX_IMAGE_SIZE: u64 = 8 * 1024 * 1024;

/// Characters in `device_type`, the same as the `device_type` a display reports.
pub const MAX_DEVICE_TYPE_LEN: usize = hub_protocol::device::MAX_DEVICE_FIELD_LEN;
/// Entries in `hardware_revisions`.
pub const MAX_HARDWARE_REVISIONS: usize = 8;
/// Characters in one hardware revision, the same as the `hardware_revision` a display reports.
pub const MAX_HARDWARE_REVISION_LEN: usize = hub_protocol::device::MAX_DEVICE_FIELD_LEN;
/// Characters in `firmware_version`.
pub const MAX_FIRMWARE_VERSION_LEN: usize = 48;
/// Characters in `summary`.
pub const MAX_SUMMARY_LEN: usize = 280;
/// Keys in a `key_set`, and trusted keys on a display.
pub const MAX_KEY_SET: usize = 3;
pub const MAX_TRUSTED_KEYS: usize = 3;
/// USB-install parts.
pub const MAX_USB_PARTS: usize = 4;
/// Characters in a USB-install part's name.
pub const MAX_PART_NAME_LEN: usize = 32;
/// Characters in `usb_install.chip`. The document names no limit; this one is generous.
pub const MAX_CHIP_NAME_LEN: usize = 32;
/// Flash offsets (`app_offset` and each part's `offset`) are multiples of this.
pub const FLASH_ALIGN: u64 = 4096;
/// The largest `min_flash_bytes`: the 32-bit flash address space. The document names no limit.
pub const MAX_FLASH_BYTES: u64 = 1 << 32;

/// SHA-256 digests, in bytes and as hex digits.
pub const SHA256_LEN: usize = 32;
pub const SHA256_HEX_LEN: usize = 64;
/// A public key as hex digits in JSON.
pub const PUBLIC_KEY_HEX_LEN: usize = PUBLIC_KEY_LEN * 2;
/// Hex digits of the key id shown as a fingerprint, and the size of each group.
pub const FINGERPRINT_HEX_LEN: usize = 32;
pub const FINGERPRINT_GROUP_LEN: usize = 4;
