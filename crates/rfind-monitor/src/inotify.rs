//! inotify backend — per-directory watches.
//!
//! Used on WSL2 and containers where fanotify is unavailable.
//!
//! # Avoiding missed events
//! - Watches are added through a [`DirWatcher`] handle *before* a directory
//!   is read, so the scanner can register each directory as it discovers
//!   it. Anything created after the watch is reported as an event; anything
//!   created before is seen by the read. Overlap is harmless because the
//!   index upserts.
//! - A directory created or moved into the tree is watched first, then
//!   walked, journaling everything already inside it (`mkdir -p`, `git
//!   clone`, `mv ~/Downloads/x .`).
//! - Watched directories that are renamed have their watch paths
//!   rewritten; directories moved out of the tree lose their watches and
//!   are journaled as deleted.
//! - IN_Q_OVERFLOW sets a flag the daemon turns into a rescan.

use crate::sink::EventSink;
use libc::{
    close, inotify_add_watch, inotify_init1, inotify_rm_watch, read, IN_CLOEXEC, IN_CLOSE_WRITE,
    IN_CREATE, IN_DELETE, IN_DELETE_SELF, IN_IGNORED, IN_ISDIR, IN_MOVED_FROM, IN_MOVED_TO,
    IN_NONBLOCK, IN_Q_OVERFLOW,
};
use rfind_core::db::EventType;
use std::{
    collections::HashMap,
    ffi::CString,
    fs, mem,
    os::unix::io::RawFd,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};
use tracing::{error, warn};

/// IN_CLOSE_WRITE keeps sizes and mtimes current after files are written.
const WATCH_MASK: u32 =
    IN_CREATE | IN_DELETE | IN_DELETE_SELF | IN_MOVED_FROM | IN_MOVED_TO | IN_CLOSE_WRITE;

/// How long a MOVED_FROM waits for its MOVED_TO before it is treated as
/// a move out of the watched tree.
const MOVE_PAIR_TIMEOUT: Duration = Duration::from_millis(500);

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
    wd: i32,
    mask: u32,
    cookie: u32,
    len: u32,
    // name follows: variable length, null-padded to next 4-byte boundary
}

/// The inotify fd plus the wd → path map. Owns the fd, so it stays valid
/// for as long as any [`DirWatcher`] or the event loop holds it.
struct Watches {
    fd: RawFd,
    map: Mutex<HashMap<i32, PathBuf>>,
    excludes: Vec<PathBuf>,
    limit_warned: AtomicBool,
}

impl Watches {
    fn is_excluded(&self, path: &Path) -> bool {
        self.excludes.iter().any(|ex| path.starts_with(ex))
    }

    fn add(&self, path: &Path) {
        if self.is_excluded(path) {
            return;
        }
        // N2: short-circuit once we've hit ENOSPC. Every subsequent
        // inotify_add_watch would fail anyway, and the daemon is already
        // scheduled for a rescan via the warning path below.
        if self.limit_warned.load(Ordering::Relaxed) {
            return;
        }
        let c = match CString::new(path.as_os_str().as_encoded_bytes()) {
            Ok(c) => c,
            Err(_) => return,
        };
        let wd = unsafe { inotify_add_watch(self.fd, c.as_ptr(), WATCH_MASK) };
        if wd >= 0 {
            self.map.lock().unwrap().insert(wd, path.to_path_buf());
            return;
        }
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::ENOSPC)
            && !self.limit_warned.swap(true, Ordering::Relaxed)
        {
            warn!(
                "inotify watch limit reached at {} watches — changes in unwatched \
                 directories will be missed. Raise fs.inotify.max_user_watches, or \
                 move large trees to scan.periodic_roots.",
                self.map.lock().unwrap().len()
            );
        }
    }

    fn get(&self, wd: i32) -> Option<PathBuf> {
        self.map.lock().unwrap().get(&wd).cloned()
    }

    fn forget(&self, wd: i32) {
        self.map.lock().unwrap().remove(&wd);
    }

    /// A watched directory was renamed: rewrite its path and its
    /// descendants' paths.
    fn rename_under(&self, old: &Path, new: &Path) {
        let mut m = self.map.lock().unwrap();
        for p in m.values_mut() {
            if let Ok(rest) = p.strip_prefix(old) {
                *p = if rest.as_os_str().is_empty() {
                    new.to_path_buf()
                } else {
                    new.join(rest)
                };
            }
        }
    }

    /// A directory left the watched tree: drop its watches. The kernel
    /// would otherwise keep reporting events for it under a stale path.
    fn remove_under(&self, path: &Path) {
        let mut m = self.map.lock().unwrap();
        let wds: Vec<i32> = m
            .iter()
            .filter(|(_, p)| p.starts_with(path))
            .map(|(wd, _)| *wd)
            .collect();
        for wd in wds {
            unsafe { inotify_rm_watch(self.fd, wd) };
            m.remove(&wd);
        }
    }
}

