//! Configuration for rfindd.
//!
//! Reads /etc/rfindd/config.toml if present, otherwise uses defaults.
//! All fields have sensible defaults so the daemon works out of the box.
//! Unknown keys are rejected, so a typo can't silently change behaviour.
//!
//! [`edit`] changes the file in place (keeping comments) for `rfind config`.

pub mod edit;

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub scan: ScanConfig,
    pub index: IndexConfig,
    pub monitor: MonitorConfig,
    pub search: SearchConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ScanConfig {
    /// Filesystem roots to index and keep current via fanotify/inotify.
    pub roots: Vec<PathBuf>,
    /// Roots kept current by periodic rescans only — no live monitoring.
    /// Use for mounts where inotify is blind or too costly, e.g. WSL2's
    /// /mnt/c (changes made from Windows never generate events).
    /// A periodic root nested inside a live root is skipped by that root.
    pub periodic_roots: Vec<PeriodicRoot>,
    /// Paths to skip entirely (no index, no recursion).
    pub exclude: Vec<PathBuf>,
    /// Worker thread count. 0 = num_cpus.
    pub workers: usize,
    /// Hours between automatic re-indexes in index-only mode. 0 = never.
    pub reindex_interval_hours: u64,
    /// Collect mtime via statx during scan (adds slight overhead).
    pub collect_mtime: bool,
    /// Batch the scanner's statx calls through io_uring when the kernel
    /// allows it (falls back to plain syscalls otherwise).
    pub io_uring: bool,
    /// Maximum directory depth to descend during a scan. Protects against
    /// symlink loops and pathologically deep trees in writable roots.
    /// 4096 is well past any real-world layout and in line with
    /// PATH_MAX-era kernel limits; set to 0 to disable (unbounded).
    pub max_depth: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PeriodicRoot {
    pub path: PathBuf,
    /// Hours between rescans of this root.
    #[serde(default = "default_periodic_interval")]
    pub interval_hours: u64,
}

fn default_periodic_interval() -> u64 {
    12
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct IndexConfig {
    pub db_path: PathBuf,
    pub batch_size: usize,
    /// Store the index zstd-compressed (about 3× smaller). Changing it
    /// rebuilds the index.
    pub compress: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MonitorConfig {
    /// Try fanotify first.
    pub prefer_fanotify: bool,
    /// Fall back to inotify if fanotify is unavailable.
    pub inotify_fallback: bool,
    /// Index and monitor filesystems mounted under a live root after
    /// startup (USB disks, network shares, ...).
    pub watch_new_mounts: bool,
}

/// Behaviour of `rfind` (the CLI), when it has a choice between querying
/// the index and walking the filesystem live.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SearchConfig {
    /// Default resolution strategy. CLI flags (--live/--no-index) win.
    pub mode: SearchModePref,
    /// Use io_uring for walk-mode statx batching when the kernel allows it.
    /// This is separate from `scan.io_uring` (the indexer's own knob).
    pub io_uring: bool,
    /// Worker threads for walk mode. 0 = cores × 2 (I/O-bound default).
    pub threads: usize,
}

/// User preference for which backend answers a query.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SearchModePref {
    /// Pick index when the daemon is ready and the filter is index-answerable;
    /// walk otherwise.
    #[default]
    Auto,
    /// Never walk; fail with a helpful error when the index is unusable.
    Index,
    /// Never query the index; always walk.
    Walk,
}

impl Default for ScanConfig {
    fn default() -> Self {
        Self {
            roots: vec![PathBuf::from("/")],
            periodic_roots: Vec::new(),
            exclude: vec![
                PathBuf::from("/proc"),
                PathBuf::from("/sys"),
                PathBuf::from("/dev"),
                PathBuf::from("/run"),
                PathBuf::from("/media"),
            ],
            workers: 0,
            reindex_interval_hours: 6,
            collect_mtime: true,
            io_uring: true,
            max_depth: 4096,
        }
    }
}

impl Default for IndexConfig {
    fn default() -> Self {
        Self {
            db_path: PathBuf::from("/var/lib/rfindd/index.db"),
            batch_size: 10_000,
            compress: true,
        }
    }
}

impl Default for MonitorConfig {
    fn default() -> Self {
        Self {
            prefer_fanotify: true,
            inotify_fallback: true,
            watch_new_mounts: true,
        }
    }
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            mode: SearchModePref::Auto,
            io_uring: true,
            threads: 0,
        }
    }
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("Failed to read config file {path}: {err}")]
    ReadError { path: PathBuf, err: std::io::Error },
    #[error("Failed to parse config file {path}: {err}")]
    ParseError { path: PathBuf, err: toml::de::Error },
}

