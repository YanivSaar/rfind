//! SQLite index: schema, insertion, search, streaming, health, stats,
//! and event journal.
//!
//! # Schema version 4 (normalized)
//! Each directory path is stored once, in `dirs`; each entry stores only
//! its name and its parent's `dirs.id`. A file's full path is
//! `dirs.path || '/' || entries.name`, with the root directory stored as
//! the empty string so the concatenation needs no special case.
//! Storing the full path per entry (v3) kept every path twice — in the
//! table and in its unique index — and cost ~2.5× the space.
//!
//! `(dir_id, name)` is UNIQUE, so inserts are idempotent upserts. Renaming
//! a directory rewrites only `dirs` rows; entries beneath it don't move.
//!
//! The index is a cache of the filesystem: an older schema is dropped and
//! rebuilt by a full scan rather than migrated.
//!
//! # Subtree queries
//! Subtrees are selected with a half-open range on `dirs.path`
//! (`p/` <= path < `p0`, since '0' follows '/' in ASCII) rather than GLOB,
//! so paths containing `*?[]` match literally and `/a/b` never matches
//! `/a/bc`.

use rusqlite::{params, Connection, OptionalExtension, Result};
use std::path::Path;

const SCHEMA_VERSION: i64 = 4;

/// Full path of an entry `e` joined with its directory `d`.
const PATH_EXPR: &str = "(d.path || '/' || e.name)";

// ---------------------------------------------------------------------------
// Open / schema
// ---------------------------------------------------------------------------

pub fn open(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path)?;
    conn.execute_batch("PRAGMA journal_mode=WAL;")?;
    conn.execute_batch("PRAGMA synchronous=NORMAL;")?;
    conn.execute_batch("PRAGMA cache_size=-65536;")?;
    conn.execute_batch("PRAGMA temp_store=MEMORY;")?;

    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version != SCHEMA_VERSION {
        // Older layouts are dropped, not migrated: the daemon sees no
        // last scan and rebuilds the index with a full scan.
        conn.execute_batch(
            "
            BEGIN;
            DROP TABLE IF EXISTS entries;
            DROP TABLE IF EXISTS dirs;
            DROP TABLE IF EXISTS pending_events;
            DROP TABLE IF EXISTS scan_metadata;
            COMMIT;
        ",
        )?;
    }

    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS dirs (
            id   INTEGER PRIMARY KEY,
            path TEXT    NOT NULL UNIQUE   -- no trailing '/'; root is ''
        );

        CREATE TABLE IF NOT EXISTS entries (
            id         INTEGER PRIMARY KEY,
            dir_id     INTEGER NOT NULL,   -- dirs.id of the parent directory
            name       TEXT    NOT NULL,
            is_dir     INTEGER NOT NULL DEFAULT 0,
            mtime_sec  INTEGER NOT NULL DEFAULT 0,
            mtime_nsec INTEGER NOT NULL DEFAULT 0,
            scan_gen   INTEGER NOT NULL DEFAULT 0
        );
        CREATE UNIQUE INDEX IF NOT EXISTS idx_entries_dir_name
            ON entries (dir_id, name);
        CREATE INDEX IF NOT EXISTS idx_entries_name
            ON entries (name);
        CREATE INDEX IF NOT EXISTS idx_entries_mtime
            ON entries (mtime_sec);

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
    ",
    )?;
    conn.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION};"))?;
    Ok(conn)
}

/// Normalize a directory path to its `dirs.path` form: no trailing '/',
/// root as ''.
fn norm_dir(path: &str) -> &str {
    path.trim_end_matches('/')
}

/// Split a full path into (`dirs.path` of its parent, name).
fn split_path(path: &str) -> (&str, &str) {
    let p = norm_dir(path);
    match p.rfind('/') {
        Some(i) => (&p[..i], &p[i + 1..]),
        None => ("", p),
    }
}

