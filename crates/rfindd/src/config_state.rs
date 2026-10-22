//! Applying configuration changes to an existing index.
//!
//! The settings that decide what is indexed are recorded in the index. When
//! rfindd starts (or re-execs on reload) with different ones, it brings the
//! index in line instead of rebuilding it:
//! - entries outside every root, or under a new exclude, are deleted now;
//! - live roots are rescanned if roots or excludes changed (that also
//!   indexes paths whose exclude was removed);
//! - a periodic root is rescanned if an exclude inside it was removed.

use crate::scan::root_scan_key;
use rfind_config::Config;
use rfind_core::db;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tracing::info;

const KEY: &str = "config_state";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct PeriodicState {
    path: PathBuf,
    interval_hours: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct State {
    roots: Vec<PathBuf>,
    // Full (path, interval_hours) so changing an interval alone is also
    // recorded; new intervals take effect from the next scheduling pass.
    #[serde(default)]
    periodic: Vec<PeriodicState>,
    exclude: Vec<PathBuf>,
    collect_mtime: bool,
}

impl State {
    fn of(cfg: &Config) -> Self {
        let mut periodic: Vec<PeriodicState> = cfg
            .scan
            .periodic_roots
            .iter()
            .map(|r| PeriodicState {
                path: r.path.clone(),
                interval_hours: r.interval_hours,
            })
            .collect();
        periodic.sort_by(|a, b| a.path.cmp(&b.path));
        let mut s = Self {
            roots: cfg.scan.roots.clone(),
            periodic,
            exclude: cfg.scan.exclude.clone(),
            collect_mtime: cfg.scan.collect_mtime,
        };
        s.roots.sort();
        s.exclude.sort();
        s
    }
}

/// What the caller still has to do after [`apply`].
#[derive(Debug, Default)]
pub struct Changes {
    /// Live roots need a full rescan.
    pub rescan_live: bool,
}

/// Compare `cfg` with the state recorded in the index and reconcile.
/// `cfg` is the effective config (bind-mount aliases already excluded).
pub fn apply(conn: &Connection, cfg: &Config) -> Changes {
    let now = State::of(cfg);
    let prev: Option<State> = db::get_metadata(conn, KEY)
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok());
    let save = |s: &State| {
        let _ = db::set_metadata(conn, KEY, &serde_json::to_string(s).unwrap_or_default());
    };
    let Some(prev) = prev else {
        save(&now); // first run, or an index from before this was recorded
        return Changes::default();
    };
    if prev == now {
        return Changes::default();
    }

    let all_roots: Vec<String> = now
        .roots
        .iter()
        .chain(now.periodic.iter().map(|p| &p.path))
        .map(|p| p.to_string_lossy().to_string())
        .collect();
    let mut purged = db::delete_outside_roots(conn, &all_roots).unwrap_or(0);
    for ex in now.exclude.iter().filter(|e| !prev.exclude.contains(e)) {
        purged += db::delete_entries_under(conn, &ex.to_string_lossy()).unwrap_or(0);
    }

    let removed_excludes: Vec<&PathBuf> = prev
        .exclude
        .iter()
        .filter(|e| !now.exclude.contains(e))
        .collect();
    // Compare roots+excludes+collect_mtime, but ignore interval changes:
    // a new interval doesn't need an immediate rescan, just reschedules.
    let roots_or_exclude_changed = prev.roots != now.roots
        || prev.exclude != now.exclude
        || prev.collect_mtime != now.collect_mtime;
    let rescan_live = roots_or_exclude_changed;
    // Periodic roots: rescan if collection settings changed or an
    // *ancestor* exclude was removed. New periodic roots are scanned
    // anyway (they have no last-scan time).
    //
    // Before v1.10.1 the ancestor test was reversed (`e.starts_with(p)`
    // instead of `p.starts_with(e)`), so removing /mnt from excludes
    // did not re-arm periodic roots like /mnt/c. Combined with v1.9's
    // overbroad default of /mnt in excludes, WSL2 installs silently
    // never indexed Windows drives on first run.
    let prev_periodic_paths: std::collections::HashSet<&PathBuf> =
        prev.periodic.iter().map(|p| &p.path).collect();
    for pp in &now.periodic {
        let p = &pp.path;
        let changed = prev.collect_mtime != now.collect_mtime
            || removed_excludes.iter().any(|e| p.starts_with(e));
        if changed && prev_periodic_paths.contains(p) {
            let _ = db::set_metadata(conn, &root_scan_key(p), "0");
            info!(
                "Config change: periodic root {} will be rescanned",
                p.display()
            );
        }
    }
    info!(
        "Config changed: removed {purged} entries now excluded or outside all roots{}",
        if rescan_live {
            "; rescanning live roots"
        } else {
            ""
        }
    );
    save(&now);
    Changes { rescan_live }
}
