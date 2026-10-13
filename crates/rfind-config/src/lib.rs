//! Configuration for rfindd.
//!
//! Reads /etc/rfindd/config.toml if present, otherwise uses defaults.
//! All fields have sensible defaults so the daemon works out of the box.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub scan: ScanConfig,
    pub index: IndexConfig,
    pub monitor: MonitorConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
#[serde(default)]
pub struct IndexConfig {
    pub db_path: PathBuf,
    pub batch_size: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MonitorConfig {
    /// Try fanotify first.
    pub prefer_fanotify: bool,
    /// Fall back to inotify if fanotify is unavailable.
    pub inotify_fallback: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            scan: ScanConfig::default(),
            index: IndexConfig::default(),
            monitor: MonitorConfig::default(),
        }
    }
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
        }
    }
}

impl Default for IndexConfig {
    fn default() -> Self {
        Self {
            db_path: PathBuf::from("/var/lib/rfindd/index.db"),
            batch_size: 10_000,
        }
    }
}

impl Default for MonitorConfig {
    fn default() -> Self {
        Self {
            prefer_fanotify: true,
            inotify_fallback: true,
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
    let default = Config::default();
    let text = toml::to_string_pretty(&default).expect("Default config serialization failed");
    std::fs::write(path, text)
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
    fn old_config_without_periodic_roots_still_loads() {
        let cfg: Config = toml::from_str("[scan]\nroots = [\"/\"]\n").unwrap();
        assert!(cfg.scan.periodic_roots.is_empty());
    }
}
