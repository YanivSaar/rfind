//! fanotify backend — one mark per filesystem covers everything on it.
//!
//! Directory-entry events (create, delete, move) are only reported in FID
//! mode: events carry a file handle of the parent directory plus the entry
//! name instead of an open fd. Handles are resolved to paths with
//! open_by_handle_at() against an fd of that filesystem's mount, so paths
//! come out relative to the mount we chose (never a bind-mount alias).
//!
//! Unlike inotify there are no per-directory watches: no watch limit, and
//! no race between a directory appearing and being watched.
//!
//! Kernel support:
//! - 5.17+: FAN_RENAME reports both ends of a rename in one event.
//! - 5.9+:  FAN_MOVED_FROM / FAN_MOVED_TO; a moved directory is re-walked.
//! - older: start() fails and the caller falls back to inotify.
//!
//! Requires CAP_SYS_ADMIN (filesystem marks) and CAP_DAC_READ_SEARCH
//! (open_by_handle_at), i.e. root.

use crate::sink::EventSink;
use libc::{
    c_uint, close, fanotify_event_metadata, fanotify_init, fanotify_mark, fstatfs, open,
    open_by_handle_at, read, FAN_CLASS_NOTIF, FAN_CLOEXEC, FAN_CLOSE_WRITE, FAN_CREATE, FAN_DELETE,
    FAN_EVENT_INFO_TYPE_DFID_NAME, FAN_EVENT_INFO_TYPE_NEW_DFID_NAME,
    FAN_EVENT_INFO_TYPE_OLD_DFID_NAME, FAN_MARK_ADD, FAN_MARK_FILESYSTEM, FAN_MARK_REMOVE,
    FAN_MOVED_FROM, FAN_MOVED_TO, FAN_NONBLOCK, FAN_ONDIR, FAN_Q_OVERFLOW, FAN_RENAME,
    FAN_REPORT_DFID_NAME, FAN_REPORT_DFID_NAME_TARGET, FAN_REPORT_FID, O_CLOEXEC, O_DIRECTORY,
    O_LARGEFILE, O_PATH, O_RDONLY,
};
use rfind_core::db::EventType;
use std::{
    ffi::CString,
    fs, mem,
    os::unix::io::RawFd,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, RwLock,
    },
    thread,
    time::Duration,
};
use thiserror::Error;
use tracing::{error, info, warn};

/// Set after the first unexpected handle-resolution failure is reported.
static RESOLVE_WARNED: AtomicBool = AtomicBool::new(false);

const MASK_COMMON: u64 = FAN_CREATE | FAN_DELETE | FAN_CLOSE_WRITE | FAN_ONDIR;

#[derive(Debug, Error)]
pub enum FanotifyError {
    #[error("fanotify_init failed (errno {0}): {1}")]
    Init(i32, String),
    #[error("fanotify_mark failed on {0} (errno {1}): {2}")]
    Mark(String, i32, String),
    #[error("no filesystems to mark")]
    NoMarks,
}

/// A marked filesystem: its fsid (as reported in events) and the mount
/// point used to resolve handles.
///
/// `mount_fd` caches an open fd on the mount point so hot event streams
/// don't `open()`+`close()` it twice per event (that doubled syscall count
/// at 100k events/s). We never pin the fd across umount: ESTALE from
/// `open_by_handle_at` invalidates the cache and the next event reopens.
struct MarkedFs {
    fsid: [i32; 2],
    point: PathBuf,
    mount_fd: std::sync::Mutex<RawFd>,
}

impl Drop for MarkedFs {
    fn drop(&mut self) {
        let fd = *self.mount_fd.lock().unwrap();
        if fd >= 0 {
            unsafe { close(fd) };
        }
    }
}

/// The fanotify fd and its marks, shared with the event loop so
/// filesystems can be added while it runs.
struct FanState {
    fd: RawFd,
    mask: u64,
    marked: RwLock<Vec<MarkedFs>>,
}

