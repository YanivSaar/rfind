//! Filesystem event monitor.
//!
//! Strategy:
//! 1. Try fanotify (one mount watch, bare metal / full VM).
//! 2. Fall back to inotify (per-directory watches, WSL2 / containers).
//! 3. If both fail, return NotSupported — daemon runs in index-only mode.
//!
//! Both backends hand events to a writer thread ([`sink`]) that journals
//! them into the pending_events SQLite table.

mod fanotify;
mod inotify;
mod sink;

pub use fanotify::FanotifyMonitor;
pub use inotify::{DirWatcher, InotifyMonitor};

use rusqlite::Connection;
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum MonitorError {
    #[error("fanotify unavailable: {0}")]
    Fanotify(String),
    #[error("inotify unavailable: {0}")]
    Inotify(String),
    #[error("No filesystem monitoring available in this environment")]
    NotSupported,
}

pub enum Monitor {
    Fanotify(FanotifyMonitor),
    Inotify(InotifyMonitor),
}

impl Monitor {
    /// Start monitoring. Tries fanotify first, then inotify.
    /// Returns the active monitor and a string describing the mode.
    ///
    /// `mark_points`: one mount point per filesystem to cover (fanotify).
    /// `dirs`: directories to watch up front (inotify fast restart).
    pub fn start(
        roots: &[PathBuf],
        mark_points: &[PathBuf],
        dirs: Vec<String>,
        db: Arc<Mutex<Connection>>,
        excludes: Vec<PathBuf>,
        prefer_fanotify: bool,
        inotify_fallback: bool,
    ) -> Result<(Self, String), MonitorError> {
        // Each backend owns its own sink + writer. If fanotify fails and
        // we fall back to inotify, the first writer has already exited
        // (the sink's EventSink was dropped on the `Err` path, which
        // ends the writer's `rx.recv()` loop), so this is not a double
        // spawn in practice — the fallback starts fresh.
        if prefer_fanotify {
            let sink = sink::spawn_writer(Arc::clone(&db));
            match FanotifyMonitor::start(mark_points, roots.to_vec(), sink, excludes.clone()) {
                Ok(m) => return Ok((Monitor::Fanotify(m), "fanotify".to_string())),
                Err(e) => {
                    tracing::warn!("fanotify unavailable: {e}");
                    if !inotify_fallback {
                        return Err(MonitorError::Fanotify(e.to_string()));
                    }
                }
            }
        }

        if inotify_fallback {
            let sink = sink::spawn_writer(Arc::clone(&db));
            match InotifyMonitor::start(dirs, sink, excludes) {
                Ok(m) => return Ok((Monitor::Inotify(m), "inotify".to_string())),
                Err(e) => {
                    tracing::warn!("inotify unavailable: {e}");
                    return Err(MonitorError::Inotify(e.to_string()));
                }
            }
        }

        Err(MonitorError::NotSupported)
    }

    pub fn add_watch(&self, path: &Path) {
        if let Monitor::Inotify(m) = self {
            m.add_watch(path);
        }
    }

    /// Handle for registering directory watches during a scan. `None` for
    /// fanotify, whose mount mark already covers new directories.
    pub fn dir_watcher(&self) -> Option<DirWatcher> {
        match self {
            Monitor::Inotify(m) => Some(m.watcher()),
            Monitor::Fanotify(_) => None,
        }
    }

    /// Watch a filesystem mounted after startup. fanotify marks it; inotify
    /// needs no action here (the scan of the new mount adds watches).
    pub fn add_filesystem(&self, point: &Path) -> Result<bool, String> {
        match self {
            Monitor::Fanotify(m) => m.add_filesystem(point).map_err(|e| e.to_string()),
            Monitor::Inotify(_) => Ok(false),
        }
    }

    /// True (once) if events were lost since the last call: kernel
    /// queue overflow OR the in-process sink filling up (writer thread
    /// fell behind). The index must be rescanned to recover.
    pub fn take_overflow(&self) -> bool {
        match self {
            Monitor::Fanotify(m) => m.take_overflow(),
            Monitor::Inotify(m) => m.take_overflow(),
        }
    }

    /// True once the backend's event-reading thread has exited (e.g.
    /// unexpected read() error on EIO/lazy unmount, or a panic). The
    /// daemon treats this as needing a full re-index + restart.
    pub fn is_dead(&self) -> bool {
        match self {
            Monitor::Fanotify(m) => m.is_dead(),
            Monitor::Inotify(m) => m.is_dead(),
        }
    }

    pub fn stop(self) {
        match self {
            Monitor::Fanotify(m) => m.stop(),
            Monitor::Inotify(m) => m.stop(),
        }
    }
}
