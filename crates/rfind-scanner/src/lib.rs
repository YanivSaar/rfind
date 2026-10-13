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
        atomic::{AtomicI64, AtomicU64, Ordering},
        Arc, Condvar, Mutex,
    },
    thread,
    time::{Duration, Instant, SystemTime},
};
use tracing::warn;

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

const STATX_MTIME: u32 = 0x40;

/// Call statx(dirfd, name, AT_SYMLINK_NOFOLLOW, STATX_MTIME, out).
/// Returns (mtime_sec, mtime_nsec) or (0,0) on error.
fn statx_mtime(dirfd: RawFd, name: &CStr) -> (i64, i64) {
    let mut stx = Statx::default();
    let ret = unsafe {
        syscall(
            libc::SYS_statx, // per-architecture number (332 on x86_64, 291 on aarch64)
            dirfd as libc::c_long,
            name.as_ptr() as libc::c_long,
            AT_SYMLINK_NOFOLLOW as libc::c_long,
            STATX_MTIME as libc::c_long,
            &mut stx as *mut Statx as libc::c_long,
        )
    };
    if ret == 0 {
        (stx.stx_mtime.tv_sec, stx.stx_mtime.tv_nsec as i64)
    } else {
        (0, 0)
    }
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
            if in_flight.load(Ordering::SeqCst) == 0 {
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
}

pub fn run_scan(config: ScanConfig) -> u64 {
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
        counters.in_flight.fetch_add(1, Ordering::SeqCst);
        wq.push(WorkItem { path: root.clone() });
    }

    let mut handles = Vec::with_capacity(config.workers);
    for _ in 0..config.workers {
        let wq = Arc::clone(&wq);
        let counters = Arc::clone(&counters);
        let on_entry = Arc::clone(&on_entry);
        let on_error = Arc::clone(&on_error);
        let skip_dir = Arc::clone(&skip_dir);
        let collect_mtime = config.collect_mtime;

        handles.push(thread::spawn(move || {
            worker_loop(wq, counters, on_entry, on_error, skip_dir, collect_mtime);
        }));
    }

    // Progress reporter on main thread.
    let start = Instant::now();
    let mut last_entries = 0u64;
    let mut last_tick = Instant::now();

    loop {
        thread::sleep(config.progress_interval);

        let entries = counters.entries_found.load(Ordering::Relaxed);
        let dirs_done = counters.dirs_completed.load(Ordering::Relaxed);
        let errors = counters.errors.load(Ordering::Relaxed);
        let in_flight = counters.in_flight.load(Ordering::Relaxed).max(0) as u64;
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
        });

        if done {
            break;
        }
    }

    for h in handles {
        let _ = h.join();
    }
    counters.entries_found.load(Ordering::Relaxed)
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

fn worker_loop(
    wq: Arc<WorkQueue>,
    counters: Arc<Counters>,
    on_entry: Arc<EntryFn>,
    on_error: Arc<ErrorFn>,
    skip_dir: Arc<SkipDirFn>,
    collect_mtime: bool,
) {
    const BUF_SIZE: usize = 65536;
    let mut buf = vec![0u8; BUF_SIZE];

    loop {
        let item = match wq.pop_or_done(&counters.in_flight) {
            Some(i) => i,
            None => {
                wq.wake_all();
                break;
            }
        };

        let dir_path = item.path;
        let dir_fd = open_dir_fd(&dir_path);
        if dir_fd < 0 {
            let err = std::io::Error::last_os_error();
            on_error(&dir_path, &err.to_string());
            counters.errors.fetch_add(1, Ordering::Relaxed);
            let remaining = counters.in_flight.fetch_sub(1, Ordering::SeqCst) - 1;
            if remaining == 0 {
                wq.wake_all();
            }
            continue;
        }

        let mut local_count = 0u64;

        // Read directory entries via getdents64.
        loop {
            let n = unsafe {
                syscall(
                    SYS_getdents64 as libc::c_long,
                    dir_fd as libc::c_long,
                    buf.as_mut_ptr() as libc::c_long,
                    BUF_SIZE as libc::c_long,
                )
            };

            if n < 0 {
                let err = std::io::Error::last_os_error();
                on_error(&dir_path, &err.to_string());
                counters.errors.fetch_add(1, Ordering::Relaxed);
                break;
            }
            if n == 0 {
                break;
            } // end of directory

            let mut pos = 0usize;
            let n = n as usize;

            while pos < n {
                let dent = unsafe { &*(buf.as_ptr().add(pos) as *const LinuxDirent64) };

                // Name starts after the fixed fields.
                let name_offset = mem::offset_of!(LinuxDirent64, d_type) + 1;
                let name_ptr = unsafe { buf.as_ptr().add(pos + name_offset) as *const c_char };
                let name_cstr = unsafe { CStr::from_ptr(name_ptr) };
                let name_bytes = name_cstr.to_bytes();

                // Skip . and ..
                if name_bytes == b"." || name_bytes == b".." {
                    pos += dent.d_reclen as usize;
                    continue;
                }

                let is_dir = dent.d_type == DT_DIR;
                // Keep the raw bytes: a lossy name could not be reopened.
                let child_path = dir_path.join(std::ffi::OsStr::from_bytes(name_bytes));

                if is_dir && skip_dir(&child_path) {
                    pos += dent.d_reclen as usize;
                    continue;
                }

                // Get mtime via statx(dirfd, relative_name).
                let (mtime_sec, mtime_nsec) = if collect_mtime
                    && (dent.d_type == DT_REG || dent.d_type == DT_DIR || dent.d_type == DT_UNKNOWN)
                {
                    statx_mtime(dir_fd, name_cstr)
                } else {
                    (0, 0)
                };

                on_entry(EntryInfo {
                    path: child_path.clone(),
                    is_dir,
                    mtime_sec,
                    mtime_nsec,
                });
                local_count += 1;

                // Push subdirectories onto the work queue.
                if is_dir {
                    counters.in_flight.fetch_add(1, Ordering::SeqCst);
                    wq.push(WorkItem { path: child_path });
                }

                pos += dent.d_reclen as usize;
            }
        }

        unsafe { close(dir_fd) };
        counters
            .entries_found
            .fetch_add(local_count, Ordering::Relaxed);
        counters.dirs_completed.fetch_add(1, Ordering::Relaxed);

        let remaining = counters.in_flight.fetch_sub(1, Ordering::SeqCst) - 1;
        if remaining == 0 {
            wq.wake_all();
        }
    }
}
