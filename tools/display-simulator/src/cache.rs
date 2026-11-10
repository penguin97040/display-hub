//! The last good snapshot, kept on disk like a display keeps it in flash, so a restart while the
//! hub is down still has something to show.

use std::io::Write;
use std::path::Path;

use hub_protocol::{Snapshot, Timestamp};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Held {
    pub snapshot: Snapshot,
    /// When this display received it, by its own clock.
    pub received_at: Timestamp,
}

/// Reads the cache. A missing or damaged file is simply no cache.
pub fn load(path: &Path) -> Option<Held> {
    let bytes = std::fs::read(path).ok()?;
    let held: Held = serde_json::from_slice(&bytes).ok()?;
    held.snapshot.validate().ok().map(|_| held)
}

/// Writes the cache so a crash part-way leaves the old file whole.
pub fn store(path: &Path, held: &Held) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&serde_json::to_vec_pretty(held).expect("serialises"))?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}