pub const DEFAULT_CONFIG_PATH: &str = "/etc/rfindd/config.toml";

/// Load config from the given path, falling back to defaults if not found.
pub fn load(path: &Path) -> Result<Config, ConfigError> {
    if !path.exists() {
        return Ok(Config::default());
    }
    let text = std::fs::read_to_string(path).map_err(|err| ConfigError::ReadError {
        path: path.to_path_buf(),
        err,
    })?;
    toml::from_str(&text).map_err(|err| ConfigError::ParseError {
        path: path.to_path_buf(),
        err,
    })
}

/// Write default config to the given path (for first-time install).
pub fn write_default(path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, default_toml())
}

/// The default configuration as TOML.
pub fn default_toml() -> String {
    toml::to_string_pretty(&Config::default()).expect("Default config serialization failed")
}

/// A problem found by [`Config::validate`].
#[derive(Debug, Clone, PartialEq)]
pub struct Problem {
    /// Errors make the config unusable; warnings are worth showing.
    pub error: bool,
    pub message: String,
}

impl Config {
    /// Check what serde can't: absolute paths, conflicting roots, ranges.
    pub fn validate(&self) -> Vec<Problem> {
        let mut out = Vec::new();
        let mut err = |m: String| {
            out.push(Problem {
                error: true,
                message: m,
            })
        };
        let check_abs = |what: &str, p: &Path, err: &mut dyn FnMut(String)| {
            if !p.is_absolute() {
                err(format!("{what} {} must be an absolute path", p.display()));
            }
        };
        for r in &self.scan.roots {
            check_abs("root", r, &mut err);
        }
        for e in &self.scan.exclude {
            check_abs("exclude", e, &mut err);
        }
        for p in &self.scan.periodic_roots {
            check_abs("periodic root", &p.path, &mut err);
            if self.scan.roots.contains(&p.path) {
                err(format!(
                    "{} is both a live root and a periodic root",
                    p.path.display()
                ));
            }
        }
        check_abs("index.db_path", &self.index.db_path, &mut err);
        if self.index.batch_size == 0 {
            err("index.batch_size must be at least 1".into());
        }
        if self.scan.workers > 1024 {
            err("scan.workers must be at most 1024 (0 = one per CPU)".into());
        }
        if self.scan.max_depth > 1_000_000 {
            err("scan.max_depth must be at most 1,000,000 (0 = unbounded)".into());
        }
        if self.search.threads > 1024 {
            err("search.threads must be at most 1024 (0 = cores × 2)".into());
        }
        if self.scan.roots.is_empty() && self.scan.periodic_roots.is_empty() {
            err("nothing to index: add a root or a periodic root".into());
        }
        // Root/exclude conflicts: a path that is both a root and an
        // exclude would be swept away by its own scan, and nested roots
        // confuse the sweep (one scan's sweep could delete the other
        // scan's entries). The scan loop handles "ancestor root scans
        // skip nested roots" correctly, but the user usually wants one
        // or the other.
        for r in &self.scan.roots {
            if self.scan.exclude.contains(r) {
                err(format!(
                    "{} is both a root and in scan.exclude — drop one",
                    r.display()
                ));
            }
            for other in &self.scan.roots {
                if other != r && r.starts_with(other) {
                    err(format!(
                        "root {} is nested inside root {} — drop the inner one",
                        r.display(),
                        other.display()
                    ));
                }
            }
        }
        let mut warn = |m: String| {
            out.push(Problem {
                error: false,
                message: m,
            })
        };
        for r in self
            .scan
            .roots
            .iter()
            .chain(self.scan.periodic_roots.iter().map(|p| &p.path))
        {
            if r.is_absolute() && !r.exists() {
                warn(format!("{} does not exist (yet)", r.display()));
            }
            if self.scan.exclude.iter().any(|e| r.starts_with(e)) {
                warn(format!(
                    "{} is inside an exclude and will not be indexed",
                    r.display()
                ));
            }
        }
        // With both live backends off and no periodic rescan, the index
        // freezes forever: nothing ever updates it. This is a legitimate
        // index-only+read-only setup, so it's a warning, not an error.
        if !self.monitor.prefer_fanotify
            && !self.monitor.inotify_fallback
            && self.scan.reindex_interval_hours == 0
            && !self.scan.roots.is_empty()
        {
            warn(
                "live monitoring is disabled and scan.reindex_interval_hours = 0 — \
                 the index will never update after startup. Set \
                 reindex_interval_hours > 0 or enable a monitor backend."
                    .into(),
            );
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_periodic_roots() {
        let cfg: Config = toml::from_str(
            r#"
            [scan]
            roots = ["/"]
            [[scan.periodic_roots]]
            path = "/mnt/c"
            [[scan.periodic_roots]]
            path = "/mnt/d"
            interval_hours = 48
        "#,
        )
        .unwrap();
        assert_eq!(cfg.scan.periodic_roots.len(), 2);
        assert_eq!(cfg.scan.periodic_roots[0].interval_hours, 12);
        assert_eq!(cfg.scan.periodic_roots[1].interval_hours, 48);
    }

    #[test]
    fn unknown_keys_are_rejected() {
        assert!(toml::from_str::<Config>("[scan]\nexlude = [\"/x\"]\n").is_err());
    }

    #[test]
    fn validate_catches_relative_paths_and_conflicts() {
        let mut c = Config::default();
        c.scan.exclude.push("relative/dir".into());
        c.scan.periodic_roots.push(PeriodicRoot {
            path: "/".into(),
            interval_hours: 1,
        });
        let errors: Vec<_> = c.validate().into_iter().filter(|p| p.error).collect();
        assert_eq!(errors.len(), 2, "{errors:?}");
        assert!(Config::default().validate().iter().all(|p| !p.error));
    }

    #[test]
    fn old_config_without_periodic_roots_still_loads() {
        let cfg: Config = toml::from_str("[scan]\nroots = [\"/\"]\n").unwrap();
        assert!(cfg.scan.periodic_roots.is_empty());
    }

    #[test]
    fn search_section_defaults() {
        let cfg: Config = toml::from_str("").unwrap();
        assert_eq!(cfg.search.mode, SearchModePref::Auto);
        assert!(cfg.search.io_uring);
        assert_eq!(cfg.search.threads, 0);
    }

    #[test]
    fn search_mode_parses_from_toml() {
        let cfg: Config = toml::from_str(
            r#"
            [search]
            mode = "walk"
            threads = 8
        "#,
        )
        .unwrap();
        assert_eq!(cfg.search.mode, SearchModePref::Walk);
        assert_eq!(cfg.search.threads, 8);
    }

    #[test]
    fn search_mode_rejects_unknown_value() {
        let r: Result<Config, _> = toml::from_str("[search]\nmode = \"wrong\"\n");
        assert!(r.is_err());
    }

    #[test]
    fn search_threads_cap_validates() {
        let mut c = Config::default();
        c.search.threads = 2048;
        assert!(c.validate().iter().any(|p| p.error));
    }
}
