//! rfindd — rfind indexing daemon.
//!
//! State machine: Scanning → Reconciling → Ready
//!
//! fanotify is attempted first. On WSL2 or restricted environments where
//! fanotify is unavailable, the daemon logs a warning and continues without
//! live monitoring. The index is still built and fully searchable.
//!
//! Environment behaviour:
//!   Bare metal / full VM : fanotify monitors entire filesystem via one mount watch
//!   WSL2                 : fanotify may be restricted; falls back to index-only mode
//!   Container (no CAP_SYS_ADMIN): fanotify unavailable; index-only mode

use rfind_core::{
    db::{self, EventType},
    db_path, progress_file_path, DaemonPhase, ScanProgress,
};
use rfind_monitor::Monitor;
use rfind_scanner::{run_scan, ScanConfig};
use rusqlite::Connection;
use std::{
    fs,
    io::Write,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime},
};
use tracing::{error, info, warn};

const DEFAULT_EXCLUDES: &[&str] = &[
    "/proc", "/sys", "/dev", "/run", "/mnt", "/media",
];

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

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
) -> ScanProgress {
    let status = match &phase {
        DaemonPhase::Scanning => format!(
            "Scanning | {} entries | {} dirs | {:.0} entries/s | {:.1}s elapsed{}",
            entries, dirs, rate, elapsed,
            if errors > 0 { format!(" | {} errors", errors) } else { String::new() }
        ),
        DaemonPhase::Reconciling => format!(
            "Reconciling | {} events pending | {} applied",
            events_pending, events_applied
        ),
        DaemonPhase::Ready => format!(
            "Ready | {} entries indexed | {} events applied",
            entries, events_applied
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
    }
}

fn apply_event(conn: &Connection, event: &db::PendingEvent) -> bool {
    let result = match &event.event_type {
        EventType::Create => {
            if !is_excluded(&PathBuf::from(&event.path)) {
                let basename = PathBuf::from(&event.path)
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                db::insert_entry(conn, None, &basename, &event.path, event.is_dir)
                    .map(|_| ())
                    .or(Ok(()))
            } else {
                Ok(())
            }
        }
        EventType::Delete => {
            if event.is_dir {
                db::delete_entries_under(conn, &event.path).map(|_| ())
            } else {
                db::delete_entry_by_path(conn, &event.path).map(|_| ())
            }
        }
        EventType::Rename => {
            if let Some(ref new_path) = event.new_path {
                db::rename_entry(conn, &event.path, new_path).map(|_| ())
            } else {
                db::delete_entry_by_path(conn, &event.path).map(|_| ())
            }
        }
        EventType::Modify => Ok(()),
    };

    match result {
        Ok(_) => true,
        Err(e) => {
            error!("Apply event error on {}: {e}", event.path);
            false
        }
    }
}

fn reconcile(conn: &Arc<Mutex<Connection>>) -> (u64, u64) {
    let events = {
        let c = conn.lock().unwrap();
        db::load_unapplied_events(&c).unwrap_or_default()
    };
    let total = events.len() as u64;
    if total == 0 { return (0, 0); }

    info!("Reconciling {} pending events", total);
    let mut applied = 0u64;
    for event in &events {
        let c = conn.lock().unwrap();
        if apply_event(&c, event) {
            let _ = db::mark_event_applied(&c, event.id);
            applied += 1;
        }
    }
    {
        let c = conn.lock().unwrap();
        let _ = db::prune_applied_events(&c);
    }
    info!("Reconciliation complete: {applied}/{total} applied");
    (total, applied)
}

fn flush_batch(conn: &Connection, batch: &mut Vec<(Option<i64>, String, String, bool)>) {
    let tx = match conn.unchecked_transaction() {
        Ok(t) => t,
        Err(e) => { error!("Transaction error: {e}"); batch.clear(); return; }
    };
    for (parent_id, basename, full_path, is_dir) in batch.drain(..) {
        if let Err(e) = db::insert_entry(&tx, parent_id, &basename, &full_path, is_dir) {
            error!("Insert error for {full_path}: {e}");
        }
    }
    if let Err(e) = tx.commit() {
        error!("Commit failed: {e}");
    }
}

fn main() {
    tracing_subscriber::fmt()
        .with_target(false)
        .with_level(true)
        .init();

    info!("rfindd starting");

    // ── Database setup ──────────────────────────────────────────────────────
    let db_file = db_path();
    fs::create_dir_all(db_file.parent().unwrap())
        .expect("Failed to create /var/lib/rfindd");
    let conn = db::open(&db_file).expect("Failed to open database");
    conn.execute_batch("DELETE FROM entries;").unwrap();
    conn.execute_batch("DELETE FROM pending_events;").unwrap();
    info!("Database ready: {:?}", db_file);
    info!("Excluding: {:?}", DEFAULT_EXCLUDES);

    let conn = Arc::new(Mutex::new(conn));

    // ── fanotify — non-blocking probe, graceful fallback ───────────────────
    let excludes: Vec<PathBuf> = DEFAULT_EXCLUDES.iter().map(PathBuf::from).collect();
    let monitor = match Monitor::start(
        &PathBuf::from("/"),
        Arc::clone(&conn),
        excludes,
    ) {
        Ok(m) => {
            info!("Live monitoring: fanotify active on /");
            Some(m)
        }
        Err(e) => {
            warn!("Live monitoring unavailable: {e}");
            warn!("Index will be built but not automatically updated.");
            warn!("This is expected on WSL2, containers, and restricted kernels.");
            warn!("On bare metal or a full VM, fanotify works automatically.");
            None
        }
    };

    // ── Initial scan ────────────────────────────────────────────────────────
    write_progress(&ScanProgress::new_scanning());

    let conn_for_entries = Arc::clone(&conn);
    let conn_for_flush  = Arc::clone(&conn);
    let conn_for_meta   = Arc::clone(&conn);

    const BATCH_SIZE: usize = 10_000;
    let batch = Arc::new(Mutex::new(Vec::<(Option<i64>, String, String, bool)>::with_capacity(BATCH_SIZE)));
    let batch_for_entries = Arc::clone(&batch);
    let batch_for_flush   = Arc::clone(&batch);

    let config = ScanConfig::new(
        vec![PathBuf::from("/")],
        {
            let conn = Arc::clone(&conn);
            move |progress| {
                let events_pending = conn.lock()
                    .ok()
                    .and_then(|c| db::pending_event_count(&c).ok())
                    .unwrap_or(0) as u64;
                let mut p = progress;
                p.events_pending = events_pending;
                info!("{} | {} events queued", p.status_line, events_pending);
                write_progress(&p);
            }
        },
        move |path, is_dir| {
            if is_excluded(path) { return; }
            let full_path = path.to_string_lossy().to_string();
            let basename  = path.file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            let mut buf = batch_for_entries.lock().unwrap();
            buf.push((None, basename, full_path, is_dir));
            if buf.len() >= BATCH_SIZE {
                let c = conn_for_entries.lock().unwrap();
                flush_batch(&c, &mut buf);
            }
        },
    )
    .with_error_handler(|path, err| warn!("Cannot read {}: {}", path.display(), err))
    .with_skip_dir(|path| is_excluded(path));

    let total = run_scan(config);

    // Flush remaining batch.
    {
        let c = conn_for_flush.lock().unwrap();
        let mut buf = batch_for_flush.lock().unwrap();
        if !buf.is_empty() { flush_batch(&c, &mut buf); }
    }
    info!("Scan complete: {total} entries");

    // ── Reconciliation ──────────────────────────────────────────────────────
    {
        let events_pending = conn.lock()
            .ok()
            .and_then(|c| db::pending_event_count(&c).ok())
            .unwrap_or(0) as u64;
        let p = make_progress(
            DaemonPhase::Reconciling, total, 0, 0, 0.0, 0,
            events_pending, 0, 0.0,
        );
        write_progress(&p);
        info!("{}", p.status_line);
    }
    let (_ev_total, events_applied) = reconcile(&conn);

    // ── WAL checkpoint + mark Ready ─────────────────────────────────────────
    {
        let c = conn_for_meta.lock().unwrap();
        let _ = c.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
        let count = db::entry_count(&c).unwrap_or(0);
        let _ = db::set_metadata(&c, "last_scan_completed", &now_secs().to_string());
        let _ = db::set_metadata(&c, "last_entry_count",    &count.to_string());
        let monitoring_mode = if monitor.is_some() { "fanotify" } else { "index-only" };
        let _ = db::set_metadata(&c, "monitoring_mode", monitoring_mode);

        let p = make_progress(
            DaemonPhase::Ready, count as u64, 0, 0, 0.0, 0,
            0, events_applied, 0.0,
        );
        write_progress(&p);
        info!("{}", p.status_line);
        info!("Monitoring mode: {monitoring_mode}");
    }

    // ── Steady state ─────────────────────────────────────────────────────────
    // If fanotify is active: drain pending events every 5 seconds.
    // If index-only mode: hold and log periodic reminders.
    info!("Daemon ready. Entering steady state.");

    let mut idle_log_counter = 0u64;
    loop {
        std::thread::sleep(Duration::from_secs(5));

        if monitor.is_some() {
            let pending = conn.lock()
                .ok()
                .and_then(|c| db::pending_event_count(&c).ok())
                .unwrap_or(0);
            if pending > 0 {
                info!("Applying {pending} live events");
                reconcile(&conn);
            }
        } else {
            // Log a reminder every 5 minutes in index-only mode.
            idle_log_counter += 1;
            if idle_log_counter % 60 == 0 {
                let count = conn.lock()
                    .ok()
                    .and_then(|c| db::entry_count(&c).ok())
                    .unwrap_or(0);
                info!("Index-only mode | {count} entries | restart daemon to re-index");
            }
        }
    }
}
