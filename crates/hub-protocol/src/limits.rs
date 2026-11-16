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
