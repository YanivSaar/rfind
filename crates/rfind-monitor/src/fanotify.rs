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
    FAN_EVENT_INFO_TYPE_OLD_DFID_NAME, FAN_MARK_ADD, FAN_MARK_FILESYSTEM, FAN_MOVED_FROM,
    FAN_MOVED_TO, FAN_NONBLOCK, FAN_ONDIR, FAN_Q_OVERFLOW, FAN_RENAME, FAN_REPORT_DFID_NAME,
    FAN_REPORT_DFID_NAME_TARGET, FAN_REPORT_FID, O_CLOEXEC, O_DIRECTORY, O_LARGEFILE, O_PATH,
    O_RDONLY,
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
/// point used to resolve handles. The mount is opened per lookup, never
/// held open: an open fd would keep the filesystem busy and make
/// `umount` of a marked disk fail.
struct MarkedFs {
    fsid: [i32; 2],
    point: PathBuf,
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
}

struct Scope {
    roots: Vec<PathBuf>,
    excludes: Vec<PathBuf>,
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
                Ok(_) => info!("fanotify: watching filesystem at {}", point.display()),
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
        let handle = {
            let (s, o, st) = (Arc::clone(&stop), Arc::clone(&overflow), Arc::clone(&state));
            let scope = Scope { roots, excludes };
            thread::spawn(move || event_loop(&st, &scope, &sink, &s, &o))
        };

        Ok(Self {
            state,
            stop,
            overflow,
            handle: Some(handle),
            writer: Some(writer),
        })
    }

    /// Start watching another filesystem (e.g. one mounted after startup).
    /// Returns false if it was already watched.
    pub fn add_filesystem(&self, point: &Path) -> Result<bool, FanotifyError> {
        self.state.add(point)
    }

    /// True (once) if the kernel queue overflowed since the last call.
    pub fn take_overflow(&self) -> bool {
        self.overflow.swap(false, Ordering::Relaxed)
    }

    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        if let Some(h) = self.writer.take() {
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
    let err = |e: i32| FanotifyError::Mark(point.display().to_string(), e, strerror(e));
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
    Ok(MarkedFs {
        fsid,
        point: point.to_path_buf(),
    })
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
    let handle_end = 20 + handle_bytes;
    if rec.len() < handle_end {
        return None;
    }
    let point = marked
        .read()
        .unwrap()
        .iter()
        .find(|m| m.fsid == fsid)?
        .point
        .clone();
    let cpoint = CString::new(point.as_os_str().as_encoded_bytes()).ok()?;
    // Not O_PATH: open_by_handle_at() rejects O_PATH fds with EBADF.
    let mount_fd = unsafe { open(cpoint.as_ptr(), O_RDONLY | O_DIRECTORY | O_CLOEXEC) };
    if mount_fd < 0 {
        return None; // unmounted meanwhile
    }

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
    unsafe { close(mount_fd) };
    if dfd < 0 && open_err != libc::ESTALE && !RESOLVE_WARNED.swap(true, Ordering::Relaxed) {
        warn!(
            "fanotify: cannot resolve file handles ({}); events are being lost",
            strerror(open_err)
        );
    }
    if dfd < 0 {
        return None;
    } // directory already gone (ESTALE)
    let dir = fs::read_link(format!("/proc/self/fd/{dfd}"));
    unsafe { close(dfd) };
    let dir = dir.ok()?;
    if dir.as_os_str().as_encoded_bytes().ends_with(b" (deleted)") {
        return None;
    }

    let name = &rec[handle_end..];
    let name = &name[..name.iter().position(|&b| b == 0).unwrap_or(name.len())];
    match name {
        b"" | b"." => Some(dir),
        _ => Some(dir.join(std::ffi::OsStr::new(std::str::from_utf8(name).ok()?))),
    }
}

fn event_loop(
    state: &FanState,
    scope: &Scope,
    sink: &EventSink,
    stop: &AtomicBool,
    overflow: &AtomicBool,
) {
    const BUF: usize = 65536;
    let mut buf = vec![0u8; BUF];
    let meta_len = mem::size_of::<fanotify_event_metadata>();
    let lossy = |p: &Path| p.to_string_lossy().to_string();

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
                    (false, true) => moved_in(sink, scope, new.as_ref().unwrap(), is_dir),
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
                moved_in(sink, scope, &path, is_dir);
            } else if meta.mask & (FAN_DELETE | FAN_MOVED_FROM) != 0 {
                sink.send(EventType::Delete, lossy(&path), None, is_dir);
            } else if meta.mask & FAN_CREATE != 0 {
                sink.send(EventType::Create, lossy(&path), None, is_dir);
            } else if meta.mask & FAN_CLOSE_WRITE != 0 {
                sink.send(EventType::Modify, lossy(&path), None, is_dir);
            }
        }
    }
}

/// An entry appeared by moving: journal it and, for a directory, its
/// contents (they were indexed under the old path, or not at all).
fn moved_in(sink: &EventSink, scope: &Scope, path: &Path, is_dir: bool) {
    let lossy = |p: &Path| p.to_string_lossy().to_string();
    sink.send(EventType::Create, lossy(path), None, is_dir);
    if !is_dir {
        return;
    }
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = fs::read_dir(&dir) else { continue };
        for entry in rd.flatten() {
            let p = entry.path();
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
