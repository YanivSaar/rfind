//! inotify backend — per-directory watches.
//!
//! Used on WSL2 and containers where fanotify is unavailable.
//! Watches all indexed directories. New directories are watched
//! automatically when a CREATE event is seen for a directory.

use libc::{
    close, inotify_add_watch, inotify_init1, read,
    IN_CREATE, IN_DELETE, IN_DELETE_SELF, IN_MOVED_FROM,
    IN_MOVED_TO, IN_NONBLOCK,
};
use rfind_core::db::{insert_pending_event, EventType};
use rusqlite::Connection;
use std::{
    collections::HashMap,
    ffi::CString,
    mem,
    os::unix::io::RawFd,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, SystemTime},
};
use tracing::{debug, error, warn};

const WATCH_MASK: u32 =
    IN_CREATE | IN_DELETE | IN_DELETE_SELF | IN_MOVED_FROM | IN_MOVED_TO;

#[derive(Debug, thiserror::Error)]
pub enum InotifyError {
    #[error("inotify_init1 failed: {0}")]
    Init(std::io::Error),
    #[error("inotify_add_watch failed for {0}: {1}")]
    Watch(String, std::io::Error),
}

/// inotify event structure (kernel ABI).
#[repr(C)]
struct InotifyEvent {
    wd:     i32,
    mask:   u32,
    cookie: u32,
    len:    u32,
    // name follows: variable length, null-padded to next 4-byte boundary
}

pub struct InotifyMonitor {
    fd:     RawFd,
    stop:   Arc<AtomicBool>,
    /// Channel to add new watches from outside the event loop.
    add_tx: std::sync::mpsc::Sender<PathBuf>,
    handle: Option<thread::JoinHandle<()>>,
}

impl InotifyMonitor {
    pub fn start(
        dirs: Vec<String>,
        db: Arc<Mutex<Connection>>,
        excludes: Vec<PathBuf>,
    ) -> Result<Self, InotifyError> {
        let fd = unsafe { inotify_init1(IN_NONBLOCK) };
        if fd < 0 {
            return Err(InotifyError::Init(std::io::Error::last_os_error()));
        }

        // wd → path mapping shared between the loop and add_watch.
        let wd_map: Arc<Mutex<HashMap<i32, PathBuf>>> =
            Arc::new(Mutex::new(HashMap::new()));

        // Register initial set of directories.
        for dir in &dirs {
            let path = PathBuf::from(dir);
            if excludes.iter().any(|ex| path.starts_with(ex)) { continue; }
            let wd = add_watch_fd(fd, &path);
            if wd >= 0 {
                wd_map.lock().unwrap().insert(wd, path);
            }
        }

        let (add_tx, add_rx) = std::sync::mpsc::channel::<PathBuf>();
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = Arc::clone(&stop);
        let wd_map2 = Arc::clone(&wd_map);

        let handle = thread::spawn(move || {
            event_loop(fd, db, excludes, stop2, wd_map2, add_rx);
            unsafe { close(fd) };
        });

        Ok(Self { fd, stop, add_tx, handle: Some(handle) })
    }

    /// Add a watch for a newly created directory.
    pub fn add_watch(&self, path: &PathBuf) {
        let _ = self.add_tx.send(path.clone());
    }

    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() { let _ = h.join(); }
    }
}

