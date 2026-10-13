//! rfindd — rfind indexing daemon.
//!
//! Runs a parallel filesystem scan, writes live progress to
//! /run/rfindd/progress.json, and logs structured status to the
//! systemd journal so `systemctl status rfindd` shows real progress.

use rfind_core::{progress_file_path, ScanProgress};
use rfind_scanner::{run_scan, ScanConfig};
use std::{
    fs,
    io::Write,
    path::PathBuf,
    sync::{Arc, Mutex},
};
use tracing::{error, info};

fn write_progress(progress: &ScanProgress) {
    let path = progress_file_path();

    // Ensure /run/rfindd/ exists.
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }

    match serde_json::to_string(progress) {
        Ok(json) => {
            // Write atomically: write to .tmp then rename.
            let tmp = path.with_extension("tmp");
            if let Ok(mut f) = fs::File::create(&tmp) {
                let _ = f.write_all(json.as_bytes());
                let _ = fs::rename(&tmp, &path);
            }
        }
        Err(e) => error!("Failed to serialize progress: {e}"),
    }
}

fn main() {
    // Initialize structured logging to stdout (captured by systemd journal).
    tracing_subscriber::fmt()
        .with_target(false)
        .with_level(true)
        .init();

    info!("rfindd starting");

    // Default: index the entire filesystem from root.
    // Later this will come from a config file.
    let roots: Vec<PathBuf> = vec![PathBuf::from("/")];

    info!("Scanning roots: {:?}", roots);
    info!("Progress file: {:?}", progress_file_path());

    // Write initial progress so the CLI shows something immediately.
    let initial = ScanProgress::new_scanning();
    write_progress(&initial);

    let last_progress: Arc<Mutex<Option<ScanProgress>>> = Arc::new(Mutex::new(None));
    let last_for_final = Arc::clone(&last_progress);

    let config = ScanConfig::new(roots, move |progress| {
        // Log one-line status to the journal (visible in systemctl status).
        info!("{}", progress.status_line);

        // Write JSON snapshot for the CLI.
        write_progress(&progress);

        // Keep a copy for the final summary.
        if let Ok(mut guard) = last_progress.lock() {
            *guard = Some(progress);
        }
    });

    let total = run_scan(config);

    // Log final summary.
    if let Ok(guard) = last_for_final.lock() {
        if let Some(ref p) = *guard {
            info!(
                "Scan complete: {} entries, {} dirs, {} errors, {:.0} entries/s peak",
                p.entries_found, p.dirs_completed, p.errors, p.entries_per_sec
            );
        }
    }

    info!("rfindd scan complete. Total entries indexed: {total}");
    info!("Daemon entering watch mode (event monitoring not yet implemented)");

    // Hold here so systemd sees the daemon as running.
    // In a future milestone this becomes the inotify/fanotify event loop.
    loop {
        std::thread::sleep(std::time::Duration::from_secs(60));
    }
}