impl Drop for Watches {
    fn drop(&mut self) {
        unsafe { close(self.fd) };
    }
}

/// Cloneable handle for registering directory watches from other threads
/// (the scanner registers each directory before reading it).
#[derive(Clone)]
pub struct DirWatcher(Arc<Watches>);

impl DirWatcher {
    pub fn watch(&self, path: &Path) {
        self.0.add(path);
    }
}

pub struct InotifyMonitor {
    watches: Arc<Watches>,
    stop: Arc<AtomicBool>,
    overflow: Arc<AtomicBool>,
    /// Kept so `take_overflow` can also report in-process sink drops
    /// (writer thread fell behind). Mirrors the fanotify backend.
    sink: EventSink,
    alive: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
    writer: Option<thread::JoinHandle<()>>,
}

/// Flips `alive` to false on Drop so the thread's exit path (clean,
/// read()-error break, or panic unwind) all end up marking the monitor
/// dead. The daemon polls `is_dead` each tick and restarts.
struct AliveGuard<'a>(&'a Arc<AtomicBool>);
impl Drop for AliveGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Relaxed);
    }
}

impl InotifyMonitor {
    pub fn start(
        dirs: Vec<String>,
        sink: (EventSink, thread::JoinHandle<()>),
        excludes: Vec<PathBuf>,
    ) -> Result<Self, InotifyError> {
        let fd = unsafe { inotify_init1(IN_NONBLOCK | IN_CLOEXEC) };
        if fd < 0 {
            return Err(InotifyError::Init(std::io::Error::last_os_error()));
        }
        let watches = Arc::new(Watches {
            fd,
            map: Mutex::new(HashMap::new()),
            excludes,
            limit_warned: AtomicBool::new(false),
        });

        for dir in &dirs {
            watches.add(Path::new(dir));
        }

        let (sink, writer) = sink;
        let stop = Arc::new(AtomicBool::new(false));
        let overflow = Arc::new(AtomicBool::new(false));
        let alive = Arc::new(AtomicBool::new(true));
        let sink_for_take = sink.clone();
        let handle = {
            let (w, s, o, a) = (
                Arc::clone(&watches),
                Arc::clone(&stop),
                Arc::clone(&overflow),
                Arc::clone(&alive),
            );
            thread::spawn(move || {
                let _guard = AliveGuard(&a);
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    event_loop(w, sink, s, o)
                }));
                if r.is_err() {
                    error!("inotify event loop panicked; marking monitor dead");
                }
            })
        };

        Ok(Self {
            watches,
            stop,
            overflow,
            sink: sink_for_take,
            alive,
            handle: Some(handle),
            writer: Some(writer),
        })
    }

    pub fn watcher(&self) -> DirWatcher {
        DirWatcher(Arc::clone(&self.watches))
    }

    pub fn add_watch(&self, path: &Path) {
        self.watches.add(path);
    }

    /// True (once) if events were lost since the last call: kernel
    /// queue (`IN_Q_OVERFLOW`) or the in-process sink (writer fell
    /// behind). The daemon treats either as "trigger a full re-index".
    pub fn take_overflow(&self) -> bool {
        let kernel = self.overflow.swap(false, Ordering::Relaxed);
        let sink = self.sink.take_overflow();
        kernel || sink
    }

    /// True once the event-reading thread has exited (unexpected read()
    /// error, panic, or stop requested).
    pub fn is_dead(&self) -> bool {
        !self.alive.load(Ordering::Relaxed)
    }

    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // The event loop owns the only sink; once it exits the writer
        // drains the remaining events and exits too.
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        if let Some(h) = self.writer.take() {
            let _ = h.join();
        }
    }
}

struct PendingMove {
    path: PathBuf,
    is_dir: bool,
    at: Instant,
}

