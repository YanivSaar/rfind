//! Noticing filesystems mounted or unmounted after startup.
//!
//! The mount table is polled (it's a small /proc read); mounts that appear
//! or disappear under a live root are reported so the daemon can index and
//! watch new ones and rescan the mount points of removed ones.

use rfind_core::mounts::{self, MountEntry};
use std::{collections::HashSet, path::PathBuf};

pub struct MountWatch {
    // Identity triple per mount: (dev, point, source). `source` catches
    // the case of a USB drive unplugged and a different drive plugged
    // in at the same mount point — the kernel may reuse the same
    // major:minor allocation, so `(dev, point)` alone would miss the
    // transition and the daemon would keep serving the old drive's
    // paths. mountinfo's field 10 (`/dev/sdX1`, `//host/share`,
    // `host:/export`) is enough to tell them apart.
    known: HashSet<(String, PathBuf, String)>,
}

/// Mounts that appeared and disappeared since the last poll.
#[derive(Debug, Default)]
pub struct MountChanges {
    pub added: Vec<MountEntry>,
    pub removed: Vec<MountEntry>,
    /// The new mount table (for alias detection).
    pub table: Vec<MountEntry>,
}

fn key(m: &MountEntry) -> (String, PathBuf, String) {
    (m.dev.clone(), m.point.clone(), m.source.clone())
}

impl MountWatch {
    pub fn new(table: &[MountEntry]) -> Self {
        Self {
            known: table.iter().map(key).collect(),
        }
    }

    pub fn poll(&mut self, previous: &[MountEntry]) -> Option<MountChanges> {
        let table = mounts::read_mountinfo();
        let now: HashSet<(String, PathBuf, String)> = table.iter().map(key).collect();
        if now == self.known {
            return None;
        }
        let added = table
            .iter()
            .filter(|m| !self.known.contains(&key(m)))
            .cloned()
            .collect();
        let removed = previous
            .iter()
            .filter(|m| !now.contains(&key(m)))
            .cloned()
            .collect();
        self.known = now;
        Some(MountChanges {
            added,
            removed,
            table,
        })
    }
}