impl Drop for FanState {
    fn drop(&mut self) {
        unsafe { close(self.fd) };
    }
}

impl FanState {
    /// Mark the filesystem at `point` (no-op if it is already marked).
    fn add(&self, point: &Path) -> Result<bool, FanotifyError> {
        let m = mark_filesystem(self.fd, point, self.mask)?;
        let mut marked = self.marked.write().unwrap();
        if marked.iter().any(|x| x.fsid == m.fsid) {
            return Ok(false);
        }
        marked.push(m);
        Ok(true)
    }

    /// Drop the mark on the filesystem that was mounted at `point`, close
    /// its cached mount fd and forget the fsid. Called by the daemon when
    /// a mount disappears from `/proc/self/mounts`; keeping the stale
    /// entry pins a (possibly dangling) fd and lets a later event with a
    /// reused fsid resolve against the wrong mount.
    ///
    /// The kernel-side `fanotify_mark(FAN_MARK_REMOVE)` is best-effort: if
    /// the mount is already gone it returns ENOENT, which we ignore. The
    /// cache entry is removed in all cases. Returns true if an entry was
    /// found and removed.
    fn remove(&self, point: &Path) -> bool {
        let mut marked = self.marked.write().unwrap();
        let Some(idx) = marked.iter().position(|m| m.point == point) else {
            return false;
        };
        if let Ok(c) = CString::new(point.as_os_str().as_encoded_bytes()) {
            unsafe {
                fanotify_mark(
                    self.fd,
                    FAN_MARK_REMOVE | FAN_MARK_FILESYSTEM,
                    self.mask,
                    libc::AT_FDCWD,
                    c.as_ptr(),
                );
            }
        }
        // Dropping the MarkedFs closes any cached mount fd.
        marked.swap_remove(idx);
        true
    }
}

struct Scope {
    roots: Vec<PathBuf>,
    excludes: Vec<PathBuf>,
}

/// Flips `alive` to false in Drop so the thread's exit path (clean,
/// `break`-on-error, or panic unwind) all end up marking the monitor
/// dead. The daemon polls `is_dead` each tick.
struct AliveGuard<'a>(&'a Arc<AtomicBool>);
impl Drop for AliveGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Relaxed);
    }
}

impl Scope {
    fn contains(&self, p: &Path) -> bool {
        self.roots.iter().any(|r| p.starts_with(r))
            && !self.excludes.iter().any(|ex| p.starts_with(ex))
    }
}

pub struct FanotifyMonitor {
    state: Arc<FanState>,
    stop: Arc<AtomicBool>,
    overflow: Arc<AtomicBool>,
    /// Kept so `take_overflow` can also report in-process sink drops
    /// (writer thread fell behind, `sync_channel` full). Without this,
    /// a sink-full event would be dropped silently and no rescan fires.
    sink: EventSink,
    alive: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
    writer: Option<thread::JoinHandle<()>>,
}

