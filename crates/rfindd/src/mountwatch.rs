//! Noticing filesystems mounted or unmounted after startup.
//!
//! The mount table is polled (it's a small /proc read); mounts that appear
//! or disappear under a live root are reported so the daemon can index and
//! watch new ones and rescan the mount points of removed ones.

use rfind_core::mounts::{self, MountEntry};
use std::{collections::HashSet, path::PathBuf};

pub struct MountWatch {
    known: HashSet<(String, PathBuf)>,
}

/// Mounts that appeared and disappeared since the last poll.
#[derive(Debug, Default)]
pub struct MountChanges {
    pub added: Vec<MountEntry>,
    pub removed: Vec<MountEntry>,
    /// The new mount table (for alias detection).
    pub table: Vec<MountEntry>,
}

impl MountWatch {
    pub fn new(table: &[MountEntry]) -> Self {
        Self {
            known: table
                .iter()
                .map(|m| (m.dev.clone(), m.point.clone()))
                .collect(),
        }
    }

    pub fn poll(&mut self, previous: &[MountEntry]) -> Option<MountChanges> {
        let table = mounts::read_mountinfo();
        let now: HashSet<(String, PathBuf)> = table
            .iter()
            .map(|m| (m.dev.clone(), m.point.clone()))
            .collect();
        if now == self.known {
            return None;
        }
        let added = table
            .iter()
            .filter(|m| !self.known.contains(&(m.dev.clone(), m.point.clone())))
            .cloned()
            .collect();
        let removed = previous
            .iter()
            .filter(|m| !now.contains(&(m.dev.clone(), m.point.clone())))
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
