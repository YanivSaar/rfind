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
    collections::{HashSet, VecDeque},
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
const DT_LNK: u8 = 10;
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

pub const STATX_TYPE: u32 = 0x1;
pub const STATX_MTIME: u32 = 0x40;
pub const STATX_SIZE: u32 = 0x200;
pub const STATX_ATIME: u32 = 0x20;
pub const STATX_CTIME: u32 = 0x80;
/// The indexer's statx mask: type + mtime + size. Kept intact so the
/// indexer sees exactly the same stat shape as before.
pub const STATX_MASK: u32 = STATX_TYPE | STATX_MTIME | STATX_SIZE;
const S_IFMT: u16 = 0o170000;
const S_IFDIR: u16 = 0o040000;
const S_IFLNK: u16 = 0o120000;

/// What the scanner needs from statx. `atime`/`ctime` are populated only
/// when the caller requests them via a wider mask; otherwise they are 0.
/// `(dev, ino)` identify an inode uniquely across bind mounts — used by
/// the `-L` symlink mode to break traversal cycles.
#[derive(Clone, Copy)]
struct Stat {
    is_dir: bool,
    is_symlink: bool,
    mtime_sec: i64,
    mtime_nsec: i64,
    atime_sec: i64,
    ctime_sec: i64,
    size: u64,
    dev: u64,
    ino: u64,
}

impl Stat {
    fn from_statx(stx: &Statx) -> Self {
        let mode = stx.stx_mode & S_IFMT;
        Self {
            is_dir: mode == S_IFDIR,
            is_symlink: mode == S_IFLNK,
            mtime_sec: stx.stx_mtime.tv_sec,
            mtime_nsec: stx.stx_mtime.tv_nsec as i64,
            atime_sec: stx.stx_atime.tv_sec,
            ctime_sec: stx.stx_ctime.tv_sec,
            size: stx.stx_size,
            dev: ((stx.stx_dev_major as u64) << 32) | stx.stx_dev_minor as u64,
            ino: stx.stx_ino,
        }
    }
}