impl FanotifyMonitor {
    /// Mark each filesystem in `mark_points` (one mount point per
    /// filesystem; the first must succeed) and report events for paths
    /// under `roots` that are not under `excludes`.
    pub fn start(
        mark_points: &[PathBuf],
        roots: Vec<PathBuf>,
        sink: (EventSink, thread::JoinHandle<()>),
        excludes: Vec<PathBuf>,
    ) -> Result<Self, FanotifyError> {
        if mark_points.is_empty() {
            return Err(FanotifyError::NoMarks);
        }
        let (fd, has_rename) = init_fanotify()?;
        let mask = MASK_COMMON
            | if has_rename {
                FAN_RENAME
            } else {
                FAN_MOVED_FROM | FAN_MOVED_TO
            };

        let state = Arc::new(FanState {
            fd,
            mask,
            marked: RwLock::new(Vec::new()),
        });
        for (i, point) in mark_points.iter().enumerate() {
            match state.add(point) {
                Ok(_) => info!(
                    "fanotify: watching filesystem at {}",
                    rfind_core::log::sanitize_path(point)
                ),
                // Dropping `state` closes the fanotify fd.
                Err(e) if i == 0 => return Err(e),
                Err(e) => {
                    warn!("{e} — changes there won't be seen live; consider scan.periodic_roots")
                }
            }
        }
        info!(
            "fanotify: {} renames",
            if has_rename {
                "single-event (FAN_RENAME)"
            } else {
                "paired move events"
            }
        );

        let (sink, writer) = sink;
        let stop = Arc::new(AtomicBool::new(false));
        let overflow = Arc::new(AtomicBool::new(false));
        let alive = Arc::new(AtomicBool::new(true));
        let sink_for_take = sink.clone();
        let handle = {
            let (s, o, st, a) = (
                Arc::clone(&stop),
                Arc::clone(&overflow),
                Arc::clone(&state),
                Arc::clone(&alive),
            );
            let scope = Arc::new(Scope { roots, excludes });
            thread::spawn(move || {
                // The event loop can `break` on an unexpected read() error
                // (lazy unmount, EIO) or panic. Either way the daemon must
                // notice — `alive` goes false here and the daemon forces a
                // full re-index + restart on the next tick.
                let _guard = AliveGuard(&a);
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    event_loop(&st, scope, &sink, &s, o)
                }));
                if r.is_err() {
                    error!("fanotify event loop panicked; marking monitor dead");
                }
            })
        };

        Ok(Self {
            state,
            stop,
            overflow,
            sink: sink_for_take,
            alive,
            handle: Some(handle),
            writer: Some(writer),
        })
    }

    /// Start watching another filesystem (e.g. one mounted after startup).
    /// Returns false if it was already watched.
    pub fn add_filesystem(&self, point: &Path) -> Result<bool, FanotifyError> {
        self.state.add(point)
    }

    /// Stop watching the filesystem that was mounted at `point` and release
    /// its cached mount fd. Called when the mount disappeared.
    pub fn remove_filesystem(&self, point: &Path) -> bool {
        self.state.remove(point)
    }

    /// True (once) if events were lost since the last call: kernel queue
    /// (`FAN_Q_OVERFLOW`) or the in-process sink (writer thread fell
    /// behind). Either way the daemon must trigger a full re-index.
    pub fn take_overflow(&self) -> bool {
        let kernel = self.overflow.swap(false, Ordering::Relaxed);
        let sink = self.sink.take_overflow();
        kernel || sink
    }

    /// True once the event-reading thread has exited (unexpected read()
    /// error, panic, or stop requested). The daemon treats this as an
    /// overflow and schedules a restart.
    pub fn is_dead(&self) -> bool {
        !self.alive.load(Ordering::Relaxed)
    }

    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        // The writer's `rx.recv()` only returns Err once every sender is
        // dropped. The event-loop thread above dropped its sender; the
        // last one is in `self.sink` (kept so `take_overflow` can report
        // in-process sink drops). Move the writer handle out, then drop
        // `self` — which drops `self.sink` — so the writer sees the
        // channel disconnected and exits. Without this, join() below
        // blocks forever and `systemctl stop` ends in SIGKILL.
        let writer = self.writer.take();
        drop(self);
        if let Some(h) = writer {
            let _ = h.join();
        }
    }
}

fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}
fn strerror(e: i32) -> String {
    std::io::Error::from_raw_os_error(e).to_string()
}

/// Initialise in FID mode, preferring FAN_RENAME support. Returns the fd
/// and whether FAN_RENAME is available.
fn init_fanotify() -> Result<(RawFd, bool), FanotifyError> {
    let base = FAN_CLASS_NOTIF | FAN_NONBLOCK | FAN_CLOEXEC;
    let event_flags = (O_RDONLY | O_LARGEFILE) as c_uint;
    let fd = unsafe { fanotify_init(base | FAN_REPORT_DFID_NAME_TARGET, event_flags) };
    if fd >= 0 {
        return Ok((fd, true));
    }
    let fd = unsafe { fanotify_init(base | FAN_REPORT_FID | FAN_REPORT_DFID_NAME, event_flags) };
    if fd >= 0 {
        return Ok((fd, false));
    }
    let e = errno();
    Err(FanotifyError::Init(e, strerror(e)))
}