/// Half-open range [lo, hi) of `dirs.path` values strictly under `dir`.
fn subtree_range(dir: &str) -> (String, String) {
    let base = norm_dir(dir);
    (format!("{base}/"), format!("{base}0"))
}

fn dir_id(conn: &Connection, dir: &str) -> Result<Option<i64>> {
    conn.prepare_cached("SELECT id FROM dirs WHERE path = ?1")?
        .query_row(params![dir], |r| r.get(0))
        .optional()
}

fn dir_id_or_create(conn: &Connection, dir: &str) -> Result<i64> {
    if let Some(id) = dir_id(conn, dir)? {
        return Ok(id);
    }
    conn.prepare_cached("INSERT INTO dirs (path) VALUES (?1)")?
        .execute(params![dir])?;
    Ok(conn.last_insert_rowid())
}

// ---------------------------------------------------------------------------
// Entry operations
// ---------------------------------------------------------------------------

/// Batch inserter that remembers the last parent directory, so the many
/// consecutive entries of one directory resolve its id once. Use within a
/// single transaction (ids can change once directories are deleted).
pub struct Inserter<'c> {
    conn: &'c Connection,
    last: Option<(String, i64)>,
}

impl<'c> Inserter<'c> {
    pub fn new(conn: &'c Connection) -> Self {
        Self { conn, last: None }
    }

    /// Insert or update the entry at `full_path`.
    ///
    /// `scan_gen`: `Some(g)` marks the row as seen by scan generation `g`
    /// (scanner); `None` leaves an existing row's generation untouched
    /// (live events).
    pub fn insert(
        &mut self,
        full_path: &str,
        is_dir: bool,
        mtime_sec: i64,
        mtime_nsec: i64,
        scan_gen: Option<i64>,
    ) -> Result<()> {
        let (dir, name) = split_path(full_path);
        if name.is_empty() {
            return Ok(());
        } // the root itself is not an entry
        let did = match &self.last {
            Some((d, id)) if d == dir => *id,
            _ => {
                let id = dir_id_or_create(self.conn, dir)?;
                self.last = Some((dir.to_string(), id));
                id
            }
        };
        self.conn
            .prepare_cached(
                "INSERT INTO entries (dir_id, name, is_dir, mtime_sec, mtime_nsec, scan_gen)
             VALUES (?1, ?2, ?3, ?4, ?5, COALESCE(?6, 0))
             ON CONFLICT(dir_id, name) DO UPDATE SET
                 is_dir     = excluded.is_dir,
                 mtime_sec  = excluded.mtime_sec,
                 mtime_nsec = excluded.mtime_nsec,
                 scan_gen   = COALESCE(?6, scan_gen)",
            )?
            .execute(params![
                did,
                name,
                is_dir as i32,
                mtime_sec,
                mtime_nsec,
                scan_gen
            ])?;
        Ok(())
    }
}

/// Insert or update a single entry. See [`Inserter::insert`].
pub fn insert_entry(
    conn: &Connection,
    full_path: &str,
    is_dir: bool,
    mtime_sec: i64,
    mtime_nsec: i64,
    scan_gen: Option<i64>,
) -> Result<()> {
    Inserter::new(conn).insert(full_path, is_dir, mtime_sec, mtime_nsec, scan_gen)
}

pub fn delete_entry_by_path(conn: &Connection, path: &str) -> Result<usize> {
    let (dir, name) = split_path(path);
    conn.execute(
        "DELETE FROM entries
         WHERE dir_id = (SELECT id FROM dirs WHERE path = ?1) AND name = ?2",
        params![dir, name],
    )
}

/// Delete `path` itself and everything beneath it.
pub fn delete_entries_under(conn: &Connection, path: &str) -> Result<usize> {
    let p = norm_dir(path);
    let (lo, hi) = subtree_range(p);
    let mut n = delete_entry_by_path(conn, p)?;
    n += conn.execute(
        "DELETE FROM entries WHERE dir_id IN
             (SELECT id FROM dirs WHERE path = ?1 OR (path >= ?2 AND path < ?3))",
        params![p, lo, hi],
    )?;
    conn.execute(
        "DELETE FROM dirs WHERE path = ?1 OR (path >= ?2 AND path < ?3)",
        params![p, lo, hi],
    )?;
    Ok(n)
}

