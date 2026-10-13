//! SQLite index: schema, insertion, search, streaming, health, stats,
//! and event journal.
//!
//! # Schema version 3
//! entries gains: mtime_sec, mtime_nsec, discovered_at (v2), scan_gen (v3)
//! full_path is UNIQUE (v3) so inserts are idempotent upserts.
//!
//! # Subtree queries
//! Subtrees are selected with a half-open range on full_path
//! (`p/` <= full_path < `p0`, since '0' follows '/' in ASCII) rather than
//! GLOB, so paths containing `*?[]` match literally and `/a/b` never
//! matches `/a/bc`.

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
            discovered_at INTEGER NOT NULL DEFAULT 0,
            scan_gen      INTEGER NOT NULL DEFAULT 0
        );

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
    // Migrate older schemas that lack the new columns, then index them.
    migrate(&conn)?;
    conn.execute_batch("
        CREATE INDEX IF NOT EXISTS idx_basename
            ON entries (basename);
        CREATE INDEX IF NOT EXISTS idx_parent
            ON entries (parent_id);
        CREATE INDEX IF NOT EXISTS idx_mtime
            ON entries (mtime_sec);
        CREATE INDEX IF NOT EXISTS idx_discovered
            ON entries (discovered_at);
    ")?;
    Ok(conn)
}

fn migrate(conn: &Connection) -> Result<()> {
    // Add columns if they don't exist (idempotent migrations).
    let migrations = [
        "ALTER TABLE entries ADD COLUMN mtime_sec     INTEGER NOT NULL DEFAULT 0",
        "ALTER TABLE entries ADD COLUMN mtime_nsec    INTEGER NOT NULL DEFAULT 0",
        "ALTER TABLE entries ADD COLUMN discovered_at INTEGER NOT NULL DEFAULT 0",
        "ALTER TABLE entries ADD COLUMN scan_gen      INTEGER NOT NULL DEFAULT 0",
    ];
    for sql in &migrations {
        // Ignore "duplicate column" errors — that means it already exists.
        let _ = conn.execute_batch(sql);
    }

    // v3: full_path becomes unique. Older indexes may hold duplicate rows
    // (scan + replayed CREATE events), so dedupe before building the index.
    let has_unique: bool = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master
         WHERE type='index' AND name='idx_full_path_unique'",
        [], |r| r.get::<_, i64>(0),
    )? > 0;
    if !has_unique {
        conn.execute_batch("
            BEGIN;
            DELETE FROM entries WHERE id NOT IN
                (SELECT MIN(id) FROM entries GROUP BY full_path);
            DROP INDEX IF EXISTS idx_full_path;
            CREATE UNIQUE INDEX idx_full_path_unique ON entries (full_path);
            COMMIT;
        ")?;
    }
    Ok(())
}

/// Half-open range [lo, hi) of full_path values strictly under `path`.
fn subtree_range(path: &str) -> (String, String) {
    let base = path.trim_end_matches('/');
    (format!("{base}/"), format!("{base}0"))
}

// ---------------------------------------------------------------------------
// Entry operations
// ---------------------------------------------------------------------------

/// Insert or update the entry at `full_path`.
///
/// `scan_gen`: `Some(g)` marks the row as seen by scan generation `g`
/// (scanner); `None` leaves an existing row's generation untouched
/// (live events). `discovered_at` is kept from the first insert.
pub fn insert_entry(
    conn: &Connection,
    parent_id: Option<i64>,
    basename: &str,
    full_path: &str,
    is_dir: bool,
    mtime_sec: i64,
    mtime_nsec: i64,
    discovered_at: i64,
    scan_gen: Option<i64>,
) -> Result<()> {
    conn.prepare_cached(
        "INSERT INTO entries
             (parent_id, basename, full_path, is_dir,
              mtime_sec, mtime_nsec, discovered_at, scan_gen)
         VALUES (?1,?2,?3,?4,?5,?6,?7,COALESCE(?8,0))
         ON CONFLICT(full_path) DO UPDATE SET
             basename   = excluded.basename,
             is_dir     = excluded.is_dir,
             mtime_sec  = excluded.mtime_sec,
             mtime_nsec = excluded.mtime_nsec,
             scan_gen   = COALESCE(?8, scan_gen)",
    )?.execute(params![
        parent_id, basename, full_path, is_dir as i32,
        mtime_sec, mtime_nsec, discovered_at, scan_gen
    ])?;
    Ok(())
}

