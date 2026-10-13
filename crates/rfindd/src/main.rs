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
//!   SIGUSR1 → force immediate re-index
//!   SIGTERM → clean shutdown
//!
//! # Monitoring
//!   fanotify preferred (bare metal / VM)
//!   inotify fallback (WSL2 / containers)
//!   index-only if both fail (periodic re-index via reindex_interval)

use rfind_config::{load as load_config, write_default, Config, DEFAULT_CONFIG_PATH};
use rfind_core::{
    db::{self, EventType},
    db_path, progress_file_path, DaemonPhase, ScanProgress,
};
use rfind_monitor::Monitor;
use rfind_scanner::{run_scan, EntryInfo, ScanConfig};
use rusqlite::Connection;
use signal_hook::{
    consts::{SIGUSR1, SIGTERM},
    iterator::Signals,
};
use std::{
    fs,
    io::Write,
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
    if let Some(p) = path.parent() { let _ = fs::create_dir_all(p); }
    if let Ok(json) = serde_json::to_string(progress) {
        let tmp = path.with_extension("tmp");
        if let Ok(mut f) = fs::File::create(&tmp) {
            let _ = f.write_all(json.as_bytes());
            let _ = fs::rename(&tmp, &path);
        }
    }
}

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
            entries, dirs, rate, elapsed,
            if errors > 0 { format!(" | {} errors", errors) } else { String::new() }
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
        entries_found:   entries,
        dirs_completed:  dirs,
        dirs_pending:    pending,
        entries_per_sec: rate,
        errors,
        timestamp_secs:  SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        status_line:     status,
        events_pending,
        events_applied,
        monitoring_mode: monitoring_mode.to_string(),
    }
}

fn flush_batch(conn: &Connection, batch: &mut Vec<EntryInfo>) {
    let now = now_secs();
    let tx = match conn.unchecked_transaction() {
        Ok(t) => t,
        Err(e) => { error!("tx error: {e}"); batch.clear(); return; }
    };
    for entry in batch.drain(..) {
        let full_path = entry.path.to_string_lossy().to_string();
        let basename  = entry.path.file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        if let Err(e) = db::insert_entry(
            &tx, None, &basename, &full_path,
            entry.is_dir, entry.mtime_sec, entry.mtime_nsec, now,
        ) {
            error!("insert {full_path}: {e}");
        }
    }
    if let Err(e) = tx.commit() { error!("commit: {e}"); }
}

fn apply_event(conn: &Connection, event: &db::PendingEvent, cfg: &Config) -> bool {
    let excluded = |p: &str| cfg.scan.exclude.iter().any(|ex| Path::new(p).starts_with(ex));
    let result = match &event.event_type {
        EventType::Create => {
            if excluded(&event.path) { return true; }
            let basename = PathBuf::from(&event.path).file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            db::insert_entry(conn, None, &basename, &event.path,
                             event.is_dir, 0, 0, now_secs())
                .map(|_| ())
                .or(Ok(()))
        }
        EventType::Delete => {
            if event.is_dir {
                db::delete_entries_under(conn, &event.path).map(|_| ())
            } else {
                db::delete_entry_by_path(conn, &event.path).map(|_| ())
            }
        }
        EventType::Rename => {
            if let Some(ref new) = event.new_path {
                db::rename_entry(conn, &event.path, new).map(|_| ())
            } else {
                db::delete_entry_by_path(conn, &event.path).map(|_| ())
            }
        }
        EventType::Modify => Ok(()),
    };
    match result {
        Ok(_) => true,
        Err(e) => { error!("apply event on {}: {e}", event.path); false }
    }
}

fn reconcile(conn: &Arc<Mutex<Connection>>, cfg: &Config) -> (u64, u64) {
    let events = {
        let c = conn.lock().unwrap();
        db::load_unapplied_events(&c).unwrap_or_default()
    };
    let total = events.len() as u64;
    if total == 0 { return (0, 0); }
    info!("Reconciling {} events", total);
    let mut applied = 0u64;
    for event in &events {
        let c = conn.lock().unwrap();
        if apply_event(&c, event, cfg) {
            let _ = db::mark_event_applied(&c, event.id);
            applied += 1;
        }
    }
    { let c = conn.lock().unwrap(); let _ = db::prune_applied_events(&c); }
    info!("Reconciled: {applied}/{total}");
    (total, applied)
}

// ---------------------------------------------------------------------------
// Full scan
// ---------------------------------------------------------------------------

