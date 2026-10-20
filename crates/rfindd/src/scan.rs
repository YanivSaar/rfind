//! Scanning roots into the index, and the daemon context they share.

use crate::{index, lock, progress};
use rfind_config::Config;
use rfind_core::{db, log::sanitize_path, DaemonPhase};
use rfind_monitor::DirWatcher;
use rfind_scanner::{run_scan, EntryInfo, ScanConfig, ScanResult};
use rusqlite::Connection;
use std::{
    path::{Path, PathBuf},
    sync::{atomic::AtomicBool, Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};
use tracing::{error, info, warn};

/// What scans, reconciles and background jobs need. Cheap to clone.
#[derive(Clone)]
pub struct Ctx {
    pub conn: Arc<Mutex<Connection>>,
    pub cfg: Arc<Config>,
    /// Monitoring mode for status output ("fanotify", "inotify", ...).
    pub mode: String,
    /// inotify only: register a watch on each directory before it is read.
    pub watcher: Option<DirWatcher>,
    /// Set on shutdown or reload: running scans stop early.
    pub cancel: Arc<AtomicBool>,
}

pub fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

pub fn periodic_paths(cfg: &Config) -> Vec<PathBuf> {
    cfg.scan
        .periodic_roots
        .iter()
        .map(|r| r.path.clone())
        .collect()
}

/// Other configured roots strictly inside `root`; each is maintained by
/// its own scan, so `root`'s scan skips them and its sweep keeps them.
fn nested_roots(cfg: &Config, root: &Path) -> Vec<PathBuf> {
    cfg.scan
        .roots
        .iter()
        .cloned()
        .chain(periodic_paths(cfg))
        .filter(|r| r != root && r.starts_with(root))
        .collect()
}

fn next_scan_gen(conn: &Mutex<Connection>) -> i64 {
    let c = lock(conn);
    let g = db::get_metadata(&c, "scan_gen")
        .ok()
        .flatten()
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(0)
        + 1;
    let _ = db::set_metadata(&c, "scan_gen", &g.to_string());
    g
}

pub fn root_scan_key(root: &Path) -> String {
    format!("last_scan:{}", root.display())
}

/// Scan one root into the index without emptying it first: rows are
/// upserted with a new generation, then rows under the root that the scan
/// didn't see are swept. A cancelled scan keeps what it inserted but
/// sweeps nothing (it didn't see everything).
///
/// `report`: write scan progress to the progress file (foreground scans).
pub fn scan_root(ctx: &Ctx, root: &Path, report: bool) -> ScanResult {
    let cfg = &ctx.cfg;
    let scan_gen = next_scan_gen(&ctx.conn);
    let nested = nested_roots(cfg, root);
    let mut skip: Vec<PathBuf> = cfg.scan.exclude.clone();
    skip.extend(nested.iter().cloned());

    if let Some(w) = &ctx.watcher {
        w.watch(root);
    }

    let batch_size = cfg.index.batch_size;
    let batch: Arc<Mutex<Vec<EntryInfo>>> = Arc::new(Mutex::new(Vec::with_capacity(batch_size)));

    let scan_cfg = ScanConfig::new(
        vec![root.to_path_buf()],
        cfg.scan.collect_mtime,
        {
            let conn = Arc::clone(&ctx.conn);
            let mode = ctx.mode.clone();
            move |mut p| {
                if !report {
                    return;
                }
                let ep = conn
                    .lock()
                    .ok()
                    .and_then(|c| db::pending_event_count(&c).ok())
                    .unwrap_or(0) as u64;
                // The scanner reports Ready when it finishes, but the
                // daemon is not ready until events are reconciled.
                p.phase = DaemonPhase::Scanning;
                p.events_pending = ep;
                p.monitoring_mode = mode.clone();
                info!("{} | {} events queued", p.status_line, ep);
                progress::write(&p);
            }
        },
        {
            let conn = Arc::clone(&ctx.conn);
            let batch = Arc::clone(&batch);
            let watcher = ctx.watcher.clone();
            move |entry: EntryInfo| {
                // Watch before the scanner reads this directory.
                if entry.is_dir {
                    if let Some(w) = &watcher {
                        w.watch(&entry.path);
                    }
                }
                // Swap the full batch out so the batch lock is never held
                // while waiting for the DB lock.
                let mut full = {
                    let mut buf = batch.lock().unwrap();
                    buf.push(entry);
                    if buf.len() < batch_size {
                        return;
                    }
                    std::mem::replace(&mut *buf, Vec::with_capacity(batch_size))
                };
                index::flush_batch(&lock(&conn), &mut full, scan_gen);
            }
        },
    )
    .with_workers(cfg.scan.workers)
    .with_io_uring(cfg.scan.io_uring)
    .with_cancel(Arc::clone(&ctx.cancel))
    .with_error_handler(|p, e| warn!("scan error {}: {}", sanitize_path(p), e))
    .with_skip_dir(move |p| skip.iter().any(|ex| p.starts_with(ex)))
    .with_max_depth(if cfg.scan.max_depth == 0 {
        None
    } else {
        Some(cfg.scan.max_depth)
    });

    let result = run_scan(scan_cfg);

    let mut rest = std::mem::take(&mut *batch.lock().unwrap());
    let c = lock(&ctx.conn);
    if !rest.is_empty() {
        index::flush_batch(&c, &mut rest, scan_gen);
    }
    if result.cancelled {
        info!(
            "Scan of {} cancelled after {} entries",
            sanitize_path(root),
            result.entries
        );
        return result;
    }

    let keep: Vec<String> = nested
        .iter()
        .map(|p| p.to_string_lossy().to_string())
        .collect();
    match db::sweep_stale(&c, &root.to_string_lossy(), &keep, scan_gen) {
        Ok(n) if n > 0 => info!("Removed {n} stale entries under {}", sanitize_path(root)),
        Ok(_) => {}
        Err(e) => error!("sweep {}: {e}", sanitize_path(root)),
    }
    let _ = db::set_metadata(&c, &root_scan_key(root), &now_secs().to_string());
    if !report && !ctx.cancel.load(std::sync::atomic::Ordering::Relaxed) {
        // Background scans end without reconcile_and_mark_ready. Skip
        // compact on cancel: it holds the DB mutex and would otherwise
        // make `systemctl reload`/`stop` block for seconds on a big DB.
        if let Err(e) = db::compact(&c) {
            warn!("compacting the index: {e}");
        }
    }
    // Persist the error count so `rfind status` / future reporting can
    // show "last scan skipped N directories" instead of forcing the
    // operator to grep journalctl. One line per scan, keyed per root.
    let _ = db::set_metadata(
        &c,
        &format!("last_scan_errors:{}", root.display()),
        &result.errors.to_string(),
    );
    info!(
        "Scanned {}: {} entries, {} dirs, {} errors in {:.1}s{}",
        sanitize_path(root),
        result.entries,
        result.dirs,
        result.errors,
        result.elapsed.as_secs_f64(),
        if result.io_uring { " (io_uring)" } else { "" }
    );
    result
}

/// Rescan every live root in the foreground, reconcile, and mark Ready.
/// Returns false if cancelled.
pub fn live_reindex(ctx: &Ctx) -> bool {
    for root in &ctx.cfg.scan.roots {
        info!("Scanning live root {}", sanitize_path(root));
        if scan_root(ctx, root, true).cancelled {
            return false;
        }
    }
    reconcile_and_mark_ready(ctx, true);
    true
}

/// `scanned`: the live roots were just fully scanned. A fast restart only
/// replays the journal, so it must not reset the index age — otherwise
/// frequent restarts would postpone the periodic full rescan forever.
pub fn reconcile_and_mark_ready(ctx: &Ctx, scanned: bool) {
    let (total, ep) = {
        let c = lock(&ctx.conn);
        (
            db::entry_count(&c).unwrap_or(0) as u64,
            db::pending_event_count(&c).unwrap_or(0) as u64,
        )
    };
    let p = progress::status(DaemonPhase::Reconciling, total, ep, 0, &ctx.mode);
    progress::write(&p);
    info!("{}", p.status_line);

    let applied = index::reconcile(&ctx.conn, &ctx.cfg);

    let c = lock(&ctx.conn);
    if let Err(e) = db::compact(&c) {
        warn!("compacting the index: {e}");
    }
    let count = db::entry_count(&c).unwrap_or(0) as u64;
    if scanned {
        let _ = db::set_metadata(&c, "last_scan_completed", &now_secs().to_string());
    }
    let _ = db::set_metadata(&c, "last_entry_count", &count.to_string());
    let _ = db::set_metadata(&c, "monitoring_mode", &ctx.mode);
    let p = progress::status(DaemonPhase::Ready, count, 0, applied, &ctx.mode);
    progress::write(&p);
    info!("{}", p.status_line);
}

/// Periodic roots whose interval has elapsed (or never scanned).
pub fn due_periodic_roots(ctx: &Ctx) -> Vec<PathBuf> {
    let c = lock(&ctx.conn);
    let now = now_secs();
    ctx.cfg
        .scan
        .periodic_roots
        .iter()
        .filter(|r| {
            if !r.path.is_dir() {
                return false; // unmounted: keep the old entries
            }
            let last = db::get_metadata(&c, &root_scan_key(&r.path))
                .ok()
                .flatten()
                .and_then(|v| v.parse::<i64>().ok());
            match last {
                None => true,
                Some(t) => r.interval_hours > 0 && now - t >= r.interval_hours as i64 * 3600,
            }
        })
        .map(|r| r.path.clone())
        .collect()
}
