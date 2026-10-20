//! In-place editing of the config file for `rfind config`.
//!
//! Edits go through `toml_edit`, so comments, ordering and formatting the
//! admin wrote are preserved. Every change is validated by parsing the
//! result into a [`Config`] before it is written, and the write is atomic
//! (temporary file + rename), so a crash or a bad value never leaves a
//! broken config behind for the daemon.

use crate::{Config, Problem};
use std::{
    fs,
    io::Write,
    path::{Component, Path, PathBuf},
};
use thiserror::Error;
use toml_edit::{value, Array, ArrayOfTables, DocumentMut, Item, Table, Value};

#[derive(Debug, Error)]
pub enum EditError {
    #[error("cannot read {0}: {1}")]
    Read(PathBuf, std::io::Error),
    #[error("cannot write {0}: {1}")]
    Write(PathBuf, std::io::Error),
    #[error("{0} is not valid TOML: {1}")]
    Syntax(PathBuf, toml_edit::TomlError),
    #[error("invalid configuration: {0}")]
    Invalid(String),
    #[error("unknown setting '{0}' (see: rfind config keys)")]
    UnknownKey(String),
    #[error("'{key}' expects {expected}, got '{got}'")]
    BadValue {
        key: String,
        expected: String,
        got: String,
    },
    #[error("'{0}' must be an absolute path")]
    NotAbsolute(String),
}

/// Kind of a scalar setting, for parsing `rfind config set` values.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Kind {
    Bool,
    Int,
    Path,
    /// Enum: one of the given lowercase strings.
    Enum(&'static [&'static str]),
}

/// Scalar settings changeable with `rfind config set`, with descriptions.
/// Lists (roots, exclude, periodic roots) have their own subcommands.
pub const KEYS: &[(&str, Kind, &str)] = &[
    (
        "scan.workers",
        Kind::Int,
        "scanner threads; 0 = one per CPU",
    ),
    (
        "scan.reindex_interval_hours",
        Kind::Int,
        "hours between full rescans of live roots; 0 = never",
    ),
    (
        "scan.collect_mtime",
        Kind::Bool,
        "record modification times (needed by rfind recent)",
    ),
    (
        "scan.io_uring",
        Kind::Bool,
        "batch statx calls through io_uring when available",
    ),
    (
        "scan.max_depth",
        Kind::Int,
        "max directory depth to descend; 0 = unbounded",
    ),
    ("index.db_path", Kind::Path, "index database location"),
    (
        "index.batch_size",
        Kind::Int,
        "entries per scanner write transaction",
    ),
    (
        "index.compress",
        Kind::Bool,
        "store the index zstd-compressed (changing it rebuilds the index)",
    ),
    (
        "monitor.prefer_fanotify",
        Kind::Bool,
        "use fanotify (one mark per filesystem) when possible",
    ),
    (
        "monitor.inotify_fallback",
        Kind::Bool,
        "fall back to inotify when fanotify is unavailable",
    ),
    (
        "monitor.watch_new_mounts",
        Kind::Bool,
        "index filesystems mounted under live roots after startup",
    ),
    (
        "search.mode",
        Kind::Enum(&["auto", "index", "walk"]),
        "which backend to prefer: auto (default), index, or walk",
    ),
    (
        "search.io_uring",
        Kind::Bool,
        "use io_uring for walk-mode statx batching when available",
    ),
    (
        "search.threads",
        Kind::Int,
        "walk-mode worker threads; 0 = cores × 2",
    ),
];

fn key_kind(key: &str) -> Option<Kind> {
    KEYS.iter()
        .find(|(k, ..)| *k == key)
        .map(|(_, kind, _)| *kind)
}

/// Lexically normalize an absolute path: drop `.`, resolve `..`, strip a
/// trailing `/`. Symlinks are not resolved (the index stores paths as seen).
pub fn normalize_path(raw: &str) -> Result<PathBuf, EditError> {
    let p = Path::new(raw);
    if !p.is_absolute() {
        return Err(EditError::NotAbsolute(raw.to_string()));
    }
    let mut out = PathBuf::from("/");
    for c in p.components() {
        match c {
            Component::Normal(n) => out.push(n),
            Component::ParentDir => {
                out.pop();
            }
            _ => {}
        }
    }
    Ok(out)
}

/// A config file loaded for editing.
pub struct ConfigFile {
    pub path: PathBuf,
    doc: DocumentMut,
}

