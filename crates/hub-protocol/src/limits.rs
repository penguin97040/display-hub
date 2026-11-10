//! Bounds a hub keeps to, so a display with a 368 x 448 screen and little RAM can always cope.

use std::ops::RangeInclusive;

/// The largest body a display must accept. A hub never sends more.
pub const MAX_SNAPSHOT_BYTES: usize = 32 * 1024;
pub const MAX_PAGES: usize = 10;
pub const MAX_METRICS: usize = 6;
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
