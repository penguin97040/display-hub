//! Bounds a hub keeps to, so a display with a 368 x 448 screen and little RAM can always cope.

use std::ops::RangeInclusive;

/// The largest body a display must accept. A hub never sends more.
pub const MAX_SNAPSHOT_BYTES: usize = 32 * 1024;
pub const MAX_PAGES: usize = 10;
/// The largest serialised page object (the whole page, not just its `data`). Ten of these and the
/// envelope fit in [`MAX_SNAPSHOT_BYTES`].
pub const MAX_PAGE_BYTES: usize = 3072;
/// Items in a `meter` page.
pub const MAX_METER_ITEMS: usize = 6;
/// Items in a `list` page.
pub const MAX_LIST_ITEMS: usize = 8;
/// Characters in a `text` page's text.
pub const MAX_TEXT_LEN: usize = 280;
/// Characters in a list item's `text` or `detail`.
pub const MAX_LIST_TEXT_LEN: usize = 48;
/// Characters in a `big_number` unit and caption.
pub const MAX_UNIT_LEN: usize = 8;
pub const MAX_CAPTION_LEN: usize = 48;
/// The largest magnitude a `big_number` value may have, so any display can draw it.
pub const MAX_BIG_NUMBER_ABS: f64 = 1e15;
/// The most decimal places a `big_number` shows.
pub const MAX_DECIMALS: u8 = 3;
/// How deeply a custom page's `data` may nest (the `data` object itself counts as one level).
pub const MAX_CUSTOM_DEPTH: usize = 8;
/// A page `kind`, including its `/version`.
pub const MAX_KIND_LEN: usize = 64;
pub const MAX_ID_LEN: usize = 64;
/// Characters (Unicode scalar values), not bytes.
pub const MAX_TITLE_LEN: usize = 48;
pub const MAX_LABEL_LEN: usize = 48;
pub const MAX_NOTE_LEN: usize = 96;
/// Poll hint bounds. Displays add their own jitter.
pub const REFRESH_RANGE: RangeInclusive<u32> = 30..=3600;
/// From one minute to one week.
pub const STALE_AFTER_RANGE: RangeInclusive<u32> = 60..=604_800;
/// The largest heartbeat, enrolment or other request body a hub accepts from a display.
pub const MAX_DEVICE_REQUEST_BYTES: usize = 2 * 1024;

// ----- signed firmware updates (`docs/firmware-updates.md`) -----

/// Trusted keys a display holds, and keys in a bundle's `firmware.trusted_keys`.
pub const MAX_TRUSTED_KEYS: usize = 3;
/// The largest whole package a hub stores, and the largest upload it accepts.
pub const MAX_FIRMWARE_PACKAGE_BYTES: u64 = 16 * 1024 * 1024;
/// The largest update form: the 137-byte header, a 4096-byte manifest and an 8 MiB image. A
/// snapshot's `firmware_update.size` is never more.
pub const MAX_FIRMWARE_UPDATE_BYTES: u64 = 137 + 4096 + 8 * 1024 * 1024;
/// An offer ends this long after it was made, if nothing else ended it first.
pub const FIRMWARE_OFFER_LIFETIME_SECONDS: i64 = 24 * 3600;
/// Downloads a hub serves at once, across all displays. Another is answered `unavailable`.
pub const MAX_CONCURRENT_FIRMWARE_DOWNLOADS: usize = 2;
/// Download starts a hub allows one display in [`FIRMWARE_DOWNLOAD_WINDOW_SECONDS`]. Another is
/// answered `rate_limited`.
pub const MAX_FIRMWARE_DOWNLOADS_PER_WINDOW: u32 = 6;
pub const FIRMWARE_DOWNLOAD_WINDOW_SECONDS: u64 = 3600;
/// The longest a download may take, end to end, on either side.
pub const FIRMWARE_DOWNLOAD_TIMEOUT_SECONDS: u64 = 15 * 60;
/// A display gives up on a download that sends nothing for this long.
pub const FIRMWARE_DOWNLOAD_STALL_SECONDS: u64 = 30;
/// The least time a display leaves between download attempts, and after
/// [`FIRMWARE_FAILURES_BEFORE_LONG_WAIT`] failures in a row, the least before the next.
pub const FIRMWARE_RETRY_WAIT_SECONDS: u64 = 10 * 60;
pub const FIRMWARE_FAILURES_BEFORE_LONG_WAIT: u32 = 3;
pub const FIRMWARE_LONG_WAIT_SECONDS: u64 = 6 * 3600;
/// The largest `attempts_left` a display reports in its `firmware` member.
pub const MAX_FIRMWARE_ATTEMPTS_LEFT: u32 = 5;
/// New firmware rolls back unless it reaches the hub within this long of its trial boot.
pub const FIRMWARE_TRIAL_SECONDS: u64 = 5 * 60;
