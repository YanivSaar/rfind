//! Parallel filesystem scanner using getdents64 + statx.
//!
//! # Performance design
//! Standard std::fs::read_dir issues one syscall per entry and resolves
//! full absolute paths repeatedly. We instead:
//!
//! 1. Open each directory with openat(O_DIRECTORY).
//! 2. Read entries in bulk via getdents64 (one syscall per ~256 entries).
//! 3. For each entry, call statx(dirfd, relative_name, STATX_MTIME)
//!    — the kernel resolves only the final component, not the full path.
//! 4. Use d_type from getdents64 to skip stat for non-regular/non-dir
//!    entries where possible.
//!
//! # Work queue
//! A Mutex<VecDeque> with Condvar avoids the channel-deadlock problem
//! (bounded channels deadlock when all workers block on send into a
//! full channel). The in_flight counter drives termination detection.

use libc::{
    c_char, close, openat, syscall, SYS_getdents64, AT_FDCWD, AT_SYMLINK_NOFOLLOW, O_CLOEXEC,
    O_DIRECTORY, O_RDONLY,
};
use rfind_core::{DaemonPhase, ScanProgress};
use std::os::unix::ffi::OsStrExt;
use std::{
    collections::VecDeque,
    ffi::{CStr, CString},
    mem,
    os::unix::io::RawFd,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering},
        Arc, Condvar, Mutex,
    },
    thread,
    time::{Duration, Instant, SystemTime},
};
use tracing::{error, warn};

// ---------------------------------------------------------------------------
// Linux kernel structs for getdents64
// ---------------------------------------------------------------------------

#[repr(C)]
struct LinuxDirent64 {
    d_ino: u64,
    d_off: i64,
    d_reclen: u16,
    d_type: u8,
    // d_name follows: variable-length null-terminated string
}

const DT_DIR: u8 = 4;
const DT_REG: u8 = 8;
const DT_UNKNOWN: u8 = 0;

// ---------------------------------------------------------------------------
// statx structures
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Default)]
struct StatxTimestamp {
    tv_sec: i64,
    tv_nsec: u32,
    _pad: i32,
}

#[repr(C)]
#[derive(Default)]
struct Statx {
    stx_mask: u32,
    stx_blksize: u32,
    stx_attributes: u64,
    stx_nlink: u32,
    stx_uid: u32,
    stx_gid: u32,
    stx_mode: u16,
    _pad1: u16,
    stx_ino: u64,
    stx_size: u64,
    stx_blocks: u64,
    stx_attributes_mask: u64,
    stx_atime: StatxTimestamp,
    stx_btime: StatxTimestamp,
    stx_ctime: StatxTimestamp,
    stx_mtime: StatxTimestamp,
    stx_rdev_major: u32,
    stx_rdev_minor: u32,
    stx_dev_major: u32,
    stx_dev_minor: u32,
    stx_mnt_id: u64,
    _pad2: [u64; 13],
}

const STATX_TYPE: u32 = 0x1;
const STATX_MTIME: u32 = 0x40;
const STATX_SIZE: u32 = 0x200;
const STATX_MASK: u32 = STATX_TYPE | STATX_MTIME | STATX_SIZE;
const S_IFMT: u16 = 0o170000;
const S_IFDIR: u16 = 0o040000;

/// What the scanner needs from statx: file type, mtime and size.
#[derive(Clone, Copy)]
struct Stat {
    is_dir: bool,
    mtime_sec: i64,
    mtime_nsec: i64,
    size: u64,
}

impl Stat {
    fn from_statx(stx: &Statx) -> Self {
        Self {
            is_dir: stx.stx_mode & S_IFMT == S_IFDIR,
            mtime_sec: stx.stx_mtime.tv_sec,
            mtime_nsec: stx.stx_mtime.tv_nsec as i64,
            size: stx.stx_size,
        }
    }
}

