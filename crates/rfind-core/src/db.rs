//! SQLite index: schema, insertion, and search queries.
//!
//! # Case sensitivity
//! SQLite LIKE is case-insensitive for ASCII by default, which is wrong
//! for Linux filesystems. We use GLOB instead, which is always
//! case-sensitive and uses * / ? wildcards matching shell glob semantics.
//! For exact lookups we use = which is always case-sensitive.
//!
//! # Schema
//! entries table:
//!   id        INTEGER PRIMARY KEY
//!   parent_id INTEGER  (NULL for roots)
//!   basename  TEXT NOT NULL
//!   full_path TEXT NOT NULL
//!   is_dir    INTEGER NOT NULL
//!
//! Indexes:
//!   idx_basename  on basename   — exact and glob name lookups
//!   idx_full_path on full_path  — path prefix and glob lookups
//!   idx_parent    on parent_id  — directory children

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
    ")?;
    Ok(conn)
}

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

/// Exact basename lookup. Always case-sensitive (uses =).
pub fn search_exact(conn: &Connection, name: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare_cached(
        "SELECT full_path FROM entries WHERE basename = ?1 ORDER BY full_path",
    )?;
    let rows = stmt.query_map(params![name], |row| row.get(0))?;
    rows.collect()
}

/// Basename glob search. Case-sensitive (uses GLOB).
/// Pattern uses * and ? — same as shell globs.
/// If pattern contains no wildcards, falls through to exact match.
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

/// Search entries under a directory prefix, with optional glob on basename.
/// Both the path prefix check and the name glob are case-sensitive.
pub fn search_under(
    conn: &Connection,
    prefix: &str,
    name_pattern: Option<&str>,
) -> Result<Vec<String>> {
    match name_pattern {
        None => {
            // No name filter — return everything under prefix.
            let mut stmt = conn.prepare_cached(
                "SELECT full_path FROM entries
                 WHERE full_path GLOB ?1
                 ORDER BY full_path",
            )?;
            let glob_prefix = format!("{}*", prefix);
            let rows = stmt.query_map(params![glob_prefix], |row| row.get(0))?;
            rows.collect()
        }
        Some(name_pat) => {
            if !name_pat.contains('*') && !name_pat.contains('?') {
                // Exact name under prefix.
                let mut stmt = conn.prepare_cached(
                    "SELECT full_path FROM entries
                     WHERE full_path GLOB ?1
                       AND basename = ?2
                     ORDER BY full_path",
                )?;
                let glob_prefix = format!("{}*", prefix);
                let rows = stmt.query_map(params![glob_prefix, name_pat], |row| row.get(0))?;
                rows.collect()
            } else {
                // Glob name under prefix.
                let mut stmt = conn.prepare_cached(
                    "SELECT full_path FROM entries
                     WHERE full_path GLOB ?1
                       AND basename GLOB ?2
                     ORDER BY full_path",
                )?;
                let glob_prefix = format!("{}*", prefix);
                let rows = stmt.query_map(params![glob_prefix, name_pat], |row| row.get(0))?;
                rows.collect()
            }
        }
    }
}

/// Full path glob search. Case-sensitive (uses GLOB).
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