fn mark_filesystem(fd: RawFd, point: &Path, mask: u64) -> Result<MarkedFs, FanotifyError> {
    let err = |e: i32| FanotifyError::Mark(rfind_core::log::sanitize_path(point), e, strerror(e));
    let c = CString::new(point.as_os_str().as_encoded_bytes()).map_err(|_| err(libc::EINVAL))?;
    if unsafe {
        fanotify_mark(
            fd,
            FAN_MARK_ADD | FAN_MARK_FILESYSTEM,
            mask,
            libc::AT_FDCWD,
            c.as_ptr(),
        )
    } < 0
    {
        return Err(err(errno()));
    }
    // From here on, if we exit without an Ok(MarkedFs), the kernel mark
    // is left set with no caller tracking (next remove_filesystem won't
    // find it, and the mark survives until the fanotify fd itself is
    // closed on daemon exit — but it still delivers events we can no
    // longer resolve). This guard removes the mark on any early return
    // or panic between here and Ok construction.
    struct UnmarkGuard<'a> {
        fd: RawFd,
        mask: u64,
        cpath: &'a std::ffi::CStr,
        armed: bool,
    }
    impl Drop for UnmarkGuard<'_> {
        fn drop(&mut self) {
            if self.armed {
                unsafe {
                    fanotify_mark(
                        self.fd,
                        FAN_MARK_REMOVE | FAN_MARK_FILESYSTEM,
                        self.mask,
                        libc::AT_FDCWD,
                        self.cpath.as_ptr(),
                    );
                }
            }
        }
    }
    let mut guard = UnmarkGuard {
        fd,
        mask,
        cpath: &c,
        armed: true,
    };

    let mount_fd = unsafe { open(c.as_ptr(), O_RDONLY | O_DIRECTORY | O_CLOEXEC) };
    if mount_fd < 0 {
        return Err(err(errno()));
    }
    let mut st: libc::statfs = unsafe { mem::zeroed() };
    let ret = unsafe { fstatfs(mount_fd, &mut st) };
    let e = errno();
    unsafe { close(mount_fd) };
    if ret < 0 {
        return Err(err(e));
    }
    // fsid_t's field is private; it is two i32s, as in event records.
    let fsid: [i32; 2] = unsafe { mem::transmute(st.f_fsid) };
    guard.armed = false; // Ownership transfers to MarkedFs; its Drop unmarks.
    Ok(MarkedFs {
        fsid,
        point: point.to_path_buf(),
        mount_fd: std::sync::Mutex::new(-1),
    })
}

/// Fuzz-only entry point: run the DFID_NAME record parser against
/// arbitrary bytes with no marked filesystems. The empty mark list
/// short-circuits the `find()` lookup before any syscall, so this
/// exercises only the bounds-check + offset-arithmetic paths.
#[doc(hidden)]
pub fn __fuzz_resolve(rec: &[u8]) -> Option<PathBuf> {
    let marked: RwLock<Vec<MarkedFs>> = RwLock::new(Vec::new());
    resolve(rec, &marked)
}