/// statx(dirfd, name, AT_SYMLINK_NOFOLLOW, TYPE|MTIME|SIZE) as a plain syscall.
fn statx_one(dirfd: RawFd, name: &CStr) -> Option<Stat> {
    let mut stx = Statx::default();
    loop {
        let ret = unsafe {
            syscall(
                libc::SYS_statx, // per-architecture number (332 on x86_64, 291 on aarch64)
                dirfd as libc::c_long,
                name.as_ptr() as libc::c_long,
                AT_SYMLINK_NOFOLLOW as libc::c_long,
                STATX_MASK as libc::c_long,
                &mut stx as *mut Statx as libc::c_long,
            )
        };
        if ret == 0 {
            return Some(Stat::from_statx(&stx));
        }
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() == Some(libc::EINTR) {
            continue;
        }
        return None;
    }
}

/// Ring size for batched statx; one batch per getdents64 buffer chunk.
const URING_ENTRIES: u32 = 256;

/// Latches when io_uring turned out to be unusable at least once; only used
/// to suppress repeated warnings and to report the result honestly. Each
/// scan still tries to open a ring so a transient hiccup doesn't disable
/// io_uring for the whole life of a long-running daemon.
static URING_UNAVAILABLE: AtomicBool = AtomicBool::new(false);

fn new_ring() -> Option<io_uring::IoUring> {
    match io_uring::IoUring::new(URING_ENTRIES) {
        Ok(r) => Some(r),
        Err(e) => {
            if !URING_UNAVAILABLE.swap(true, Ordering::Relaxed) {
                warn!("io_uring unavailable ({e}); using plain statx syscalls");
            }
            None
        }
    }
}

