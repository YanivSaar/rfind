//! Filesystem event monitor using Linux fanotify.
//!
//! Designed to degrade gracefully when fanotify is unavailable — on WSL2,
//! restricted containers, or kernels with fanotify disabled. The daemon
//! continues normally without live updates; the index is still built and
//! searchable, just not automatically kept current.
//!
//! On bare metal Linux and full VMs, fanotify works with CAP_SYS_ADMIN
//! (root). One FAN_MARK_MOUNT watch covers the entire filesystem.

use libc::{
    close, fanotify_event_metadata, fanotify_init, fanotify_mark, read,
    FAN_CLASS_NOTIF, FAN_CLOSE_WRITE, FAN_CREATE, FAN_DELETE,
    FAN_DELETE_SELF, FAN_EVENT_ON_CHILD, FAN_MARK_ADD, FAN_MARK_MOUNT,
    FAN_MOVE, FAN_MOVE_SELF, FAN_NONBLOCK, FAN_ONDIR, O_LARGEFILE,
    O_RDONLY,
};
use rfind_core::db::{insert_pending_event, EventType};
use rusqlite::Connection;
use std::{
    ffi::CString,
    fs,
    mem,
    os::unix::io::RawFd,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, SystemTime},
};
use tracing::{debug, error, info, warn};

const WATCH_MASK: u64 = (FAN_CREATE
    | FAN_DELETE
    | FAN_MOVE
    | FAN_CLOSE_WRITE
    | FAN_DELETE_SELF
    | FAN_MOVE_SELF
    | FAN_ONDIR
    | FAN_EVENT_ON_CHILD) as u64;

#[derive(Debug, thiserror::Error)]
pub enum MonitorError {
    #[error("fanotify_init failed (errno {errno}): {msg}")]
    Init { errno: i32, msg: String },
    #[error("fanotify_mark failed for {path} (errno {errno}): {msg}")]
    Mark { path: String, errno: i32, msg: String },
    #[error("fanotify not supported in this environment: {0}")]
    NotSupported(String),
}