/// Resolve a DFID_NAME info record (header + fsid + file_handle + name)
/// to the full path of the entry it names.
fn resolve(rec: &[u8], marked: &RwLock<Vec<MarkedFs>>) -> Option<PathBuf> {
    let i32_at = |o: usize| {
        rec.get(o..o + 4)
            .map(|b| i32::from_ne_bytes(b.try_into().unwrap()))
    };
    let fsid = [i32_at(4)?, i32_at(8)?];
    let handle_bytes = i32_at(12)? as u32 as usize;
    // MAX_HANDLE_SZ is 128 on every current kernel. A larger value is
    // either kernel corruption, an unusual filesystem, or a crafted
    // record from a tampered journal; in all three cases allocating
    // handle_bytes bytes and then passing them to open_by_handle_at
    // under CAP_DAC_READ_SEARCH is strictly worse than dropping the
    // event, so cap defensively.
    const MAX_HANDLE_SZ: usize = 128;
    if handle_bytes > MAX_HANDLE_SZ {
        return None;
    }
    let handle_end = 20 + handle_bytes;
    if rec.len() < handle_end {
        return None;
    }
    // Look up the mount point AND take a snapshot (or lazily create) of
    // its cached mount fd without holding the RwLock across the syscalls.
    //
    // Critically, we `dup()` the cached fd while still under the read
    // lock: without that, `remove_filesystem` can drop the MarkedFs
    // between our `read()` and `open_by_handle_at`, closing the cached
    // fd — the kernel then reuses the fd number for an unrelated open
    // and open_by_handle_at resolves against the WRONG inode (worst
    // case: a file the caller never meant to touch). dup() gives us
    // our own reference whose validity is independent of the cache.
    let (point, mount_fd_owned) = {
        let g = marked.read().unwrap();
        let m = g.iter().find(|m| m.fsid == fsid)?;
        let cached = *m.mount_fd.lock().unwrap();
        let dup_fd = if cached >= 0 {
            let d = unsafe { libc::dup(cached) };
            if d >= 0 {
                Some(d)
            } else {
                None
            }
        } else {
            None
        };
        (m.point.clone(), dup_fd)
    };
    let cpoint = CString::new(point.as_os_str().as_encoded_bytes()).ok()?;
    let mut mount_fd = mount_fd_owned.unwrap_or(-1);
    // We always own `mount_fd` from here: either dup gave us a fresh fd,
    // or we open() a fresh one below. In either case we close on exit.
    let close_on_exit = true;
    if mount_fd < 0 {
        // Not O_PATH: open_by_handle_at() rejects O_PATH fds with EBADF.
        mount_fd = unsafe { open(cpoint.as_ptr(), O_RDONLY | O_DIRECTORY | O_CLOEXEC) };
        if mount_fd < 0 {
            return None; // unmounted meanwhile
        }
        // Publish into the cache (dup) so the next event skips open();
        // keep our owned fd for the syscall below and close it on exit.
        if let Some(m) = marked.read().unwrap().iter().find(|m| m.fsid == fsid) {
            let mut slot = m.mount_fd.lock().unwrap();
            if *slot < 0 {
                let d = unsafe { libc::dup(mount_fd) };
                if d >= 0 {
                    *slot = d;
                }
            }
        }
    }
    let _ = close_on_exit;

    // struct file_handle { u32 handle_bytes; i32 handle_type; u8 f_handle[] },
    // copied into a 4-byte-aligned buffer.
    let mut fh = vec![0u32; (handle_end - 12).div_ceil(4)];
    unsafe {
        std::ptr::copy_nonoverlapping(
            rec[12..handle_end].as_ptr(),
            fh.as_mut_ptr() as *mut u8,
            handle_end - 12,
        );
    }
    let dfd = unsafe {
        open_by_handle_at(
            mount_fd,
            fh.as_mut_ptr() as *mut libc::file_handle,
            O_PATH | O_CLOEXEC,
        )
    };
    let open_err = errno();
    // We always own our mount_fd (dup'd from the cache, or opened fresh
    // and dup'd back into the cache). Close ours unconditionally.
    unsafe { close(mount_fd) };
    if open_err == libc::ESTALE {
        // The directory went away. The mount itself may still be fine, so
        // don't drop the cached fd; a subsequent event with a live handle
        // will succeed. If instead the mount went away, open() will fail
        // next time and we'll reopen the cached slot via `-1`.
    }
    if dfd < 0 && open_err != libc::ESTALE && !RESOLVE_WARNED.swap(true, Ordering::Relaxed) {
        warn!(
            "fanotify: cannot resolve file handles ({}); events are being lost",
            strerror(open_err)
        );
    }
    if dfd < 0 {
        return None;
    } // directory already gone (ESTALE)
      // Was the dentry unlinked while the fd is open? The old check for
      // " (deleted)" suffix on the proc readlink also dropped events on
      // real files named "foo (deleted)". Use fstat + nlink instead:
      // nlink becomes 0 the moment the last dentry is unlinked, and no
      // legitimate on-disk file can have nlink==0.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    let st_err = unsafe { libc::fstat(dfd, &mut st) };
    let dir = fs::read_link(format!("/proc/self/fd/{dfd}"));
    unsafe { close(dfd) };
    if st_err != 0 || st.st_nlink == 0 {
        return None;
    }
    let dir = dir.ok()?;

    let name = &rec[handle_end..];
    let name = &name[..name.iter().position(|&b| b == 0).unwrap_or(name.len())];
    match name {
        b"" | b"." => Some(dir),
        // Linux allows arbitrary bytes (except NUL and /) in filenames, so
        // we must NOT drop events with non-UTF-8 names. Build the Path from
        // raw bytes via OsStrExt instead of str::from_utf8.
        _ => {
            use std::os::unix::ffi::OsStrExt;
            Some(dir.join(std::ffi::OsStr::from_bytes(name)))
        }
    }
}

