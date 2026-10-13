//! SQLite index: schema, insertion, search, streaming, health, stats,
//! and event journal.
//!
//! # Schema version 2
//! entries gains: mtime_sec, mtime_nsec, discovered_at
//! New indexes: idx_mtime, idx_discovered

use rusqlite::{params, Connection, Result, Row};
use std::path::Path;

// ---------------------------------------------------------------------------
// Open / schema
// ---------------------------------------------------------------------------

pub fn open(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path)?;
    conn.execute_batch("PRAGMA journal_mode=WAL;")?;
    conn.execute_batch("PRAGMA synchronous=NORMAL;")?;
    conn.execute_batch("PRAGMA cache_size=-65536;")?;
    conn.execute_batch("PRAGMA temp_store=MEMORY;")?;
    conn.execute_batch("
        CREATE TABLE IF NOT EXISTS entries (
            id            INTEGER PRIMARY KEY AUTOINCREMENT,
            parent_id     INTEGER,
            basename      TEXT    NOT NULL,
            full_path     TEXT    NOT NULL,
            is_dir        INTEGER NOT NULL DEFAULT 0,
            mtime_sec     INTEGER NOT NULL DEFAULT 0,
            mtime_nsec    INTEGER NOT NULL DEFAULT 0,
            discovered_at INTEGER NOT NULL DEFAULT 0
        );
        CREATE INDEX IF NOT EXISTS idx_basename
            ON entries (basename);
        CREATE INDEX IF NOT EXISTS idx_full_path
            ON entries (full_path);
        CREATE INDEX IF NOT EXISTS idx_parent
            ON entries (parent_id);
        CREATE INDEX IF NOT EXISTS idx_mtime
            ON entries (mtime_sec);
        CREATE INDEX IF NOT EXISTS idx_discovered
            ON entries (discovered_at);

        CREATE TABLE IF NOT EXISTS pending_events (
            id         INTEGER PRIMARY KEY AUTOINCREMENT,
            event_type TEXT    NOT NULL,
            path       TEXT    NOT NULL,
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
    // Migrate older schemas that lack the new columns.
    migrate(&conn)?;
    Ok(conn)
}

fn migrate(conn: &Connection) -> Result<()> {
    // Add columns if they don't exist (idempotent migrations).
    let migrations = [
        "ALTER TABLE entries ADD COLUMN mtime_sec     INTEGER NOT NULL DEFAULT 0",
        "ALTER TABLE entries ADD COLUMN mtime_nsec    INTEGER NOT NULL DEFAULT 0",
        "ALTER TABLE entries ADD COLUMN discovered_at INTEGER NOT NULL DEFAULT 0",
    ];
    for sql in &migrations {
        // Ignore "duplicate column" errors — that means it already exists.
        let _ = conn.execute_batch(sql);
    }
    Ok(())
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
    mtime_sec: i64,
    mtime_nsec: i64,
    discovered_at: i64,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO entries
             (parent_id, basename, full_path, is_dir,
              mtime_sec, mtime_nsec, discovered_at)
         VALUES (?1,?2,?3,?4,?5,?6,?7)",
        params![
            parent_id, basename, full_path, is_dir as i32,
            mtime_sec, mtime_nsec, discovered_at
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn delete_entry_by_path(conn: &Connection, path: &str) -> Result<usize> {
    conn.execute(
        "DELETE FROM entries WHERE full_path = ?1",
        params![path],
    )
}

pub fn delete_entries_under(conn: &Connection, prefix: &str) -> Result<usize> {
    conn.execute(
        "DELETE FROM entries WHERE full_path GLOB ?1",
        params![format!("{}*", prefix)],
    )
}

pub fn rename_entry(conn: &Connection, old: &str, new: &str) -> Result<usize> {
    let new_basename = Path::new(new)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    conn.execute(
        "UPDATE entries SET full_path=?1, basename=?2 WHERE full_path=?3",
        params![new, new_basename, old],
    )?;
    conn.execute(
        "UPDATE entries
         SET full_path = ?1 || SUBSTR(full_path, LENGTH(?2)+1)
         WHERE full_path GLOB ?3",
        params![new, old, format!("{}/*", old)],
    )?;
    Ok(1)
}

pub fn entry_exists(conn: &Connection, path: &str) -> Result<bool> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM entries WHERE full_path=?1",
        params![path],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

pub fn entry_count(conn: &Connection) -> Result<i64> {
    conn.query_row("SELECT COUNT(*) FROM entries", [], |r| r.get(0))
}

pub fn get_all_dirs(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT full_path FROM entries WHERE is_dir=1 ORDER BY full_path",
    )?;
    let rows = stmt.query_map([], |r| r.get(0))?;
    rows.collect()
}

// ---------------------------------------------------------------------------
// Search — streaming via callback to avoid Vec allocation
// ---------------------------------------------------------------------------

/// Call `on_result` for each matching full_path. Return false from the
/// callback to stop early (e.g. --limit reached).
pub fn search_by_glob_streaming(
    conn: &Connection,
    pattern: &str,
    type_filter: Option<bool>, // Some(true)=dirs, Some(false)=files, None=all
    limit: Option<usize>,
    mut on_result: impl FnMut(&str) -> bool,
) -> Result<u64> {
    let is_glob = pattern.contains('*') || pattern.contains('?');
    let mut count = 0u64;
    let limit_val = limit.unwrap_or(usize::MAX);

    let sql = match (is_glob, type_filter) {
        (true, Some(d)) =>
            "SELECT full_path FROM entries WHERE basename GLOB ?1 AND is_dir=?2 ORDER BY full_path",
        (true, None) =>
            "SELECT full_path FROM entries WHERE basename GLOB ?1 ORDER BY full_path",
        (false, Some(_)) =>
            "SELECT full_path FROM entries WHERE basename=?1 AND is_dir=?2 ORDER BY full_path",
        (false, None) =>
            "SELECT full_path FROM entries WHERE basename=?1 ORDER BY full_path",
    };

    let mut stmt = conn.prepare_cached(sql)?;

    macro_rules! run {
        ($stmt:expr, $($p:expr),*) => {{
            let mut rows = $stmt.query(params![$($p),*])?;
            while let Some(row) = rows.next()? {
                if count as usize >= limit_val { break; }
                let path: String = row.get(0)?;
                if !on_result(&path) { break; }
                count += 1;
            }
        }};
    }

    match (is_glob, type_filter) {
        (true,  Some(d)) => run!(stmt, pattern, d as i32),
        (true,  None)    => run!(stmt, pattern),
        (false, Some(d)) => run!(stmt, pattern, d as i32),
        (false, None)    => run!(stmt, pattern),
    }

    Ok(count)
}

pub fn search_under_streaming(
    conn: &Connection,
    prefix: &str,
    name_pattern: Option<&str>,
    type_filter: Option<bool>,
    limit: Option<usize>,
    mut on_result: impl FnMut(&str) -> bool,
) -> Result<u64> {
    let glob_prefix = format!("{}*", prefix);
    let name_pat = name_pattern.unwrap_or("*");
    let is_exact = !name_pat.contains('*') && !name_pat.contains('?');
    let limit_val = limit.unwrap_or(usize::MAX);
    let mut count = 0u64;

    let sql = match (is_exact, type_filter) {
        (true, Some(_)) =>
            "SELECT full_path FROM entries WHERE full_path GLOB ?1 AND basename=?2 AND is_dir=?3 ORDER BY full_path",
        (true, None) =>
            "SELECT full_path FROM entries WHERE full_path GLOB ?1 AND basename=?2 ORDER BY full_path",
        (false, Some(_)) =>
            "SELECT full_path FROM entries WHERE full_path GLOB ?1 AND basename GLOB ?2 AND is_dir=?3 ORDER BY full_path",
        (false, None) =>
            "SELECT full_path FROM entries WHERE full_path GLOB ?1 AND basename GLOB ?2 ORDER BY full_path",
    };

    let mut stmt = conn.prepare_cached(sql)?;

    macro_rules! run {
        ($stmt:expr, $($p:expr),*) => {{
            let mut rows = $stmt.query(params![$($p),*])?;
            while let Some(row) = rows.next()? {
                if count as usize >= limit_val { break; }
                let path: String = row.get(0)?;
                if !on_result(&path) { break; }
                count += 1;
            }
        }};
    }

    match (is_exact, type_filter) {
        (true,  Some(d)) => run!(stmt, glob_prefix, name_pat, d as i32),
        (true,  None)    => run!(stmt, glob_prefix, name_pat),
        (false, Some(d)) => run!(stmt, glob_prefix, name_pat, d as i32),
        (false, None)    => run!(stmt, glob_prefix, name_pat),
    }

    Ok(count)
}

pub fn search_by_path_glob_streaming(
    conn: &Connection,
    pattern: &str,
    type_filter: Option<bool>,
    limit: Option<usize>,
    mut on_result: impl FnMut(&str) -> bool,
) -> Result<u64> {
    let limit_val = limit.unwrap_or(usize::MAX);
    let mut count = 0u64;

    let sql = match type_filter {
        Some(_) => "SELECT full_path FROM entries WHERE full_path GLOB ?1 AND is_dir=?2 ORDER BY full_path",
        None    => "SELECT full_path FROM entries WHERE full_path GLOB ?1 ORDER BY full_path",
    };
    let mut stmt = conn.prepare_cached(sql)?;
    let mut rows = match type_filter {
        Some(d) => stmt.query(params![pattern, d as i32])?,
        None    => stmt.query(params![pattern])?,
    };
    while let Some(row) = rows.next()? {
        if count as usize >= limit_val { break; }
        let path: String = row.get(0)?;
        if !on_result(&path) { break; }
        count += 1;
    }
    Ok(count)
}

/// Regex search using SQLite REGEXP (user-defined function registered at open time).
pub fn search_by_regex_streaming(
    conn: &Connection,
    pattern: &str,
    field: &str, // "basename" or "full_path"
    type_filter: Option<bool>,
    limit: Option<usize>,
    mut on_result: impl FnMut(&str) -> bool,
) -> Result<u64> {
    // Register the REGEXP function if not already registered.
    register_regexp(conn)?;

    let limit_val = limit.unwrap_or(usize::MAX);
    let mut count = 0u64;

    let sql = match type_filter {
        Some(_) => format!(
            "SELECT full_path FROM entries WHERE {} REGEXP ?1 AND is_dir=?2 ORDER BY full_path",
            field
        ),
        None => format!(
            "SELECT full_path FROM entries WHERE {} REGEXP ?1 ORDER BY full_path",
            field
        ),
    };

    let mut stmt = conn.prepare_cached(&sql)?;
    let mut rows = match type_filter {
        Some(d) => stmt.query(params![pattern, d as i32])?,
        None    => stmt.query(params![pattern])?,
    };
    while let Some(row) = rows.next()? {
        if count as usize >= limit_val { break; }
        let path: String = row.get(0)?;
        if !on_result(&path) { break; }
        count += 1;
    }
    Ok(count)
}

fn register_regexp(conn: &Connection) -> Result<()> {
    conn.create_scalar_function(
        "REGEXP",
        2,
        rusqlite::functions::FunctionFlags::SQLITE_UTF8
            | rusqlite::functions::FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let pattern: String = ctx.get(0)?;
            let text: String = ctx.get(1)?;
            // Simple regex using standard library — no extra dep needed
            // for basic patterns. For full regex support add the `regex` crate.
            let re = regex_lite::Regex::new(&pattern)
                .map_err(|e| rusqlite::Error::UserFunctionError(e.into()))?;
            Ok(re.is_match(&text))
        },
    )
}

// ---------------------------------------------------------------------------
// Recent files — by mtime or discovery time
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct RecentEntry {
    pub full_path: String,
    pub mtime_sec: i64,
    pub mtime_nsec: i64,
    pub is_dir: bool,
}

/// Return entries with mtime in [from_sec, to_sec], newest first.
pub fn search_recent_streaming(
    conn: &Connection,
    from_sec: i64,
    to_sec: i64,
    type_filter: Option<bool>,
    limit: Option<usize>,
    mut on_result: impl FnMut(&RecentEntry) -> bool,
) -> Result<u64> {
    let limit_val = limit.unwrap_or(usize::MAX);
    let mut count = 0u64;

    let sql = match type_filter {
        Some(_) => "SELECT full_path, mtime_sec, mtime_nsec, is_dir
                    FROM entries
                    WHERE mtime_sec BETWEEN ?1 AND ?2 AND is_dir=?3
                    ORDER BY mtime_sec DESC, mtime_nsec DESC",
        None =>    "SELECT full_path, mtime_sec, mtime_nsec, is_dir
                    FROM entries
                    WHERE mtime_sec BETWEEN ?1 AND ?2
                    ORDER BY mtime_sec DESC, mtime_nsec DESC",
    };

    let mut stmt = conn.prepare_cached(sql)?;
    let mut rows = match type_filter {
        Some(d) => stmt.query(params![from_sec, to_sec, d as i32])?,
        None    => stmt.query(params![from_sec, to_sec])?,
    };
    while let Some(row) = rows.next()? {
        if count as usize >= limit_val { break; }
        let entry = RecentEntry {
            full_path:  row.get(0)?,
            mtime_sec:  row.get(1)?,
            mtime_nsec: row.get(2)?,
            is_dir:     row.get::<_, i32>(3)? != 0,
        };
        if !on_result(&entry) { break; }
        count += 1;
    }
    Ok(count)
}

// ---------------------------------------------------------------------------
// Health
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct HealthInfo {
    pub entry_count: i64,
    pub dir_count: i64,
    pub file_count: i64,
    pub last_scan_completed: Option<i64>,
    pub last_entry_count: Option<i64>,
    pub monitoring_mode: String,
    pub pending_events: i64,
    pub db_size_bytes: u64,
}

pub fn get_health(conn: &Connection, db_path: &Path) -> Result<HealthInfo> {
    let entry_count = entry_count(conn)?;
    let dir_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM entries WHERE is_dir=1", [], |r| r.get(0))?;
    let file_count = entry_count - dir_count;
    let pending_events = pending_event_count(conn)?;

    let last_scan = get_metadata(conn, "last_scan_completed")?
        .and_then(|v| v.parse::<i64>().ok());
    let last_count = get_metadata(conn, "last_entry_count")?
        .and_then(|v| v.parse::<i64>().ok());
    let monitoring_mode = get_metadata(conn, "monitoring_mode")?
        .unwrap_or_else(|| "unknown".to_string());

    let db_size_bytes = std::fs::metadata(db_path)
        .map(|m| m.len())
        .unwrap_or(0);

    Ok(HealthInfo {
        entry_count,
        dir_count,
        file_count,
        last_scan_completed: last_scan,
        last_entry_count: last_count,
        monitoring_mode,
        pending_events,
        db_size_bytes,
    })
}

// ---------------------------------------------------------------------------
// Stats
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ExtStat {
    pub ext: String,
    pub count: i64,
}

#[derive(Debug, Clone)]
pub struct DirStat {
    pub path: String,
    pub count: i64,
}

#[derive(Debug, Clone)]
pub struct IndexStats {
    pub entry_count: i64,
    pub dir_count: i64,
    pub file_count: i64,
    pub top_extensions: Vec<ExtStat>,
    pub largest_dirs: Vec<DirStat>,
    pub db_size_bytes: u64,
    pub index_age_secs: Option<i64>,
}

pub fn get_stats(conn: &Connection, db_path: &Path) -> Result<IndexStats> {
    let entry_count = entry_count(conn)?;
    let dir_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM entries WHERE is_dir=1", [], |r| r.get(0))?;
    let file_count = entry_count - dir_count;

    // Top 10 extensions by count.
    let mut ext_stmt = conn.prepare(
        "SELECT
            CASE
                WHEN INSTR(basename, '.') > 0
                THEN LOWER(SUBSTR(basename, INSTR(basename, '.')+1))
                ELSE ''
            END as ext,
            COUNT(*) as cnt
         FROM entries
         WHERE is_dir=0 AND basename LIKE '%.%'
         GROUP BY ext
         ORDER BY cnt DESC
         LIMIT 10",
    )?;
    let top_extensions: Vec<ExtStat> = ext_stmt
        .query_map([], |r| Ok(ExtStat { ext: r.get(0)?, count: r.get(1)? }))?
        .flatten()
        .collect();

    // Top 10 largest directories by direct child count.
    let mut dir_stmt = conn.prepare(
        "SELECT p.full_path, COUNT(c.id) as cnt
         FROM entries p
         JOIN entries c ON c.parent_id = p.id
         WHERE p.is_dir = 1
         GROUP BY p.id
         ORDER BY cnt DESC
         LIMIT 10",
    )?;
    let largest_dirs: Vec<DirStat> = dir_stmt
        .query_map([], |r| Ok(DirStat { path: r.get(0)?, count: r.get(1)? }))?
        .flatten()
        .collect();

    let db_size_bytes = std::fs::metadata(db_path)
        .map(|m| m.len())
        .unwrap_or(0);

    let index_age_secs = get_metadata(conn, "last_scan_completed")?
        .and_then(|v| v.parse::<i64>().ok())
        .map(|t| {
            std::time::SystemTime::now()
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64 - t
        });

    Ok(IndexStats {
        entry_count,
        dir_count,
        file_count,
        top_extensions,
        largest_dirs,
        db_size_bytes,
        index_age_secs,
    })
}

// ---------------------------------------------------------------------------
// Event journal
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
        "INSERT INTO pending_events
             (event_type,path,new_path,is_dir,timestamp)
         VALUES (?1,?2,?3,?4,?5)",
        params![event_type.as_str(), path, new_path, is_dir as i32, timestamp],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn load_unapplied_events(conn: &Connection) -> Result<Vec<PendingEvent>> {
    let mut stmt = conn.prepare(
        "SELECT id,event_type,path,new_path,is_dir,timestamp
         FROM pending_events WHERE applied=0 ORDER BY id ASC",
    )?;
    let rows = stmt.query_map([], |r| {
        let et: String = r.get(1)?;
        Ok(PendingEvent {
            id:         r.get(0)?,
            event_type: EventType::from_str(&et).unwrap_or(EventType::Modify),
            path:       r.get(2)?,
            new_path:   r.get(3)?,
            is_dir:     r.get::<_,i32>(4)? != 0,
            timestamp:  r.get(5)?,
        })
    })?;
    rows.collect()
}

pub fn mark_event_applied(conn: &Connection, id: i64) -> Result<()> {
    conn.execute(
        "UPDATE pending_events SET applied=1 WHERE id=?1",
        params![id],
    )?;
    Ok(())
}

pub fn pending_event_count(conn: &Connection) -> Result<i64> {
    conn.query_row(
        "SELECT COUNT(*) FROM pending_events WHERE applied=0",
        [], |r| r.get(0),
    )
}

pub fn prune_applied_events(conn: &Connection) -> Result<usize> {
    conn.execute("DELETE FROM pending_events WHERE applied=1", [])
}

// ---------------------------------------------------------------------------
// Metadata
// ---------------------------------------------------------------------------

pub fn set_metadata(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO scan_metadata (key,value) VALUES (?1,?2)",
        params![key, value],
    )?;
    Ok(())
}

pub fn get_metadata(conn: &Connection, key: &str) -> Result<Option<String>> {
    match conn.query_row(
        "SELECT value FROM scan_metadata WHERE key=?1",
        params![key], |r| r.get(0),
    ) {
        Ok(v) => Ok(Some(v)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e),
    }
}
