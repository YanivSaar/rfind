//! The progress file read by `rfind status`.

use rfind_core::{progress_file_path, DaemonPhase, ScanProgress};
use std::{fs, io::Write, os::unix::fs::OpenOptionsExt, time::SystemTime};

/// Write atomically (temp file + rename) so readers never see half a file.
///
/// Uses O_NOFOLLOW on the tmp open: /run/rfindd is root-only today, but
/// if its mode ever loosens or an admin reconfigures the runtime dir, a
/// pre-seeded symlink at `<progress>.tmp` would otherwise redirect our
/// write. 0o600 means only root sees the file; the systemd unit's
/// RuntimeDirectoryMode controls who can see the directory itself.
pub fn write(progress: &ScanProgress) {
    let path = progress_file_path();
    if let Some(p) = path.parent() {
        let _ = fs::create_dir_all(p);
    }
    if let Ok(json) = serde_json::to_string(progress) {
        let tmp = path.with_extension("tmp");
        let open = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .mode(0o600)
            .open(&tmp);
        if let Ok(mut f) = open {
            let _ = f.write_all(json.as_bytes());
            let _ = fs::rename(&tmp, &path);
        }
    }
}

/// Progress for the non-scanning phases (scans report their own).
pub fn status(
    phase: DaemonPhase,
    entries: u64,
    pending: u64,
    applied: u64,
    mode: &str,
) -> ScanProgress {
    let status_line = match &phase {
        DaemonPhase::Reconciling => {
            format!("Reconciling | {pending} events pending | {applied} applied")
        }
        DaemonPhase::Ready => {
            format!("Ready | {entries} entries | {applied} events applied | monitor: {mode}")
        }
        DaemonPhase::Scanning => format!("Scanning | {entries} entries"),
        DaemonPhase::Error(e) => format!("Error: {e}"),
    };
    ScanProgress {
        phase,
        entries_found: entries,
        dirs_completed: 0,
        dirs_pending: 0,
        entries_per_sec: 0.0,
        errors: 0,
        timestamp_secs: SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        status_line,
        events_pending: pending,
        events_applied: applied,
        monitoring_mode: mode.to_string(),
        background_scan: None,
    }
}