/// Move `old` (and its subtree) to `new`. Anything already indexed at
/// `new` is replaced, matching rename(2) semantics. Returns the number of
/// entries moved (0 if `old` was not indexed).
pub fn rename_entry(conn: &Connection, old: &str, new: &str) -> Result<usize> {
    let (old, new) = (norm_dir(old), norm_dir(new));
    if old == new {
        return Ok(0);
    }
    let (old_dir, old_name) = split_path(old);
    let (new_dir, new_name) = split_path(new);
    let Some(old_did) = dir_id(conn, old_dir)? else {
        return Ok(0);
    };

    delete_entries_under(conn, new)?;
    let new_did = dir_id_or_create(conn, new_dir)?;
    let mut moved = conn.execute(
        "UPDATE entries SET dir_id = ?1, name = ?2 WHERE dir_id = ?3 AND name = ?4",
        params![new_did, new_name, old_did, old_name],
    )?;
    if moved == 0 {
        return Ok(0);
    }

    // Re-root the directory rows of the subtree; entries follow by id.
    let (lo, hi) = subtree_range(old);
    conn.execute(
        "UPDATE dirs SET path = ?1 || SUBSTR(path, LENGTH(?2) + 1)
         WHERE path = ?2 OR (path >= ?3 AND path < ?4)",
        params![new, old, lo, hi],
    )?;
    let (lo, hi) = subtree_range(new);
    moved += conn.query_row(
        "SELECT COUNT(*) FROM entries WHERE dir_id IN
             (SELECT id FROM dirs WHERE path = ?1 OR (path >= ?2 AND path < ?3))",
        params![new, lo, hi],
        |r| r.get::<_, i64>(0),
    )? as usize;
    Ok(moved)
}

/// Delete entries under `root` not seen by scan generation `scan_gen`,
/// leaving alone any subtree listed in `keep` (nested roots maintained by
/// their own scans), then drop directory rows left without entries.
pub fn sweep_stale(conn: &Connection, root: &str, keep: &[String], scan_gen: i64) -> Result<usize> {
    let root = norm_dir(root);
    let (lo, hi) = subtree_range(root);
    let mut sql = "DELETE FROM entries WHERE id IN (
             SELECT e.id FROM entries e JOIN dirs d ON d.id = e.dir_id
             WHERE e.scan_gen < ?1
               AND (d.path = ?2 OR (d.path >= ?3 AND d.path < ?4))"
        .to_string();
    let mut args: Vec<String> = vec![root.to_string(), lo, hi];
    for k in keep {
        let k = norm_dir(k);
        let (klo, khi) = subtree_range(k);
        let n = args.len();
        sql.push_str(&format!(
            " AND d.path <> ?{a} AND NOT (d.path >= ?{b} AND d.path < ?{c})
              AND {PATH_EXPR} <> ?{a}",
            a = n + 2,
            b = n + 3,
            c = n + 4,
        ));
        args.extend([k.to_string(), klo, khi]);
    }
    sql.push(')');
    let mut stmt = conn.prepare(&sql)?;
    stmt.raw_bind_parameter(1, scan_gen)?;
    for (i, a) in args.iter().enumerate() {
        stmt.raw_bind_parameter(i + 2, a)?;
    }
    let n = stmt.raw_execute()?;
    if n > 0 {
        conn.execute(
            "DELETE FROM dirs WHERE NOT EXISTS
                 (SELECT 1 FROM entries WHERE entries.dir_id = dirs.id)",
            [],
        )?;
    }
    Ok(n)
}

