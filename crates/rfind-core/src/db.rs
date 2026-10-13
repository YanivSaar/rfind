//! SQLite index: schema, insertion, and search queries.
//!
//! Schema
//! ------
//! One table: `entries`
//!   id        INTEGER PRIMARY KEY
//!   parent_id INTEGER  (NULL for roots)
//!   basename  TEXT NOT NULL
//!   full_path TEXT NOT NULL
//!   is_dir    INTEGER NOT NULL  (0/1)
//!
//! Indexes
//! -------
//!   idx_basename   on basename          — exact name lookups
//!   idx_full_path  on full_path         — full path lookups
//!   idx_parent     on parent_id         — directory children
//!
//! WAL mode is enabled so reads never block writes during the initial scan.

use rusqlite::{params, Connection, Result};
use std::path::Path;

/// Open (or create) the index database at the given path.
pub fn open(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path)?;

    // WAL mode: readers don't block writers and vice versa.
    conn.execute_batch("PRAGMA journal_mode=WAL;")?;
    // Relaxed durability during bulk insert — we can rebuild from a scan.
    conn.execute_batch("PRAGMA synchronous=NORMAL;")?;
    // 64 MB page cache.
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
    ")?;

    Ok(conn)
}

/// Insert one entry. Returns the new row id.
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

/// Search by exact basename.
pub fn search_by_name(conn: &Connection, name: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare_cached(
        "SELECT full_path FROM entries WHERE basename = ?1 ORDER BY full_path",
    )?;
    let rows = stmt.query_map(params![name], |row| row.get(0))?;
    rows.collect()
}

/// Search by basename glob (SQL LIKE pattern, case-sensitive on Linux).
/// Caller should convert shell globs: * -> %, ? -> _
pub fn search_by_glob(conn: &Connection, pattern: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare_cached(
        "SELECT full_path FROM entries WHERE basename LIKE ?1 ORDER BY full_path",
    )?;
    let rows = stmt.query_map(params![pattern], |row| row.get(0))?;
    rows.collect()
}

/// Search entries under a specific directory prefix.
pub fn search_under(
    conn: &Connection,
    prefix: &str,
    name_pattern: Option<&str>,
) -> Result<Vec<String>> {
    let path_pattern = format!("{}%", prefix);
    let name_pat = name_pattern.unwrap_or("%");
    let mut stmt = conn.prepare_cached(
        "SELECT full_path FROM entries
         WHERE full_path LIKE ?1
           AND basename LIKE ?2
         ORDER BY full_path",
    )?;
    let rows = stmt.query_map(params![path_pattern, name_pat], |row| row.get(0))?;
    rows.collect()
}

/// Total number of indexed entries.
pub fn entry_count(conn: &Connection) -> Result<i64> {
    conn.query_row("SELECT COUNT(*) FROM entries", [], |row| row.get(0))
}
