//! SQLite index: schema, insertion, search, and event journal.
//!
//! # Tables
//!
//! ## entries
//! The filesystem index. One row per filesystem entry.
//!
//! ## pending_events
//! Durable event journal. fanotify events are written here before being
//! applied to `entries`. On daemon restart, unapplied events are replayed.
//! This means no filesystem changes are lost across restarts.
//!
//! ## scan_metadata
//! Key/value store for daemon state: last scan time, last applied event ID,
//! daemon version. Used to decide whether a full rescan is needed on restart.
//!
//! # Case sensitivity
//! SQLite GLOB is used for all pattern matching — it is always case-sensitive
//! and uses * / ? wildcards matching shell glob semantics.

use rusqlite::{params, Connection, Result};
use std::path::Path;

pub fn open(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path)?;
    conn.execute_batch("PRAGMA journal_mode=WAL;")?;
    conn.execute_batch("PRAGMA synchronous=NORMAL;")?;
    conn.execute_batch("PRAGMA cache_size=-65536;")?;
    conn.execute_batch("PRAGMA temp_store=MEMORY;")?;
    conn.execute_batch("
        CREATE TABLE IF NOT EXISTS entries (
            id        INTEGER PRIMARY KEY AUTOINCREMENT,
            parent_id INTEGER,
            basename  TEXT NOT NULL,
            full_path TEXT NOT NULL,
            is_dir    INTEGER NOT NULL DEFAULT 0
        );
        CREATE INDEX IF NOT EXISTS idx_basename
            ON entries (basename);
        CREATE INDEX IF NOT EXISTS idx_full_path
            ON entries (full_path);
        CREATE INDEX IF NOT EXISTS idx_parent
            ON entries (parent_id);

        CREATE TABLE IF NOT EXISTS pending_events (
            id         INTEGER PRIMARY KEY AUTOINCREMENT,
            event_type TEXT NOT NULL,
            path       TEXT NOT NULL,
            new_path   TEXT,
            is_dir     INTEGER NOT NULL DEFAULT 0,
            timestamp  INTEGER NOT NULL,
            applied    INTEGER NOT NULL DEFAULT 0
        );
        CREATE INDEX IF NOT EXISTS idx_pending_applied
            ON pending_events (applied);

        CREATE TABLE IF NOT EXISTS scan_metadata (
            key   TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );
    ")?;
    Ok(conn)
}

// ---------------------------------------------------------------------------
// Entry operations
// ---------------------------------------------------------------------------

