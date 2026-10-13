//! rfindd — rfind indexing daemon.
//!
//! # Startup logic
//!
//! On first run (no existing index):
//!   → full scan
//!
//! On restart with existing index:
//!   → if index age < reindex_interval: replay journal only (fast restart)
//!   → if index age > reindex_interval: full rescan
//!   → always apply unapplied journal events before declaring Ready
//!
//! # Signal handling
//!   SIGUSR1 → force immediate re-index (signals arriving during that
//!             re-index are coalesced into it)
//!   SIGTERM → clean shutdown
//!
//! # Monitoring
//!   fanotify preferred (bare metal / VM)
//!   inotify fallback (WSL2 / containers)
//!   index-only if both fail (periodic re-index via reindex_interval)
//!   An event-queue overflow triggers a live re-index.
//!
//! # Roots
//!   scan.roots          — live: scanned in the foreground, then monitored.
//!   scan.periodic_roots — rescanned in a background thread on their own
//!                         interval, never monitored (e.g. WSL2 /mnt/c).
//!   A root nested in another root is skipped by the outer root's scan.
//!
//! # Scans never empty the index
//!   Each scan upserts rows tagged with a new generation, then sweeps the
//!   rows under its root that it did not see. Searches see the previous
//!   complete index for the whole scan.
//!
//! # Event replay
//!   Journaled events are re-checked against the filesystem when applied
//!   (exists → upsert with real metadata, gone → delete subtree), so
//!   replaying an event twice, or after a fresh scan, is harmless.

use rfind_config::{load as load_config, write_default, Config, DEFAULT_CONFIG_PATH};
use rfind_core::{
    db::{self, EventType},
    mounts, progress_file_path, DaemonPhase, ScanProgress,
};
use rfind_monitor::DirWatcher;
use rfind_monitor::Monitor;
use rfind_scanner::{run_scan, EntryInfo, ScanConfig};
use rusqlite::Connection;
use signal_hook::{
    consts::{SIGTERM, SIGUSR1},
    iterator::Signals,
};
use std::{
    fs,
    io::Write,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, SystemTime},
};
use tracing::{error, info, warn};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn write_progress(progress: &ScanProgress) {
    let path = progress_file_path();
    if let Some(p) = path.parent() {
        let _ = fs::create_dir_all(p);
    }
    if let Ok(json) = serde_json::to_string(progress) {
        let tmp = path.with_extension("tmp");
        if let Ok(mut f) = fs::File::create(&tmp) {
            let _ = f.write_all(json.as_bytes());
            let _ = fs::rename(&tmp, &path);
        }
    }
}

// One argument per ScanProgress field; a builder would add nothing.
#[allow(clippy::too_many_arguments)]
fn make_progress(
    phase: DaemonPhase,
    entries: u64,
    dirs: u64,
    pending: u64,
    rate: f64,
    errors: u64,
    events_pending: u64,
    events_applied: u64,
    elapsed: f64,
    monitoring_mode: &str,
) -> ScanProgress {
    let status = match &phase {
        DaemonPhase::Scanning => format!(
            "Scanning | {} entries | {} dirs | {:.0}/s | {:.1}s{}",
            entries,
            dirs,
            rate,
            elapsed,
            if errors > 0 {
                format!(" | {} errors", errors)
            } else {
                String::new()
            }
        ),
        DaemonPhase::Reconciling => format!(
            "Reconciling | {} events pending | {} applied",
            events_pending, events_applied
        ),
        DaemonPhase::Ready => format!(
            "Ready | {} entries | {} events applied | monitor: {}",
            entries, events_applied, monitoring_mode
        ),
        DaemonPhase::Error(e) => format!("Error: {e}"),
    };
    ScanProgress {
        phase,
        entries_found: entries,
        dirs_completed: dirs,
        dirs_pending: pending,
        entries_per_sec: rate,
        errors,
        timestamp_secs: SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        status_line: status,
        events_pending,
        events_applied,
        monitoring_mode: monitoring_mode.to_string(),
    }
}