pub fn entry_exists(conn: &Connection, path: &str) -> Result<bool> {
    let (dir, name) = split_path(path);
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM entries e JOIN dirs d ON d.id = e.dir_id
         WHERE d.path = ?1 AND e.name = ?2",
        params![dir, name],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

pub fn entry_count(conn: &Connection) -> Result<i64> {
    conn.query_row("SELECT COUNT(*) FROM entries", [], |r| r.get(0))
}

/// Full paths of every indexed directory.
pub fn get_all_dirs(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {PATH_EXPR} FROM entries e JOIN dirs d ON d.id = e.dir_id
         WHERE e.is_dir = 1 ORDER BY 1"
    ))?;
    let rows = stmt.query_map([], |r| r.get(0))?;
    rows.collect()
}

// ---------------------------------------------------------------------------
// Search — streaming via callback to avoid Vec allocation
// ---------------------------------------------------------------------------

/// Run a path-returning query, streaming each path to `on_result` until
/// it returns false or `limit` is reached.
fn stream_paths(
    conn: &Connection,
    sql: &str,
    params: &[&dyn rusqlite::ToSql],
    limit: Option<usize>,
    on_result: &mut dyn FnMut(&str) -> bool,
) -> Result<u64> {
    let limit_val = limit.unwrap_or(usize::MAX);
    let mut count = 0u64;
    let mut stmt = conn.prepare_cached(sql)?;
    let mut rows = stmt.query(params)?;
    while let Some(row) = rows.next()? {
        if count as usize >= limit_val {
            break;
        }
        let path = row.get_ref(0)?.as_str()?;
        if !on_result(path) {
            break;
        }
        count += 1;
    }
    Ok(count)
}

/// `AND e.is_dir = ?N` when filtering by type, else nothing.
fn type_clause(type_filter: Option<bool>, n: usize) -> String {
    match type_filter {
        Some(_) => format!(" AND e.is_dir = ?{n}"),
        None => String::new(),
    }
}

/// Call `on_result` for each entry whose name matches `pattern` (exact, or
/// GLOB when it contains `*` or `?`). Return false from the callback to
/// stop early (e.g. --limit reached).
pub fn search_by_glob_streaming(
    conn: &Connection,
    pattern: &str,
    type_filter: Option<bool>, // Some(true)=dirs, Some(false)=files, None=all
    limit: Option<usize>,
    mut on_result: impl FnMut(&str) -> bool,
) -> Result<u64> {
    let is_glob = pattern.contains('*') || pattern.contains('?');
    let op = if is_glob { "GLOB" } else { "=" };
    let sql = format!(
        "SELECT {PATH_EXPR} FROM entries e JOIN dirs d ON d.id = e.dir_id
         WHERE e.name {op} ?1{} ORDER BY 1",
        type_clause(type_filter, 2),
    );
    let d = type_filter.map(|d| d as i32);
    match &d {
        Some(d) => stream_paths(conn, &sql, &[&pattern, d], limit, &mut on_result),
        None => stream_paths(conn, &sql, &[&pattern], limit, &mut on_result),
    }
}

/// Entries under directory `prefix` whose name matches `name_pattern`
/// (exact or GLOB; all names when `None`).
pub fn search_under_streaming(
    conn: &Connection,
    prefix: &str,
    name_pattern: Option<&str>,
    type_filter: Option<bool>,
    limit: Option<usize>,
    mut on_result: impl FnMut(&str) -> bool,
) -> Result<u64> {
    let dir = norm_dir(prefix);
    let (lo, hi) = subtree_range(dir);
    let name_pat = name_pattern.unwrap_or("*");
    let op = if name_pat.contains('*') || name_pat.contains('?') {
        "GLOB"
    } else {
        "="
    };
    let sql = format!(
        "SELECT {PATH_EXPR} FROM entries e JOIN dirs d ON d.id = e.dir_id
         WHERE (d.path = ?1 OR (d.path >= ?2 AND d.path < ?3))
           AND e.name {op} ?4{} ORDER BY 1",
        type_clause(type_filter, 5),
    );
    let d = type_filter.map(|d| d as i32);
    match &d {
        Some(d) => stream_paths(
            conn,
            &sql,
            &[&dir, &lo, &hi, &name_pat, d],
            limit,
            &mut on_result,
        ),
        None => stream_paths(
            conn,
            &sql,
            &[&dir, &lo, &hi, &name_pat],
            limit,
            &mut on_result,
        ),
    }
}