pub fn insert_entry(
    conn: &Connection,
    parent_id: Option<i64>,
    basename: &str,
    full_path: &str,
    is_dir: bool,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO entries (parent_id, basename, full_path, is_dir)
         VALUES (?1, ?2, ?3, ?4)",
        params![parent_id, basename, full_path, is_dir as i32],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn delete_entry_by_path(conn: &Connection, path: &str) -> Result<usize> {
    conn.execute("DELETE FROM entries WHERE full_path = ?1", params![path])
}

pub fn delete_entries_under(conn: &Connection, prefix: &str) -> Result<usize> {
    let pattern = format!("{}*", prefix);
    conn.execute(
        "DELETE FROM entries WHERE full_path GLOB ?1",
        params![pattern],
    )
}

pub fn rename_entry(conn: &Connection, old_path: &str, new_path: &str) -> Result<usize> {
    let new_basename = Path::new(new_path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");

    // Update the entry itself.
    conn.execute(
        "UPDATE entries SET full_path = ?1, basename = ?2 WHERE full_path = ?3",
        params![new_path, new_basename, old_path],
    )?;

    // Update all children (directory rename).
    // Replace the old path prefix with the new one for all descendants.
    conn.execute(
        "UPDATE entries
         SET full_path = ?1 || SUBSTR(full_path, LENGTH(?2) + 1)
         WHERE full_path GLOB ?3",
        params![new_path, old_path, format!("{}/*", old_path)],
    )?;

    Ok(1)
}

pub fn entry_exists(conn: &Connection, path: &str) -> Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM entries WHERE full_path = ?1",
        params![path],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

// ---------------------------------------------------------------------------
// Search operations
// ---------------------------------------------------------------------------

pub fn search_exact(conn: &Connection, name: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare_cached(
        "SELECT full_path FROM entries WHERE basename = ?1 ORDER BY full_path",
    )?;
    let rows = stmt.query_map(params![name], |row| row.get(0))?;
    rows.collect()
}

pub fn search_by_glob(conn: &Connection, pattern: &str) -> Result<Vec<String>> {
    if !pattern.contains('*') && !pattern.contains('?') {
        return search_exact(conn, pattern);
    }
    let mut stmt = conn.prepare_cached(
        "SELECT full_path FROM entries WHERE basename GLOB ?1 ORDER BY full_path",
    )?;
    let rows = stmt.query_map(params![pattern], |row| row.get(0))?;
    rows.collect()
}

pub fn search_under(
    conn: &Connection,
    prefix: &str,
    name_pattern: Option<&str>,
) -> Result<Vec<String>> {
    let glob_prefix = format!("{}*", prefix);
    match name_pattern {
        None => {
            let mut stmt = conn.prepare_cached(
                "SELECT full_path FROM entries
                 WHERE full_path GLOB ?1 ORDER BY full_path",
            )?;
            let rows = stmt.query_map(params![glob_prefix], |row| row.get(0))?;
            rows.collect()
        }
        Some(name_pat) if !name_pat.contains('*') && !name_pat.contains('?') => {
            let mut stmt = conn.prepare_cached(
                "SELECT full_path FROM entries
                 WHERE full_path GLOB ?1 AND basename = ?2
                 ORDER BY full_path",
            )?;
            let rows = stmt.query_map(params![glob_prefix, name_pat], |row| row.get(0))?;
            rows.collect()
        }
        Some(name_pat) => {
            let mut stmt = conn.prepare_cached(
                "SELECT full_path FROM entries
                 WHERE full_path GLOB ?1 AND basename GLOB ?2
                 ORDER BY full_path",
            )?;
            let rows = stmt.query_map(params![glob_prefix, name_pat], |row| row.get(0))?;
            rows.collect()
        }
    }
}

pub fn search_by_path_glob(conn: &Connection, pattern: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare_cached(
        "SELECT full_path FROM entries WHERE full_path GLOB ?1 ORDER BY full_path",
    )?;
    let rows = stmt.query_map(params![pattern], |row| row.get(0))?;
    rows.collect()
}

pub fn entry_count(conn: &Connection) -> Result<i64> {
    conn.query_row("SELECT COUNT(*) FROM entries", [], |row| row.get(0))
}

// ---------------------------------------------------------------------------
// Event journal operations
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum EventType {
    Create,
    Delete,
    Rename,
    Modify,
}

impl EventType {
    pub fn as_str(&self) -> &'static str {
        match self {
            EventType::Create => "CREATE",
            EventType::Delete => "DELETE",
            EventType::Rename => "RENAME",
            EventType::Modify => "MODIFY",
        }
    }
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "CREATE" => Some(EventType::Create),
            "DELETE" => Some(EventType::Delete),
            "RENAME" => Some(EventType::Rename),
            "MODIFY" => Some(EventType::Modify),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct PendingEvent {
    pub id: i64,
    pub event_type: EventType,
    pub path: String,
    pub new_path: Option<String>,
    pub is_dir: bool,
    pub timestamp: i64,
}

pub fn insert_pending_event(
    conn: &Connection,
    event_type: &EventType,
    path: &str,
    new_path: Option<&str>,
    is_dir: bool,
    timestamp: i64,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO pending_events (event_type, path, new_path, is_dir, timestamp)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            event_type.as_str(),
            path,
            new_path,
            is_dir as i32,
            timestamp
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn load_unapplied_events(conn: &Connection) -> Result<Vec<PendingEvent>> {
    let mut stmt = conn.prepare(
        "SELECT id, event_type, path, new_path, is_dir, timestamp
         FROM pending_events WHERE applied = 0
         ORDER BY id ASC",
    )?;
    let rows = stmt.query_map([], |row| {
        let event_type_str: String = row.get(1)?;
        Ok(PendingEvent {
            id: row.get(0)?,
            event_type: EventType::from_str(&event_type_str)
                .unwrap_or(EventType::Modify),
            path: row.get(2)?,
            new_path: row.get(3)?,
            is_dir: row.get::<_, i32>(4)? != 0,
            timestamp: row.get(5)?,
        })
    })?;
    rows.collect()
}

pub fn mark_event_applied(conn: &Connection, event_id: i64) -> Result<()> {
    conn.execute(
        "UPDATE pending_events SET applied = 1 WHERE id = ?1",
        params![event_id],
    )?;
    Ok(())
}

pub fn pending_event_count(conn: &Connection) -> Result<i64> {
    conn.query_row(
        "SELECT COUNT(*) FROM pending_events WHERE applied = 0",
        [],
        |row| row.get(0),
    )
}

pub fn prune_applied_events(conn: &Connection) -> Result<usize> {
    conn.execute("DELETE FROM pending_events WHERE applied = 1", [])
}

// ---------------------------------------------------------------------------
// Scan metadata
// ---------------------------------------------------------------------------

pub fn set_metadata(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO scan_metadata (key, value) VALUES (?1, ?2)",
        params![key, value],
    )?;
    Ok(())
}

pub fn get_metadata(conn: &Connection, key: &str) -> Result<Option<String>> {
    match conn.query_row(
        "SELECT value FROM scan_metadata WHERE key = ?1",
        params![key],
        |row| row.get(0),
    ) {
        Ok(v) => Ok(Some(v)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e),
    }
}
