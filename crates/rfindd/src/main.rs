//! rfindd — rfind indexing daemon.

use rfind_core::{db, db_path, progress_file_path, ScanProgress};
use rfind_scanner::{run_scan, ScanConfig};
use rusqlite::Connection;
use std::{
    fs,
    io::Write,
    path::PathBuf,
    sync::{Arc, Mutex},
};
use tracing::{error, info, warn};

/// Paths to skip entirely — not indexed, not recursed into.
/// These are pseudo-filesystems, volatile kernel interfaces, and
/// Windows/WSL interop mounts that have no useful Linux file data.
const DEFAULT_EXCLUDES: &[&str] = &[
    "/proc",
    "/sys",
    "/dev",
    "/run",
    "/mnt",
    "/media",
];

fn is_excluded(path: &PathBuf) -> bool {
    DEFAULT_EXCLUDES.iter().any(|ex| path.starts_with(ex))
}

fn write_progress(progress: &ScanProgress) {
    let path = progress_file_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string(progress) {
        let tmp = path.with_extension("tmp");
        if let Ok(mut f) = fs::File::create(&tmp) {
            let _ = f.write_all(json.as_bytes());
            let _ = fs::rename(&tmp, &path);
        }
    }
}

fn flush_batch(conn: &Connection, batch: &mut Vec<(Option<i64>, String, String, bool)>) {
    let tx = match conn.unchecked_transaction() {
        Ok(t) => t,
        Err(e) => { error!("Failed to begin transaction: {e}"); batch.clear(); return; }
    };
    for (parent_id, basename, full_path, is_dir) in batch.drain(..) {
        if let Err(e) = db::insert_entry(&tx, parent_id, &basename, &full_path, is_dir) {
            error!("Insert error for {full_path}: {e}");
        }
    }
    if let Err(e) = tx.commit() {
        error!("Transaction commit failed: {e}");
    }
}

fn main() {
    tracing_subscriber::fmt()
        .with_target(false)
        .with_level(true)
        .init();

    info!("rfindd starting");

    let db_file = db_path();
    if let Some(parent) = db_file.parent() {
        fs::create_dir_all(parent).expect("Failed to create /var/lib/rfindd");
    }

    let conn = db::open(&db_file).expect("Failed to open index database");
    conn.execute_batch("DELETE FROM entries;").expect("Failed to clear entries");
    info!("Database opened at {:?}", db_file);
    info!("Excluding paths: {:?}", DEFAULT_EXCLUDES);

    let conn = Arc::new(Mutex::new(conn));
    let conn_for_entries = Arc::clone(&conn);
    let conn_for_flush = Arc::clone(&conn);
    let conn_for_checkpoint = Arc::clone(&conn);

    const BATCH_SIZE: usize = 10_000;
    let batch: Arc<Mutex<Vec<(Option<i64>, String, String, bool)>>> =
        Arc::new(Mutex::new(Vec::with_capacity(BATCH_SIZE)));
    let batch_for_entries = Arc::clone(&batch);
    let batch_for_flush = Arc::clone(&batch);

    let roots: Vec<PathBuf> = vec![PathBuf::from("/")];
    info!("Scanning roots: {:?}", roots);

    write_progress(&ScanProgress::new_scanning());

    let config = ScanConfig::new(
        roots,
        move |progress| {
            info!("{}", progress.status_line);
            write_progress(&progress);
        },
        move |path, is_dir| {
            // Skip excluded paths: don't index them and don't recurse.
            // This catches both top-level excludes AND their subdirectories
            // since we check every entry before inserting or re-queuing.
            if is_excluded(path) {
                return;
            }

            let full_path = path.to_string_lossy().to_string();
            let basename = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();

            let mut buf = batch_for_entries.lock().unwrap();
            buf.push((None, basename, full_path, is_dir));

            if buf.len() >= BATCH_SIZE {
                let conn = conn_for_entries.lock().unwrap();
                flush_batch(&conn, &mut buf);
            }
        },
    )
    .with_error_handler(|path, err| {
        warn!("Cannot read dir {}: {}", path.display(), err);
    })
    .with_skip_dir(|path| {
        DEFAULT_EXCLUDES.iter().any(|ex| path.starts_with(ex))
    });

    let total = run_scan(config);

    // Flush remaining entries.
    {
        let conn = conn_for_flush.lock().unwrap();
        let mut buf = batch_for_flush.lock().unwrap();
        if !buf.is_empty() {
            info!("Flushing final batch of {} entries", buf.len());
            flush_batch(&conn, &mut buf);
        }
    }

    // Checkpoint WAL into main db so reads are fast.
    info!("Checkpointing WAL...");
    {
        let conn = conn_for_checkpoint.lock().unwrap();
        match conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);") {
            Ok(_) => info!("WAL checkpoint complete"),
            Err(e) => warn!("WAL checkpoint failed: {e}"),
        }
    }

    {
        let conn = conn.lock().unwrap();
        match db::entry_count(&conn) {
            Ok(n) => info!("Index complete: {n} entries in database"),
            Err(e) => error!("Count query failed: {e}"),
        }
    }

    info!("Total entries discovered: {total}");
    info!("Daemon holding — inotify event monitoring coming next");

    loop {
        std::thread::sleep(std::time::Duration::from_secs(60));
    }
}
