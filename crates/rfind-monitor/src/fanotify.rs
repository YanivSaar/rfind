//! fanotify backend — one mount watch covers the entire filesystem.

use libc::{
    close, fanotify_event_metadata, fanotify_init, fanotify_mark, read,
    FAN_CLASS_NOTIF, FAN_CLOSE_WRITE, FAN_CREATE, FAN_DELETE,
    FAN_DELETE_SELF, FAN_EVENT_ON_CHILD, FAN_MARK_ADD, FAN_MARK_MOUNT,
    FAN_MOVE, FAN_MOVE_SELF, FAN_NONBLOCK, FAN_ONDIR, O_LARGEFILE, O_RDONLY,
};
use rfind_core::db::{insert_pending_event, EventType};
use rusqlite::Connection;
use std::{
    ffi::CString,
    fs, mem,
    os::unix::io::RawFd,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, SystemTime},
};
use thiserror::Error;
use tracing::{debug, error, warn};

const WATCH_MASK: u64 = (FAN_CREATE | FAN_DELETE | FAN_MOVE | FAN_CLOSE_WRITE
    | FAN_DELETE_SELF | FAN_MOVE_SELF | FAN_ONDIR | FAN_EVENT_ON_CHILD) as u64;

#[derive(Debug, Error)]
pub enum FanotifyError {
    #[error("fanotify_init failed (errno {0}): {1}")]
    Init(i32, String),
    #[error("fanotify_mark failed (errno {0}): {1}")]
    Mark(i32, String),
}

pub struct FanotifyMonitor {
    stop:   Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl FanotifyMonitor {
    pub fn start(
        mount: &PathBuf,
        db: Arc<Mutex<Connection>>,
        excludes: Vec<PathBuf>,
    ) -> Result<Self, FanotifyError> {
        let fd = init_fanotify()?;
        mark_mount(fd, mount)?;

        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = Arc::clone(&stop);

        let handle = thread::spawn(move || {
            event_loop(fd, db, excludes, stop2);
            unsafe { close(fd) };
        });

        Ok(Self { stop, handle: Some(handle) })
    }

    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() { let _ = h.join(); }
    }
}

fn errno() -> i32 { unsafe { *libc::__errno_location() } }

fn init_fanotify() -> Result<RawFd, FanotifyError> {
    let fd = unsafe {
        fanotify_init(
            (FAN_CLASS_NOTIF | FAN_NONBLOCK) as u32,
            (O_RDONLY | O_LARGEFILE) as u32,
        )
    };
    if fd < 0 {
        let e = errno();
        return Err(FanotifyError::Init(e,
            std::io::Error::from_raw_os_error(e).to_string()));
    }
    Ok(fd)
}

fn mark_mount(fd: RawFd, mount: &Path) -> Result<(), FanotifyError> {
    let c = CString::new(mount.as_os_str().as_encoded_bytes())
        .map_err(|_| FanotifyError::Mark(0, "invalid path".into()))?;
    let ret = unsafe {
        fanotify_mark(fd, FAN_MARK_ADD | FAN_MARK_MOUNT,
                      WATCH_MASK, libc::AT_FDCWD, c.as_ptr())
    };
    if ret < 0 {
        let e = errno();
        unsafe { close(fd) };
        return Err(FanotifyError::Mark(e,
            std::io::Error::from_raw_os_error(e).to_string()));
    }
    Ok(())
}

fn fd_to_path(fd: libc::c_int) -> Option<PathBuf> {
    fs::read_link(format!("/proc/self/fd/{fd}")).ok()
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn event_loop(
    fd: RawFd,
    db: Arc<Mutex<Connection>>,
    excludes: Vec<PathBuf>,
    stop: Arc<AtomicBool>,
) {
    const BUF: usize = 4096;
    let mut buf = vec![0u8; BUF];

    loop {
        if stop.load(Ordering::Relaxed) { break; }

        let len = unsafe { read(fd, buf.as_mut_ptr() as *mut _, BUF) };

        if len < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::WouldBlock {
                thread::sleep(Duration::from_millis(50));
                continue;
            }
            error!("fanotify read: {e}");
            break;
        }
        if len == 0 { thread::sleep(Duration::from_millis(50)); continue; }

        let mut off = 0usize;
        while off + mem::size_of::<fanotify_event_metadata>() <= len as usize {
            let meta = unsafe {
                &*(buf.as_ptr().add(off) as *const fanotify_event_metadata)
            };
            if meta.vers != libc::FANOTIFY_METADATA_VERSION { break; }

            let efd = meta.fd;
            if efd >= 0 {
                if let Some(path) = fd_to_path(efd) {
                    unsafe { close(efd) };
                    if !excludes.iter().any(|ex| path.starts_with(ex)) {
                        let et = if meta.mask & FAN_CREATE as u64 != 0 {
                            Some(EventType::Create)
                        } else if meta.mask & (FAN_DELETE | FAN_DELETE_SELF) as u64 != 0 {
                            Some(EventType::Delete)
                        } else if meta.mask & (FAN_MOVE | FAN_MOVE_SELF) as u64 != 0 {
                            Some(EventType::Rename)
                        } else if meta.mask & FAN_CLOSE_WRITE as u64 != 0 {
                            Some(EventType::Modify)
                        } else { None };

                        if let Some(et) = et {
                            let is_dir = path.is_dir();
                            let c = db.lock().unwrap();
                            if let Err(e) = insert_pending_event(
                                &c, &et, &path.to_string_lossy(), None, is_dir, now_secs(),
                            ) { error!("insert event: {e}"); }
                        }
                    }
                } else {
                    unsafe { close(efd) };
                }
            }
            off += meta.event_len as usize;
        }
    }
}
