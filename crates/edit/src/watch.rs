//! Poll-based file-change detection.
//!
//! No inotify -- every consumer here already has a tick (the editor's 2s
//! disk poll, eat's viewer poll), so a `stat` per tick is cheaper than
//! carrying a watcher. The question is only "did this file change at
//! all?": compare two [`FileStat`]s. The mtime is in the comparison, so a
//! same-length rewrite onto the same inode still reads as a change.

use std::fs::Metadata;
use std::io;
use std::path::Path;

/// What a single `stat` saw. `id` is the inode on unix and 0 elsewhere,
/// where rotation is detected by size and content alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileStat {
    pub size: u64,
    pub id: u64,
    /// Nanoseconds since the unix epoch.
    pub mtime_ns: i128,
}

#[cfg(unix)]
pub fn id_of(meta: &Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt as _;
    meta.ino()
}

#[cfg(not(unix))]
pub fn id_of(_meta: &Metadata) -> u64 {
    0
}

impl FileStat {
    pub fn from_metadata(meta: &Metadata) -> Self {
        let mtime_ns = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos() as i128)
            .unwrap_or(0);
        Self { size: meta.len(), id: id_of(meta), mtime_ns }
    }

    pub fn from_path(path: &Path) -> io::Result<Self> {
        Ok(Self::from_metadata(&std::fs::metadata(path)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stat(size: u64, id: u64, mtime_ns: i128) -> FileStat {
        FileStat { size, id, mtime_ns }
    }

    #[test]
    fn a_touch_alone_is_a_change() {
        // The same bytes rewritten in place: size and inode cannot see it,
        // the mtime can.
        assert_ne!(stat(100, 7, 1), stat(100, 7, 999));
        assert_eq!(stat(100, 7, 1), stat(100, 7, 1));
    }
}
