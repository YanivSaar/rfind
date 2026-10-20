//! Writing to the index: scanner batches and journaled events.

use crate::lock;
use rfind_config::Config;
use rfind_core::db::{self, EventType};
use rfind_core::log::sanitize_path;
use rfind_core::safewalk::stat_safe;
use rfind_scanner::EntryInfo;
use rusqlite::Connection;
use std::{
    os::unix::fs::MetadataExt,
    path::Path,
    sync::{Arc, Mutex},
};
use tracing::{error, info};

/// Insert a batch of scanned entries in one transaction. Returns the
/// count of entries that could NOT be inserted (SQLITE_FULL, BUSY,
/// constraint violation, commit failure). The caller must treat a
/// nonzero return as a scan integrity failure and skip `sweep_stale`,
/// otherwise rows tagged with the current `scan_gen` are missing but
/// the sweep would still wipe anything that didn't get re-tagged.
#[must_use]
pub fn flush_batch(conn: &Connection, batch: &mut Vec<EntryInfo>, scan_gen: i64) -> usize {
    let mut failed = 0usize;
    let tx = match conn.unchecked_transaction() {
        Ok(t) => t,
        Err(e) => {
            error!("tx error: {e}");
            failed = batch.len();
            batch.clear();
            return failed;
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
                error!("insert {}: {e}", sanitize_path(&entry.path));
                failed += 1;
            }
        }
    }
    if let Err(e) = tx.commit() {
        error!("commit: {e}");
        // Everything in this batch is lost; assume the whole set failed
        // so the caller is appropriately conservative about sweeping.
        failed = failed.max(1);
    }
    failed
}

pub fn is_excluded(cfg: &Config, path: &Path) -> bool {
    cfg.scan.exclude.iter().any(|ex| path.starts_with(ex))
}

/// Make the index agree with the filesystem for one path.
///
/// Uses `stat_safe` so no parent component may be a symlink: previously
/// `fs::symlink_metadata` only AT_SYMLINK_NOFOLLOW'd the leaf, letting
/// an attacker who owned an ancestor directory of a monitored path
/// redirect the replay stat to a different file. On ELOOP (any component
/// is a symlink), treat the entry as absent and delete it from the
/// index — the daemon should not record metadata for attacker-redirected
/// paths.
pub fn sync_path(conn: &Connection, cfg: &Config, path: &str) -> rusqlite::Result<()> {
    let p = Path::new(path);
    if is_excluded(cfg, p) {
        return Ok(());
    }
    match stat_safe(p) {
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
        Err(e) if e.raw_os_error() == Some(libc::ELOOP) => {
            // Treat ELOOP as transient and skip. Deleting on ELOOP would
            // hand attackers a self-erase primitive: an ancestor swap by
            // the directory owner would otherwise erase their own
            // subtree, or (via the Rename reconcile path) erase entries
            // just renamed INTO attacker space by root. The next event
            // or the periodic rescan will reconcile when the symlink is
            // gone.
            Ok(())
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
        let c = lock(conn);
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
        let c = lock(conn);
        let _ = db::prune_applied_events(&c);
        info!("Applied {applied} live events");
    }
    applied
}