pub fn delete_entry_by_path(conn: &Connection, path: &str) -> Result<usize> {
    conn.execute(
        "DELETE FROM entries WHERE full_path = ?1",
        params![path],
    )
}

/// Delete `path` itself and everything beneath it.
pub fn delete_entries_under(conn: &Connection, path: &str) -> Result<usize> {
    let (lo, hi) = subtree_range(path);
    conn.execute(
        "DELETE FROM entries
         WHERE full_path = ?1 OR (full_path >= ?2 AND full_path < ?3)",
        params![path, lo, hi],
    )
}

/// Move `old` (and its subtree) to `new`. Anything already indexed at
/// `new` is replaced, matching rename(2) semantics. Returns the number of
/// rows moved (0 if `old` was not indexed).
pub fn rename_entry(conn: &Connection, old: &str, new: &str) -> Result<usize> {
    if old == new { return Ok(0); }
    let new_basename = Path::new(new)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let (old_lo, old_hi) = subtree_range(old);
    delete_entries_under(conn, new)?;
    let mut moved = conn.execute(
        "UPDATE entries SET full_path=?1, basename=?2 WHERE full_path=?3",
        params![new, new_basename, old],
    )?;
    moved += conn.execute(
        "UPDATE entries
         SET full_path = ?1 || SUBSTR(full_path, LENGTH(?2)+1)
         WHERE full_path >= ?3 AND full_path < ?4",
        params![new, old.trim_end_matches('/'), old_lo, old_hi],
    )?;
    Ok(moved)
}