impl ConfigFile {
    /// Load `path`, or start from the defaults if it doesn't exist.
    pub fn open(path: &Path) -> Result<Self, EditError> {
        let text = match fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => crate::default_toml(),
            Err(e) => return Err(EditError::Read(path.to_path_buf(), e)),
        };
        let doc = text
            .parse::<DocumentMut>()
            .map_err(|e| EditError::Syntax(path.to_path_buf(), e))?;
        Ok(Self {
            path: path.to_path_buf(),
            doc,
        })
    }

    /// The configuration this file describes (defaults filled in).
    pub fn config(&self) -> Result<Config, EditError> {
        toml::from_str(&self.doc.to_string())
            .map_err(|e| EditError::Invalid(e.message().to_string()))
    }

    fn table(&mut self, section: &str) -> &mut Table {
        let item = self
            .doc
            .entry(section)
            .or_insert_with(|| Item::Table(Table::new()));
        if !item.is_table() {
            *item = Item::Table(Table::new());
        }
        item.as_table_mut().unwrap()
    }

    fn array(&mut self, section: &str, key: &str) -> &mut Array {
        let t = self.table(section);
        let item = t.entry(key).or_insert_with(|| value(Array::new()));
        if !item.is_array() {
            *item = value(Array::new());
        }
        item.as_array_mut().unwrap()
    }

    /// The paths in a list setting (`scan.roots` or `scan.exclude`).
    pub fn list(&self, section: &str, key: &str) -> Vec<String> {
        self.doc
            .get(section)
            .and_then(|t| t.get(key))
            .and_then(|i| i.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Add a path to a list setting. Returns false if it was already there.
    pub fn list_add(&mut self, section: &str, key: &str, path: &Path) -> bool {
        let s = path.to_string_lossy().to_string();
        let arr = self.array(section, key);
        if arr.iter().any(|v| v.as_str() == Some(&s)) {
            return false;
        }
        arr.push(s);
        multiline(arr);
        true
    }

    /// Remove a path from a list setting. Returns false if it wasn't there.
    pub fn list_remove(&mut self, section: &str, key: &str, path: &Path) -> bool {
        let s = path.to_string_lossy().to_string();
        let arr = self.array(section, key);
        let before = arr.len();
        arr.retain(|v| v.as_str() != Some(&s));
        before != arr.len()
    }

    fn periodic(&mut self) -> &mut ArrayOfTables {
        let t = self.table("scan");
        let item = t
            .entry("periodic_roots")
            .or_insert_with(|| Item::ArrayOfTables(ArrayOfTables::new()));
        if let Item::Value(Value::Array(a)) = item {
            // `periodic_roots = []` written by the default serializer.
            if a.is_empty() {
                *item = Item::ArrayOfTables(ArrayOfTables::new());
            }
        }
        item.as_array_of_tables_mut()
            .expect("scan.periodic_roots must be [[scan.periodic_roots]] tables")
    }

    /// Add or update a periodic root. Returns false if nothing changed.
    pub fn periodic_set(&mut self, path: &Path, interval_hours: u64) -> bool {
        let s = path.to_string_lossy().to_string();
        let roots = self.periodic();
        if let Some(t) = roots
            .iter_mut()
            .find(|t| t.get("path").and_then(|v| v.as_str()) == Some(&s))
        {
            let old = t.get("interval_hours").and_then(|v| v.as_integer());
            if old == Some(interval_hours as i64) {
                return false;
            }
            t["interval_hours"] = value(interval_hours as i64);
            return true;
        }
        let mut t = Table::new();
        t["path"] = value(s);
        t["interval_hours"] = value(interval_hours as i64);
        roots.push(t);
        true
    }

    /// Remove a periodic root. Returns false if it wasn't configured.
    pub fn periodic_remove(&mut self, path: &Path) -> bool {
        let s = path.to_string_lossy().to_string();
        let roots = self.periodic();
        let before = roots.len();
        roots.retain(|t| t.get("path").and_then(|v| v.as_str()) != Some(&s));
        before != roots.len()
    }

    /// Set a scalar setting from its string form.
    pub fn set(&mut self, key: &str, raw: &str) -> Result<(), EditError> {
        let kind = key_kind(key).ok_or_else(|| EditError::UnknownKey(key.to_string()))?;
        let bad = |expected: String| EditError::BadValue {
            key: key.to_string(),
            expected,
            got: raw.to_string(),
        };
        let v = match kind {
            Kind::Bool => match raw.to_ascii_lowercase().as_str() {
                "true" | "yes" | "on" | "1" => value(true),
                "false" | "no" | "off" | "0" => value(false),
                _ => return Err(bad("true or false".into())),
            },
            Kind::Int => value(
                raw.parse::<u32>()
                    .map_err(|_| bad("a non-negative integer".into()))?
                    as i64,
            ),
            Kind::Path => value(normalize_path(raw)?.to_string_lossy().to_string()),
            Kind::Enum(allowed) => {
                let lo = raw.to_ascii_lowercase();
                if !allowed.contains(&lo.as_str()) {
                    return Err(bad(format!("one of {}", allowed.join(", "))));
                }
                value(lo)
            }
        };
        let (section, field) = key.split_once('.').unwrap();
        self.table(section)[field] = v;
        Ok(())
    }

    /// Remove a scalar setting so its default applies.
    pub fn unset(&mut self, key: &str) -> Result<bool, EditError> {
        key_kind(key).ok_or_else(|| EditError::UnknownKey(key.to_string()))?;
        let (section, field) = key.split_once('.').unwrap();
        Ok(self.table(section).remove(field).is_some())
    }

    /// Validate and atomically write the file. Returns warnings.
    pub fn save(&self) -> Result<Vec<Problem>, EditError> {
        let problems = self.config()?.validate();
        if let Some(e) = problems.iter().find(|p| p.error) {
            return Err(EditError::Invalid(e.message.clone()));
        }
        write_atomic(&self.path, self.doc.to_string().as_bytes())?;
        Ok(problems)
    }

    pub fn text(&self) -> String {
        self.doc.to_string()
    }
}

/// Validate config text (e.g. after `rfind config edit`) without writing.
pub fn check_text(path: &Path, text: &str) -> Result<Vec<Problem>, EditError> {
    text.parse::<DocumentMut>()
        .map_err(|e| EditError::Syntax(path.to_path_buf(), e))?;
    let cfg: Config =
        toml::from_str(text).map_err(|e| EditError::Invalid(e.message().to_string()))?;
    let problems = cfg.validate();
    if let Some(e) = problems.iter().find(|p| p.error) {
        return Err(EditError::Invalid(e.message.clone()));
    }
    Ok(problems)
}

/// Write via a temporary file in the same directory, then rename over.
/// The parent directory is fsynced after the rename so the rename itself
/// is durable: without it, a hard crash can leave the pre-edit file in
/// place even though `rfind config set …` returned success.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), EditError> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let err = |e| EditError::Write(path.to_path_buf(), e);
    let dir = path.parent().unwrap_or(Path::new("/"));
    fs::create_dir_all(dir).map_err(err)?;
    // Random suffix so a pre-seeded symlink at ".<name>.tmp" cannot be
    // the exact file we open (`create_new` fails on EEXIST and we
    // retry). O_NOFOLLOW prevents following a symlink regardless.
    let base = format!(
        ".{}.tmp",
        path.file_name().unwrap_or_default().to_string_lossy()
    );
    let mut tmp = dir.join(&base);
    let mut f = loop {
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .mode(0o640)
            .open(&tmp)
        {
            Ok(f) => break f,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                // Add a short random suffix and retry; 8 hex chars of
                // nanos+pid is plenty for a per-admin edit flow.
                let r = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.subsec_nanos())
                    .unwrap_or(0)
                    ^ std::process::id();
                tmp = dir.join(format!("{base}.{r:08x}"));
            }
            Err(e) => return Err(err(e)),
        }
    };
    f.write_all(bytes).and_then(|_| f.sync_all()).map_err(err)?;
    // 0o640 is already set via mode() above, but set_permissions also
    // handles the (unusual) case where umask or an inherited ACL
    // altered the final mode.
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o640)).map_err(err)?;
    fs::rename(&tmp, path).map_err(err)?;
    // Fsync the directory so the rename survives a crash. Opening the
    // dir read-only and calling sync_all does fsync on Linux.
    if let Ok(d) = fs::File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(())
}

