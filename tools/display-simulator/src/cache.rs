//! The last good snapshot, kept on disk like a display keeps it in flash, so a restart while the
//! hub is down still has something to show.

use std::io::Write;
use std::path::Path;

use hub_protocol::{receive, Snapshot, Timestamp};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq)]
pub struct Held {
    pub snapshot: Snapshot,
    /// When this display received it, by its own clock.
    pub received_at: Timestamp,
}

/// What is on disk: the snapshot as the display read it, so reading it back takes the same
/// lenient path as a fresh response (cards, tolerated fields and unknown qualities all survive).
#[derive(Serialize, Deserialize)]
struct Saved {
    received_at: Timestamp,
    snapshot: serde_json::Value,
}

/// Reads the cache. A missing or damaged file is simply no cache.
pub fn load(path: &Path) -> Option<Held> {
    let bytes = std::fs::read(path).ok()?;
    let saved: Saved = serde_json::from_slice(&bytes).ok()?;
    let snapshot = receive(&serde_json::to_vec(&saved.snapshot).ok()?).ok()?;
    Some(Held {
        snapshot,
        received_at: saved.received_at,
    })
}

/// Writes the cache so a crash part-way leaves the old file whole.
pub fn store(path: &Path, held: &Held) -> std::io::Result<()> {
    let saved = Saved {
        received_at: held.received_at,
        snapshot: serde_json::to_value(&held.snapshot).map_err(std::io::Error::other)?,
    };
    let tmp = path.with_extension("tmp");
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&serde_json::to_vec_pretty(&saved).map_err(std::io::Error::other)?)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}