fn event_loop(
    state: &FanState,
    scope: Arc<Scope>,
    sink: &EventSink,
    stop: &AtomicBool,
    overflow: Arc<AtomicBool>,
) {
    const BUF: usize = 65536;
    let mut buf = vec![0u8; BUF];
    let meta_len = mem::size_of::<fanotify_event_metadata>();
    // Trade-off: a non-UTF-8 filename is rendered with U+FFFD. The index
    // and the SQLite journal are TEXT columns, so a lossy render is what
    // the DB can store; a subsequent rename of the real filename won't
    // match the lossy one and will show up as delete+create. Acceptable
    // for the real-world cost (near-zero on modern systems). To fix
    // properly, the schema would need to carry raw bytes.
    let lossy = |p: &Path| p.to_string_lossy().to_string();

    // Background walker: moved_in otherwise blocks the event drain while
    // read_dir()-ing a potentially huge new subtree, long enough for
    // FAN_Q_OVERFLOW to fire. Offload the walk here.
    let (walk_tx, walk_rx) = std::sync::mpsc::channel::<(PathBuf, bool)>();
    let walker = {
        let sc = Arc::clone(&scope);
        let sk = sink.clone();
        let overflow_for_walker = Arc::clone(&overflow);
        thread::spawn(move || {
            while let Ok((path, is_dir)) = walk_rx.recv() {
                // Without this guard, a single panic in moved_in (e.g. a
                // transient scope lock poison, or an OS error inside
                // read_dir that panics on an older libstd) silently kills
                // the walker. The event thread would keep running — and
                // every FAN_CREATE|FAN_ONDIR afterwards would queue
                // forever in the unbounded channel while the index drifts
                // behind the filesystem. Promote to overflow instead; the
                // next tick forces a reconcile.
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    moved_in(&sk, &sc, &path, is_dir);
                }));
                if r.is_err() {
                    error!(
                        "fanotify walker panicked on {}; scheduling rescan",
                        rfind_core::log::sanitize_path(&path)
                    );
                    overflow_for_walker.store(true, Ordering::Relaxed);
                }
            }
        })
    };

    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let len = unsafe { read(state.fd, buf.as_mut_ptr() as *mut _, BUF) };
        if len < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::WouldBlock {
                thread::sleep(Duration::from_millis(50));
                continue;
            }
            error!("fanotify read: {e}");
            break;
        }
        if len == 0 {
            thread::sleep(Duration::from_millis(50));
            continue;
        }
        let len = len as usize;

        let mut off = 0usize;
        while off + meta_len <= len {
            let meta: fanotify_event_metadata =
                unsafe { std::ptr::read_unaligned(buf.as_ptr().add(off) as *const _) };
            if meta.vers != libc::FANOTIFY_METADATA_VERSION || meta.event_len as usize == 0 {
                break;
            }
            let end = (off + meta.event_len as usize).min(len);
            if meta.fd >= 0 {
                unsafe { close(meta.fd) };
            }
            if meta.mask & FAN_Q_OVERFLOW != 0 {
                warn!("fanotify queue overflow — events lost, rescan scheduled");
                overflow.store(true, Ordering::Relaxed);
            }

            // Info records: the entry (DFID_NAME) or both rename ends.
            let (mut path, mut old, mut new) = (None, None, None);
            let mut p = off + meta.metadata_len as usize;
            while p + 4 <= end {
                let (kind, rec_len) = (
                    buf[p],
                    u16::from_ne_bytes([buf[p + 2], buf[p + 3]]) as usize,
                );
                if rec_len < 4 || p + rec_len > end {
                    break;
                }
                let rec = &buf[p..p + rec_len];
                match kind {
                    FAN_EVENT_INFO_TYPE_DFID_NAME => path = resolve(rec, &state.marked),
                    FAN_EVENT_INFO_TYPE_OLD_DFID_NAME => old = resolve(rec, &state.marked),
                    FAN_EVENT_INFO_TYPE_NEW_DFID_NAME => new = resolve(rec, &state.marked),
                    _ => {}
                }
                p += rec_len;
            }
            off = end;

            let is_dir = meta.mask & FAN_ONDIR != 0;
            let in_scope = |p: &Option<PathBuf>| p.as_deref().is_some_and(|p| scope.contains(p));
            if meta.mask & FAN_RENAME != 0 {
                match (in_scope(&old), in_scope(&new)) {
                    (true, true) => sink.send(
                        EventType::Rename,
                        lossy(old.as_ref().unwrap()),
                        Some(lossy(new.as_ref().unwrap())),
                        is_dir,
                    ),
                    (false, true) => {
                        let _ = walk_tx.send((new.unwrap(), is_dir));
                    }
                    (true, false) => sink.send(
                        EventType::Delete,
                        lossy(old.as_ref().unwrap()),
                        None,
                        is_dir,
                    ),
                    (false, false) => {}
                }
                continue;
            }
            if !in_scope(&path) {
                continue;
            }
            let path = path.unwrap();
            if meta.mask & FAN_MOVED_TO != 0 {
                let _ = walk_tx.send((path, is_dir));
            } else if meta.mask & (FAN_DELETE | FAN_MOVED_FROM) != 0 {
                sink.send(EventType::Delete, lossy(&path), None, is_dir);
            } else if meta.mask & FAN_CREATE != 0 {
                sink.send(EventType::Create, lossy(&path), None, is_dir);
            } else if meta.mask & FAN_CLOSE_WRITE != 0 {
                sink.send(EventType::Modify, lossy(&path), None, is_dir);
            }
        }
    }
    drop(walk_tx);
    let _ = walker.join();
}

/// An entry appeared by moving: journal it and, for a directory, its
/// contents (they were indexed under the old path, or not at all).
///
/// Uses symlink-safe reads: between FAN_CREATE|FAN_ONDIR and this
/// descent, an attacker who owns the parent could swap the new entry
/// for a symlink to an out-of-scope target. `read_dir_safe` rejects
/// any symlinked component.
fn moved_in(sink: &EventSink, scope: &Scope, path: &Path, is_dir: bool) {
    use rfind_core::safewalk::read_dir_safe;
    let lossy = |p: &Path| p.to_string_lossy().to_string();
    sink.send(EventType::Create, lossy(path), None, is_dir);
    if !is_dir {
        return;
    }
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = read_dir_safe(&dir) else {
            continue;
        };
        for entry in rd.flatten() {
            // Reconstruct from the real dir — see watch_new_tree.
            let p = dir.join(entry.file_name());
            if !scope.contains(&p) {
                continue;
            }
            let d = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
            sink.send(EventType::Create, lossy(&p), None, d);
            if d {
                stack.push(p);
            }
        }
    }
}