fn flush_batch(conn: &Connection, batch: &mut Vec<EntryInfo>, scan_gen: i64) {
    let tx = match conn.unchecked_transaction() {
        Ok(t) => t,
        Err(e) => {
            error!("tx error: {e}");
            batch.clear();
            return;
        }
    };
    {
        let mut ins = db::Inserter::new(&tx);
        for entry in batch.drain(..) {
            let full_path = entry.path.to_string_lossy();
            if let Err(e) = ins.insert(
                &full_path,
                entry.is_dir,
                entry.mtime_sec,
                entry.mtime_nsec,
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

fn is_excluded(cfg: &Config, path: &Path) -> bool {
    cfg.scan.exclude.iter().any(|ex| path.starts_with(ex))
}

/// Make the index agree with the filesystem for one path.
fn sync_path(conn: &Connection, cfg: &Config, path: &str) -> rusqlite::Result<()> {
    let p = Path::new(path);
    if is_excluded(cfg, p) {
        return Ok(());
    }
    match fs::symlink_metadata(p) {
        Ok(md) => db::insert_entry(conn, path, md.is_dir(), md.mtime(), md.mtime_nsec(), None),
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
fn reconcile(conn: &Arc<Mutex<Connection>>, cfg: &Config) -> (u64, u64) {
    const RECONCILE_BATCH: usize = 1000;
    let mut applied = 0u64;
    loop {
        let c = conn.lock().unwrap();
        let events = db::load_unapplied_events(&c, RECONCILE_BATCH).unwrap_or_default();
        if events.is_empty() {
            break;
        }
        if applied == 0 {
            info!("Reconciling events");
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
        info!("Reconciled {applied} events");
    }
    (applied, applied)
}

// ---------------------------------------------------------------------------
// Scanning
// ---------------------------------------------------------------------------

fn periodic_paths(cfg: &Config) -> Vec<PathBuf> {
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

fn next_scan_gen(conn: &Arc<Mutex<Connection>>) -> i64 {
    let c = conn.lock().unwrap();
    let g = db::get_metadata(&c, "scan_gen")
        .ok()
        .flatten()
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(0)
        + 1;
    let _ = db::set_metadata(&c, "scan_gen", &g.to_string());
    g
}

fn root_scan_key(root: &Path) -> String {
    format!("last_scan:{}", root.display())
}

/// Scan one root into the index without emptying it first.
///
/// `watcher`: register an inotify watch on each directory before it is
/// read, so changes made during the scan are journaled.
/// `report`: write scan progress to the progress file (foreground scans).
fn scan_root(
    conn: &Arc<Mutex<Connection>>,
    cfg: &Config,
    root: &Path,
    watcher: Option<DirWatcher>,
    report: bool,
    monitoring_mode: &str,
) -> u64 {
    let scan_gen = next_scan_gen(conn);
    let nested = nested_roots(cfg, root);
    let mut skip: Vec<PathBuf> = cfg.scan.exclude.clone();
    skip.extend(nested.iter().cloned());

    if let Some(w) = &watcher {
        w.watch(root);
    }

    let batch_size = cfg.index.batch_size;
    let batch: Arc<Mutex<Vec<EntryInfo>>> = Arc::new(Mutex::new(Vec::with_capacity(batch_size)));

    let scan_cfg = ScanConfig::new(
        vec![root.to_path_buf()],
        cfg.scan.collect_mtime,
        {
            let conn = Arc::clone(conn);
            let mm = monitoring_mode.to_string();
            move |mut progress| {
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
                progress.phase = DaemonPhase::Scanning;
                progress.events_pending = ep;
                progress.monitoring_mode = mm.clone();
                info!("{} | {} events queued", progress.status_line, ep);
                write_progress(&progress);
            }
        },
        {
            let conn = Arc::clone(conn);
            let batch = Arc::clone(&batch);
            move |entry: EntryInfo| {
                // Watch before the scanner reads this directory.
                if entry.is_dir {
                    if let Some(w) = &watcher {
                        w.watch(&entry.path);
                    }
                }
                // Swap the full batch out so the batch lock is never held
                // while waiting for the DB lock.
                let full = {
                    let mut buf = batch.lock().unwrap();
                    buf.push(entry);
                    if buf.len() < batch_size {
                        return;
                    }
                    std::mem::replace(&mut *buf, Vec::with_capacity(batch_size))
                };
                let mut full = full;
                flush_batch(&conn.lock().unwrap(), &mut full, scan_gen);
            }
        },
    )
    .with_workers(cfg.scan.workers)
    .with_error_handler(|p, e| warn!("scan error {}: {}", p.display(), e))
    .with_skip_dir(move |p| skip.iter().any(|ex| p.starts_with(ex)));

    let total = run_scan(scan_cfg);

    let mut rest = std::mem::take(&mut *batch.lock().unwrap());
    let c = conn.lock().unwrap();
    if !rest.is_empty() {
        flush_batch(&c, &mut rest, scan_gen);
    }

    let keep: Vec<String> = nested
        .iter()
        .map(|p| p.to_string_lossy().to_string())
        .collect();
    match db::sweep_stale(&c, &root.to_string_lossy(), &keep, scan_gen) {
        Ok(n) if n > 0 => info!("Removed {n} stale entries under {}", root.display()),
        Ok(_) => {}
        Err(e) => error!("sweep {}: {e}", root.display()),
    }
    let _ = db::set_metadata(&c, &root_scan_key(root), &now_secs().to_string());
    total
}

/// Rescan every live root in the foreground, reconcile, and mark Ready.
fn live_reindex(
    conn: &Arc<Mutex<Connection>>,
    cfg: &Config,
    watcher: Option<&DirWatcher>,
    monitoring_mode: &str,
) {
    for root in &cfg.scan.roots {
        info!("Scanning live root {}", root.display());
        scan_root(conn, cfg, root, watcher.cloned(), true, monitoring_mode);
    }
    reconcile_and_mark_ready(conn, cfg, monitoring_mode, true);
}

/// `scanned`: the live roots were just fully scanned. A fast restart only
/// replays the journal, so it must not reset the index age — otherwise
/// frequent restarts would postpone the periodic full rescan forever.
fn reconcile_and_mark_ready(
    conn: &Arc<Mutex<Connection>>,
    cfg: &Config,
    monitoring_mode: &str,
    scanned: bool,
) {
    {
        let ep = conn
            .lock()
            .ok()
            .and_then(|c| db::pending_event_count(&c).ok())
            .unwrap_or(0) as u64;
        let total = conn
            .lock()
            .ok()
            .and_then(|c| db::entry_count(&c).ok())
            .unwrap_or(0) as u64;
        let p = make_progress(
            DaemonPhase::Reconciling,
            total,
            0,
            0,
            0.0,
            0,
            ep,
            0,
            0.0,
            monitoring_mode,
        );
        write_progress(&p);
        info!("{}", p.status_line);
    }
    let (_, events_applied) = reconcile(conn, cfg);

    let c = conn.lock().unwrap();
    let _ = c.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
    let count = db::entry_count(&c).unwrap_or(0);
    if scanned {
        let _ = db::set_metadata(&c, "last_scan_completed", &now_secs().to_string());
    }
    let _ = db::set_metadata(&c, "last_entry_count", &count.to_string());
    let _ = db::set_metadata(&c, "monitoring_mode", monitoring_mode);
    let p = make_progress(
        DaemonPhase::Ready,
        count as u64,
        0,
        0,
        0.0,
        0,
        0,
        events_applied,
        0.0,
        monitoring_mode,
    );
    write_progress(&p);
    info!("{}", p.status_line);
}

/// First periodic root whose interval has elapsed (or never scanned).
fn due_periodic_root(conn: &Arc<Mutex<Connection>>, cfg: &Config) -> Option<PathBuf> {
    let c = conn.lock().unwrap();
    let now = now_secs();
    cfg.scan
        .periodic_roots
        .iter()
        .find(|r| {
            if !r.path.is_dir() {
                return false;
            } // unmounted: keep the old entries
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
}

/// Warn about Windows drives (WSL2 drvfs) that would be monitored live:
/// inotify never sees changes made from Windows, and watching a whole
/// drive can exhaust the inotify watch limit.
fn warn_drvfs_under_live_roots(cfg: &Config, mounts: &[mounts::MountEntry]) {
    let handled: Vec<PathBuf> = cfg
        .scan
        .exclude
        .iter()
        .cloned()
        .chain(periodic_paths(cfg))
        .collect();
    for m in mounts.iter().filter(|m| m.is_drvfs()) {
        let live = cfg.scan.roots.iter().any(|r| m.point.starts_with(r));
        if live && !handled.iter().any(|h| m.point.starts_with(h)) {
            warn!(
                "{} is a Windows drive under a live root: inotify cannot see changes \
                 made from Windows. Add it to scan.periodic_roots (or scan.exclude).",
                m.point.display()
            );
        }
    }
}

/// Make the index directory and files readable by root only: the index
/// lists every file name on the system.
fn restrict_index_permissions(db_file: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Some(dir) = db_file.parent() {
        let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
    }
    for suffix in ["", "-wal", "-shm"] {
        let mut p = db_file.as_os_str().to_owned();
        p.push(suffix);
        let _ = fs::set_permissions(&p, fs::Permissions::from_mode(0o600));
    }
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn main() {
    tracing_subscriber::fmt()
        .with_target(false)
        .with_level(true)
        .init();

    info!("rfindd starting");

    // Everything rfindd creates (index, WAL, progress file) is root-only.
    unsafe { libc::umask(0o077) };

    // Load config.
    let config_path = PathBuf::from(DEFAULT_CONFIG_PATH);
    if !config_path.exists() {
        if let Err(e) = write_default(&config_path) {
            warn!("Could not write default config: {e}");
        } else {
            info!("Wrote default config to {:?}", config_path);
        }
    }
    // A broken config must not silently fall back to defaults: those could
    // index (and live-monitor) paths the admin deliberately excluded.
    let mut cfg = match load_config(&config_path) {
        Ok(c) => {
            info!("Config loaded from {:?}", config_path);
            c
        }
        Err(e) => {
            error!("Config error: {e}");
            std::process::exit(1);
        }
    };

    // Skip bind-mount aliases of filesystems that are indexed elsewhere.
    let mount_table = mounts::read_mountinfo();
    for (alias, canonical) in mounts::alias_mounts(&mount_table) {
        if is_excluded(&cfg, &alias) {
            continue;
        }
        info!(
            "Skipping {} (bind mount; same files as {})",
            alias.display(),
            canonical.display()
        );
        cfg.scan.exclude.push(alias);
    }

    info!("Roots: {:?}", cfg.scan.roots);
    for r in &cfg.scan.periodic_roots {
        info!(
            "Periodic root: {} (every {}h)",
            r.path.display(),
            r.interval_hours
        );
    }
    info!("Excludes: {:?}", cfg.scan.exclude);
    info!(
        "Workers: {}",
        if cfg.scan.workers == 0 {
            num_cpus::get()
        } else {
            cfg.scan.workers
        }
    );
    info!("Collect mtime: {}", cfg.scan.collect_mtime);
    info!("Reindex interval: {}h", cfg.scan.reindex_interval_hours);
    warn_drvfs_under_live_roots(&cfg, &mount_table);

    // Ensure directories exist.
    let db_file = cfg.index.db_path.clone();
    fs::create_dir_all(db_file.parent().unwrap()).expect("Failed to create db directory");

    let conn = Arc::new(Mutex::new(
        db::open(&db_file).expect("Failed to open database"),
    ));
    restrict_index_permissions(&db_file);

    // ── Signal handling ──────────────────────────────────────────────────────
    let force_reindex = Arc::new(AtomicBool::new(false));
    let shutdown = Arc::new(AtomicBool::new(false));
    {
        let fr = Arc::clone(&force_reindex);
        let sd = Arc::clone(&shutdown);
        let mut signals = Signals::new([SIGUSR1, SIGTERM]).expect("Failed to register signals");
        thread::spawn(move || {
            for sig in signals.forever() {
                match sig {
                    SIGUSR1 => {
                        info!("SIGUSR1: forcing re-index");
                        fr.store(true, Ordering::Relaxed);
                    }
                    SIGTERM => {
                        info!("SIGTERM: shutting down");
                        sd.store(true, Ordering::Relaxed);
                    }
                    _ => {}
                }
            }
        });
    }

    // ── Decide: full scan or journal-only restart ────────────────────────────
    let last_scan = {
        let c = conn.lock().unwrap();
        db::get_metadata(&c, "last_scan_completed")
            .ok()
            .flatten()
            .and_then(|v| v.parse::<i64>().ok())
    };

    let reindex_interval_secs = cfg.scan.reindex_interval_hours as i64 * 3600;
    let age_secs = last_scan.map(|t| now_secs() - t).unwrap_or(i64::MAX);

    let needs_full_scan = last_scan.is_none()
        || force_reindex.load(Ordering::Relaxed)
        || (reindex_interval_secs > 0 && age_secs > reindex_interval_secs);

    if needs_full_scan {
        info!(
            "Full scan needed ({})",
            if last_scan.is_none() {
                "first run".to_string()
            } else {
                format!(
                    "index age {}s > interval {}s",
                    age_secs, reindex_interval_secs
                )
            }
        );
    } else {
        info!(
            "Index age {}s < interval {}s — fast restart, journal only",
            age_secs, reindex_interval_secs
        );
    }

    // ── Start monitor ────────────────────────────────────────────────────────
    write_progress(&ScanProgress::new_scanning());

    // Periodic roots are never monitored.
    let mut monitor_excludes: Vec<PathBuf> = cfg.scan.exclude.clone();
    monitor_excludes.extend(periodic_paths(&cfg));

    // Fast restart: watch every indexed live directory up front. A full
    // scan instead registers watches as it discovers directories.
    let dirs_for_inotify: Vec<String> = if !needs_full_scan {
        let c = conn.lock().unwrap();
        db::get_all_dirs(&c)
            .unwrap_or_default()
            .into_iter()
            .filter(|d| {
                !monitor_excludes
                    .iter()
                    .any(|ex| Path::new(d).starts_with(ex))
            })
            .collect()
    } else {
        Vec::new()
    };

    let (mut mark_points, unmonitored) =
        mounts::fanotify_mark_points(&mount_table, &cfg.scan.roots, &monitor_excludes);
    // Files bind-mounted over files (e.g. WSL2's /init) can't be marked.
    mark_points.retain(|p| p.is_dir());
    for m in &unmonitored {
        if !m.is_drvfs() && m.point.is_dir() {
            info!(
                "{} ({}) is indexed but not monitored live; consider scan.periodic_roots",
                m.point.display(),
                m.fstype
            );
        }
    }

    let monitor_result = if cfg.scan.roots.is_empty() {
        Err(rfind_monitor::MonitorError::NotSupported)
    } else {
        Monitor::start(
            &cfg.scan.roots,
            &mark_points,
            dirs_for_inotify,
            Arc::clone(&conn),
            monitor_excludes,
            cfg.monitor.prefer_fanotify,
            cfg.monitor.inotify_fallback,
        )
    };

    let (monitor_opt, monitoring_mode) = match monitor_result {
        Ok((m, mode)) => {
            info!("Monitoring: {mode}");
            (Some(m), mode)
        }
        Err(_) if cfg.scan.roots.is_empty() => {
            info!("No live roots — periodic roots only");
            (None, "periodic-only".to_string())
        }
        Err(e) => {
            warn!("Monitoring unavailable: {e}");
            warn!("Running in index-only mode.");
            (None, "index-only".to_string())
        }
    };
    let watcher = monitor_opt.as_ref().and_then(|m| m.dir_watcher());

    // ── Scan or fast-restart, then reconcile ─────────────────────────────────
    if needs_full_scan {
        force_reindex.store(false, Ordering::Relaxed);
        info!("Starting full scan");
        live_reindex(&conn, &cfg, watcher.as_ref(), &monitoring_mode);
    } else {
        reconcile_and_mark_ready(&conn, &cfg, &monitoring_mode, false);
    }

    info!("Daemon ready | monitoring: {monitoring_mode}");

    // ── Steady state loop ────────────────────────────────────────────────────
    // Only one background (periodic-root) scan runs at a time.
    let bg_scanning = Arc::new(AtomicBool::new(false));
    let mut tick = 0u64;
    loop {
        std::thread::sleep(Duration::from_secs(5));
        tick += 1;

        if shutdown.load(Ordering::Relaxed) {
            info!("Shutting down cleanly");
            if let Some(m) = monitor_opt {
                m.stop();
            }
            break;
        }

        let overflowed = monitor_opt.as_ref().is_some_and(|m| m.take_overflow());
        let forced = force_reindex.swap(false, Ordering::Relaxed);
        if forced {
            // Mark every periodic root due; the background scheduler
            // picks them up after the live re-index.
            let c = conn.lock().unwrap();
            for r in &cfg.scan.periodic_roots {
                let _ = db::set_metadata(&c, &root_scan_key(&r.path), "0");
            }
        }
        if forced || overflowed {
            info!(
                "Re-index triggered by {}",
                if forced { "SIGUSR1" } else { "event overflow" }
            );
            live_reindex(&conn, &cfg, watcher.as_ref(), &monitoring_mode);
            // This re-index already covers any SIGUSR1 that arrived
            // while it ran.
            force_reindex.store(false, Ordering::Relaxed);
            continue;
        }

        // Apply pending events.
        let pending = conn
            .lock()
            .ok()
            .and_then(|c| db::pending_event_count(&c).ok())
            .unwrap_or(0);
        if pending > 0 {
            info!("Applying {} live events", pending);
            reconcile(&conn, &cfg);
        }

        // Periodic re-index in index-only mode.
        if monitor_opt.is_none()
            && cfg.scan.reindex_interval_hours > 0
            && tick.is_multiple_of(cfg.scan.reindex_interval_hours * 720)
        {
            info!(
                "Periodic re-index (index-only mode, interval {}h)",
                cfg.scan.reindex_interval_hours
            );
            live_reindex(&conn, &cfg, None, &monitoring_mode);
        }

        // Periodic roots: rescan in the background so live events keep
        // being applied while a slow drive is walked.
        if !bg_scanning.load(Ordering::Acquire) {
            if let Some(root) = due_periodic_root(&conn, &cfg) {
                bg_scanning.store(true, Ordering::Release);
                let (conn, cfg, mm, flag) = (
                    Arc::clone(&conn),
                    cfg.clone(),
                    monitoring_mode.clone(),
                    Arc::clone(&bg_scanning),
                );
                thread::spawn(move || {
                    info!("Background scan of periodic root {}", root.display());
                    let t = std::time::Instant::now();
                    let n = scan_root(&conn, &cfg, &root, None, false, &mm);
                    info!(
                        "Background scan of {} done: {n} entries in {:.1}s",
                        root.display(),
                        t.elapsed().as_secs_f64()
                    );
                    flag.store(false, Ordering::Release);
                });
            }
        }

        // Log heartbeat every 5 minutes.
        if tick.is_multiple_of(60) {
            let c = conn.lock().unwrap();
            let cnt = db::entry_count(&c).unwrap_or(0);
            info!("Heartbeat | {cnt} entries | monitoring: {monitoring_mode}");
        }
    }
}

// Bring thread::spawn into scope.
use std::thread;
