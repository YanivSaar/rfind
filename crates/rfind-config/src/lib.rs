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
    /// Filesystem roots to index.
    pub roots: Vec<PathBuf>,
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
            exclude: vec![
                PathBuf::from("/proc"),
                PathBuf::from("/sys"),
                PathBuf::from("/dev"),
                PathBuf::from("/run"),
                PathBuf::from("/mnt"),
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
    let text = toml::to_string_pretty(&default)
        .expect("Default config serialization failed");
    std::fs::write(path, text)
}
