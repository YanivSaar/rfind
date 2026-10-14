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
        if self.scan.roots.is_empty() && self.scan.periodic_roots.is_empty() {
            err("nothing to index: add a root or a periodic root".into());
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
}
