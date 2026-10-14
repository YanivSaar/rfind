//! Writing to the index: scanner batches and journaled events.

use rfind_config::Config;
use rfind_core::db::{self, EventType};
use rfind_scanner::EntryInfo;
use rusqlite::Connection;
use std::{
    fs,
    os::unix::fs::MetadataExt,
    path::Path,
    sync::{Arc, Mutex},
};
use tracing::{error, info};

/// Insert a batch of scanned entries in one transaction.
pub fn flush_batch(conn: &Connection, batch: &mut Vec<EntryInfo>, scan_gen: i64) {
    let tx = match conn.unchecked_transaction() {
        Ok(t) => t,
        Err(e) => {
            error!("tx error: {e}");
            batch.clear();
            return;
        }
    };
    // Group entries by directory: scanner threads interleave directories,
    // and the inserter resolves each directory's names once per run.
    batch.sort_unstable_by(|a, b| a.path.cmp(&b.path));
    {
        let mut ins = db::Inserter::new(&tx);
        for entry in batch.drain(..) {
            let full_path = entry.path.to_string_lossy();
            if let Err(e) = ins.insert(
                &full_path,
                entry.is_dir,
                entry.mtime_sec,
                entry.mtime_nsec,
                entry.size,
                Some(scan_gen),
            ) {
                error!("insert {full_path}: {e}");
            }
        }
    }
    if let Err(e) = tx.commit() {
        error!("commit: {e}");
    }
}

pub fn is_excluded(cfg: &Config, path: &Path) -> bool {
    cfg.scan.exclude.iter().any(|ex| path.starts_with(ex))
}

/// Make the index agree with the filesystem for one path.
pub fn sync_path(conn: &Connection, cfg: &Config, path: &str) -> rusqlite::Result<()> {
    let p = Path::new(path);
    if is_excluded(cfg, p) {
        return Ok(());
    }
    match fs::symlink_metadata(p) {
        Ok(md) => db::insert_entry(
            conn,
            path,
            md.is_dir(),
            md.mtime(),
            md.mtime_nsec(),
            md.len(),
            None,
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            db::delete_entries_under(conn, path).map(|_| ())
        }
        // Permission errors etc.: leave the index as it is.
        Err(_) => Ok(()),
    }
}

fn apply_event(conn: &Connection, event: &db::PendingEvent, cfg: &Config) {
    let result = match (&event.event_type, &event.new_path) {
        (EventType::Rename, Some(new)) => {
            // Move the indexed subtree, then verify both ends.
            db::rename_entry(conn, &event.path, new)
                .and_then(|_| sync_path(conn, cfg, &event.path))
                .and_then(|_| sync_path(conn, cfg, new))
        }
        _ => sync_path(conn, cfg, &event.path),
    };
    if let Err(e) = result {
        error!("apply {:?} on {}: {e}", event.event_type, event.path);
    }
}

/// Apply journaled events in transactions of RECONCILE_BATCH, releasing
/// the DB lock between batches so the event writer and scanners can run.
/// An event is marked applied in the same transaction that applies it.
/// Returns the number of events applied.
pub fn reconcile(conn: &Arc<Mutex<Connection>>, cfg: &Config) -> u64 {
    const RECONCILE_BATCH: usize = 1000;
    let mut applied = 0u64;
    loop {
        let c = conn.lock().unwrap();
        let events = db::load_unapplied_events(&c, RECONCILE_BATCH).unwrap_or_default();
        if events.is_empty() {
            break;
        }
        let tx = match c.unchecked_transaction() {
            Ok(t) => t,
            Err(e) => {
                error!("reconcile tx: {e}");
                break;
            }
        };
        for event in &events {
            apply_event(&tx, event, cfg);
            let _ = db::mark_event_applied(&tx, event.id);
        }
        if let Err(e) = tx.commit() {
            error!("reconcile commit: {e}");
            break;
        }
        applied += events.len() as u64;
    }
    if applied > 0 {
        let c = conn.lock().unwrap();
        let _ = db::prune_applied_events(&c);
        info!("Applied {applied} live events");
    }
    applied
}
