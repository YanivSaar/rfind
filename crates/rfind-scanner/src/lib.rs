//! Parallel filesystem scanner using a bounded work queue.
//!
//! # Design
//! A fixed pool of worker threads pulls directory paths from a crossbeam
//! channel. Each worker enumerates one directory, records the entries, and
//! pushes any child directories back onto the queue. A separate progress
//! thread aggregates counters and writes periodic snapshots.

use crossbeam_channel::{bounded, Receiver, Sender};
use rfind_core::{DaemonPhase, ScanProgress};
use std::{
    fs,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant, SystemTime},
};

/// Counters shared between worker threads and the progress reporter.
#[derive(Default)]
struct Counters {
    entries_found: AtomicU64,
    dirs_completed: AtomicU64,
    errors: AtomicU64,
}

/// Configuration for a scan run.
pub struct ScanConfig {
    /// Filesystem roots to index.
    pub roots: Vec<PathBuf>,
    /// Number of worker threads. Defaults to number of logical CPUs.
    pub workers: usize,
    /// How often to emit a progress snapshot.
    pub progress_interval: Duration,
    /// Called each time a progress snapshot is ready.
    pub on_progress: Box<dyn Fn(ScanProgress) + Send + Sync + 'static>,
}

impl ScanConfig {
    pub fn new(
        roots: Vec<PathBuf>,
        on_progress: impl Fn(ScanProgress) + Send + Sync + 'static,
    ) -> Self {
        Self {
            roots,
            workers: num_cpus::get().max(2),
            progress_interval: Duration::from_secs(1),
            on_progress: Box::new(on_progress),
        }
    }
}

/// Run a parallel scan and block until complete.
/// Returns the final entry count.
pub fn run_scan(config: ScanConfig) -> u64 {
    // Channel capacity: large enough to keep workers busy, bounded to
    // avoid unbounded memory growth on very wide directory trees.
    let (dir_tx, dir_rx): (Sender<PathBuf>, Receiver<PathBuf>) = bounded(64_000);

    let counters = Arc::new(Counters::default());
    let done = Arc::new(AtomicBool::new(false));
    let on_progress = Arc::new(config.on_progress);

    // Seed the queue with the configured roots.
    for root in &config.roots {
        let _ = dir_tx.send(root.clone());
    }

    // Spawn worker threads.
    let mut handles = Vec::new();
    for _ in 0..config.workers {
        let rx = dir_rx.clone();
        let tx = dir_tx.clone();
        let counters = Arc::clone(&counters);
        let done = Arc::clone(&done);

        handles.push(thread::spawn(move || {
            worker_loop(rx, tx, counters, done);
        }));
    }

    // Drop our sender clone so the channel closes when all workers finish.
    drop(dir_tx);

    // Progress reporter runs on this thread while workers run in background.
    let start = Instant::now();
    let mut last_entries: u64 = 0;
    let mut last_tick = Instant::now();

    loop {
        thread::sleep(config.progress_interval);

        let entries = counters.entries_found.load(Ordering::Relaxed);
        let dirs_done = counters.dirs_completed.load(Ordering::Relaxed);
        let errors = counters.errors.load(Ordering::Relaxed);

        // Check if all workers have exited.
        let all_done = done.load(Ordering::Relaxed);

        let elapsed_tick = last_tick.elapsed().as_secs_f64();
        let rate = if elapsed_tick > 0.0 {
            (entries.saturating_sub(last_entries)) as f64 / elapsed_tick
        } else {
            0.0
        };
        last_entries = entries;
        last_tick = Instant::now();

        let elapsed_total = start.elapsed();
        let status = format!(
            "Scanning | {entries} entries | {dirs_done} dirs | {:.0} entries/s | {:.1}s elapsed{}",
            rate,
            elapsed_total.as_secs_f64(),
            if errors > 0 {
                format!(" | {errors} errors")
            } else {
                String::new()
            }
        );

        let progress = ScanProgress {
            phase: if all_done {
                DaemonPhase::Ready
            } else {
                DaemonPhase::Scanning
            },
            entries_found: entries,
            dirs_completed: dirs_done,
            dirs_pending: 0, // approximate; channel length not exposed cheaply
            entries_per_sec: rate,
            errors,
            timestamp_secs: SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            status_line: status,
        };

        on_progress(progress);

        if all_done {
            break;
        }
    }

    // Join all worker threads.
    for h in handles {
        let _ = h.join();
    }

    counters.entries_found.load(Ordering::Relaxed)
}

/// A single worker: pull directories from the channel, enumerate them,
/// push child directories back, count entries.
fn worker_loop(
    rx: Receiver<PathBuf>,
    tx: Sender<PathBuf>,
    counters: Arc<Counters>,
    done: Arc<AtomicBool>,
) {
    // We track how many directories are "in flight" across all workers using
    // a simple approach: each worker decrements a shared counter when it
    // finishes a directory. When the channel is empty AND in-flight == 0,
    // we are done. Here we use a simpler heuristic: when recv() returns
    // Err (channel empty + all senders dropped), the worker exits.
    //
    // Because we drop the original dir_tx in run_scan, the channel closes
    // only when every worker has also dropped its tx clone, which happens
    // when recv() returns Err on every worker simultaneously.
    loop {
        let dir = match rx.recv() {
            Ok(d) => d,
            Err(_) => break, // channel closed: all work done
        };

        match fs::read_dir(&dir) {
            Ok(entries) => {
                let mut local_count: u64 = 0;
                for entry in entries.flatten() {
                    local_count += 1;
                    // If this entry is a directory, push it for enumeration.
                    // Avoid following symlinks to prevent cycles.
                    if let Ok(ft) = entry.file_type() {
                        if ft.is_dir() {
                            // Best-effort: if the channel is full we skip
                            // rather than block forever. The entry is still
                            // counted; we just won't recurse into it.
                            let _ = tx.try_send(entry.path());
                        }
                    }
                }
                counters
                    .entries_found
                    .fetch_add(local_count, Ordering::Relaxed);
                counters.dirs_completed.fetch_add(1, Ordering::Relaxed);
            }
            Err(_) => {
                // Permission denied or I/O error — count it, move on.
                counters.errors.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    // Signal that this worker is done. When all workers set done=true the
    // last one wins and the progress reporter exits its loop.
    done.store(true, Ordering::Relaxed);
}