fn event_loop(
    watches: Arc<Watches>,
    sink: EventSink,
    stop: Arc<AtomicBool>,
    overflow: Arc<AtomicBool>,
) {
    const BUF: usize = 65536;
    let mut buf = vec![0u8; BUF];
    // Cookie map for MOVED_FROM/MOVED_TO pairing.
    let mut moves: HashMap<u32, PendingMove> = HashMap::new();
    let hdr = mem::size_of::<InotifyEvent>();

    // Background walker: when a directory is created or moved into the
    // watched tree, draining the subtree inline would hold the kernel
    // event drain long enough that a large `mv` can overflow IN_Q.
    // Offload the walk here instead.
    let (walk_tx, walk_rx) = std::sync::mpsc::channel::<PathBuf>();
    let walker = {
        let w = Arc::clone(&watches);
        let s = sink.clone();
        thread::spawn(move || {
            while let Ok(root) = walk_rx.recv() {
                watch_new_tree(&w, &s, &root);
            }
        })
    };

    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        // Run expiration every iteration, not only on WouldBlock: a busy
        // filesystem keeps `read()` returning data and the WouldBlock
        // branch never fires, so pending MOVED_FROM cookies would never
        // be flushed to deletes while the churn continues.
        expire_moves(&watches, &sink, &mut moves);

        let len = unsafe { read(watches.fd, buf.as_mut_ptr() as *mut _, BUF) };

        if len < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::WouldBlock {
                thread::sleep(Duration::from_millis(50));
                continue;
            }
            error!("inotify read: {e}");
            break;
        }
        if len == 0 {
            thread::sleep(Duration::from_millis(50));
            continue;
        }

        let mut off = 0usize;
        while off + hdr <= len as usize {
            let ev: InotifyEvent =
                unsafe { std::ptr::read_unaligned(buf.as_ptr().add(off) as *const InotifyEvent) };
            let name = if ev.len > 0 {
                let s = unsafe {
                    std::ffi::CStr::from_ptr(buf.as_ptr().add(off + hdr) as *const libc::c_char)
                };
                s.to_string_lossy().to_string()
            } else {
                String::new()
            };
            off += hdr + ev.len as usize;

            if ev.mask & IN_Q_OVERFLOW != 0 {
                warn!("inotify queue overflow — events lost, rescan scheduled");
                overflow.store(true, Ordering::Relaxed);
                continue;
            }
            if ev.mask & IN_IGNORED != 0 {
                watches.forget(ev.wd);
                continue;
            }
            let Some(dir) = watches.get(ev.wd) else {
                continue;
            };

            let is_dir = ev.mask & IN_ISDIR != 0;
            let full_path = if name.is_empty() {
                dir
            } else {
                dir.join(&name)
            };
            if watches.is_excluded(&full_path) {
                continue;
            }

            if ev.mask & IN_CREATE != 0 {
                sink.send(EventType::Create, lossy(&full_path), None, is_dir);
                if is_dir {
                    let _ = walk_tx.send(full_path.clone());
                }
            } else if ev.mask & IN_DELETE_SELF != 0 {
                // IN_DELETE on the parent is authoritative for this entry;
                // IN_DELETE_SELF is just a cue to drop our watch. Forget
                // the wd now so stray post-delete events are ignored.
                watches.forget(ev.wd);
            } else if ev.mask & IN_DELETE != 0 {
                sink.send(EventType::Delete, lossy(&full_path), None, is_dir);
            } else if ev.mask & IN_MOVED_FROM != 0 {
                moves.insert(
                    ev.cookie,
                    PendingMove {
                        path: full_path,
                        is_dir,
                        at: Instant::now(),
                    },
                );
            } else if ev.mask & IN_MOVED_TO != 0 {
                if let Some(m) = moves.remove(&ev.cookie) {
                    if is_dir {
                        watches.rename_under(&m.path, &full_path);
                    }
                    sink.send(
                        EventType::Rename,
                        lossy(&m.path),
                        Some(lossy(&full_path)),
                        is_dir,
                    );
                } else {
                    // Moved in from outside the watched tree.
                    sink.send(EventType::Create, lossy(&full_path), None, is_dir);
                    if is_dir {
                        let _ = walk_tx.send(full_path.clone());
                    }
                }
            } else if ev.mask & IN_CLOSE_WRITE != 0 {
                sink.send(EventType::Modify, lossy(&full_path), None, is_dir);
            }
        }

        expire_moves(&watches, &sink, &mut moves);
    }
    // Dropping walk_tx closes the channel; the walker drains any
    // outstanding roots and then exits.
    drop(walk_tx);
    let _ = walker.join();
}

/// MOVED_FROM with no MOVED_TO in time: the entry left the watched tree.
fn expire_moves(watches: &Watches, sink: &EventSink, moves: &mut HashMap<u32, PendingMove>) {
    if moves.is_empty() {
        return;
    }
    moves.retain(|_, m| {
        if m.at.elapsed() < MOVE_PAIR_TIMEOUT {
            return true;
        }
        if m.is_dir {
            watches.remove_under(&m.path);
        }
        sink.send(EventType::Delete, lossy(&m.path), None, m.is_dir);
        false
    });
}

/// Watch a directory that just appeared, then journal what is already in
/// it. Watch-before-read means nothing created in between is missed.
fn watch_new_tree(watches: &Watches, sink: &EventSink, root: &Path) {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        watches.add(&dir);
        let Ok(rd) = fs::read_dir(&dir) else { continue };
        for entry in rd.flatten() {
            let path = entry.path();
            if watches.is_excluded(&path) {
                continue;
            }
            let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
            sink.send(EventType::Create, lossy(&path), None, is_dir);
            if is_dir {
                stack.push(path);
            }
        }
    }
}

fn lossy(p: &Path) -> String {
    p.to_string_lossy().to_string()
}