/// Delete rows under `root` not seen by scan generation `scan_gen`,
/// leaving alone any subtree listed in `keep` (nested roots and excludes
/// that are maintained separately).
pub fn sweep_stale(
    conn: &Connection,
    root: &str,
    keep: &[String],
    scan_gen: i64,
) -> Result<usize> {
    let (lo, hi) = subtree_range(root);
    let mut sql = String::from(
        "DELETE FROM entries
         WHERE scan_gen < ?1 AND full_path >= ?2 AND full_path < ?3",
    );
    let mut args: Vec<String> = vec![lo, hi];
    for k in keep {
        let (klo, khi) = subtree_range(k);
        let n = args.len();
        sql.push_str(&format!(
            " AND full_path <> ?{} AND NOT (full_path >= ?{} AND full_path < ?{})",
            n + 2, n + 3, n + 4,
        ));
        args.extend([k.clone(), klo, khi]);
    }
    let mut stmt = conn.prepare(&sql)?;
    stmt.raw_bind_parameter(1, scan_gen)?;
    for (i, a) in args.iter().enumerate() {
        stmt.raw_bind_parameter(i + 2, a)?;
    }
    stmt.raw_execute()
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
            // Compile the pattern once per statement, not once per row.
            let re: std::sync::Arc<regex_lite::Regex> = ctx.get_or_create_aux(
                0,
                |vr| -> std::result::Result<_, Box<dyn std::error::Error + Send + Sync>> {
                    Ok(regex_lite::Regex::new(vr.as_str()?)?)
                },
            )?;
            let text = ctx.get_raw(1).as_str().unwrap_or("");
            Ok(re.is_match(text))
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

/// Load up to `limit` unapplied events, oldest first.
pub fn load_unapplied_events(conn: &Connection, limit: usize) -> Result<Vec<PendingEvent>> {
    let mut stmt = conn.prepare_cached(
        "SELECT id,event_type,path,new_path,is_dir,timestamp
         FROM pending_events WHERE applied=0 ORDER BY id ASC LIMIT ?1",
    )?;
    let rows = stmt.query_map(params![limit as i64], |r| {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> Connection {
        let dir = std::env::temp_dir().join(format!(
            "rfind-db-test-{}-{:?}", std::process::id(), std::thread::current().id()));
        let _ = std::fs::remove_file(&dir);
        open(&dir).unwrap()
    }

    fn add(c: &Connection, p: &str, is_dir: bool, scan_gen: Option<i64>) {
        let b = Path::new(p).file_name().unwrap().to_string_lossy().to_string();
        insert_entry(c, None, &b, p, is_dir, 0, 0, 0, scan_gen).unwrap();
    }

    fn paths(c: &Connection) -> Vec<String> {
        let mut s = c.prepare("SELECT full_path FROM entries ORDER BY full_path").unwrap();
        s.query_map([], |r| r.get(0)).unwrap().map(|r| r.unwrap()).collect()
    }

    #[test]
    fn insert_is_idempotent() {
        let c = mem();
        add(&c, "/a/x", false, None);
        add(&c, "/a/x", false, None);
        assert_eq!(entry_count(&c).unwrap(), 1);
    }

    #[test]
    fn delete_under_does_not_touch_sibling_prefix() {
        let c = mem();
        for p in ["/a/b", "/a/b/c", "/a/bc", "/a/b.txt"] { add(&c, p, false, None); }
        delete_entries_under(&c, "/a/b").unwrap();
        assert_eq!(paths(&c), vec!["/a/b.txt", "/a/bc"]);
    }

    #[test]
    fn glob_metacharacters_in_paths_are_literal() {
        let c = mem();
        for p in ["/w/photo [1]", "/w/photo [1]/x.jpg", "/w/photo 1/y.jpg"] {
            add(&c, p, false, None);
        }
        delete_entries_under(&c, "/w/photo [1]").unwrap();
        assert_eq!(paths(&c), vec!["/w/photo 1/y.jpg"]);
    }

    #[test]
    fn rename_moves_subtree_and_replaces_target() {
        let c = mem();
        for p in ["/a/d", "/a/d/f", "/a/dd/g", "/b/d", "/b/d/old"] { add(&c, p, false, None); }
        assert_eq!(rename_entry(&c, "/a/d", "/b/d").unwrap(), 2);
        assert_eq!(paths(&c), vec!["/a/dd/g", "/b/d", "/b/d/f"]);
    }

    #[test]
    fn sweep_removes_unseen_rows_but_keeps_nested_roots() {
        let c = mem();
        add(&c, "/etc/old", false, Some(1));
        add(&c, "/etc/new", false, Some(2));
        add(&c, "/mnt/c/win", false, Some(1));
        add(&c, "/mnt/cx", false, Some(1));
        sweep_stale(&c, "/", &["/mnt/c".to_string()], 2).unwrap();
        assert_eq!(paths(&c), vec!["/etc/new", "/mnt/c/win"]);
    }

    #[test]
    fn live_upsert_keeps_scan_gen() {
        let c = mem();
        add(&c, "/x", false, Some(5));
        add(&c, "/x", false, None);
        let g: i64 = c.query_row("SELECT scan_gen FROM entries", [], |r| r.get(0)).unwrap();
        assert_eq!(g, 5);
    }

    #[test]
    fn migration_dedupes_existing_rows() {
        let path = std::env::temp_dir().join(format!("rfind-mig-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        {
            let c = Connection::open(&path).unwrap();
            c.execute_batch("
                CREATE TABLE entries (id INTEGER PRIMARY KEY AUTOINCREMENT,
                    parent_id INTEGER, basename TEXT NOT NULL, full_path TEXT NOT NULL,
                    is_dir INTEGER NOT NULL DEFAULT 0);
                CREATE INDEX idx_full_path ON entries (full_path);
                INSERT INTO entries (basename, full_path) VALUES ('x','/x'),('x','/x'),('y','/y');
            ").unwrap();
        }
        let c = open(&path).unwrap();
        assert_eq!(entry_count(&c).unwrap(), 2);
        let _ = std::fs::remove_file(&path);
    }
}
