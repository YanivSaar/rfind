pub mod db;

use serde::{Deserialize, Serialize};
use std::time::SystemTime;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DaemonPhase {
    /// Initial parallel filesystem scan in progress.
    Scanning,
    /// Scan complete; applying queued events to bring index current.
    Reconciling,
    /// Index is current and being kept up to date by fanotify.
    Ready,
    /// A problem occurred.
    Error(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanProgress {
    pub phase: DaemonPhase,
    pub entries_found: u64,
    pub dirs_completed: u64,
    pub dirs_pending: u64,
    pub entries_per_sec: f64,
    pub errors: u64,
    pub timestamp_secs: u64,
    pub status_line: String,
    /// Events queued during scan, waiting to be reconciled.
    pub events_pending: u64,
    /// Events applied during reconciliation.
    pub events_applied: u64,
}

impl ScanProgress {
    pub fn new_scanning() -> Self {
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        Self {
            phase: DaemonPhase::Scanning,
            entries_found: 0,
            dirs_completed: 0,
            dirs_pending: 0,
            entries_per_sec: 0.0,
            errors: 0,
            timestamp_secs: now,
            status_line: "Initializing scanner...".to_string(),
            events_pending: 0,
            events_applied: 0,
        }
    }
}

pub fn progress_file_path() -> std::path::PathBuf {
    std::path::PathBuf::from("/run/rfindd/progress.json")
}

pub fn db_path() -> std::path::PathBuf {
    std::path::PathBuf::from("/var/lib/rfindd/index.db")
}