/// statx(dirfd, name, flags, mask) as a plain syscall. `flags` typically
/// includes `AT_SYMLINK_NOFOLLOW`; omit it to follow a symlink target.
fn statx_one(dirfd: RawFd, name: &CStr, mask: u32, flags: i32) -> Option<Stat> {
    let mut stx = Statx::default();
    loop {
        let ret = unsafe {
            syscall(
                libc::SYS_statx, // per-architecture number (332 on x86_64, 291 on aarch64)
                dirfd as libc::c_long,
                name.as_ptr() as libc::c_long,
                flags as libc::c_long,
                mask as libc::c_long,
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
/// Safety: `bufs` AND the name allocations are handed to the kernel by raw
/// pointer. We must not drop them (or the ring) until every submitted SQE
/// has completed — closing the uring fd does not synchronously cancel
/// in-flight ops, so a late kernel read of the name or write to the buf
/// would otherwise be a use-after-free. The caller's CStrings go away on
/// the next getdents iteration (`batch.clear()`), so the ring path owns
/// its own copies for the duration of the submission: on a clean path
/// they drop normally; on the leak path they are leaked alongside the
/// ring and bufs.
fn statx_batch(
    ring: &mut Option<io_uring::IoUring>,
    dirfd: RawFd,
    names: &[&CStr],
    mask: u32,
    flags: i32,
) -> Vec<Option<Stat>> {
    let Some(r) = ring.as_mut() else {
        return names
            .iter()
            .map(|n| statx_one(dirfd, n, mask, flags))
            .collect();
    };
    // Own the names submitted to the kernel: the caller's `batch` is
    // cleared on the next getdents iteration, which would free the
    // backing CString heap bytes while outstanding SQEs still reference
    // them on the leak path. These are tiny allocations (dir-entry
    // names), and this clone only runs on the ring path.
    let owned_names: Vec<CString> = names.iter().map(|n| CString::from(*n)).collect();
    let mut out = vec![None; names.len()];
    let mut bufs: Vec<Statx> = (0..names.len()).map(|_| Statx::default()).collect();
    for (chunk_start, chunk) in owned_names.chunks(URING_ENTRIES as usize).enumerate() {
        let base = chunk_start * URING_ENTRIES as usize;
        let mut submitted = 0usize;
        for (i, name) in chunk.iter().enumerate() {
            let sqe = io_uring::opcode::Statx::new(
                io_uring::types::Fd(dirfd),
                name.as_ptr(),
                &mut bufs[base + i] as *mut Statx as *mut io_uring::types::statx,
            )
            .flags(flags)
            .mask(mask)
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
                        out[i] = statx_one(dirfd, name, mask, flags);
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
                        // Still outstanding SQEs. The kernel can read from
                        // the name pointers and write to `bufs` after this
                        // returns, so none of ring / bufs / owned_names may
                        // be dropped. Leak the whole set.
                        warn!(
                            "io_uring: {} ops still outstanding; leaking ring+buffers+names",
                            submitted - done
                        );
                        if let Some(old) = ring.take() {
                            std::mem::forget(old);
                        }
                        std::mem::forget(bufs);
                        std::mem::forget(owned_names);
                        URING_UNAVAILABLE.store(true, Ordering::Relaxed);
                        return names
                            .iter()
                            .map(|n| statx_one(dirfd, n, mask, flags))
                            .collect();
                    }
                    URING_UNAVAILABLE.store(true, Ordering::Relaxed);
                    *ring = None;
                    return names
                        .iter()
                        .map(|n| statx_one(dirfd, n, mask, flags))
                        .collect();
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
    /// Depth from the originating root (root itself = 0). Used by the
    /// `--max-depth` cutoff in walk-mode search.
    depth: u32,
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

    fn pop_or_done(&self, in_flight: &AtomicI64, cancel: &AtomicBool) -> Option<WorkItem> {
        let mut q = self.queue.lock().unwrap();
        loop {
            if let Some(item) = q.pop_front() {
                return Some(item);
            }
            // Treat <=0 as done: panic cleanup can force in_flight to 0 while
            // another worker is still mid-item, and that worker's next
            // fetch_sub leaves the counter negative. Returning None here
            // keeps everyone else from parking forever.
            if in_flight.load(Ordering::Acquire) <= 0 {
                return None;
            }
            // Honour cancellation at the tail: without this, SIGHUP/SIGTERM
            // only takes effect after the last straggler finishes, since
            // the cancel check in worker_loop runs after pop.
            if cancel.load(Ordering::Relaxed) {
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
///
/// `is_symlink` reflects whether the dirent itself is a symlink (not its
/// target). `atime_sec` / `ctime_sec` are 0 unless the scan was configured
/// with a mask that requested them (see [`ScanConfig::with_statx_mask`]).
pub struct EntryInfo {
    pub path: PathBuf,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub mtime_sec: i64,
    pub mtime_nsec: i64,
    pub atime_sec: i64,
    pub ctime_sec: i64,
    /// Size in bytes (0 for directories).
    pub size: u64,
    /// Depth from the scan's originating root (root itself = 0).
    pub depth: u32,
}

pub type ProgressFn = Box<dyn Fn(ScanProgress) + Send + Sync + 'static>;
pub type EntryFn = Box<dyn Fn(EntryInfo) + Send + Sync + 'static>;
pub type ErrorFn = Box<dyn Fn(&PathBuf, &str) + Send + Sync + 'static>;
pub type SkipDirFn = Box<dyn Fn(&PathBuf) -> bool + Send + Sync + 'static>;

/// Symlink traversal mode. Matches `find(1)`'s `-P` / `-H` / `-L`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymlinkMode {
    /// Never follow symlinks. The indexer's behaviour; the scanner's
    /// historical default.
    P,
    /// Follow symlinks only when they appear as a scan root.
    H,
    /// Follow symlinks everywhere; a visited `(dev, ino)` set breaks cycles.
    L,
}

/// Shared "seen inodes" set for the `-L` traversal mode. Keyed by
/// `(dev, ino)` so a symlink into a bind-mounted copy of the same tree is
/// still detected as a cycle. Used only when `symlinks = SymlinkMode::L`.
type VisitedSet = Arc<Mutex<HashSet<(u64, u64)>>>;

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
    /// Maximum directory depth to descend; roots are depth 0. `None` =
    /// no cap (the indexer never sets this).
    pub max_depth: Option<u32>,
    /// How to treat symlinks. The indexer always uses `P`.
    pub symlinks: SymlinkMode,
    /// statx mask applied to every statx call. Walk-mode search can narrow
    /// this to [`STATX_TYPE`] alone, or widen it with `STATX_ATIME`/
    /// `STATX_CTIME` when `--atime`/`--ctime` is in use.
    pub statx_mask: u32,
    /// Visited-inode set for `SymlinkMode::L`. `None` otherwise.
    pub visited: Option<VisitedSet>,
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
            on_error: Box::new(|p, e| {
                eprintln!("SCAN ERROR {}: {}", rfind_core::log::sanitize_path(p), e)
            }),
            skip_dir: Box::new(|_| false),
            max_depth: None,
            symlinks: SymlinkMode::P,
            statx_mask: STATX_MASK,
            visited: None,
        }
    }

    pub fn with_max_depth(mut self, depth: Option<u32>) -> Self {
        self.max_depth = depth;
        self
    }

    pub fn with_symlinks(mut self, mode: SymlinkMode) -> Self {
        self.symlinks = mode;
        if matches!(mode, SymlinkMode::L) && self.visited.is_none() {
            self.visited = Some(Arc::new(Mutex::new(HashSet::new())));
        }
        self
    }

    pub fn with_statx_mask(mut self, mask: u32) -> Self {
        self.statx_mask = mask;
        self
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

pub type PredicateFn = Arc<dyn Fn(&EntryInfo) -> bool + Send + Sync + 'static>;
pub type HitFn = Arc<dyn Fn(EntryInfo) + Send + Sync + 'static>;

/// Walk-mode search configuration. A thin wrapper around [`ScanConfig`]
/// that bakes the predicate+sink into a `on_entry` closure. The indexer
/// path (`run_scan`) is unaffected.
pub struct SearchConfig {
    pub roots: Vec<PathBuf>,
    pub workers: usize,
    pub max_depth: Option<u32>,
    pub symlinks: SymlinkMode,
    pub io_uring: bool,
    /// Base statx mask; narrows to `STATX_TYPE` for pure name queries,
    /// widens with `STATX_ATIME` / `STATX_CTIME` on demand. Caller is
    /// responsible for picking the mask that matches its predicate.
    pub statx_mask: u32,
    pub cancel: Arc<AtomicBool>,
    pub predicate: PredicateFn,
    pub on_hit: HitFn,
    pub on_error: ErrorFn,
}

impl SearchConfig {
    pub fn new(
        roots: Vec<PathBuf>,
        predicate: impl Fn(&EntryInfo) -> bool + Send + Sync + 'static,
        on_hit: impl Fn(EntryInfo) + Send + Sync + 'static,
    ) -> Self {
        Self {
            roots,
            // Walk search is I/O-bound; double the usual compute default.
            workers: (num_cpus::get() * 2).max(4),
            max_depth: None,
            symlinks: SymlinkMode::P,
            io_uring: true,
            statx_mask: STATX_MASK,
            cancel: Arc::new(AtomicBool::new(false)),
            predicate: Arc::new(predicate),
            on_hit: Arc::new(on_hit),
            on_error: Box::new(|p, e| {
                eprintln!("WALK ERROR {}: {}", rfind_core::log::sanitize_path(p), e)
            }),
        }
    }
}

/// Walk the filesystem live, applying `predicate` to each entry and
/// routing matches to `on_hit`. The underlying machinery is `run_scan`;
/// the predicate runs in-worker so non-matching entries never cross the
/// callback boundary.
pub fn run_search(cfg: SearchConfig) -> ScanResult {
    let predicate = cfg.predicate;
    let on_hit = cfg.on_hit;
    let scan = ScanConfig {
        roots: cfg.roots,
        workers: cfg.workers,
        progress_interval: Duration::from_secs(60),
        // Metadata is driven by `statx_mask`; the indexer's own
        // "collect mtime for every file" flag is irrelevant here. We keep
        // it `true` so the existing `wants_stat` path pulls the metadata
        // the mask asks for.
        collect_mtime: true,
        io_uring: cfg.io_uring,
        cancel: cfg.cancel,
        on_progress: Box::new(|_| {}),
        on_entry: Box::new(move |e| {
            if predicate(&e) {
                on_hit(e);
            }
        }),
        on_error: cfg.on_error,
        skip_dir: Box::new(|_| false),
        max_depth: cfg.max_depth,
        symlinks: cfg.symlinks,
        statx_mask: cfg.statx_mask,
        visited: if matches!(cfg.symlinks, SymlinkMode::L) {
            Some(Arc::new(Mutex::new(HashSet::new())))
        } else {
            None
        },
    };
    run_scan(scan)
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
        wq.push(WorkItem {
            path: root.clone(),
            depth: 0,
        });
    }

    let mut handles = Vec::with_capacity(config.workers);
    let shared = Arc::new(WorkerShared {
        max_depth: config.max_depth,
        symlinks: config.symlinks,
        statx_mask: config.statx_mask,
        visited: config.visited.clone(),
    });
    for _ in 0..config.workers {
        let wq = Arc::clone(&wq);
        let counters = Arc::clone(&counters);
        let on_entry = Arc::clone(&on_entry);
        let on_error = Arc::clone(&on_error);
        let skip_dir = Arc::clone(&skip_dir);
        let cancel = Arc::clone(&config.cancel);
        let shared = Arc::clone(&shared);
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
                    &shared,
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
    let mut cancel_wake_sent = false;
    loop {
        // Poll often (so short scans and cancellation finish promptly),
        // report every progress_interval.
        thread::sleep(Duration::from_millis(20));
        // When cancel is set, workers parked in pop_or_done need a nudge:
        // they now bail on cancel but still only re-check after a notify.
        if config.cancel.load(Ordering::Relaxed) && !cancel_wake_sent {
            wq.wake_all();
            cancel_wake_sent = true;
        }
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

/// Linux `struct open_how` — passed to `openat2(2)` (kernel 5.6+).
#[repr(C)]
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}

const SYS_OPENAT2: libc::c_long = 437;
const RESOLVE_NO_SYMLINKS: u64 = 0x04;
static OPENAT2_UNAVAILABLE: AtomicBool = AtomicBool::new(false);

/// Open a directory for scanning.
///
/// `is_root` → scan root (admin-configured). Open with plain `openat`:
/// the admin may deliberately point a root at a symlink, and `-L` mode
/// also expects to follow it.
///
/// `follow_links` → `-L` mode descent: user asked us to follow symlinks,
/// so a symlinked child is intentional. Open with plain `openat`.
///
/// Otherwise this is a `-P`-mode CHILD, and we use `openat2` with
/// `RESOLVE_NO_SYMLINKS` so no path component may be a symlink. That
/// closes the race where an unprivileged attacker in a writable scanned
/// directory (`/tmp`, `/home/<u>`) swaps a real directory for a symlink
/// between `getdents64` and our `open` — without this the daemon would
/// follow the symlink and index (or walk) an out-of-scope tree such as
/// `/root/`. On kernels older than 5.6 we fall back to `openat` with
/// `O_NOFOLLOW`, which still rejects a symlink at the leaf (the only
/// component attackers can swap given sticky-bit /tmp semantics).
fn open_dir_fd(path: &std::path::Path, is_root: bool, follow_links: bool) -> RawFd {
    let cpath = match CString::new(path.as_os_str().as_encoded_bytes()) {
        Ok(c) => c,
        Err(_) => return -1,
    };
    let base_flags = O_RDONLY | O_DIRECTORY | O_CLOEXEC;
    if is_root || follow_links {
        // Admin-trusted root, or `-L` mode where symlinks are expected.
        return unsafe { openat(AT_FDCWD, cpath.as_ptr(), base_flags) };
    }
    let strict_flags = base_flags | libc::O_NOFOLLOW;
    if !OPENAT2_UNAVAILABLE.load(Ordering::Relaxed) {
        let how = OpenHow {
            flags: strict_flags as u64,
            mode: 0,
            resolve: RESOLVE_NO_SYMLINKS,
        };
        let fd = unsafe {
            syscall(
                SYS_OPENAT2,
                AT_FDCWD as libc::c_long,
                cpath.as_ptr() as libc::c_long,
                &how as *const OpenHow as libc::c_long,
                std::mem::size_of::<OpenHow>() as libc::c_long,
            )
        } as RawFd;
        if fd >= 0 {
            return fd;
        }
        let err = std::io::Error::last_os_error();
        match err.raw_os_error() {
            Some(libc::ENOSYS) => {
                // Older kernel: fall back once and remember.
                OPENAT2_UNAVAILABLE.store(true, Ordering::Relaxed);
            }
            // ELOOP (final leaf is a symlink), EXDEV (crossed a mount with
            // RESOLVE_NO_XDEV — we don't set that but future-proof), or any
            // other refusal — do not retry with a less strict open.
            _ => return -1,
        }
    }
    // Fallback for pre-5.6 kernels. Protects only the leaf against the
    // symlink-swap race; mid-path races between getdents and this open
    // are a residual risk accepted on these kernels.
    unsafe { openat(AT_FDCWD, cpath.as_ptr(), strict_flags) }
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
    // Panic cleanup can store 0 while this worker is between pop and finish;
    // the fetch_sub then leaves the counter negative. Clamp back to 0 and
    // always notify in that case so pop_or_done sees "done".
    if remaining <= 0 {
        counters.in_flight.store(0, Ordering::Release);
        wq.condvar.notify_all();
    }
}

/// Immutable configuration shared with every worker thread. Split out of
/// `ScanConfig` because the `on_*` closures need `Arc` wrapping but these
/// knobs are plain data; one `Arc` for the whole bundle keeps the worker
/// signature readable.
struct WorkerShared {
    max_depth: Option<u32>,
    symlinks: SymlinkMode,
    statx_mask: u32,
    visited: Option<VisitedSet>,
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
    shared: &WorkerShared,
) {
    const BUF_SIZE: usize = 65536;
    let mut buf = vec![0u8; BUF_SIZE];
    // Entries of the current getdents64 buffer: (name, d_type).
    let mut batch: Vec<(CString, u8)> = Vec::with_capacity(512);

    loop {
        let item = match wq.pop_or_done(&counters.in_flight, cancel) {
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
        let item_depth = item.depth;
        let follow_links_descent = matches!(shared.symlinks, SymlinkMode::L);
        let dir_fd = open_dir_fd(&dir_path, item_depth == 0, follow_links_descent);
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

            // statx when we need the metadata the caller asked for
            // (mtime/size/atime/ctime), or when getdents didn't say whether
            // the entry is a directory (DT_UNKNOWN on some network and
            // FUSE filesystems) — otherwise those would never be scanned.
            // In `-L` mode we always statx so we can check the (dev, ino)
            // of the resolved target for the visited set.
            let follow_descent = matches!(shared.symlinks, SymlinkMode::L);
            let want_meta =
                collect_mtime || (shared.statx_mask & !STATX_TYPE) != 0 || follow_descent;
            let wants_stat = |d_type: u8| {
                d_type == DT_UNKNOWN
                    || (want_meta && (d_type == DT_REG || d_type == DT_DIR))
                    || (follow_descent && d_type == DT_LNK)
            };
            // `-L` resolves the symlink target; every other mode holds the
            // nofollow invariant used throughout the indexer.
            let statx_flags = if follow_descent {
                0
            } else {
                AT_SYMLINK_NOFOLLOW
            };
            let to_stat: Vec<&CStr> = batch
                .iter()
                .filter(|(_, t)| wants_stat(*t))
                .map(|(n, _)| n.as_c_str())
                .collect();
            let mut stats =
                statx_batch(&mut ring, dir_fd, &to_stat, shared.statx_mask, statx_flags)
                    .into_iter();

            let child_depth = item_depth + 1;
            // find(1) semantics for `--max-depth N`:
            //   emit when child_depth <= N (child is at or above the cap)
            //   push when child_depth < N  (do not descend past the cap)
            // None = unbounded on both.
            let emit_allowed = shared.max_depth.is_none_or(|m| child_depth <= m);
            let push_allowed = shared.max_depth.is_none_or(|m| child_depth < m);
            if !emit_allowed {
                continue;
            }

            for (name, d_type) in &batch {
                let st = if wants_stat(*d_type) {
                    stats.next().flatten()
                } else {
                    None
                };
                // `is_dir` after symlink resolution in `-L` mode (statx
                // without NOFOLLOW); otherwise prefer the fresh `statx`
                // (post-getdents) over the dirent's stale `DT_DIR` so an
                // attacker can't trick the scanner into enqueueing a
                // path that was briefly a directory and is now a
                // symlink. The dirent is only trusted when no stat was
                // taken (fast-path for non-regular, non-dir entries).
                let is_dir = match (follow_descent, *d_type, st) {
                    (true, _, Some(s)) => s.is_dir,
                    (_, _, Some(s)) => s.is_dir,
                    (_, DT_DIR, None) => true,
                    _ => false,
                };
                let is_symlink = *d_type == DT_LNK
                    || (*d_type == DT_UNKNOWN && st.is_some_and(|s| s.is_symlink));
                // Keep the raw bytes: a lossy name could not be reopened.
                let child_path = dir_path.join(std::ffi::OsStr::from_bytes(name.to_bytes()));
                if is_dir && skip_dir(&child_path) {
                    continue;
                }
                let (mtime_sec, mtime_nsec, size, atime_sec, ctime_sec) = match st {
                    // Directory sizes are derived from their files at query time.
                    Some(s) => (
                        s.mtime_sec,
                        s.mtime_nsec,
                        if is_dir { 0 } else { s.size },
                        s.atime_sec,
                        s.ctime_sec,
                    ),
                    None => (0, 0, 0, 0, 0),
                };
                on_entry(EntryInfo {
                    path: child_path.clone(),
                    is_dir,
                    is_symlink,
                    mtime_sec,
                    mtime_nsec,
                    atime_sec,
                    ctime_sec,
                    size,
                    depth: child_depth,
                });
                local_count += 1;

                if is_dir && push_allowed {
                    // `-L`: break cycles via (dev, ino) visited set.
                    if follow_descent {
                        if let (Some(set), Some(s)) = (&shared.visited, st) {
                            let mut g = set.lock().unwrap();
                            if !g.insert((s.dev, s.ino)) {
                                continue;
                            }
                        }
                    }
                    counters.in_flight.fetch_add(1, Ordering::Release);
                    wq.push(WorkItem {
                        path: child_path,
                        depth: child_depth,
                    });
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::os::unix::fs::symlink;
    use std::path::Path;
    use std::sync::Mutex;
    use tempfile::TempDir;

    /// Build a temp tree:
    ///   root/
    ///     a.txt
    ///     sub/
    ///       b.txt
    ///       deep/c.txt
    ///     link_rel -> sub           (symlink to directory)
    ///     link_dangling -> nope     (dangling symlink)
    fn fixture() -> TempDir {
        let d = TempDir::new().unwrap();
        let r = d.path();
        std::fs::write(r.join("a.txt"), b"a").unwrap();
        std::fs::create_dir(r.join("sub")).unwrap();
        std::fs::write(r.join("sub/b.txt"), b"b").unwrap();
        std::fs::create_dir(r.join("sub/deep")).unwrap();
        std::fs::write(r.join("sub/deep/c.txt"), b"c").unwrap();
        symlink("sub", r.join("link_rel")).unwrap();
        symlink("nope", r.join("link_dangling")).unwrap();
        d
    }

    fn collect(cfg: SearchConfig) -> (Vec<EntryInfo>, ScanResult) {
        let hits: Arc<Mutex<Vec<EntryInfo>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = {
            let h = Arc::clone(&hits);
            move |e: EntryInfo| h.lock().unwrap().push(e)
        };
        let on_hit: HitFn = Arc::new(sink);
        let cfg = SearchConfig { on_hit, ..cfg };
        let r = run_search(cfg);
        let v = std::mem::take(&mut *hits.lock().unwrap());
        (v, r)
    }

    fn names(entries: &[EntryInfo]) -> HashSet<String> {
        entries
            .iter()
            .map(|e| {
                e.path
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default()
            })
            .collect()
    }

    fn under(
        root: &Path,
        matcher: impl Fn(&EntryInfo) -> bool + Send + Sync + 'static,
    ) -> SearchConfig {
        SearchConfig::new(vec![root.to_path_buf()], matcher, |_| {})
            // Deterministic worker count for test stability.
            .with_workers_opt(2)
    }

    impl SearchConfig {
        fn with_workers_opt(mut self, n: usize) -> Self {
            self.workers = n.max(1);
            self
        }
    }

    #[test]
    fn walks_regular_tree_and_matches_all() {
        let d = fixture();
        let (hits, r) = collect(under(d.path(), |_| true));
        let n = names(&hits);
        assert!(n.contains("a.txt"));
        assert!(n.contains("sub"));
        assert!(n.contains("b.txt"));
        assert!(n.contains("deep"));
        assert!(n.contains("c.txt"));
        assert!(!r.cancelled);
    }

    #[test]
    fn max_depth_zero_emits_nothing() {
        let d = fixture();
        // find(1): maxdepth 0 = roots only, and the scanner does not
        // emit roots themselves.
        let (hits, _) = collect(SearchConfig {
            max_depth: Some(0),
            ..under(d.path(), |_| true)
        });
        assert!(hits.is_empty());
    }

    #[test]
    fn max_depth_one_emits_direct_children_only() {
        let d = fixture();
        let (hits, _) = collect(SearchConfig {
            max_depth: Some(1),
            ..under(d.path(), |_| true)
        });
        let n = names(&hits);
        assert!(n.contains("a.txt"));
        assert!(n.contains("sub"));
        // sub's own children must not be emitted.
        assert!(!n.contains("b.txt"));
    }

    #[test]
    fn max_depth_two_descends_one_level_into_each_root_child() {
        let d = fixture();
        let (hits, _) = collect(SearchConfig {
            max_depth: Some(2),
            ..under(d.path(), |_| true)
        });
        let n = names(&hits);
        assert!(n.contains("b.txt")); // depth 2
        assert!(n.contains("deep")); // depth 2
                                     // deep's contents sit at depth 3; still past the cap.
        assert!(!n.contains("c.txt"));
    }

    #[test]
    fn symlinks_detected_but_not_followed_in_mode_p() {
        let d = fixture();
        let (hits, _) = collect(under(d.path(), |_| true));
        let link = hits
            .iter()
            .find(|e| e.path.file_name().unwrap() == "link_rel")
            .expect("symlink entry present");
        assert!(link.is_symlink);
        assert!(!link.is_dir);
        // Default `-P`: no second traversal of `sub` via the symlink.
        let b_hits = hits
            .iter()
            .filter(|e| e.path.file_name().unwrap() == "b.txt")
            .count();
        assert_eq!(b_hits, 1);
    }

    #[test]
    fn mode_l_follows_symlinked_dirs_without_cycling() {
        let d = fixture();
        // Add a self-cycle: sub/cycle -> ..
        symlink("..", d.path().join("sub/cycle")).unwrap();
        let (hits, _) = collect(SearchConfig {
            symlinks: SymlinkMode::L,
            ..under(d.path(), |_| true)
        });
        let n = names(&hits);
        // `-L` resolves `link_rel` to `sub`, so we should hit `b.txt`
        // through both routes without the cycle looping forever.
        assert!(n.contains("b.txt"));
        // (dev, ino) visited set must dedupe the sub directory itself.
        let sub_dirs: Vec<_> = hits
            .iter()
            .filter(|e| e.is_dir && e.path.file_name().unwrap() == "sub")
            .collect();
        // One real sub + up to one followed symlink; critically, finite.
        assert!(sub_dirs.len() <= 2);
    }

    #[test]
    fn depth_field_tracks_distance_from_root() {
        let d = fixture();
        let (hits, _) = collect(under(d.path(), |_| true));
        let a = hits
            .iter()
            .find(|e| e.path.file_name().unwrap() == "a.txt")
            .unwrap();
        let b = hits
            .iter()
            .find(|e| e.path.file_name().unwrap() == "b.txt")
            .unwrap();
        let c = hits
            .iter()
            .find(|e| e.path.file_name().unwrap() == "c.txt")
            .unwrap();
        assert_eq!(a.depth, 1);
        assert_eq!(b.depth, 2);
        assert_eq!(c.depth, 3);
    }

    #[test]
    fn predicate_filters_before_sink_sees_the_entry() {
        let d = fixture();
        let (hits, _) = collect(under(d.path(), |e| {
            e.path
                .file_name()
                .is_some_and(|s| s.to_string_lossy().ends_with(".txt"))
        }));
        let n = names(&hits);
        assert!(n.contains("a.txt"));
        assert!(n.contains("b.txt"));
        assert!(n.contains("c.txt"));
        // Directories must have been filtered out.
        assert!(!n.contains("sub"));
        assert!(!n.contains("deep"));
    }

    #[test]
    fn cancel_stops_further_hits_quickly() {
        let d = fixture();
        let cancel = Arc::new(AtomicBool::new(false));
        let seen: Arc<AtomicU64> = Arc::new(AtomicU64::new(0));
        let s = Arc::clone(&seen);
        let c = Arc::clone(&cancel);
        let predicate = move |_: &EntryInfo| {
            s.fetch_add(1, Ordering::Relaxed);
            c.store(true, Ordering::Relaxed);
            true
        };
        let cfg = SearchConfig {
            cancel: Arc::clone(&cancel),
            ..SearchConfig::new(vec![d.path().to_path_buf()], predicate, |_| {}).with_workers_opt(1)
        };
        let (_hits, r) = collect(cfg);
        assert!(r.cancelled);
    }

    /// Regression: if `in_flight` is forced to 0 while a worker is between
    /// pop and finish_item, the next fetch_sub takes the counter negative.
    /// Old code only notified on `remaining == 0`, which left parked
    /// workers in condvar.wait forever and hung the scan on join().
    #[test]
    fn finish_item_notifies_and_clamps_on_underflow() {
        let wq = WorkQueue::new();
        let counters = Counters::default();
        // Simulate the "panic cleanup already stored 0" state: in_flight==0
        // but a worker is about to call finish_item for its popped item.
        counters.in_flight.store(0, Ordering::Release);
        let cancel = AtomicBool::new(false);

        // Park a "waiter" thread in pop_or_done to prove it gets released.
        let wq_a = Arc::new(wq);
        let counters_a = Arc::new(counters);
        let w1 = Arc::clone(&wq_a);
        let c1 = Arc::clone(&counters_a);
        // Pre-condition: pop_or_done must return None when in_flight<=0,
        // without parking.
        let got = w1.pop_or_done(&c1.in_flight, &cancel);
        assert!(got.is_none(), "pop_or_done must bail when in_flight<=0");

        // Now exercise finish_item underflow: start from 0, decrement, expect
        // the counter to be clamped back to 0 (not left at -1).
        finish_item(&wq_a, &counters_a);
        assert_eq!(counters_a.in_flight.load(Ordering::Acquire), 0);
    }

    /// Regression: in default `-P` mode, a symlink in the scanned tree
    /// must not be opened as a directory. This closes the TOCTOU where
    /// an attacker swaps a real dir for a symlink to an out-of-scope
    /// target between getdents and open.
    #[test]
    fn p_mode_refuses_to_descend_into_symlinked_dir() {
        // Build: outer/{real/inner.txt, link_to_real -> real}.
        // In -P mode the scanner should emit `real` and `link_to_real`
        // as entries but only descend into `real`, not through the
        // symlink. Previously the open_dir_fd call followed the symlink
        // and indexed `link_to_real/inner.txt`.
        let d = TempDir::new().unwrap();
        let outer = d.path();
        std::fs::create_dir(outer.join("real")).unwrap();
        std::fs::write(outer.join("real/inner.txt"), b"x").unwrap();
        symlink("real", outer.join("link_to_real")).unwrap();

        let (hits, _r) = collect(under(outer, |_| true));
        // The scanner may see `inner.txt` exactly once (through the
        // real dir), never twice (which would mean it descended the
        // symlink too).
        let inner_count = hits
            .iter()
            .filter(|e| e.path.ends_with("inner.txt"))
            .count();
        assert_eq!(
            inner_count, 1,
            "scanner descended through symlinked directory in -P mode (count={inner_count})"
        );
    }

    /// Regression: setting `cancel` while workers are parked in pop_or_done
    /// (end-of-scan tail) must release them, not wait for a straggler.
    #[test]
    fn pop_or_done_bails_on_cancel() {
        let wq = Arc::new(WorkQueue::new());
        let counters = Arc::new(Counters::default());
        // Pretend one item is still in flight so the <=0 guard won't fire.
        counters.in_flight.store(1, Ordering::Release);
        let cancel = Arc::new(AtomicBool::new(false));

        let wq_t = Arc::clone(&wq);
        let counters_t = Arc::clone(&counters);
        let cancel_t = Arc::clone(&cancel);
        let h = std::thread::spawn(move || wq_t.pop_or_done(&counters_t.in_flight, &cancel_t));
        // Give the thread a moment to park.
        std::thread::sleep(std::time::Duration::from_millis(30));
        cancel.store(true, Ordering::Relaxed);
        wq.wake_all();
        let got = h.join().unwrap();
        assert!(got.is_none(), "pop_or_done must bail on cancel");
    }
}
