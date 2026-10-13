//! Parallel filesystem scanner using a shared Mutex<VecDeque> work queue.

use rfind_core::{DaemonPhase, ScanProgress};
use std::{
    collections::VecDeque,
    fs,
    path::PathBuf,
    sync::{
        atomic::{AtomicI64, AtomicU64, Ordering},
        Arc, Condvar, Mutex,
    },
    thread,
    time::{Duration, Instant, SystemTime},
};

struct WorkQueue {
    queue: Mutex<VecDeque<PathBuf>>,
    condvar: Condvar,
}

impl WorkQueue {
    fn new() -> Self {
        Self {
            queue: Mutex::new(VecDeque::new()),
            condvar: Condvar::new(),
        }
    }
    fn push(&self, path: PathBuf) {
        self.queue.lock().unwrap().push_back(path);
        self.condvar.notify_one();
    }
    fn pop_or_done(&self, in_flight: &AtomicI64) -> Option<PathBuf> {
        let mut q = self.queue.lock().unwrap();
        loop {
            if let Some(path) = q.pop_front() { return Some(path); }
            if in_flight.load(Ordering::SeqCst) == 0 { return None; }
            q = self.condvar.wait(q).unwrap();
        }
    }
    fn wake_all(&self) { self.condvar.notify_all(); }
}

#[derive(Default)]
struct Counters {
    entries_found: AtomicU64,
    dirs_completed: AtomicU64,
    errors: AtomicU64,
    in_flight: AtomicI64,
}

pub struct ScanConfig {
    pub roots: Vec<PathBuf>,
    pub workers: usize,
    pub progress_interval: Duration,
    pub on_progress: Box<dyn Fn(ScanProgress) + Send + Sync + 'static>,
    pub on_entry: Box<dyn Fn(&PathBuf, bool) + Send + Sync + 'static>,
    /// Called when a directory cannot be read, with path and error message.
    pub on_error: Box<dyn Fn(&PathBuf, &str) + Send + Sync + 'static>,
}

impl ScanConfig {
    pub fn new(
        roots: Vec<PathBuf>,
        on_progress: impl Fn(ScanProgress) + Send + Sync + 'static,
        on_entry: impl Fn(&PathBuf, bool) + Send + Sync + 'static,
    ) -> Self {
        Self {
            roots,
            workers: num_cpus::get().max(2),
            progress_interval: Duration::from_secs(1),
            on_progress: Box::new(on_progress),
            on_entry: Box::new(on_entry),
            on_error: Box::new(|path, err| {
                eprintln!("SCAN ERROR: {} — {}", path.display(), err);
            }),
        }
    }

    /// Override the error handler.
    pub fn with_error_handler(
        mut self,
        f: impl Fn(&PathBuf, &str) + Send + Sync + 'static,
    ) -> Self {
        self.on_error = Box::new(f);
        self
    }
}

pub fn run_scan(config: ScanConfig) -> u64 {
    let wq = Arc::new(WorkQueue::new());
    let counters = Arc::new(Counters::default());
    let on_entry = Arc::new(config.on_entry);
    let on_error = Arc::new(config.on_error);

    for root in &config.roots {
        counters.in_flight.fetch_add(1, Ordering::SeqCst);
        wq.push(root.clone());
    }

    let mut handles = Vec::with_capacity(config.workers);
    for _ in 0..config.workers {
        let wq = Arc::clone(&wq);
        let counters = Arc::clone(&counters);
        let on_entry = Arc::clone(&on_entry);
        let on_error = Arc::clone(&on_error);
        handles.push(thread::spawn(move || {
            worker_loop(wq, counters, on_entry, on_error);
        }));
    }

    let start = Instant::now();
    let mut last_entries: u64 = 0;
    let mut last_tick = Instant::now();

    loop {
        thread::sleep(config.progress_interval);

        let entries = counters.entries_found.load(Ordering::Relaxed);
        let dirs_done = counters.dirs_completed.load(Ordering::Relaxed);
        let errors = counters.errors.load(Ordering::Relaxed);
        let in_flight = counters.in_flight.load(Ordering::Relaxed).max(0) as u64;
        let done = in_flight == 0;

        let elapsed_tick = last_tick.elapsed().as_secs_f64();
        let rate = if elapsed_tick > 0.0 {
            (entries.saturating_sub(last_entries)) as f64 / elapsed_tick
        } else { 0.0 };
        last_entries = entries;
        last_tick = Instant::now();

        let elapsed = start.elapsed();
        let status = format!(
            "{} | {} entries | {} dirs | {:.0} entries/s | {:.1}s elapsed{}",
            if done { "Ready" } else { "Scanning" },
            entries, dirs_done, rate, elapsed.as_secs_f64(),
            if errors > 0 { format!(" | {} errors", errors) } else { String::new() },
        );

        let progress = ScanProgress {
            phase: if done { DaemonPhase::Ready } else { DaemonPhase::Scanning },
            entries_found: entries,
            dirs_completed: dirs_done,
            dirs_pending: in_flight,
            entries_per_sec: rate,
            errors,
            timestamp_secs: SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            status_line: status,
        };

        (config.on_progress)(progress);
        if done { break; }
    }

    for h in handles { let _ = h.join(); }
    counters.entries_found.load(Ordering::Relaxed)
}

fn worker_loop(
    wq: Arc<WorkQueue>,
    counters: Arc<Counters>,
    on_entry: Arc<Box<dyn Fn(&PathBuf, bool) + Send + Sync + 'static>>,
    on_error: Arc<Box<dyn Fn(&PathBuf, &str) + Send + Sync + 'static>>,
) {
    loop {
        let dir = match wq.pop_or_done(&counters.in_flight) {
            Some(d) => d,
            None => { wq.wake_all(); break; }
        };

        match fs::read_dir(&dir) {
            Ok(read_dir) => {
                let mut local_count: u64 = 0;
                for entry in read_dir.flatten() {
                    let path = entry.path();
                    let is_dir = entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false);
                    on_entry(&path, is_dir);
                    local_count += 1;
                    if is_dir {
                        counters.in_flight.fetch_add(1, Ordering::SeqCst);
                        wq.push(path);
                    }
                }
                counters.entries_found.fetch_add(local_count, Ordering::Relaxed);
                counters.dirs_completed.fetch_add(1, Ordering::Relaxed);
            }
            Err(e) => {
                // Log path + OS error so we know exactly what failed.
                on_error(&dir, &e.to_string());
                counters.errors.fetch_add(1, Ordering::Relaxed);
            }
        }

        let remaining = counters.in_flight.fetch_sub(1, Ordering::SeqCst) - 1;
        if remaining == 0 { wq.wake_all(); }
    }
}
