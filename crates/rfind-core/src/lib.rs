// rfind is a 64-bit-only project: the zvfs on-disk format and the
// fanotify event parsing both do `usize == u64` casts, and the
// release pipeline only publishes x86_64 and aarch64. Catch anyone
// attempting a 32-bit build loudly.
#[cfg(not(target_pointer_width = "64"))]
compile_error!(
    "rfind requires a 64-bit target: the zvfs page store and fanotify event \
     parsing assume usize == u64. Build for x86_64, aarch64, riscv64, or s390x."
);

#[cfg(not(target_os = "linux"))]
compile_error!(
    "rfind is Linux-only: it uses fanotify, inotify, statx and openat2 via \
     direct syscalls. There is no plan to port to other kernels."
);

pub mod db;
pub mod filters;
pub mod log;
pub mod mounts;
pub mod safewalk;

use serde::{Deserialize, Serialize};
use std::time::SystemTime;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DaemonPhase {
    Scanning,
    Reconciling,
    Ready,
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
    pub events_pending: u64,
    pub events_applied: u64,
    pub monitoring_mode: String,
    /// A periodic or new-mount scan running on a background thread while
    /// the daemon is otherwise Ready. None when nothing is in flight.
    /// `#[serde(default)]` keeps older progress files readable after an
    /// in-place upgrade — the daemon may still be running an old binary
    /// that wrote the file without this field.
    #[serde(default)]
    pub background_scan: Option<BackgroundScan>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackgroundScan {
    pub path: String,
    pub entries: u64,
    pub started_at: u64,
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
            status_line: "Initializing...".to_string(),
            events_pending: 0,
            events_applied: 0,
            monitoring_mode: "unknown".to_string(),
            background_scan: None,
        }
    }
}

pub fn progress_file_path() -> std::path::PathBuf {
    // Debug builds honor RFINDD_PROGRESS_PATH so integration tests can point
    // the daemon at a tempdir. Release builds (production) ignore it: the
    // daemon runs as root and env is controlled by whoever can set the
    // systemd unit, so an override there would be defense-in-depth noise.
    #[cfg(debug_assertions)]
    if let Some(v) = std::env::var_os("RFINDD_PROGRESS_PATH") {
        return std::path::PathBuf::from(v);
    }
    std::path::PathBuf::from("/run/rfindd/progress.json")
}

pub fn db_path() -> std::path::PathBuf {
    std::path::PathBuf::from("/var/lib/rfindd/index.db")
}