/// One path per line once a list grows past a couple of entries.
fn multiline(arr: &mut Array) {
    if arr.len() < 3 {
        return;
    }
    for v in arr.iter_mut() {
        v.decor_mut().set_prefix("\n    ");
    }
    arr.set_trailing("\n");
    arr.set_trailing_comma(true);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str, text: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("rfind-cfg-{}-{name}.toml", std::process::id()));
        fs::write(&p, text).unwrap();
        p
    }

    #[test]
    fn add_exclude_keeps_comments_and_is_idempotent() {
        let p = tmp(
            "exclude",
            "# my config\n[scan]\n# keep these out\nexclude = [\"/proc\"]\n",
        );
        let mut f = ConfigFile::open(&p).unwrap();
        assert!(f.list_add("scan", "exclude", Path::new("/home/moshe")));
        assert!(!f.list_add("scan", "exclude", Path::new("/home/moshe")));
        f.save().unwrap();
        let text = fs::read_to_string(&p).unwrap();
        assert!(
            text.contains("# my config") && text.contains("# keep these out"),
            "{text}"
        );
        assert_eq!(
            f.config().unwrap().scan.exclude,
            vec![PathBuf::from("/proc"), PathBuf::from("/home/moshe")]
        );
        assert!(f.list_remove("scan", "exclude", Path::new("/proc")));
        assert_eq!(f.list("scan", "exclude"), vec!["/home/moshe"]);
        fs::remove_file(p).unwrap();
    }

    #[test]
    fn periodic_roots_add_update_remove() {
        let p = tmp("periodic", "[scan]\nroots = [\"/\"]\n");
        let mut f = ConfigFile::open(&p).unwrap();
        assert!(f.periodic_set(Path::new("/mnt/c"), 12));
        assert!(!f.periodic_set(Path::new("/mnt/c"), 12));
        assert!(f.periodic_set(Path::new("/mnt/c"), 24));
        let c = f.config().unwrap();
        assert_eq!(c.scan.periodic_roots.len(), 1);
        assert_eq!(c.scan.periodic_roots[0].interval_hours, 24);
        assert!(f.periodic_remove(Path::new("/mnt/c")));
        assert!(f.config().unwrap().scan.periodic_roots.is_empty());
        fs::remove_file(p).unwrap();
    }

    #[test]
    fn periodic_works_on_default_serialized_file() {
        // write_default emits `periodic_roots = []`.
        let p = tmp("default", &crate::default_toml());
        let mut f = ConfigFile::open(&p).unwrap();
        assert!(f.periodic_set(Path::new("/mnt/c"), 12));
        assert_eq!(f.config().unwrap().scan.periodic_roots.len(), 1);
        fs::remove_file(p).unwrap();
    }

    #[test]
    fn set_parses_by_type_and_rejects_bad_values() {
        let p = tmp("set", "");
        let mut f = ConfigFile::open(&p).unwrap();
        f.set("scan.workers", "8").unwrap();
        f.set("scan.io_uring", "off").unwrap();
        f.set("index.db_path", "/data/rfind/../rfind/index.db/")
            .unwrap();
        let c = f.config().unwrap();
        assert_eq!((c.scan.workers, c.scan.io_uring), (8, false));
        assert_eq!(c.index.db_path, PathBuf::from("/data/rfind/index.db"));
        assert!(matches!(
            f.set("scan.workers", "many"),
            Err(EditError::BadValue { .. })
        ));
        assert!(matches!(
            f.set("scan.nope", "1"),
            Err(EditError::UnknownKey(_))
        ));
        assert!(f.unset("scan.workers").unwrap());
        assert_eq!(f.config().unwrap().scan.workers, 0);
        fs::remove_file(p).unwrap();
    }

    #[test]
    fn save_refuses_invalid_config() {
        let p = tmp("invalid", "[scan]\nroots = [\"/\"]\n");
        let mut f = ConfigFile::open(&p).unwrap();
        f.periodic_set(Path::new("/"), 1); // same as a live root
        assert!(matches!(f.save(), Err(EditError::Invalid(_))));
        assert_eq!(fs::read_to_string(&p).unwrap(), "[scan]\nroots = [\"/\"]\n");
        fs::remove_file(p).unwrap();
    }

    #[test]
    fn normalize_rejects_relative_and_cleans_absolute() {
        assert!(normalize_path("home/x").is_err());
        assert_eq!(
            normalize_path("/home/./moshe/").unwrap(),
            PathBuf::from("/home/moshe")
        );
        assert_eq!(normalize_path("/").unwrap(), PathBuf::from("/"));
    }
}