/// statx for every name in `names` (relative to `dirfd`). With a ring the
/// calls are submitted in batches: one syscall per batch instead of one
/// per entry, and on slow filesystems (network, WSL2 drvfs) the kernel
/// runs them concurrently.
///
/// Safety: `bufs` is handed to the kernel by raw pointer. We must not drop
/// `bufs` (or the ring) until every submitted SQE has completed — closing
/// the uring fd does not synchronously cancel in-flight ops, so a late
/// kernel write would otherwise be a use-after-free. On any error path we
/// first drain the completion queue; if we cannot drain cleanly we leak
/// both the ring and the buffer rather than risk UAF.
fn statx_batch(
    ring: &mut Option<io_uring::IoUring>,
    dirfd: RawFd,
    names: &[&CStr],
) -> Vec<Option<Stat>> {
    let Some(r) = ring.as_mut() else {
        return names.iter().map(|n| statx_one(dirfd, n)).collect();
    };
    let mut out = vec![None; names.len()];
    let mut bufs: Vec<Statx> = (0..names.len()).map(|_| Statx::default()).collect();
    for (chunk_start, chunk) in names.chunks(URING_ENTRIES as usize).enumerate() {
        let base = chunk_start * URING_ENTRIES as usize;
        let mut submitted = 0usize;
        for (i, name) in chunk.iter().enumerate() {
            let sqe = io_uring::opcode::Statx::new(
                io_uring::types::Fd(dirfd),
                name.as_ptr(),
                &mut bufs[base + i] as *mut Statx as *mut io_uring::types::statx,
            )
            .flags(AT_SYMLINK_NOFOLLOW)
            .mask(STATX_MASK)
            .build()
            .user_data((base + i) as u64);
            // The ring has URING_ENTRIES slots and the chunk is no larger,
            // so this push should always succeed. If something ever changes
            // that invariant, fall back to the per-call statx path instead
            // of crashing the scanner.
            if unsafe { r.submission().push(&sqe) }.is_err() {
                warn!("io_uring submission queue full ({submitted}/{}); falling back to statx syscall", chunk.len());
                URING_UNAVAILABLE.store(true, Ordering::Relaxed);
                // Drain anything we did push so we don't leak the buffers.
                if submitted > 0 {
                    let _ = r.submit_and_wait(submitted);
                    for cqe in r.completion() {
                        let i = cqe.user_data() as usize;
                        if cqe.result() >= 0 {
                            out[i] = Some(Stat::from_statx(&bufs[i]));
                        }
                    }
                }
                // Fill the rest (and any later chunks) with plain statx.
                for (i, name) in names.iter().enumerate() {
                    if out[i].is_none() {
                        out[i] = statx_one(dirfd, name);
                    }
                }
                return out;
            }
            submitted += 1;
        }
        let mut done = 0usize;
        while done < submitted {
            match r.submit_and_wait(submitted - done) {
                Ok(_) => {}
                Err(e) if e.raw_os_error() == Some(libc::EINTR) => {
                    // Drain whatever came back and retry.
                }
                Err(e) => {
                    warn!("io_uring submit_and_wait failed ({e}); draining and giving up");
                    // Non-blocking drain of whatever's ready.
                    for cqe in r.completion() {
                        let i = cqe.user_data() as usize;
                        if cqe.result() >= 0 {
                            out[i] = Some(Stat::from_statx(&bufs[i]));
                        }
                        done += 1;
                    }
                    if done < submitted {
                        // Still outstanding SQEs. The kernel can write to
                        // `bufs` after this returns, so neither the ring
                        // nor the buffer may be dropped. Leak them.
                        warn!(
                            "io_uring: {} ops still outstanding; leaking ring+buffers",
                            submitted - done
                        );
                        if let Some(old) = ring.take() {
                            std::mem::forget(old);
                        }
                        std::mem::forget(bufs);
                        URING_UNAVAILABLE.store(true, Ordering::Relaxed);
                        return names.iter().map(|n| statx_one(dirfd, n)).collect();
                    }
                    URING_UNAVAILABLE.store(true, Ordering::Relaxed);
                    *ring = None;
                    return names.iter().map(|n| statx_one(dirfd, n)).collect();
                }
            }
            for cqe in r.completion() {
                let i = cqe.user_data() as usize;
                if cqe.result() >= 0 {
                    out[i] = Some(Stat::from_statx(&bufs[i]));
                }
                done += 1;
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Work queue
// ---------------------------------------------------------------------------

/// A directory waiting to be read. Only the path is queued: the directory
/// is opened when a worker picks it up, so open fds are bounded by the
/// worker count rather than by queue width (wide trees such as a Windows
/// drive queue tens of thousands of directories and hit EMFILE).
struct WorkItem {
    /// Absolute path of this directory.
    path: PathBuf,
}

struct WorkQueue {
    queue: Mutex<VecDeque<WorkItem>>,
    condvar: Condvar,
}

impl WorkQueue {
    fn new() -> Self {
        Self {
            queue: Mutex::new(VecDeque::new()),
            condvar: Condvar::new(),
        }
    }

    fn push(&self, item: WorkItem) {
        self.queue.lock().unwrap().push_back(item);
        self.condvar.notify_one();
    }

    fn pop_or_done(&self, in_flight: &AtomicI64) -> Option<WorkItem> {
        let mut q = self.queue.lock().unwrap();
        loop {
            if let Some(item) = q.pop_front() {
                return Some(item);
            }
            if in_flight.load(Ordering::Acquire) == 0 {
                return None;
            }
            q = self.condvar.wait(q).unwrap();
        }
    }

    fn wake_all(&self) {
        self.condvar.notify_all();
    }
}

#[derive(Default)]
struct Counters {
    entries_found: AtomicU64,
    dirs_completed: AtomicU64,
    errors: AtomicU64,
    in_flight: AtomicI64,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Metadata collected for each entry.
pub struct EntryInfo {
    pub path: PathBuf,
    pub is_dir: bool,
    pub mtime_sec: i64,
    pub mtime_nsec: i64,
    /// Size in bytes (0 for directories).
    pub size: u64,
}

pub type ProgressFn = Box<dyn Fn(ScanProgress) + Send + Sync + 'static>;
pub type EntryFn = Box<dyn Fn(EntryInfo) + Send + Sync + 'static>;
pub type ErrorFn = Box<dyn Fn(&PathBuf, &str) + Send + Sync + 'static>;
pub type SkipDirFn = Box<dyn Fn(&PathBuf) -> bool + Send + Sync + 'static>;

pub struct ScanConfig {
    pub roots: Vec<PathBuf>,
    pub workers: usize,
    pub progress_interval: Duration,
    pub collect_mtime: bool,
    /// Batch statx through io_uring when available.
    pub io_uring: bool,
    /// Set to stop the scan early; the result is then marked cancelled.
    pub cancel: Arc<AtomicBool>,
    pub on_progress: ProgressFn,
    pub on_entry: EntryFn,
    pub on_error: ErrorFn,
    pub skip_dir: SkipDirFn,
}

impl ScanConfig {
    pub fn new(
        roots: Vec<PathBuf>,
        collect_mtime: bool,
        on_progress: impl Fn(ScanProgress) + Send + Sync + 'static,
        on_entry: impl Fn(EntryInfo) + Send + Sync + 'static,
    ) -> Self {
        Self {
            roots,
            workers: num_cpus::get().max(2),
            progress_interval: Duration::from_secs(1),
            collect_mtime,
            io_uring: false,
            cancel: Arc::new(AtomicBool::new(false)),
            on_progress: Box::new(on_progress),
            on_entry: Box::new(on_entry),
            on_error: Box::new(|p, e| eprintln!("SCAN ERROR {}: {}", p.display(), e)),
            skip_dir: Box::new(|_| false),
        }
    }

    pub fn with_workers(mut self, n: usize) -> Self {
        if n > 0 {
            self.workers = n;
        }
        self
    }

    pub fn with_error_handler(
        mut self,
        f: impl Fn(&PathBuf, &str) + Send + Sync + 'static,
    ) -> Self {
        self.on_error = Box::new(f);
        self
    }

    pub fn with_skip_dir(mut self, f: impl Fn(&PathBuf) -> bool + Send + Sync + 'static) -> Self {
        self.skip_dir = Box::new(f);
        self
    }

    pub fn with_io_uring(mut self, on: bool) -> Self {
        self.io_uring = on;
        self
    }

    pub fn with_cancel(mut self, cancel: Arc<AtomicBool>) -> Self {
        self.cancel = cancel;
        self
    }
}

/// Outcome of [`run_scan`].
#[derive(Debug, Clone, Copy)]
pub struct ScanResult {
    pub entries: u64,
    pub dirs: u64,
    pub errors: u64,
    /// Stopped early via [`ScanConfig::cancel`]: the scan is incomplete
    /// and must not be used to delete entries it didn't see.
    pub cancelled: bool,
    /// statx calls went through io_uring.
    pub io_uring: bool,
    pub elapsed: Duration,
}

pub fn run_scan(config: ScanConfig) -> ScanResult {
    let wq = Arc::new(WorkQueue::new());
    let counters = Arc::new(Counters::default());
    let on_entry = Arc::new(config.on_entry);
    let on_error = Arc::new(config.on_error);
    let skip_dir = Arc::new(config.skip_dir);

    // Seed queue with roots.
    for root in &config.roots {
        if !root.is_dir() {
            warn!("Cannot open root {:?}", root);
            continue;
        }
        counters.in_flight.fetch_add(1, Ordering::Release);
        wq.push(WorkItem { path: root.clone() });
    }

    let mut handles = Vec::with_capacity(config.workers);
    for _ in 0..config.workers {
        let wq = Arc::clone(&wq);
        let counters = Arc::clone(&counters);
        let on_entry = Arc::clone(&on_entry);
        let on_error = Arc::clone(&on_error);
        let skip_dir = Arc::clone(&skip_dir);
        let cancel = Arc::clone(&config.cancel);
        let (collect_mtime, io_uring) = (config.collect_mtime, config.io_uring);

        handles.push(thread::spawn(move || {
            let ring = if io_uring { new_ring() } else { None };
            // Catch panics from any of the user callbacks: without this,
            // one panic would leave `in_flight > 0` and every other worker
            // would hang in `pop_or_done` forever.
            let cleanup_wq = Arc::clone(&wq);
            let cleanup_counters = Arc::clone(&counters);
            let cleanup_cancel = Arc::clone(&cancel);
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                worker_loop(
                    wq,
                    counters,
                    on_entry,
                    on_error,
                    skip_dir,
                    &cancel,
                    collect_mtime,
                    ring,
                );
            }));
            if result.is_err() {
                error!("scanner worker panicked; cancelling the scan");
                cleanup_cancel.store(true, Ordering::Relaxed);
                let _g = cleanup_wq.queue.lock().unwrap();
                cleanup_counters.in_flight.store(0, Ordering::Release);
                cleanup_wq.condvar.notify_all();
            }
        }));
    }

    // Progress reporter on main thread.
    let start = Instant::now();
    let mut last_entries = 0u64;
    let mut last_tick = Instant::now();

    let mut last_report = Instant::now();
    loop {
        // Poll often (so short scans and cancellation finish promptly),
        // report every progress_interval.
        thread::sleep(Duration::from_millis(20));
        let in_flight = counters.in_flight.load(Ordering::Relaxed).max(0) as u64;
        if in_flight > 0 && last_report.elapsed() < config.progress_interval {
            continue;
        }
        last_report = Instant::now();

        let entries = counters.entries_found.load(Ordering::Relaxed);
        let dirs_done = counters.dirs_completed.load(Ordering::Relaxed);
        let errors = counters.errors.load(Ordering::Relaxed);
        let done = in_flight == 0;

        let tick_secs = last_tick.elapsed().as_secs_f64();
        let rate = if tick_secs > 0.0 {
            (entries.saturating_sub(last_entries)) as f64 / tick_secs
        } else {
            0.0
        };
        last_entries = entries;
        last_tick = Instant::now();

        let elapsed = start.elapsed().as_secs_f64();
        let status = format!(
            "{} | {} entries | {} dirs | {:.0} entries/s | {:.1}s{}",
            if done { "Ready" } else { "Scanning" },
            entries,
            dirs_done,
            rate,
            elapsed,
            if errors > 0 {
                format!(" | {} errors", errors)
            } else {
                String::new()
            },
        );

        (config.on_progress)(ScanProgress {
            phase: if done {
                DaemonPhase::Ready
            } else {
                DaemonPhase::Scanning
            },
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
            events_pending: 0,
            events_applied: 0,
            monitoring_mode: String::new(),
            background_scan: None,
        });

        if done {
            break;
        }
    }

    for h in handles {
        let _ = h.join();
    }
    ScanResult {
        entries: counters.entries_found.load(Ordering::Relaxed),
        dirs: counters.dirs_completed.load(Ordering::Relaxed),
        errors: counters.errors.load(Ordering::Relaxed),
        cancelled: config.cancel.load(Ordering::Relaxed),
        io_uring: config.io_uring && !URING_UNAVAILABLE.load(Ordering::Relaxed),
        elapsed: start.elapsed(),
    }
}

// ---------------------------------------------------------------------------
// Worker
// ---------------------------------------------------------------------------

fn open_dir_fd(path: &std::path::Path) -> RawFd {
    let cpath = match CString::new(path.as_os_str().as_encoded_bytes()) {
        Ok(c) => c,
        Err(_) => return -1,
    };
    unsafe { openat(AT_FDCWD, cpath.as_ptr(), O_RDONLY | O_DIRECTORY | O_CLOEXEC) }
}

/// Finish one queued directory: drop it from the in-flight count and wake
/// the others if it was the last.
///
/// The decrement runs under the queue mutex, so a waiter that just observed
/// `in_flight > 0` under the same mutex cannot slip into `condvar.wait`
/// after this fires — i.e. no lost wakeups and no scanner hang.
fn finish_item(wq: &WorkQueue, counters: &Counters) {
    let _q = wq.queue.lock().unwrap();
    let remaining = counters.in_flight.fetch_sub(1, Ordering::AcqRel) - 1;
    if remaining == 0 {
        wq.condvar.notify_all();
    }
}

#[allow(clippy::too_many_arguments)]
fn worker_loop(
    wq: Arc<WorkQueue>,
    counters: Arc<Counters>,
    on_entry: Arc<EntryFn>,
    on_error: Arc<ErrorFn>,
    skip_dir: Arc<SkipDirFn>,
    cancel: &AtomicBool,
    collect_mtime: bool,
    mut ring: Option<io_uring::IoUring>,
) {
    const BUF_SIZE: usize = 65536;
    let mut buf = vec![0u8; BUF_SIZE];
    // Entries of the current getdents64 buffer: (name, d_type).
    let mut batch: Vec<(CString, u8)> = Vec::with_capacity(512);

    loop {
        let item = match wq.pop_or_done(&counters.in_flight) {
            Some(i) => i,
            None => {
                wq.wake_all();
                break;
            }
        };
        // Cancelled: drain the queue without reading anything.
        if cancel.load(Ordering::Relaxed) {
            finish_item(&wq, &counters);
            continue;
        }

        let dir_path = item.path;
        let dir_fd = open_dir_fd(&dir_path);
        if dir_fd < 0 {
            let err = std::io::Error::last_os_error();
            on_error(&dir_path, &err.to_string());
            counters.errors.fetch_add(1, Ordering::Relaxed);
            finish_item(&wq, &counters);
            continue;
        }

        let mut local_count = 0u64;

        // Read directory entries via getdents64.
        loop {
            let n = loop {
                let r = unsafe {
                    syscall(
                        SYS_getdents64 as libc::c_long,
                        dir_fd as libc::c_long,
                        buf.as_mut_ptr() as libc::c_long,
                        BUF_SIZE as libc::c_long,
                    )
                };
                if r >= 0 {
                    break r;
                }
                let err = std::io::Error::last_os_error();
                if err.raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                break r;
            };
            if n < 0 {
                let err = std::io::Error::last_os_error();
                on_error(&dir_path, &err.to_string());
                counters.errors.fetch_add(1, Ordering::Relaxed);
                break;
            }
            if n == 0 {
                break; // end of directory
            }

            batch.clear();
            let mut pos = 0usize;
            while pos < n as usize {
                let dent = unsafe { &*(buf.as_ptr().add(pos) as *const LinuxDirent64) };
                let name_offset = mem::offset_of!(LinuxDirent64, d_type) + 1;
                let name =
                    unsafe { CStr::from_ptr(buf.as_ptr().add(pos + name_offset) as *const c_char) };
                pos += dent.d_reclen as usize;
                if name.to_bytes() != b"." && name.to_bytes() != b".." {
                    batch.push((name.to_owned(), dent.d_type));
                }
            }

            // statx when we want mtimes, or when getdents didn't say
            // whether the entry is a directory (DT_UNKNOWN on some network
            // and FUSE filesystems) — otherwise those would never be scanned.
            let wants_stat = |d_type: u8| {
                d_type == DT_UNKNOWN || (collect_mtime && (d_type == DT_REG || d_type == DT_DIR))
            };
            let to_stat: Vec<&CStr> = batch
                .iter()
                .filter(|(_, t)| wants_stat(*t))
                .map(|(n, _)| n.as_c_str())
                .collect();
            let mut stats = statx_batch(&mut ring, dir_fd, &to_stat).into_iter();

            for (name, d_type) in &batch {
                let st = if wants_stat(*d_type) {
                    stats.next().flatten()
                } else {
                    None
                };
                let is_dir =
                    *d_type == DT_DIR || (*d_type == DT_UNKNOWN && st.is_some_and(|s| s.is_dir));
                // Keep the raw bytes: a lossy name could not be reopened.
                let child_path = dir_path.join(std::ffi::OsStr::from_bytes(name.to_bytes()));
                if is_dir && skip_dir(&child_path) {
                    continue;
                }
                let (mtime_sec, mtime_nsec, size) = match st {
                    // Directory sizes are derived from their files at query time.
                    Some(s) if collect_mtime => {
                        (s.mtime_sec, s.mtime_nsec, if is_dir { 0 } else { s.size })
                    }
                    _ => (0, 0, 0),
                };
                on_entry(EntryInfo {
                    path: child_path.clone(),
                    is_dir,
                    mtime_sec,
                    mtime_nsec,
                    size,
                });
                local_count += 1;

                if is_dir {
                    counters.in_flight.fetch_add(1, Ordering::Release);
                    wq.push(WorkItem { path: child_path });
                }
            }
        }

        unsafe { close(dir_fd) };
        counters
            .entries_found
            .fetch_add(local_count, Ordering::Relaxed);
        counters.dirs_completed.fetch_add(1, Ordering::Relaxed);
        finish_item(&wq, &counters);
    }
}