fn do_full_scan(
    conn: Arc<Mutex<Connection>>,
    cfg: &Config,
    monitoring_mode: &str,
) -> u64 {
    // Clear existing entries.
    {
        let c = conn.lock().unwrap();
        let _ = c.execute_batch("DELETE FROM entries;");
    }

    let batch_size = cfg.index.batch_size;
    let conn_for_entries = Arc::clone(&conn);
    let conn_for_flush   = Arc::clone(&conn);
    let excludes = cfg.scan.exclude.clone();
    let monitoring_mode = monitoring_mode.to_string();

    let batch: Arc<Mutex<Vec<EntryInfo>>> =
        Arc::new(Mutex::new(Vec::with_capacity(batch_size)));
    let batch_entries = Arc::clone(&batch);
    let batch_flush   = Arc::clone(&batch);

    let scan_cfg = ScanConfig::new(
        cfg.scan.roots.clone(),
        cfg.scan.collect_mtime,
        {
            let conn  = Arc::clone(&conn);
            let mm    = monitoring_mode.clone();
            let excl  = excludes.clone();
            move |mut progress| {
                let ep = conn.lock().ok()
                    .and_then(|c| db::pending_event_count(&c).ok())
                    .unwrap_or(0) as u64;
                progress.events_pending  = ep;
                progress.monitoring_mode = mm.clone();
                info!("{} | {} events queued", progress.status_line, ep);
                write_progress(&progress);
            }
        },
        move |entry| {
            let mut buf = batch_entries.lock().unwrap();
            buf.push(entry);
            if buf.len() >= batch_size {
                let c = conn_for_entries.lock().unwrap();
                flush_batch(&c, &mut buf);
            }
        },
    )
    .with_workers(cfg.scan.workers)
    .with_error_handler(|p, e| warn!("scan error {}: {}", p.display(), e))
    .with_skip_dir({
        let excl = excludes.clone();
        move |p| excl.iter().any(|ex| p.starts_with(ex))
    });

    let total = run_scan(scan_cfg);

    // Flush remaining batch.
    {
        let c = conn_for_flush.lock().unwrap();
        let mut buf = batch_flush.lock().unwrap();
        if !buf.is_empty() { flush_batch(&c, &mut buf); }
    }

    total
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

    // Load config.
    let config_path = PathBuf::from(DEFAULT_CONFIG_PATH);
    if !config_path.exists() {
        if let Err(e) = write_default(&config_path) {
            warn!("Could not write default config: {e}");
        } else {
            info!("Wrote default config to {:?}", config_path);
        }
    }
    let cfg = match load_config(&config_path) {
        Ok(c)  => { info!("Config loaded from {:?}", config_path); c }
        Err(e) => { warn!("Config error: {e} — using defaults"); Config::default() }
    };

    info!("Roots: {:?}", cfg.scan.roots);
    info!("Excludes: {:?}", cfg.scan.exclude);
    info!("Workers: {}", if cfg.scan.workers == 0 { num_cpus::get() } else { cfg.scan.workers });
    info!("Collect mtime: {}", cfg.scan.collect_mtime);
    info!("Reindex interval: {}h", cfg.scan.reindex_interval_hours);

    // Ensure directories exist.
    let db_file = cfg.index.db_path.clone();
    fs::create_dir_all(db_file.parent().unwrap())
        .expect("Failed to create db directory");

    let conn = Arc::new(Mutex::new(
        db::open(&db_file).expect("Failed to open database"),
    ));

    // ── Signal handling ──────────────────────────────────────────────────────
    let force_reindex = Arc::new(AtomicBool::new(false));
    let shutdown      = Arc::new(AtomicBool::new(false));
    {
        let fr = Arc::clone(&force_reindex);
        let sd = Arc::clone(&shutdown);
        let mut signals = Signals::new([SIGUSR1, SIGTERM])
            .expect("Failed to register signals");
        thread::spawn(move || {
            for sig in signals.forever() {
                match sig {
                    SIGUSR1 => { info!("SIGUSR1: forcing re-index"); fr.store(true, Ordering::Relaxed); }
                    SIGTERM => { info!("SIGTERM: shutting down");    sd.store(true, Ordering::Relaxed); }
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
            if last_scan.is_none() { "first run".to_string() }
            else { format!("index age {}s > interval {}s", age_secs, reindex_interval_secs) }
        );
    } else {
        info!("Index age {}s < interval {}s — fast restart, journal only", age_secs, reindex_interval_secs);
    }

    // ── Start monitor ────────────────────────────────────────────────────────
    write_progress(&ScanProgress::new_scanning());

    let dirs_for_inotify: Vec<String> = if !needs_full_scan {
        let c = conn.lock().unwrap();
        db::get_all_dirs(&c).unwrap_or_default()
    } else {
        Vec::new()
    };

    let excludes: Vec<PathBuf> = cfg.scan.exclude.clone();
    let monitor_result = Monitor::start(
        &cfg.scan.roots[0],
        dirs_for_inotify,
        Arc::clone(&conn),
        excludes.clone(),
        cfg.monitor.prefer_fanotify,
        cfg.monitor.inotify_fallback,
    );

    let (monitor_opt, monitoring_mode) = match monitor_result {
        Ok((m, mode)) => {
            info!("Monitoring: {mode}");
            (Some(m), mode)
        }
        Err(e) => {
            warn!("Monitoring unavailable: {e}");
            warn!("Running in index-only mode.");
            (None, "index-only".to_string())
        }
    };

    // ── Scan or fast-restart ─────────────────────────────────────────────────
    let total = if needs_full_scan {
        force_reindex.store(false, Ordering::Relaxed);
        info!("Starting full scan");
        do_full_scan(Arc::clone(&conn), &cfg, &monitoring_mode)
    } else {
        // Fast restart: count existing entries.
        let c = conn.lock().unwrap();
        db::entry_count(&c).unwrap_or(0) as u64
    };

    info!("Index entries: {total}");

    // ── Reconcile ────────────────────────────────────────────────────────────
    {
        let ep = conn.lock().ok()
            .and_then(|c| db::pending_event_count(&c).ok())
            .unwrap_or(0) as u64;
        let p = make_progress(
            DaemonPhase::Reconciling, total, 0, 0, 0.0, 0,
            ep, 0, 0.0, &monitoring_mode,
        );
        write_progress(&p);
        info!("{}", p.status_line);
    }
    let (_, events_applied) = reconcile(&conn, &cfg);

    // ── WAL checkpoint + mark Ready ──────────────────────────────────────────
    {
        let c = conn.lock().unwrap();
        let _ = c.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
        let count = db::entry_count(&c).unwrap_or(0);
        let _ = db::set_metadata(&c, "last_scan_completed", &now_secs().to_string());
        let _ = db::set_metadata(&c, "last_entry_count",   &count.to_string());
        let _ = db::set_metadata(&c, "monitoring_mode",    &monitoring_mode);

        let p = make_progress(
            DaemonPhase::Ready, count as u64, 0, 0, 0.0, 0,
            0, events_applied, 0.0, &monitoring_mode,
        );
        write_progress(&p);
        info!("{}", p.status_line);
    }

    // After scan: if using inotify, register newly discovered directories.
    if let Some(Monitor::Inotify(ref m)) = monitor_opt {
        let c = conn.lock().unwrap();
        if let Ok(dirs) = db::get_all_dirs(&c) {
            info!("Registering {} inotify watches", dirs.len());
            for dir in dirs {
                m.add_watch(&PathBuf::from(dir));
            }
        }
    }

    info!("Daemon ready | monitoring: {monitoring_mode}");

    // ── Steady state loop ────────────────────────────────────────────────────
    let mut tick = 0u64;
    loop {
        std::thread::sleep(Duration::from_secs(5));
        tick += 1;

        if shutdown.load(Ordering::Relaxed) {
            info!("Shutting down cleanly");
            if let Some(m) = monitor_opt { m.stop(); }
            break;
        }

        if force_reindex.load(Ordering::Relaxed) {
            info!("Re-index triggered by SIGUSR1");
            force_reindex.store(false, Ordering::Relaxed);
            let total = do_full_scan(Arc::clone(&conn), &cfg, &monitoring_mode);
            let (_, ea) = reconcile(&conn, &cfg);
            let c = conn.lock().unwrap();
            let count = db::entry_count(&c).unwrap_or(0);
            let _ = db::set_metadata(&c, "last_scan_completed", &now_secs().to_string());
            let p = make_progress(
                DaemonPhase::Ready, count as u64, 0, 0, 0.0, 0,
                0, ea, 0.0, &monitoring_mode,
            );
            write_progress(&p);
            info!("{}", p.status_line);
            continue;
        }

        // Apply pending events.
        let pending = conn.lock().ok()
            .and_then(|c| db::pending_event_count(&c).ok())
            .unwrap_or(0);
        if pending > 0 {
            info!("Applying {} live events", pending);
            reconcile(&conn, &cfg);
        }

        // Periodic re-index in index-only mode.
        if monitor_opt.is_none()
            && cfg.scan.reindex_interval_hours > 0
            && tick % (cfg.scan.reindex_interval_hours * 720) == 0
        {
            info!("Periodic re-index (index-only mode, interval {}h)",
                  cfg.scan.reindex_interval_hours);
            let total = do_full_scan(Arc::clone(&conn), &cfg, &monitoring_mode);
            let (_, ea) = reconcile(&conn, &cfg);
            let c = conn.lock().unwrap();
            let cnt = db::entry_count(&c).unwrap_or(0);
            let _ = db::set_metadata(&c, "last_scan_completed", &now_secs().to_string());
            let p = make_progress(
                DaemonPhase::Ready, cnt as u64, 0, 0, 0.0, 0,
                0, ea, 0.0, &monitoring_mode,
            );
            write_progress(&p);
            info!("{}", p.status_line);
        }

        // Log heartbeat every 5 minutes.
        if tick % 60 == 0 {
            let c = conn.lock().unwrap();
            let cnt = db::entry_count(&c).unwrap_or(0);
            info!("Heartbeat | {cnt} entries | monitoring: {monitoring_mode}");
        }
    }
}

// Bring thread::spawn into scope.
use std::thread;
