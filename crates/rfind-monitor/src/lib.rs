//! Filesystem event monitor.
//!
//! Strategy:
//! 1. Try fanotify (one mount watch, bare metal / full VM).
//! 2. Fall back to inotify (per-directory watches, WSL2 / containers).
//! 3. If both fail, return NotSupported — daemon runs in index-only mode.
//!
//! Both backends write events to the same pending_events SQLite table.

mod fanotify;
mod inotify;

pub use fanotify::FanotifyMonitor;
pub use inotify::InotifyMonitor;

use rfind_core::db::{insert_pending_event, EventType};
use rusqlite::Connection;
use std::{
    path::PathBuf,
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
    pub fn start(
        mount: &PathBuf,
        dirs: Vec<String>,       // for inotify: list of dirs to watch
        db: Arc<Mutex<Connection>>,
        excludes: Vec<PathBuf>,
        prefer_fanotify: bool,
        inotify_fallback: bool,
    ) -> Result<(Self, String), MonitorError> {
        if prefer_fanotify {
            match FanotifyMonitor::start(mount, Arc::clone(&db), excludes.clone()) {
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
            match InotifyMonitor::start(dirs, Arc::clone(&db), excludes) {
                Ok(m) => return Ok((Monitor::Inotify(m), "inotify".to_string())),
                Err(e) => {
                    tracing::warn!("inotify unavailable: {e}");
                    return Err(MonitorError::Inotify(e.to_string()));
                }
            }
        }

        Err(MonitorError::NotSupported)
    }

    pub fn add_watch(&self, path: &PathBuf) {
        if let Monitor::Inotify(m) = self {
            m.add_watch(path);
        }
    }

    pub fn stop(self) {
        match self {
            Monitor::Fanotify(m) => m.stop(),
            Monitor::Inotify(m)  => m.stop(),
        }
    }
}