fn add_watch_fd(ifd: RawFd, path: &PathBuf) -> i32 {
    let c = match CString::new(path.as_os_str().as_encoded_bytes()) {
        Ok(c) => c,
        Err(_) => return -1,
    };
    unsafe { inotify_add_watch(ifd, c.as_ptr(), WATCH_MASK) }
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
    wd_map: Arc<Mutex<HashMap<i32, PathBuf>>>,
    add_rx: std::sync::mpsc::Receiver<PathBuf>,
) {
    const BUF: usize = 65536;
    let mut buf = vec![0u8; BUF];
    // Cookie map for MOVED_FROM/MOVED_TO pairing.
    let mut move_cookies: HashMap<u32, (PathBuf, String)> = HashMap::new();

    loop {
        if stop.load(Ordering::Relaxed) { break; }

        // Process any pending add_watch requests.
        while let Ok(path) = add_rx.try_recv() {
            if excludes.iter().any(|ex| path.starts_with(ex)) { continue; }
            let wd = add_watch_fd(fd, &path);
            if wd >= 0 {
                wd_map.lock().unwrap().insert(wd, path);
            }
        }

        let len = unsafe { read(fd, buf.as_mut_ptr() as *mut _, BUF) };

        if len < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::WouldBlock {
                thread::sleep(Duration::from_millis(50));
                continue;
            }
            error!("inotify read: {e}");
            break;
        }
        if len == 0 { thread::sleep(Duration::from_millis(50)); continue; }

        let mut off = 0usize;
        while off + mem::size_of::<InotifyEvent>() <= len as usize {
            let ev = unsafe {
                &*(buf.as_ptr().add(off) as *const InotifyEvent)
            };

            let dir_path = {
                let m = wd_map.lock().unwrap();
                m.get(&ev.wd).cloned()
            };

            if let Some(dir) = dir_path {
                // Extract the filename from the event (follows the struct).
                let name = if ev.len > 0 {
                    let name_ptr = unsafe {
                        buf.as_ptr().add(off + mem::size_of::<InotifyEvent>())
                    };
                    let s = unsafe {
                        std::ffi::CStr::from_ptr(name_ptr as *const libc::c_char)
                    };
                    s.to_string_lossy().to_string()
                } else {
                    String::new()
                };

                let is_dir = ev.mask & libc::IN_ISDIR != 0;
                let full_path = if name.is_empty() {
                    dir.clone()
                } else {
                    dir.join(&name)
                };

                if !excludes.iter().any(|ex| full_path.starts_with(ex)) {
                    if ev.mask & IN_CREATE != 0 {
                        // Auto-watch newly created directories.
                        if is_dir {
                            let wd = add_watch_fd(fd, &full_path);
                            if wd >= 0 {
                                wd_map.lock().unwrap().insert(wd, full_path.clone());
                            }
                        }
                        record_event(&db, EventType::Create, &full_path, None, is_dir);
                    } else if ev.mask & (IN_DELETE | IN_DELETE_SELF) != 0 {
                        record_event(&db, EventType::Delete, &full_path, None, is_dir);
                    } else if ev.mask & IN_MOVED_FROM != 0 {
                        // Store for pairing with MOVED_TO.
                        move_cookies.insert(ev.cookie, (dir.clone(), name));
                    } else if ev.mask & IN_MOVED_TO != 0 {
                        if let Some((_, old_name)) = move_cookies.remove(&ev.cookie) {
                            let old_path = dir.join(&old_name);
                            record_event(
                                &db, EventType::Rename,
                                &old_path, Some(&full_path), is_dir,
                            );
                        } else {
                            // MOVED_TO without a matching MOVED_FROM
                            // (cross-watch or overflow) — treat as Create.
                            record_event(&db, EventType::Create, &full_path, None, is_dir);
                        }
                    }
                }
            }

            off += mem::size_of::<InotifyEvent>() + ev.len as usize;
        }

        // Expire stale move cookies (no matching MOVED_TO arrived).
        if move_cookies.len() > 1000 {
            move_cookies.clear();
        }
    }
}

fn record_event(
    db: &Arc<Mutex<Connection>>,
    et: EventType,
    path: &PathBuf,
    new_path: Option<&PathBuf>,
    is_dir: bool,
) {
    let c = db.lock().unwrap();
    if let Err(e) = insert_pending_event(
        &c, &et,
        &path.to_string_lossy(),
        new_path.map(|p| p.to_string_lossy().to_string()).as_deref(),
        is_dir,
        now_secs(),
    ) {
        error!("insert inotify event: {e}");
    }
}
