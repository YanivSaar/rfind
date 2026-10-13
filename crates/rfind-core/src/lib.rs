//! Shared types for rfind: status, progress, and errors.

use serde::{Deserialize, Serialize};
use std::time::SystemTime;

/// Current phase of the daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DaemonPhase {
    /// Initial parallel filesystem scan in progress.
    Scanning,
    /// Scan complete; index is ready and being kept current.
    Ready,
    /// A problem occurred; see message.
    Error(String),
}

/// Live progress snapshot written by the daemon and read by the CLI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanProgress {
    pub phase: DaemonPhase,
    /// Total filesystem entries discovered so far.
    pub entries_found: u64,
    /// Directories fully enumerated.
    pub dirs_completed: u64,
    /// Directories currently queued or in progress.
    pub dirs_pending: u64,
    /// Entries per second over the last reporting interval.
    pub entries_per_sec: f64,
    /// Errors encountered (permission denied, I/O errors, etc.).
    pub errors: u64,
    /// When this snapshot was written (Unix seconds).
    pub timestamp_secs: u64,
    /// Human-readable status line for systemd journal / CLI display.
    pub status_line: String,
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
        }
    }
}

/// Path to the progress file the daemon writes and the CLI reads.
pub fn progress_file_path() -> std::path::PathBuf {
    std::path::PathBuf::from("/run/rfindd/progress.json")
}