pub struct Monitor {
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl Drop for Monitor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Monitor {
    /// Attempt to start fanotify monitoring.
    ///
    /// Returns Ok(Monitor) on success.
    /// Returns Err(MonitorError) when fanotify is unavailable — the caller
    /// should log the error and continue without live monitoring.
    ///
    /// This function never blocks. If fanotify_init or fanotify_mark would
    /// block or fail, the error is returned immediately.
    pub fn start(
        mount: &Path,
        db: Arc<Mutex<Connection>>,
        excludes: Vec<PathBuf>,
    ) -> Result<Self, MonitorError> {
        // Probe fanotify availability before spawning any threads.
        // This returns immediately with an error on WSL2, containers,
        // or kernels with fanotify restricted.
        let fd = init_fanotify()?;
        mark_mount(fd, mount)?;

        info!("fanotify watching mount: {:?}", mount);

        let stop = Arc::new(AtomicBool::new(false));
        let stop_clone = Arc::clone(&stop);

        let handle = thread::spawn(move || {
            event_loop(fd, db, excludes, stop_clone);
            unsafe { close(fd) };
        });

        Ok(Self { stop, handle: Some(handle) })
    }

    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn errno() -> i32 {
    unsafe { *libc::__errno_location() }
}

fn init_fanotify() -> Result<RawFd, MonitorError> {
    let fd = unsafe {
        fanotify_init(
            (FAN_CLASS_NOTIF | FAN_NONBLOCK) as u32,
            (O_RDONLY | O_LARGEFILE) as u32,
        )
    };
    if fd < 0 {
        let e = errno();
        let msg = std::io::Error::from_raw_os_error(e).to_string();
        // EPERM (1) or ENOSYS (38) = fanotify not available in this env.
        return Err(if e == libc::EPERM || e == libc::ENOSYS {
            MonitorError::NotSupported(format!(
                "fanotify_init: {msg} — running on WSL2, container, or \
                 restricted kernel? Live updates disabled."
            ))
        } else {
            MonitorError::Init { errno: e, msg }
        });
    }
    Ok(fd)
}

fn mark_mount(fd: RawFd, mount: &Path) -> Result<(), MonitorError> {
    let path_str = mount.to_string_lossy().to_string();
    let c_path = CString::new(path_str.as_bytes()).map_err(|_| MonitorError::Mark {
        path: path_str.clone(),
        errno: 0,
        msg: "invalid path".into(),
    })?;

    let ret = unsafe {
        fanotify_mark(
            fd,
            (FAN_MARK_ADD | FAN_MARK_MOUNT) as u32,
            WATCH_MASK,
            libc::AT_FDCWD,
            c_path.as_ptr(),
        )
    };

    if ret < 0 {
        let e = errno();
        let msg = std::io::Error::from_raw_os_error(e).to_string();
        unsafe { close(fd) };
        return Err(MonitorError::Mark { path: path_str, errno: e, msg });
    }
    Ok(())
}

fn fd_to_path(fd: libc::c_int) -> Option<PathBuf> {
    fs::read_link(format!("/proc/self/fd/{fd}")).ok()
}

fn is_excluded(path: &Path, excludes: &[PathBuf]) -> bool {
    excludes.iter().any(|ex| path.starts_with(ex))
}

fn process_event(path: &PathBuf, mask: u64, db: &Arc<Mutex<Connection>>) {
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;

    let path_str = path.to_string_lossy();
    let is_dir = path.is_dir();

    let event_type = if mask & FAN_CREATE as u64 != 0 {
        Some(EventType::Create)
    } else if mask & (FAN_DELETE | FAN_DELETE_SELF) as u64 != 0 {
        Some(EventType::Delete)
    } else if mask & (FAN_MOVE | FAN_MOVE_SELF) as u64 != 0 {
        Some(EventType::Rename)
    } else if mask & FAN_CLOSE_WRITE as u64 != 0 {
        Some(EventType::Modify)
    } else {
        None
    };

    if let Some(et) = event_type {
        debug!("fanotify {:?} on {}", et, path_str);
        let conn = db.lock().unwrap();
        if let Err(e) = insert_pending_event(
            &conn, &et, &path_str, None, is_dir, now,
        ) {
            error!("Failed to record event for {}: {e}", path_str);
        }
    }
}

fn event_loop(
    fd: RawFd,
    db: Arc<Mutex<Connection>>,
    excludes: Vec<PathBuf>,
    stop: Arc<AtomicBool>,
) {
    const BUF_SIZE: usize = 4096;
    let mut buf = vec![0u8; BUF_SIZE];

    loop {
        if stop.load(Ordering::Relaxed) { break; }

        let len = unsafe {
            read(fd, buf.as_mut_ptr() as *mut libc::c_void, BUF_SIZE)
        };

        if len < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::WouldBlock {
                thread::sleep(Duration::from_millis(50));
                continue;
            }
            error!("fanotify read error: {err}");
            break;
        }

        if len == 0 {
            thread::sleep(Duration::from_millis(50));
            continue;
        }

        let mut offset = 0usize;
        while offset + mem::size_of::<fanotify_event_metadata>() <= len as usize {
            let meta = unsafe {
                &*(buf.as_ptr().add(offset) as *const fanotify_event_metadata)
            };

            if meta.vers != libc::FANOTIFY_METADATA_VERSION {
                warn!("fanotify metadata version mismatch");
                break;
            }

            let event_fd = meta.fd;
            if event_fd >= 0 {
                if let Some(path) = fd_to_path(event_fd) {
                    unsafe { close(event_fd) };
                    if !is_excluded(&path, &excludes) {
                        process_event(&path, meta.mask, &db);
                    }
                } else {
                    unsafe { close(event_fd) };
                }
            }

            offset += meta.event_len as usize;
        }
    }
}