/// Directory part of the literal prefix of a path glob ("/etc/*.conf" →
/// "/etc"), used to narrow the scan to that subtree.
fn literal_dir_prefix(pattern: &str) -> &str {
    let lit = pattern
        .find(['*', '?', '['])
        .map_or(pattern, |i| &pattern[..i]);
    lit.rfind('/').map_or("", |i| &lit[..i])
}

/// Entries whose full path matches the GLOB `pattern`.
pub fn search_by_path_glob_streaming(
    conn: &Connection,
    pattern: &str,
    type_filter: Option<bool>,
    limit: Option<usize>,
    mut on_result: impl FnMut(&str) -> bool,
) -> Result<u64> {
    let dir = literal_dir_prefix(pattern);
    let (lo, hi) = subtree_range(dir);
    let sql = format!(
        "SELECT {PATH_EXPR} FROM entries e JOIN dirs d ON d.id = e.dir_id
         WHERE (d.path = ?2 OR (d.path >= ?3 AND d.path < ?4))
           AND {PATH_EXPR} GLOB ?1{} ORDER BY 1",
        type_clause(type_filter, 5),
    );
    let d = type_filter.map(|d| d as i32);
    match &d {
        Some(d) => stream_paths(
            conn,
            &sql,
            &[&pattern, &dir, &lo, &hi, d],
            limit,
            &mut on_result,
        ),
        None => stream_paths(
            conn,
            &sql,
            &[&pattern, &dir, &lo, &hi],
            limit,
            &mut on_result,
        ),
    }
}

/// Regex search using SQLite REGEXP (user-defined function registered here),
/// optionally restricted to the subtree under directory `under`.
pub fn search_by_regex_streaming(
    conn: &Connection,
    pattern: &str,
    field: &str, // "basename" or "full_path"
    under: Option<&str>,
    type_filter: Option<bool>,
    limit: Option<usize>,
    mut on_result: impl FnMut(&str) -> bool,
) -> Result<u64> {
    register_regexp(conn)?;
    let target = if field == "full_path" {
        PATH_EXPR
    } else {
        "e.name"
    };
    let dir = under.map(norm_dir).unwrap_or("");
    let (lo, hi) = subtree_range(dir);
    let sql = format!(
        "SELECT {PATH_EXPR} FROM entries e JOIN dirs d ON d.id = e.dir_id
         WHERE (d.path = ?2 OR (d.path >= ?3 AND d.path < ?4))
           AND {target} REGEXP ?1{} ORDER BY 1",
        type_clause(type_filter, 5),
    );
    let d = type_filter.map(|d| d as i32);
    match &d {
        Some(d) => stream_paths(
            conn,
            &sql,
            &[&pattern, &dir, &lo, &hi, d],
            limit,
            &mut on_result,
        ),
        None => stream_paths(
            conn,
            &sql,
            &[&pattern, &dir, &lo, &hi],
            limit,
            &mut on_result,
        ),
    }
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
// Recent files — by mtime
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

    let sql = format!(
        "SELECT {PATH_EXPR}, e.mtime_sec, e.mtime_nsec, e.is_dir
         FROM entries e JOIN dirs d ON d.id = e.dir_id
         WHERE e.mtime_sec BETWEEN ?1 AND ?2{}
         ORDER BY e.mtime_sec DESC, e.mtime_nsec DESC",
        type_clause(type_filter, 3),
    );
    let mut stmt = conn.prepare_cached(&sql)?;
    let mut rows = match type_filter {
        Some(d) => stmt.query(params![from_sec, to_sec, d as i32])?,
        None => stmt.query(params![from_sec, to_sec])?,
    };
    while let Some(row) = rows.next()? {
        if count as usize >= limit_val {
            break;
        }
        let entry = RecentEntry {
            full_path: row.get(0)?,
            mtime_sec: row.get(1)?,
            mtime_nsec: row.get(2)?,
            is_dir: row.get::<_, i32>(3)? != 0,
        };
        if !on_result(&entry) {
            break;
        }
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
    let dir_count: i64 =
        conn.query_row("SELECT COUNT(*) FROM entries WHERE is_dir=1", [], |r| {
            r.get(0)
        })?;
    let file_count = entry_count - dir_count;
    let pending_events = pending_event_count(conn)?;

    let last_scan = get_metadata(conn, "last_scan_completed")?.and_then(|v| v.parse::<i64>().ok());
    let last_count = get_metadata(conn, "last_entry_count")?.and_then(|v| v.parse::<i64>().ok());
    let monitoring_mode =
        get_metadata(conn, "monitoring_mode")?.unwrap_or_else(|| "unknown".to_string());

    Ok(HealthInfo {
        entry_count,
        dir_count,
        file_count,
        last_scan_completed: last_scan,
        last_entry_count: last_count,
        monitoring_mode,
        pending_events,
        db_size_bytes: db_size_bytes(db_path),
    })
}

/// Database file plus its WAL.
fn db_size_bytes(db_path: &Path) -> u64 {
    let size = |p: &Path| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
    size(db_path) + size(&db_path.with_extension("db-wal"))
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
    let dir_count: i64 =
        conn.query_row("SELECT COUNT(*) FROM entries WHERE is_dir=1", [], |r| {
            r.get(0)
        })?;
    let file_count = entry_count - dir_count;

    // Top 10 extensions by count.
    let mut ext_stmt = conn.prepare(
        "SELECT
            CASE
                WHEN INSTR(name, '.') > 0
                THEN LOWER(SUBSTR(name, INSTR(name, '.')+1))
                ELSE ''
            END as ext,
            COUNT(*) as cnt
         FROM entries
         WHERE is_dir=0 AND name LIKE '%.%'
         GROUP BY ext
         ORDER BY cnt DESC
         LIMIT 10",
    )?;
    let top_extensions: Vec<ExtStat> = ext_stmt
        .query_map([], |r| {
            Ok(ExtStat {
                ext: r.get(0)?,
                count: r.get(1)?,
            })
        })?
        .flatten()
        .collect();

    // Top 10 largest directories by direct child count.
    let mut dir_stmt = conn.prepare(
        "SELECT CASE WHEN d.path = '' THEN '/' ELSE d.path END, c.cnt
         FROM (SELECT dir_id, COUNT(*) AS cnt FROM entries
               GROUP BY dir_id ORDER BY cnt DESC LIMIT 10) c
         JOIN dirs d ON d.id = c.dir_id
         ORDER BY c.cnt DESC",
    )?;
    let largest_dirs: Vec<DirStat> = dir_stmt
        .query_map([], |r| {
            Ok(DirStat {
                path: r.get(0)?,
                count: r.get(1)?,
            })
        })?
        .flatten()
        .collect();

    let index_age_secs = get_metadata(conn, "last_scan_completed")?
        .and_then(|v| v.parse::<i64>().ok())
        .map(|t| {
            std::time::SystemTime::now()
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64
                - t
        });

    Ok(IndexStats {
        entry_count,
        dir_count,
        file_count,
        top_extensions,
        largest_dirs,
        db_size_bytes: db_size_bytes(db_path),
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
    pub fn parse(s: &str) -> Option<Self> {
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

/// Load up to `limit` unapplied events, oldest first.
pub fn load_unapplied_events(conn: &Connection, limit: usize) -> Result<Vec<PendingEvent>> {
    let mut stmt = conn.prepare_cached(
        "SELECT id,event_type,path,new_path,is_dir,timestamp
         FROM pending_events WHERE applied=0 ORDER BY id ASC LIMIT ?1",
    )?;
    let rows = stmt.query_map(params![limit as i64], |r| {
        let et: String = r.get(1)?;
        Ok(PendingEvent {
            id: r.get(0)?,
            event_type: EventType::parse(&et).unwrap_or(EventType::Modify),
            path: r.get(2)?,
            new_path: r.get(3)?,
            is_dir: r.get::<_, i32>(4)? != 0,
            timestamp: r.get(5)?,
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
        [],
        |r| r.get(0),
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
        params![key],
        |r| r.get(0),
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
        let path = std::env::temp_dir().join(format!(
            "rfind-db-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_file(&path);
        open(&path).unwrap()
    }

    fn add(c: &Connection, p: &str, is_dir: bool, scan_gen: Option<i64>) {
        insert_entry(c, p, is_dir, 0, 0, scan_gen).unwrap();
    }

    fn paths(c: &Connection) -> Vec<String> {
        let mut s = c
            .prepare(&format!(
                "SELECT {PATH_EXPR} FROM entries e JOIN dirs d ON d.id = e.dir_id ORDER BY 1"
            ))
            .unwrap();
        s.query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    }

    fn glob(c: &Connection, pat: &str) -> Vec<String> {
        let mut v = Vec::new();
        search_by_glob_streaming(c, pat, None, None, |p| {
            v.push(p.to_string());
            true
        })
        .unwrap();
        v
    }

    #[test]
    fn insert_is_idempotent() {
        let c = mem();
        add(&c, "/a/x", false, None);
        add(&c, "/a/x", false, None);
        assert_eq!(entry_count(&c).unwrap(), 1);
    }

    #[test]
    fn paths_round_trip_including_root_children() {
        let c = mem();
        for p in ["/etc", "/etc/passwd", "/a b/c[1]/d.txt"] {
            add(&c, p, false, None);
        }
        assert_eq!(paths(&c), vec!["/a b/c[1]/d.txt", "/etc", "/etc/passwd"]);
        assert!(entry_exists(&c, "/etc/passwd").unwrap());
        assert!(!entry_exists(&c, "/etc/shadow").unwrap());
    }

    #[test]
    fn delete_under_does_not_touch_sibling_prefix() {
        let c = mem();
        for p in ["/a/b", "/a/b/c", "/a/b/c/d", "/a/bc", "/a/b.txt"] {
            add(&c, p, false, None);
        }
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
        for p in ["/a/d", "/a/d/f", "/a/d/s/g", "/a/dd/g", "/b/d", "/b/d/old"] {
            add(&c, p, false, None);
        }
        assert_eq!(rename_entry(&c, "/a/d", "/b/d").unwrap(), 3);
        assert_eq!(paths(&c), vec!["/a/dd/g", "/b/d", "/b/d/f", "/b/d/s/g"]);
        // Searches resolve the new paths.
        assert_eq!(glob(&c, "g"), vec!["/a/dd/g", "/b/d/s/g"]);
    }

    #[test]
    fn rename_of_unindexed_source_is_a_no_op() {
        let c = mem();
        add(&c, "/b/x", false, None);
        assert_eq!(rename_entry(&c, "/nowhere/y", "/b/x").unwrap(), 0);
        assert_eq!(paths(&c), vec!["/b/x"]);
    }

    #[test]
    fn sweep_removes_unseen_rows_but_keeps_nested_roots() {
        let c = mem();
        add(&c, "/etc/old", false, Some(1));
        add(&c, "/etc/new", false, Some(2));
        add(&c, "/mnt/c", true, Some(1));
        add(&c, "/mnt/c/win", false, Some(1));
        add(&c, "/mnt/cx", false, Some(1));
        sweep_stale(&c, "/", &["/mnt/c".to_string()], 2).unwrap();
        assert_eq!(paths(&c), vec!["/etc/new", "/mnt/c", "/mnt/c/win"]);
        // Directory rows left without entries are dropped: only the parents
        // of surviving entries remain ('' never held an entry here).
        let mut s = c.prepare("SELECT path FROM dirs ORDER BY path").unwrap();
        let dirs: Vec<String> = s
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(dirs, vec!["/etc", "/mnt", "/mnt/c"]);
    }

    #[test]
    fn live_upsert_keeps_scan_gen() {
        let c = mem();
        add(&c, "/x", false, Some(5));
        add(&c, "/x", false, None);
        let g: i64 = c
            .query_row("SELECT scan_gen FROM entries", [], |r| r.get(0))
            .unwrap();
        assert_eq!(g, 5);
    }

    #[test]
    fn search_under_and_path_glob_respect_subtree() {
        let c = mem();
        for p in [
            "/etc/a.conf",
            "/etc/x/b.conf",
            "/etcfoo/c.conf",
            "/usr/d.conf",
        ] {
            add(&c, p, false, None);
        }
        let mut v = Vec::new();
        search_under_streaming(&c, "/etc", Some("*.conf"), None, None, |p| {
            v.push(p.to_string());
            true
        })
        .unwrap();
        assert_eq!(v, vec!["/etc/a.conf", "/etc/x/b.conf"]);
        let mut v = Vec::new();
        search_by_path_glob_streaming(&c, "/etc/*.conf", None, None, |p| {
            v.push(p.to_string());
            true
        })
        .unwrap();
        assert_eq!(v, vec!["/etc/a.conf", "/etc/x/b.conf"]);
    }

    #[test]
    fn regex_matches_basename_or_full_path() {
        let c = mem();
        for p in ["/lib/libz.so.1", "/lib/libz.so", "/opt/lib/x"] {
            add(&c, p, false, None);
        }
        let mut v = Vec::new();
        search_by_regex_streaming(
            &c,
            r"^lib.*\.so\.[0-9]+$",
            "basename",
            None,
            None,
            None,
            |p| {
                v.push(p.to_string());
                true
            },
        )
        .unwrap();
        assert_eq!(v, vec!["/lib/libz.so.1"]);
        let mut v = Vec::new();
        search_by_regex_streaming(&c, "^/opt/", "full_path", None, None, None, |p| {
            v.push(p.to_string());
            true
        })
        .unwrap();
        assert_eq!(v, vec!["/opt/lib/x"]);
        // --under restricts the subtree; the regex still applies to names.
        let mut v = Vec::new();
        search_by_regex_streaming(&c, "^lib", "basename", Some("/opt"), None, None, |p| {
            v.push(p.to_string());
            true
        })
        .unwrap();
        assert!(v.is_empty());
        search_by_regex_streaming(&c, "^x$", "basename", Some("/opt"), None, None, |p| {
            v.push(p.to_string());
            true
        })
        .unwrap();
        assert_eq!(v, vec!["/opt/lib/x"]);
    }

    #[test]
    fn largest_dirs_counts_direct_children() {
        let c = mem();
        for p in ["/a/1", "/a/2", "/a/3", "/b/1"] {
            add(&c, p, false, None);
        }
        let s = get_stats(&c, Path::new("/nonexistent")).unwrap();
        assert_eq!(
            (s.largest_dirs[0].path.as_str(), s.largest_dirs[0].count),
            ("/a", 3)
        );
    }

    #[test]
    fn old_schema_is_dropped_and_rebuilt() {
        let path = std::env::temp_dir().join(format!("rfind-old-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        {
            let c = Connection::open(&path).unwrap();
            c.execute_batch(
                "
                CREATE TABLE entries (id INTEGER PRIMARY KEY, basename TEXT, full_path TEXT);
                INSERT INTO entries (basename, full_path) VALUES ('x','/x');
                CREATE TABLE scan_metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                INSERT INTO scan_metadata VALUES ('last_scan_completed', '1');
            ",
            )
            .unwrap();
        }
        let c = open(&path).unwrap();
        assert_eq!(entry_count(&c).unwrap(), 0);
        assert_eq!(get_metadata(&c, "last_scan_completed").unwrap(), None);
        let _ = std::fs::remove_file(&path);
    }
}
