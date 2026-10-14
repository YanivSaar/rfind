//! SQLite index: schema, insertion, search, streaming, health, stats,
//! and event journal.
//!
//! # Schema version 6 (compact)
//! Nothing is stored twice:
//! - `names`: every distinct file or directory name, once. Looked up by
//!   an 8-byte hash of the name (indexing the text would store it again).
//! - `dirs`: the directory tree as (parent, name) pairs. The root is id 1.
//!   A row exists only for directories that contain indexed entries (or
//!   lead to them), so paths can be built from it.
//! - `entries`: (dir_id, name_id) plus metadata, clustered on that key
//!   (WITHOUT ROWID), so the table is its own unique index.
//!
//! A full path is built in Rust from the `dirs` chain ([`PathCache`]).
//! Renaming a directory updates one `dirs` row; nothing beneath it moves.
//! Subtrees are found by walking `dirs` children (a recursive CTE), so
//! paths containing `*?[]` are literal and `/a/b` never matches `/a/bc`.
//!
//! The index is a cache of the filesystem: an older schema is dropped and
//! rebuilt by a full scan rather than migrated.

use rusqlite::{params, Connection, OptionalExtension, Result};
use std::{collections::HashMap, path::Path};

const SCHEMA_VERSION: i64 = 6;

/// `dirs.id` of the root directory `/`.
const ROOT: i64 = 1;

/// Threshold of the partial index `idx_entries_big` (1 MiB).
const BIG_FILE: u64 = 1 << 20;

// ---------------------------------------------------------------------------
// Open / schema
// ---------------------------------------------------------------------------

/// Open (creating if needed) the index for the daemon, compressed with
/// [`rfind_zvfs`].
pub fn open(path: &Path) -> Result<Connection> {
    open_with(path, true)
}

/// Open the index for the daemon. `compress`: store pages zstd-compressed
/// (about 3× smaller). An index in the other storage format is deleted
/// first; the daemon then rebuilds it with a full scan.
pub fn open_with(path: &Path, compress: bool) -> Result<Connection> {
    let compressed_now = rfind_zvfs::is_compressed(path);
    let plain_now = std::fs::metadata(path).is_ok_and(|m| m.len() > 0);
    if (compress && plain_now) || (!compress && compressed_now) {
        remove_index_files(path);
    }
    let conn = if compress {
        rfind_zvfs::open(path, rusqlite::OpenFlags::default())?
    } else {
        Connection::open(path)?
    };
    conn.execute_batch("PRAGMA journal_mode=WAL;")?;
    conn.execute_batch("PRAGMA synchronous=NORMAL;")?;
    // Checkpoint every ~40 MB of WAL instead of 4 MB: index pages that bulk
    // scans rewrite many times are written to the database (and compressed)
    // once per checkpoint rather than once per small batch.
    conn.execute_batch("PRAGMA wal_autocheckpoint=10000;")?;
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
            DROP TABLE IF EXISTS names;
            DROP TABLE IF EXISTS pending_events;
            DROP TABLE IF EXISTS scan_metadata;
            COMMIT;
            VACUUM;
        ",
        )?;
    }

    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS names (
            id   INTEGER PRIMARY KEY,
            name TEXT    NOT NULL,
            hash INTEGER NOT NULL          -- name_hash(name)
        );
        CREATE INDEX IF NOT EXISTS idx_names_hash ON names (hash);

        CREATE TABLE IF NOT EXISTS dirs (
            id      INTEGER PRIMARY KEY,
            parent  INTEGER,               -- NULL only for the root (id 1)
            name_id INTEGER
        );
        CREATE UNIQUE INDEX IF NOT EXISTS idx_dirs_parent_name ON dirs (parent, name_id);
        INSERT OR IGNORE INTO dirs (id, parent, name_id) VALUES (1, NULL, NULL);

        CREATE TABLE IF NOT EXISTS entries (
            dir_id     INTEGER NOT NULL,   -- dirs.id of the parent directory
            name_id    INTEGER NOT NULL,
            is_dir     INTEGER NOT NULL DEFAULT 0,
            mtime_sec  INTEGER NOT NULL DEFAULT 0,
            mtime_nsec INTEGER NOT NULL DEFAULT 0,
            size       INTEGER NOT NULL DEFAULT 0,  -- bytes; 0 for directories
            scan_gen   INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY (dir_id, name_id)
        ) WITHOUT ROWID;
        CREATE INDEX IF NOT EXISTS idx_entries_name  ON entries (name_id);
        CREATE INDEX IF NOT EXISTS idx_entries_mtime ON entries (mtime_sec);
        -- Partial: only files of 1 MiB+ (a few percent of entries), so
        -- `largest` and large --larger-than filters are index walks.
        CREATE INDEX IF NOT EXISTS idx_entries_big
            ON entries (size) WHERE size >= 1048576;

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

/// After bulk writes: checkpoint the WAL and let a compressed store drop
/// old page versions.
pub fn compact(conn: &Connection) -> Result<()> {
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
    rfind_zvfs::compact(conn)
}

/// Delete an index in either storage format (database, WAL, shared
/// memory, compressed store).
pub fn remove_index_files(path: &Path) {
    let with = |suffix: &str| std::path::PathBuf::from(format!("{}{suffix}", path.display()));
    for p in [
        path.to_path_buf(),
        with("-wal"),
        with("-shm"),
        with("-journal"),
    ]
    .into_iter()
    .chain(rfind_zvfs::store_files(path))
    {
        let _ = std::fs::remove_file(p);
    }
}

/// Open the index for reading only (rfind). Never creates, migrates or
/// drops anything: only rfindd owns the schema. An index in another
/// format (e.g. right after an upgrade, before rfindd has rebuilt it) is
/// reported instead of touched.
pub fn open_readonly(path: &Path) -> std::result::Result<Connection, String> {
    use rusqlite::OpenFlags;
    if !path.exists() {
        return Err(format!(
            "no index at {} (is rfindd installed and running?)",
            path.display()
        ));
    }
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let conn = if rfind_zvfs::is_compressed(path) {
        rfind_zvfs::open(path, flags)
    } else {
        Connection::open_with_flags(path, flags)
    }
    .map_err(|e| format!("cannot open index {}: {e}", path.display()))?;
    // Room for the pages one search touches: with a compressed index each
    // page is then decompressed once per search, not once per visit.
    let _ = conn.execute_batch("PRAGMA cache_size=-65536;");
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .map_err(|e| format!("cannot read index {}: {e}", path.display()))?;
    if version != SCHEMA_VERSION {
        return Err(format!(
            "the index is format v{version}, this rfind expects v{SCHEMA_VERSION}; \
             rfindd rebuilds it on its next start (sudo systemctl restart rfindd)"
        ));
    }
    Ok(conn)
}

// ---------------------------------------------------------------------------
// Names, directories and paths
// ---------------------------------------------------------------------------

/// FNV-1a 64-bit hash of a name, as stored in `names.hash`. Collisions are
/// harmless: lookups also compare the name.
fn name_hash(name: &str) -> i64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in name.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h as i64
}

fn find_name(conn: &Connection, name: &str) -> Result<Option<i64>> {
    conn.prepare_cached("SELECT id FROM names WHERE hash = ?1 AND name = ?2")?
        .query_row(params![name_hash(name), name], |r| r.get(0))
        .optional()
}

fn name_or_create(conn: &Connection, name: &str) -> Result<i64> {
    if let Some(id) = find_name(conn, name)? {
        return Ok(id);
    }
    conn.prepare_cached("INSERT INTO names (name, hash) VALUES (?1, ?2)")?
        .execute(params![name, name_hash(name)])?;
    Ok(conn.last_insert_rowid())
}

fn child_dir(conn: &Connection, parent: i64, name_id: i64) -> Result<Option<i64>> {
    conn.prepare_cached("SELECT id FROM dirs WHERE parent = ?1 AND name_id = ?2")?
        .query_row(params![parent, name_id], |r| r.get(0))
        .optional()
}

/// Path components of a directory path ("/a/b/" → ["a", "b"]).
fn components(path: &str) -> impl Iterator<Item = &str> {
    path.split('/').filter(|c| !c.is_empty())
}

/// `dirs.id` of the directory at `path`, if it has a row.
fn find_dir(conn: &Connection, path: &str) -> Result<Option<i64>> {
    let mut id = ROOT;
    for comp in components(path) {
        let Some(nid) = find_name(conn, comp)? else {
            return Ok(None);
        };
        match child_dir(conn, id, nid)? {
            Some(c) => id = c,
            None => return Ok(None),
        }
    }
    Ok(Some(id))
}

/// Split a full path into (parent directory, name).
fn split_path(path: &str) -> (&str, &str) {
    let p = path.trim_end_matches('/');
    match p.rfind('/') {
        Some(i) => (&p[..i], &p[i + 1..]),
        None => ("", p),
    }
}

/// Builds full paths from `dirs` ids, remembering every directory it has
/// resolved. The root is "", so `dir + "/" + name` needs no special case.
#[derive(Default)]
pub struct PathCache {
    dirs: HashMap<i64, String>,
}

impl PathCache {
    pub fn dir(&mut self, conn: &Connection, id: i64) -> Result<String> {
        if id == ROOT {
            return Ok(String::new());
        }
        if let Some(p) = self.dirs.get(&id) {
            return Ok(p.clone());
        }
        // Walk up to the first cached ancestor, then build back down.
        let mut chain: Vec<(i64, String)> = Vec::new();
        let mut cur = id;
        let base = loop {
            if cur == ROOT {
                break String::new();
            }
            if let Some(p) = self.dirs.get(&cur) {
                break p.clone();
            }
            let (parent, name): (Option<i64>, String) = conn
                .prepare_cached(
                    "SELECT d.parent, n.name FROM dirs d JOIN names n ON n.id = d.name_id
                     WHERE d.id = ?1",
                )?
                .query_row(params![cur], |r| Ok((r.get(0)?, r.get(1)?)))?;
            chain.push((cur, name));
            cur = parent.unwrap_or(ROOT);
        };
        let mut path = base;
        for (did, name) in chain.into_iter().rev() {
            path.push('/');
            path.push_str(&name);
            self.dirs.insert(did, path.clone());
        }
        Ok(path)
    }

    pub fn full(&mut self, conn: &Connection, dir_id: i64, name: &str) -> Result<String> {
        let mut p = self.dir(conn, dir_id)?;
        p.push('/');
        p.push_str(name);
        Ok(p)
    }
}

/// `WITH RECURSIVE sub(id)`: directory `?1` and every directory below it.
const SUBTREE: &str =
    "WITH RECURSIVE sub(id) AS (SELECT ?1 UNION ALL SELECT d.id FROM dirs d JOIN sub ON d.parent = sub.id) ";

// ---------------------------------------------------------------------------
// Entry operations
// ---------------------------------------------------------------------------

/// Batch inserter that remembers directory and name ids, so the many
/// entries of one directory and the many repeated names resolve once. Use
/// within a single transaction: ids can change once rows are deleted.
pub struct Inserter<'c> {
    conn: &'c Connection,
    dirs: HashMap<String, i64>,
    names: HashMap<String, i64>,
    /// Names already indexed in the current directory, preloaded in one
    /// ordered read: a rescan then finds almost every name here instead of
    /// a random hash-index probe per entry.
    dir_names: Option<(i64, HashMap<String, i64>)>,
}

/// Names remembered per Inserter (they repeat: index.js, __init__.py, ...).
const NAME_CACHE: usize = 100_000;

impl<'c> Inserter<'c> {
    pub fn new(conn: &'c Connection) -> Self {
        Self {
            conn,
            dirs: HashMap::new(),
            names: HashMap::new(),
            dir_names: None,
        }
    }

    fn name(&mut self, name: &str) -> Result<i64> {
        if let Some(id) = self.names.get(name) {
            return Ok(*id);
        }
        let id = name_or_create(self.conn, name)?;
        if self.names.len() < NAME_CACHE {
            self.names.insert(name.to_string(), id);
        }
        Ok(id)
    }

    /// `dirs.id` of `path`, creating it and missing ancestors.
    fn dir(&mut self, path: &str) -> Result<i64> {
        let path = path.trim_end_matches('/');
        if path.is_empty() {
            return Ok(ROOT);
        }
        if let Some(id) = self.dirs.get(path) {
            return Ok(*id);
        }
        let (parent, name) = split_path(path);
        let pid = self.dir(parent)?;
        let nid = self.name(name)?;
        let id = match child_dir(self.conn, pid, nid)? {
            Some(id) => id,
            None => {
                self.conn
                    .prepare_cached("INSERT INTO dirs (parent, name_id) VALUES (?1, ?2)")?
                    .execute(params![pid, nid])?;
                self.conn.last_insert_rowid()
            }
        };
        self.dirs.insert(path.to_string(), id);
        Ok(id)
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
        size: u64,
        scan_gen: Option<i64>,
    ) -> Result<()> {
        let (dir, name) = split_path(full_path);
        if name.is_empty() {
            return Ok(()); // the root itself is not an entry
        }
        let did = self.dir(dir)?;
        if self.dir_names.as_ref().is_none_or(|(d, _)| *d != did) {
            let mut known = HashMap::new();
            let mut stmt = self.conn.prepare_cached(
                "SELECT n.name, e.name_id FROM entries e JOIN names n ON n.id = e.name_id
                 WHERE e.dir_id = ?1",
            )?;
            let mut rows = stmt.query(params![did])?;
            while let Some(r) = rows.next()? {
                known.insert(r.get(0)?, r.get(1)?);
            }
            self.dir_names = Some((did, known));
        }
        let nid = match self.dir_names.as_ref().and_then(|(_, m)| m.get(name)) {
            Some(id) => *id,
            None => self.name(name)?,
        };
        self.conn
            .prepare_cached(
                "INSERT INTO entries (dir_id, name_id, is_dir, mtime_sec, mtime_nsec, size, scan_gen)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, COALESCE(?7, 0))
                 ON CONFLICT(dir_id, name_id) DO UPDATE SET
                     is_dir     = excluded.is_dir,
                     mtime_sec  = excluded.mtime_sec,
                     mtime_nsec = excluded.mtime_nsec,
                     size       = excluded.size,
                     scan_gen   = COALESCE(?7, scan_gen)",
            )?
            .execute(params![
                did,
                nid,
                is_dir as i32,
                mtime_sec,
                mtime_nsec,
                if is_dir { 0 } else { size as i64 },
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
    size: u64,
    scan_gen: Option<i64>,
) -> Result<()> {
    Inserter::new(conn).insert(full_path, is_dir, mtime_sec, mtime_nsec, size, scan_gen)
}

/// (parent dir id, name id) of an indexed path, if both exist.
fn locate(conn: &Connection, path: &str) -> Result<Option<(i64, i64)>> {
    let (dir, name) = split_path(path);
    if name.is_empty() {
        return Ok(None);
    }
    let Some(did) = find_dir(conn, dir)? else {
        return Ok(None);
    };
    Ok(find_name(conn, name)?.map(|nid| (did, nid)))
}

pub fn delete_entry_by_path(conn: &Connection, path: &str) -> Result<usize> {
    match locate(conn, path)? {
        Some((did, nid)) => conn.execute(
            "DELETE FROM entries WHERE dir_id = ?1 AND name_id = ?2",
            params![did, nid],
        ),
        None => Ok(0),
    }
}

/// Delete `path` itself and everything beneath it.
pub fn delete_entries_under(conn: &Connection, path: &str) -> Result<usize> {
    let Some((did, nid)) = locate(conn, path)? else {
        return Ok(0);
    };
    let mut n = conn.execute(
        "DELETE FROM entries WHERE dir_id = ?1 AND name_id = ?2",
        params![did, nid],
    )?;
    if let Some(sub) = child_dir(conn, did, nid)? {
        n += conn.execute(
            &format!("{SUBTREE} DELETE FROM entries WHERE dir_id IN sub"),
            params![sub],
        )?;
        conn.execute(
            &format!("{SUBTREE} DELETE FROM dirs WHERE id IN sub"),
            params![sub],
        )?;
    }
    Ok(n)
}

/// Move `old` (and its subtree) to `new`. Anything already indexed at
/// `new` is replaced, matching rename(2) semantics. Returns the number of
/// entries moved (0 if `old` was not indexed).
pub fn rename_entry(conn: &Connection, old: &str, new: &str) -> Result<usize> {
    let (old, new) = (old.trim_end_matches('/'), new.trim_end_matches('/'));
    if old == new {
        return Ok(0);
    }
    let Some((old_did, old_nid)) = locate(conn, old)? else {
        return Ok(0);
    };
    let exists: bool = conn
        .prepare_cached("SELECT 1 FROM entries WHERE dir_id = ?1 AND name_id = ?2")?
        .exists(params![old_did, old_nid])?;
    if !exists {
        return Ok(0);
    }
    delete_entries_under(conn, new)?;
    let (new_dir, new_name) = split_path(new);
    let mut ins = Inserter::new(conn);
    let new_did = ins.dir(new_dir)?;
    let new_nid = ins.name(new_name)?;
    conn.execute(
        "UPDATE entries SET dir_id = ?1, name_id = ?2 WHERE dir_id = ?3 AND name_id = ?4",
        params![new_did, new_nid, old_did, old_nid],
    )?;
    let mut moved = 1;
    // A directory: re-parent its row; everything beneath follows by id.
    if let Some(d) = child_dir(conn, old_did, old_nid)? {
        conn.execute(
            "UPDATE dirs SET parent = ?1, name_id = ?2 WHERE id = ?3",
            params![new_did, new_nid, d],
        )?;
        moved += conn.query_row(
            &format!("{SUBTREE} SELECT COUNT(*) FROM entries WHERE dir_id IN sub"),
            params![d],
            |r| r.get::<_, i64>(0),
        )? as usize;
    }
    Ok(moved)
}

/// Drop `dirs` rows that no longer hold entries or lead to any, and
/// names nothing refers to. Run after bulk deletes (sweeps).
fn collect_garbage(conn: &Connection) -> Result<()> {
    loop {
        let n = conn.execute(
            "DELETE FROM dirs WHERE id <> 1
               AND id NOT IN (SELECT DISTINCT dir_id FROM entries)
               AND id NOT IN (SELECT parent FROM dirs WHERE parent IS NOT NULL)",
            [],
        )?;
        if n == 0 {
            break;
        }
    }
    conn.execute(
        "DELETE FROM names
         WHERE id NOT IN (SELECT name_id FROM entries)
           AND id NOT IN (SELECT name_id FROM dirs WHERE name_id IS NOT NULL)",
        [],
    )?;
    Ok(())
}

/// Integer list for an SQL `IN (...)`, from typed ids (injection-safe).
fn id_list(ids: &[i64]) -> String {
    if ids.is_empty() {
        // Not NULL: `x NOT IN (NULL)` is never true. No row has id -1.
        "-1".to_string()
    } else {
        ids.iter()
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(",")
    }
}

/// Delete entries under `root` not seen by scan generation `scan_gen`,
/// leaving alone any subtree listed in `keep` (nested roots maintained by
/// their own scans), then drop rows left unused.
pub fn sweep_stale(conn: &Connection, root: &str, keep: &[String], scan_gen: i64) -> Result<usize> {
    let Some(rid) = find_dir(conn, root)? else {
        return Ok(0);
    };
    let mut keep_dirs = Vec::new();
    let mut keep_entries = Vec::new();
    for k in keep {
        if let Some(d) = find_dir(conn, k)? {
            keep_dirs.push(d);
        }
        if let Some((did, nid)) = locate(conn, k)? {
            keep_entries.push(format!("({did},{nid})"));
        }
    }
    let keep_entries = if keep_entries.is_empty() {
        String::new()
    } else {
        format!(
            " AND (dir_id, name_id) NOT IN (VALUES {})",
            keep_entries.join(",")
        )
    };
    let n = conn.execute(
        &format!(
            "WITH RECURSIVE sub(id) AS (SELECT ?1 UNION ALL SELECT d.id FROM dirs d
                 JOIN sub ON d.parent = sub.id WHERE d.id NOT IN ({}))
             DELETE FROM entries WHERE dir_id IN sub AND scan_gen < ?2{keep_entries}",
            id_list(&keep_dirs)
        ),
        params![rid, scan_gen],
    )?;
    if n > 0 {
        collect_garbage(conn)?;
    }
    Ok(n)
}

/// Delete entries that are not under any of `roots` (after roots were
/// removed from the config), then drop rows left unused.
pub fn delete_outside_roots(conn: &Connection, roots: &[String]) -> Result<usize> {
    let mut ids = Vec::new();
    for r in roots {
        if let Some(d) = find_dir(conn, r)? {
            ids.push(d);
        }
    }
    let n = conn.execute(
        &format!(
            "WITH RECURSIVE sub(id) AS (SELECT id FROM dirs WHERE id IN ({})
                 UNION ALL SELECT d.id FROM dirs d JOIN sub ON d.parent = sub.id)
             DELETE FROM entries WHERE dir_id NOT IN sub",
            id_list(&ids)
        ),
        [],
    )?;
    if n > 0 {
        collect_garbage(conn)?;
    }
    Ok(n)
}

pub fn entry_exists(conn: &Connection, path: &str) -> Result<bool> {
    match locate(conn, path)? {
        Some((did, nid)) => conn
            .prepare_cached("SELECT 1 FROM entries WHERE dir_id = ?1 AND name_id = ?2")?
            .exists(params![did, nid]),
        None => Ok(false),
    }
}

pub fn entry_count(conn: &Connection) -> Result<i64> {
    conn.query_row("SELECT COUNT(*) FROM entries", [], |r| r.get(0))
}

/// Full paths of every indexed directory, sorted.
pub fn get_all_dirs(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT e.dir_id, n.name FROM entries e JOIN names n ON n.id = e.name_id
         WHERE e.is_dir = 1",
    )?;
    let mut rows = stmt.query([])?;
    let mut paths = PathCache::default();
    let mut out = Vec::new();
    while let Some(r) = rows.next()? {
        out.push(paths.full(conn, r.get(0)?, r.get_ref(1)?.as_str()?)?);
    }
    out.sort();
    Ok(out)
}

// ---------------------------------------------------------------------------
// Search — streaming via callback
// ---------------------------------------------------------------------------

/// One search result.
#[derive(Debug, Clone, Copy)]
pub struct Hit<'a> {
    pub path: &'a str,
    pub is_dir: bool,
    pub mtime_sec: i64,
    /// Bytes; 0 for directories.
    pub size: u64,
}

/// Columns every search selects: dir_id, name, is_dir, mtime_sec, size.
const HIT_COLS: &str = "e.dir_id, n.name, e.is_dir, e.mtime_sec, e.size";

/// Result filters shared by all searches.
#[derive(Debug, Clone, Copy, Default)]
pub struct Filter {
    /// Some(true) = directories only, Some(false) = files only.
    pub is_dir: Option<bool>,
    /// Files of at least this many bytes (implies files only).
    pub min_size: Option<u64>,
    /// Files of at most this many bytes (implies files only).
    pub max_size: Option<u64>,
}

impl Filter {
    pub fn files() -> Self {
        Self {
            is_dir: Some(false),
            ..Self::default()
        }
    }

    /// Only files of at least BIG_FILE bytes match.
    fn big_only(&self) -> bool {
        self.min_size.is_some_and(|n| n >= BIG_FILE)
    }

    /// SQL conditions to append to a WHERE clause. Only integers from typed
    /// fields are inlined, so this is injection-safe.
    fn clause(&self) -> String {
        let mut c = String::new();
        let sized = self.min_size.is_some() || self.max_size.is_some();
        match (self.is_dir, sized) {
            (Some(true), _) => c.push_str(" AND e.is_dir = 1"),
            (Some(false), _) | (None, true) => c.push_str(" AND e.is_dir = 0"),
            (None, false) => {}
        }
        if let Some(n) = self.min_size {
            c.push_str(&format!(" AND e.size >= {}", n.min(i64::MAX as u64)));
            if n >= BIG_FILE {
                // The literal term of idx_entries_big, so it can be used.
                c.push_str(&format!(" AND e.size >= {BIG_FILE}"));
            }
        }
        if let Some(n) = self.max_size {
            c.push_str(&format!(" AND e.size <= {}", n.min(i64::MAX as u64)));
        }
        c
    }
}

/// `entries e`, forced onto the big-file partial index when the query is
/// restricted to files of 1 MiB+ (SQLite's planner otherwise walks
/// directories: 2.5 s instead of <1 ms at 10M entries). Only when the
/// index exists, since INDEXED BY fails otherwise.
fn entries_from(conn: &Connection, big_only: bool) -> &'static str {
    let has_index = || {
        conn.query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'index' AND name = 'idx_entries_big'",
            [],
            |_| Ok(()),
        )
        .is_ok()
    };
    if big_only && has_index() {
        "entries e INDEXED BY idx_entries_big"
    } else {
        "entries e"
    }
}

/// What a row filter decides beyond SQL (path globs, path regexes).
type RowCheck<'a> = Option<&'a dyn Fn(&str) -> bool>;

/// Note: name searches use `names n CROSS JOIN entries e` — CROSS JOIN fixes
/// the order. Left to itself SQLite scans every entry and looks its name up
/// (9.3 s at 10M entries) instead of filtering names first (0.8 s).
///
/// Run a query selecting [`HIT_COLS`], build each path, and stream hits to
/// `on_result` until it returns false or `limit` is reached.
///
/// `sort`: order by path. That means reading every match before returning
/// the first, so callers sort only when no limit is set; a limited search
/// returns the first matches found.
fn stream_hits(
    conn: &Connection,
    sql: &str,
    params: &[&dyn rusqlite::ToSql],
    limit: Option<usize>,
    sort: bool,
    check: RowCheck,
    on_result: &mut dyn FnMut(&Hit) -> bool,
) -> Result<u64> {
    let limit_val = limit.unwrap_or(usize::MAX);
    let mut paths = PathCache::default();
    let mut stmt = conn.prepare_cached(sql)?;
    let mut rows = stmt.query(params)?;
    let mut collected: Vec<(String, bool, i64, u64)> = Vec::new();
    let mut count = 0u64;
    while let Some(row) = rows.next()? {
        if !sort && count as usize >= limit_val {
            break;
        }
        let path = paths.full(conn, row.get(0)?, row.get_ref(1)?.as_str()?)?;
        if check.is_some_and(|f| !f(&path)) {
            continue;
        }
        let (is_dir, mtime_sec, size) = (
            row.get::<_, i32>(2)? != 0,
            row.get(3)?,
            row.get::<_, i64>(4)? as u64,
        );
        if sort {
            collected.push((path, is_dir, mtime_sec, size));
        } else {
            let hit = Hit {
                path: &path,
                is_dir,
                mtime_sec,
                size,
            };
            if !on_result(&hit) {
                break;
            }
            count += 1;
        }
    }
    if sort {
        collected.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        for (path, is_dir, mtime_sec, size) in collected.iter().take(limit_val) {
            let hit = Hit {
                path,
                is_dir: *is_dir,
                mtime_sec: *mtime_sec,
                size: *size,
            };
            if !on_result(&hit) {
                break;
            }
            count += 1;
        }
    }
    Ok(count)
}

fn is_glob(pattern: &str) -> bool {
    pattern.contains(['*', '?', '['])
}

/// Call `on_result` for each entry whose name matches `pattern` (exact, or
/// GLOB when it contains `*`, `?` or `[`). Return false from the callback
/// to stop early (e.g. --limit reached).
pub fn search_by_glob_streaming(
    conn: &Connection,
    pattern: &str,
    filter: &Filter,
    limit: Option<usize>,
    mut on_result: impl FnMut(&Hit) -> bool,
) -> Result<u64> {
    if !is_glob(pattern) {
        let Some(nid) = find_name(conn, pattern)? else {
            return Ok(0);
        };
        let sql = format!(
            "SELECT {HIT_COLS} FROM entries e JOIN names n ON n.id = e.name_id
             WHERE e.name_id = ?1{}",
            filter.clause()
        );
        return stream_hits(
            conn,
            &sql,
            &[&nid],
            limit,
            limit.is_none(),
            None,
            &mut on_result,
        );
    }
    let sql = if filter.big_only() {
        // Few files are this large: walk them, then test the name.
        format!(
            "SELECT {HIT_COLS} FROM {} JOIN names n ON n.id = e.name_id
             WHERE n.name GLOB ?1{}",
            entries_from(conn, true),
            filter.clause()
        )
    } else {
        // Scan the (smaller) names table, then the entries using each name.
        format!(
            "SELECT {HIT_COLS} FROM names n CROSS JOIN entries e ON e.name_id = n.id
             WHERE n.name GLOB ?1{}",
            filter.clause()
        )
    };
    stream_hits(
        conn,
        &sql,
        &[&pattern],
        limit,
        limit.is_none(),
        None,
        &mut on_result,
    )
}

/// Entries under directory `prefix` whose name matches `name_pattern`
/// (exact or GLOB; all names when `None`).
pub fn search_under_streaming(
    conn: &Connection,
    prefix: &str,
    name_pattern: Option<&str>,
    filter: &Filter,
    limit: Option<usize>,
    mut on_result: impl FnMut(&Hit) -> bool,
) -> Result<u64> {
    let Some(root) = find_dir(conn, prefix)? else {
        return Ok(0);
    };
    let pattern = name_pattern.unwrap_or("*");
    if root == ROOT {
        // Everything is under "/": skip materializing the whole tree.
        return search_by_glob_streaming(conn, pattern, filter, limit, on_result);
    }
    let (cond, arg): (&str, Box<dyn rusqlite::ToSql>) = if is_glob(pattern) {
        if pattern == "*" {
            ("", Box::new(0))
        } else {
            (" AND n.name GLOB ?2", Box::new(pattern.to_string()))
        }
    } else {
        match find_name(conn, pattern)? {
            Some(nid) => (" AND e.name_id = ?2", Box::new(nid)),
            None => return Ok(0),
        }
    };
    let sql = if filter.big_only() {
        format!(
            "{SUBTREE} SELECT {HIT_COLS} FROM {} JOIN names n ON n.id = e.name_id
             WHERE e.dir_id IN sub{cond}{}",
            entries_from(conn, true),
            filter.clause()
        )
    } else {
        format!(
            "{SUBTREE} SELECT {HIT_COLS} FROM sub JOIN entries e ON e.dir_id = sub.id
             JOIN names n ON n.id = e.name_id WHERE 1{cond}{}",
            filter.clause()
        )
    };
    if cond.is_empty() {
        stream_hits(
            conn,
            &sql,
            &[&root],
            limit,
            limit.is_none(),
            None,
            &mut on_result,
        )
    } else {
        stream_hits(
            conn,
            &sql,
            &[&root, &*arg],
            limit,
            limit.is_none(),
            None,
            &mut on_result,
        )
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

/// SQLite GLOB semantics: `*` any run (including '/'), `?` one character,
/// `[...]` a set with ranges, `[^...]` a negated set. Case-sensitive.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let (p, t): (Vec<char>, Vec<char>) = (pattern.chars().collect(), text.chars().collect());
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star, mut mark) = (None::<usize>, 0usize);
    // Length of the set at p[i] ("[...]"), and whether c matches it.
    let set = |i: usize, c: char| -> Option<(usize, bool)> {
        let mut j = i + 1;
        let negate = p.get(j) == Some(&'^');
        if negate {
            j += 1;
        }
        let mut matched = false;
        let mut first = true;
        while j < p.len() && (p[j] != ']' || first) {
            if j + 2 < p.len() && p[j + 1] == '-' && p[j + 2] != ']' {
                matched |= p[j] <= c && c <= p[j + 2];
                j += 3;
            } else {
                matched |= p[j] == c;
                j += 1;
            }
            first = false;
        }
        (j < p.len()).then_some((j + 1 - i, matched != negate))
    };
    while ti < t.len() {
        let step = match p.get(pi) {
            Some('*') => {
                star = Some(pi);
                mark = ti;
                pi += 1;
                continue;
            }
            Some('?') => Some(1),
            Some('[') => match set(pi, t[ti]) {
                Some((len, true)) => Some(len),
                Some((_, false)) => None,
                None => (t[ti] == '[').then_some(1),
            },
            Some(c) => (*c == t[ti]).then_some(1),
            None => None,
        };
        match step {
            Some(len) => {
                pi += len;
                ti += 1;
            }
            None => match star {
                Some(s) => {
                    pi = s + 1;
                    mark += 1;
                    ti = mark;
                }
                None => return false,
            },
        }
    }
    p[pi..].iter().all(|c| *c == '*')
}

/// Entries whose full path matches the GLOB `pattern`.
pub fn search_by_path_glob_streaming(
    conn: &Connection,
    pattern: &str,
    filter: &Filter,
    limit: Option<usize>,
    mut on_result: impl FnMut(&Hit) -> bool,
) -> Result<u64> {
    let Some(root) = find_dir(conn, literal_dir_prefix(pattern))? else {
        return Ok(0);
    };
    let sql = format!(
        "{SUBTREE} SELECT {HIT_COLS} FROM sub JOIN entries e ON e.dir_id = sub.id
         JOIN names n ON n.id = e.name_id WHERE 1{}",
        filter.clause()
    );
    let check = |path: &str| glob_match(pattern, path);
    stream_hits(
        conn,
        &sql,
        &[&root],
        limit,
        limit.is_none(),
        Some(&check),
        &mut on_result,
    )
}

/// Regex search, matched against names (`field` "basename") or full paths
/// ("full_path"), optionally restricted to the subtree under `under`.
pub fn search_by_regex_streaming(
    conn: &Connection,
    pattern: &str,
    field: &str,
    under: Option<&str>,
    filter: &Filter,
    limit: Option<usize>,
    mut on_result: impl FnMut(&Hit) -> bool,
) -> Result<u64> {
    let re = regex_lite::Regex::new(pattern)
        .map_err(|e| rusqlite::Error::UserFunctionError(e.into()))?;
    let root = match under {
        Some(u) => match find_dir(conn, u)? {
            Some(d) => Some(d),
            None => return Ok(0),
        },
        None => None,
    };
    let by_path = field == "full_path";
    let check = |path: &str| re.is_match(path);
    let check: RowCheck = if by_path { Some(&check) } else { None };
    let sql = match (root, by_path) {
        // Names: test each distinct name once, in SQL.
        (None, false) => {
            register_regexp(conn)?;
            format!(
                "SELECT {HIT_COLS} FROM names n CROSS JOIN entries e ON e.name_id = n.id
                 WHERE n.name REGEXP ?1{}",
                filter.clause()
            )
        }
        (Some(_), false) => {
            register_regexp(conn)?;
            format!(
                "{SUBTREE} SELECT {HIT_COLS} FROM sub JOIN entries e ON e.dir_id = sub.id
                 JOIN names n ON n.id = e.name_id WHERE n.name REGEXP ?2{}",
                filter.clause()
            )
        }
        (None, true) => format!(
            "SELECT {HIT_COLS} FROM entries e JOIN names n ON n.id = e.name_id WHERE 1{}",
            filter.clause()
        ),
        (Some(_), true) => format!(
            "{SUBTREE} SELECT {HIT_COLS} FROM sub JOIN entries e ON e.dir_id = sub.id
             JOIN names n ON n.id = e.name_id WHERE 1{}",
            filter.clause()
        ),
    };
    let sort = limit.is_none();
    match (root, by_path) {
        (None, false) => stream_hits(conn, &sql, &[&pattern], limit, sort, None, &mut on_result),
        (Some(r), false) => stream_hits(
            conn,
            &sql,
            &[&r, &pattern],
            limit,
            sort,
            None,
            &mut on_result,
        ),
        (None, true) => stream_hits(conn, &sql, &[], limit, sort, check, &mut on_result),
        (Some(r), true) => stream_hits(conn, &sql, &[&r], limit, sort, check, &mut on_result),
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
    pub size: u64,
}

/// Return entries with mtime in [from_sec, to_sec], newest first.
pub fn search_recent_streaming(
    conn: &Connection,
    from_sec: i64,
    to_sec: i64,
    filter: &Filter,
    limit: Option<usize>,
    mut on_result: impl FnMut(&RecentEntry) -> bool,
) -> Result<u64> {
    let limit_val = limit.unwrap_or(usize::MAX);
    let mut count = 0u64;
    let sql = format!(
        // Sort and limit the entries first, then join names for the
        // survivors only (not for every candidate in the time window).
        "SELECT e.dir_id, n.name, e.mtime_sec, e.mtime_nsec, e.is_dir, e.size
         FROM (SELECT * FROM entries e WHERE e.mtime_sec BETWEEN ?1 AND ?2{}
               ORDER BY e.mtime_sec DESC, e.mtime_nsec DESC LIMIT {}) e
         JOIN names n ON n.id = e.name_id
         ORDER BY e.mtime_sec DESC, e.mtime_nsec DESC",
        filter.clause(),
        limit.map_or(-1, |l| l as i64),
    );
    let mut paths = PathCache::default();
    let mut stmt = conn.prepare_cached(&sql)?;
    let mut rows = stmt.query(params![from_sec, to_sec])?;
    while let Some(row) = rows.next()? {
        if count as usize >= limit_val {
            break;
        }
        let entry = RecentEntry {
            full_path: paths.full(conn, row.get(0)?, row.get_ref(1)?.as_str()?)?,
            mtime_sec: row.get(2)?,
            mtime_nsec: row.get(3)?,
            is_dir: row.get::<_, i32>(4)? != 0,
            size: row.get::<_, i64>(5)? as u64,
        };
        if !on_result(&entry) {
            break;
        }
        count += 1;
    }
    Ok(count)
}

// ---------------------------------------------------------------------------
// Disk usage — directory sizes derived from file sizes at query time
// ---------------------------------------------------------------------------

/// Totals for a directory tree.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Usage {
    /// Name of the child (for breakdowns) or the directory itself.
    pub name: String,
    pub files: u64,
    pub dirs: u64,
    pub bytes: u64,
}

/// Disk usage of the tree under `path`: the total, and the `top` largest
/// direct children (subdirectories summed over their whole tree; files
/// directly in `path` as themselves), largest first — like `du -d1`.
///
/// One pass: the recursive walk tags every directory with the child of
/// `path` it belongs to, and entries are summed per child in SQL.
pub fn usage(conn: &Connection, path: &str, top: usize) -> Result<(Usage, Vec<Usage>)> {
    let name = if path.trim_end_matches('/').is_empty() {
        "/".to_string()
    } else {
        path.trim_end_matches('/').to_string()
    };
    let mut total = Usage {
        name,
        ..Usage::default()
    };
    let Some(root) = find_dir(conn, path)? else {
        return Ok((total, vec![]));
    };
    let mut children: HashMap<i64, Usage> = HashMap::new(); // by child name_id

    // Which child of `path` each directory below it belongs to.
    let mut top_of: HashMap<i64, i64> = HashMap::new();
    {
        let mut stmt = conn.prepare_cached(
            "WITH RECURSIVE sub(id, top) AS (
                 SELECT id, name_id FROM dirs WHERE parent = ?1
                 UNION ALL SELECT d.id, sub.top FROM dirs d JOIN sub ON d.parent = sub.id)
             SELECT id, top FROM sub",
        )?;
        let mut rows = stmt.query(params![root])?;
        while let Some(r) = rows.next()? {
            top_of.insert(r.get(0)?, r.get(1)?);
        }
    }
    // Sums per directory. `entries` is clustered on (dir_id, name_id), so
    // grouping by dir_id streams in key order without a sort.
    let sql = if root == ROOT {
        "SELECT dir_id, COUNT(*) - SUM(is_dir), SUM(is_dir), SUM(size) FROM entries GROUP BY dir_id"
            .to_string()
    } else {
        format!(
            "{SUBTREE} SELECT dir_id, COUNT(*) - SUM(is_dir), SUM(is_dir), SUM(size) FROM entries
             WHERE dir_id IN sub GROUP BY dir_id"
        )
    };
    let mut stmt = conn.prepare_cached(&sql)?;
    let mut rows = if root == ROOT {
        stmt.query([])?
    } else {
        stmt.query(params![root])?
    };
    while let Some(r) = rows.next()? {
        let Some(top) = top_of.get(&r.get::<_, i64>(0)?) else {
            continue; // `path` itself: its direct entries are listed below
        };
        let u = children.entry(*top).or_default();
        u.files += r.get::<_, i64>(1)? as u64;
        u.dirs += r.get::<_, i64>(2)? as u64;
        u.bytes += r.get::<_, i64>(3)? as u64;
    }
    // Entries directly in `path`: each file is its own child; a directory
    // entry counts toward the child of the same name.
    let mut stmt = conn.prepare_cached(
        "SELECT e.name_id, n.name, e.is_dir, e.size FROM entries e
         JOIN names n ON n.id = e.name_id WHERE e.dir_id = ?1",
    )?;
    let mut rows = stmt.query(params![root])?;
    while let Some(r) = rows.next()? {
        let u = children.entry(r.get(0)?).or_default();
        u.name = r.get(1)?;
        if r.get::<_, i32>(2)? != 0 {
            u.dirs += 1;
        } else {
            u.files += 1;
            u.bytes += r.get::<_, i64>(3)? as u64;
        }
    }
    // Children that are only containers (no entry of their own, e.g. the
    // parent of a nested root) are named from the dictionary.
    for (nid, u) in children.iter_mut() {
        if u.name.is_empty() {
            u.name = conn
                .prepare_cached("SELECT name FROM names WHERE id = ?1")?
                .query_row(params![nid], |r| r.get(0))?;
        }
    }
    let mut out: Vec<Usage> = children.into_values().collect();
    for u in &out {
        total.files += u.files;
        total.dirs += u.dirs;
        total.bytes += u.bytes;
    }
    out.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.name.cmp(&b.name)));
    out.truncate(top);
    Ok((total, out))
}

/// Total files, directories and bytes under `path` (recursively).
pub fn dir_usage(conn: &Connection, path: &str) -> Result<Usage> {
    usage(conn, path, 0).map(|(t, _)| t)
}

/// The `limit` largest direct children of `path`; see [`usage`].
pub fn usage_breakdown(conn: &Connection, path: &str, limit: usize) -> Result<Vec<Usage>> {
    usage(conn, path, limit).map(|(_, c)| c)
}

/// Largest files (optionally under `under`), biggest first.
pub fn largest_files_streaming(
    conn: &Connection,
    under: Option<&str>,
    filter: &Filter,
    limit: Option<usize>,
    mut on_result: impl FnMut(&Hit) -> bool,
) -> Result<u64> {
    let root = match under {
        Some(u) => match find_dir(conn, u)? {
            Some(d) => d,
            None => return Ok(0),
        },
        None => ROOT,
    };
    let filter = Filter {
        is_dir: Some(false),
        ..*filter
    };
    let scope = if root == ROOT {
        ""
    } else {
        " AND e.dir_id IN sub"
    };
    let query = |big_only: bool| {
        format!(
            // Limit the entries first, then join names for those only.
            "{SUBTREE} SELECT {HIT_COLS} FROM (SELECT * FROM {}
                 WHERE 1{scope}{}{} ORDER BY e.size DESC LIMIT {}) e
             JOIN names n ON n.id = e.name_id ORDER BY e.size DESC",
            entries_from(conn, big_only),
            filter.clause(),
            if big_only {
                format!(" AND e.size >= {BIG_FILE}")
            } else {
                String::new()
            },
            limit.map_or(-1, |l| l as i64),
        )
    };
    // Usually enough files of 1 MiB+ exist: walk the partial index. If
    // not, fall back to scanning everything.
    if let Some(n) = limit {
        let big: i64 = conn.query_row(
            &format!(
                "{SUBTREE} SELECT COUNT(*) FROM (SELECT 1 FROM {} WHERE 1{scope}{} AND e.size >= {BIG_FILE} LIMIT ?2)",
                entries_from(conn, true),
                filter.clause()
            ),
            params![root, n as i64],
            |r| r.get(0),
        )?;
        if big as usize >= n {
            return stream_hits(
                conn,
                &query(true),
                &[&root],
                limit,
                false,
                None,
                &mut on_result,
            );
        }
    }
    stream_hits(
        conn,
        &query(false),
        &[&root],
        limit,
        false,
        None,
        &mut on_result,
    )
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
    /// Sum of all indexed file sizes.
    pub total_bytes: u64,
}

pub fn get_health(conn: &Connection, db_path: &Path) -> Result<HealthInfo> {
    let (entry_count, dir_count, total_bytes): (i64, i64, i64) = conn.query_row(
        "SELECT COUNT(*), COALESCE(SUM(is_dir), 0), COALESCE(SUM(size), 0) FROM entries",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    let last_scan = get_metadata(conn, "last_scan_completed")?.and_then(|v| v.parse::<i64>().ok());
    let last_count = get_metadata(conn, "last_entry_count")?.and_then(|v| v.parse::<i64>().ok());
    let monitoring_mode =
        get_metadata(conn, "monitoring_mode")?.unwrap_or_else(|| "unknown".to_string());

    Ok(HealthInfo {
        entry_count,
        dir_count,
        file_count: entry_count - dir_count,
        last_scan_completed: last_scan,
        last_entry_count: last_count,
        monitoring_mode,
        pending_events: pending_event_count(conn)?,
        db_size_bytes: db_size_bytes(db_path),
        total_bytes: total_bytes as u64,
    })
}

/// Database file plus its WAL.
fn db_size_bytes(db_path: &Path) -> u64 {
    let size = |p: &Path| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
    let wal = size(&std::path::PathBuf::from(format!(
        "{}-wal",
        db_path.display()
    )));
    if rfind_zvfs::is_compressed(db_path) {
        rfind_zvfs::store_files(db_path)
            .iter()
            .map(|p| size(p))
            .sum::<u64>()
            + wal
    } else {
        size(db_path) + wal
    }
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
    let (entry_count, dir_count): (i64, i64) = conn.query_row(
        "SELECT COUNT(*), COALESCE(SUM(is_dir), 0) FROM entries",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;

    // Top 10 extensions by count.
    let mut ext_stmt = conn.prepare(
        "SELECT LOWER(SUBSTR(n.name, INSTR(n.name, '.') + 1)) AS ext, COUNT(*) AS cnt
         FROM entries e JOIN names n ON n.id = e.name_id
         WHERE e.is_dir = 0 AND INSTR(n.name, '.') > 0
         GROUP BY ext ORDER BY cnt DESC LIMIT 10",
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
        "SELECT dir_id, COUNT(*) AS cnt FROM entries GROUP BY dir_id ORDER BY cnt DESC LIMIT 10",
    )?;
    let counts: Vec<(i64, i64)> = dir_stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .flatten()
        .collect();
    let mut paths = PathCache::default();
    let mut largest_dirs = Vec::new();
    for (id, count) in counts {
        let p = paths.dir(conn, id)?;
        largest_dirs.push(DirStat {
            path: if p.is_empty() { "/".into() } else { p },
            count,
        });
    }

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
        file_count: entry_count - dir_count,
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
        insert_entry(c, p, is_dir, 0, 0, 0, scan_gen).unwrap();
    }

    fn paths(c: &Connection) -> Vec<String> {
        let mut v = Vec::new();
        search_under_streaming(c, "/", None, &Filter::default(), None, |h| {
            v.push(h.path.to_string());
            true
        })
        .unwrap();
        v
    }

    fn dir_paths(c: &Connection) -> Vec<String> {
        let mut cache = PathCache::default();
        let ids: Vec<i64> = c
            .prepare("SELECT id FROM dirs")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        let mut v: Vec<String> = ids
            .into_iter()
            .map(|id| cache.dir(c, id).unwrap())
            .collect();
        v.sort();
        v
    }

    fn glob(c: &Connection, pat: &str) -> Vec<String> {
        let mut v = Vec::new();
        search_by_glob_streaming(c, pat, &Filter::default(), None, |p| {
            v.push(p.path.to_string());
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
        // Directory rows left without entries are dropped: only the root
        // and the parents of surviving entries remain.
        assert_eq!(dir_paths(&c), vec!["", "/etc", "/mnt", "/mnt/c"]);
    }

    #[test]
    fn delete_outside_roots_keeps_only_configured_trees() {
        let c = mem();
        for p in ["/home/a/x", "/mnt/c/w", "/mnt/cx/y", "/srv/z"] {
            add(&c, p, false, None);
        }
        let n = delete_outside_roots(&c, &["/home".to_string(), "/mnt/c".to_string()]).unwrap();
        assert_eq!(n, 2);
        assert_eq!(paths(&c), vec!["/home/a/x", "/mnt/c/w"]);
        // Root "/" keeps everything.
        assert_eq!(delete_outside_roots(&c, &["/".to_string()]).unwrap(), 0);
    }

    #[test]
    fn hits_carry_type_and_mtime() {
        let c = mem();
        insert_entry(&c, "/a/dir", true, 42, 0, 0, None).unwrap();
        let mut got = None;
        search_by_glob_streaming(&c, "dir", &Filter::default(), None, |h| {
            got = Some((h.path.to_string(), h.is_dir, h.mtime_sec));
            true
        })
        .unwrap();
        assert_eq!(got, Some(("/a/dir".to_string(), true, 42)));
    }

    #[test]
    fn sizes_roll_up_into_directory_usage() {
        let c = mem();
        insert_entry(&c, "/d", true, 0, 0, 999, None).unwrap(); // dir size ignored
        insert_entry(&c, "/d/a.iso", false, 0, 0, 1000, None).unwrap();
        insert_entry(&c, "/d/sub", true, 0, 0, 0, None).unwrap();
        insert_entry(&c, "/d/sub/b.txt", false, 0, 0, 20, None).unwrap();
        insert_entry(&c, "/d/sub/deep/c.txt", false, 0, 0, 3, None).unwrap();
        insert_entry(&c, "/dx/other", false, 0, 0, 5, None).unwrap();

        let u = dir_usage(&c, "/d").unwrap();
        assert_eq!((u.files, u.dirs, u.bytes), (3, 1, 1023));
        assert_eq!(dir_usage(&c, "/").unwrap().bytes, 1028);

        let b = usage_breakdown(&c, "/d", 10).unwrap();
        let rows: Vec<_> = b
            .iter()
            .map(|u| (u.name.as_str(), u.files, u.bytes))
            .collect();
        assert_eq!(rows, vec![("a.iso", 1, 1000), ("sub", 2, 23)]);
        let top: Vec<_> = usage_breakdown(&c, "/", 10)
            .unwrap()
            .into_iter()
            .map(|u| (u.name, u.bytes))
            .collect();
        assert_eq!(top, vec![("d".to_string(), 1023), ("dx".to_string(), 5)]);
    }

    #[test]
    fn size_filters_and_largest_files() {
        let c = mem();
        for (p, sz) in [("/a/big", 5000u64), ("/a/mid", 500), ("/a/small", 5)] {
            insert_entry(&c, p, false, 0, 0, sz, None).unwrap();
        }
        insert_entry(&c, "/a/dir", true, 0, 0, 0, None).unwrap();
        let mut v = Vec::new();
        let f = Filter {
            min_size: Some(100),
            max_size: Some(1000),
            ..Filter::default()
        };
        search_under_streaming(&c, "/a", None, &f, None, |h| {
            v.push(h.path.to_string());
            true
        })
        .unwrap();
        assert_eq!(v, vec!["/a/mid"]);
        let mut v = Vec::new();
        largest_files_streaming(&c, Some("/a"), &Filter::default(), Some(2), |h| {
            v.push((h.path.to_string(), h.size));
            true
        })
        .unwrap();
        assert_eq!(v, vec![("/a/big".into(), 5000), ("/a/mid".into(), 500)]);
    }

    #[test]
    fn names_are_stored_once_and_collected_when_unused() {
        let c = mem();
        for p in ["/a/index.js", "/b/index.js", "/c/index.js", "/c/other.js"] {
            add(&c, p, false, Some(1));
        }
        let names: i64 = c
            .query_row("SELECT COUNT(*) FROM names", [], |r| r.get(0))
            .unwrap();
        assert_eq!(names, 5); // a, b, c, index.js, other.js
        add(&c, "/a/index.js", false, Some(2));
        sweep_stale(&c, "/", &[], 2).unwrap();
        assert_eq!(paths(&c), vec!["/a/index.js"]);
        let mut left: Vec<String> = c
            .prepare("SELECT name FROM names ORDER BY name")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        left.sort();
        assert_eq!(left, vec!["a", "index.js"]);
        assert_eq!(dir_paths(&c), vec!["", "/a"]);
    }

    #[test]
    fn renaming_a_directory_rewrites_one_row() {
        let c = mem();
        for p in ["/x/old", "/x/old/a/b/c.txt", "/x/old/d.txt"] {
            add(&c, p, true, None);
        }
        let dirs_before: i64 = c
            .query_row("SELECT COUNT(*) FROM dirs", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rename_entry(&c, "/x/old", "/y/new").unwrap(), 3);
        assert_eq!(
            paths(&c),
            vec!["/y/new", "/y/new/a/b/c.txt", "/y/new/d.txt"]
        );
        let mut v = Vec::new();
        search_under_streaming(&c, "/y/new/a", None, &Filter::default(), None, |h| {
            v.push(h.path.to_string());
            true
        })
        .unwrap();
        assert_eq!(v, vec!["/y/new/a/b/c.txt"]);
        // /y was created; nothing beneath the renamed directory was copied.
        let dirs_after: i64 = c
            .query_row("SELECT COUNT(*) FROM dirs", [], |r| r.get(0))
            .unwrap();
        assert_eq!(dirs_after, dirs_before + 1);
    }

    #[test]
    fn path_glob_matching_follows_sqlite_glob() {
        for (p, t, m) in [
            ("/etc/*.conf", "/etc/a.conf", true),
            ("/etc/*.conf", "/etc/x/b.conf", true), // * crosses '/'
            ("/etc/*.conf", "/etcfoo/c.conf", false),
            ("a?c", "abc", true),
            ("a?c", "ac", false),
            ("[ab]x", "bx", true),
            ("[^ab]x", "bx", false),
            ("[a-c]1", "c1", true),
            ("photo [1]*", "photo 1.jpg", true), // [1] is a set
            ("*", "", true),
            ("x*", "y", false),
        ] {
            assert_eq!(glob_match(p, t), m, "{p} vs {t}");
        }
    }

    #[test]
    fn under_root_matches_a_plain_search() {
        let c = mem();
        for (p, sz) in [("/a/big.iso", 5u64 << 20), ("/b/small", 10)] {
            insert_entry(&c, p, false, 0, 0, sz, None).unwrap();
        }
        let f = Filter {
            min_size: Some(1 << 20),
            ..Filter::default()
        };
        let mut v = Vec::new();
        search_under_streaming(&c, "/", None, &f, None, |h| {
            v.push(h.path.to_string());
            true
        })
        .unwrap();
        assert_eq!(v, vec!["/a/big.iso"]);
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
        search_under_streaming(&c, "/etc", Some("*.conf"), &Filter::default(), None, |p| {
            v.push(p.path.to_string());
            true
        })
        .unwrap();
        assert_eq!(v, vec!["/etc/a.conf", "/etc/x/b.conf"]);
        let mut v = Vec::new();
        search_by_path_glob_streaming(&c, "/etc/*.conf", &Filter::default(), None, |p| {
            v.push(p.path.to_string());
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
            &Filter::default(),
            None,
            |p| {
                v.push(p.path.to_string());
                true
            },
        )
        .unwrap();
        assert_eq!(v, vec!["/lib/libz.so.1"]);
        let mut v = Vec::new();
        search_by_regex_streaming(
            &c,
            "^/opt/",
            "full_path",
            None,
            &Filter::default(),
            None,
            |p| {
                v.push(p.path.to_string());
                true
            },
        )
        .unwrap();
        assert_eq!(v, vec!["/opt/lib/x"]);
        // --under restricts the subtree; the regex still applies to names.
        let mut v = Vec::new();
        search_by_regex_streaming(
            &c,
            "^lib",
            "basename",
            Some("/opt"),
            &Filter::default(),
            None,
            |p| {
                v.push(p.path.to_string());
                true
            },
        )
        .unwrap();
        assert!(v.is_empty());
        search_by_regex_streaming(
            &c,
            "^x$",
            "basename",
            Some("/opt"),
            &Filter::default(),
            None,
            |p| {
                v.push(p.path.to_string());
                true
            },
        )
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
    fn readonly_open_refuses_other_versions_and_never_writes() {
        let path = std::env::temp_dir().join(format!("rfind-ro-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        assert!(open_readonly(&path).is_err()); // missing: not created
        assert!(!path.exists());
        {
            let c = Connection::open(&path).unwrap();
            c.execute_batch("CREATE TABLE entries (x); INSERT INTO entries VALUES (1); PRAGMA user_version = 4;")
                .unwrap();
        }
        let err = open_readonly(&path).unwrap_err();
        assert!(err.contains("v4"), "{err}");
        // The old table is untouched.
        let n: i64 = Connection::open(&path)
            .unwrap()
            .query_row("SELECT COUNT(*) FROM entries", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
        drop(open(&path).unwrap()); // the daemon's open rebuilds it
        assert!(open_readonly(&path).is_ok());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn switching_storage_format_rebuilds_and_readers_follow() {
        let dir = std::env::temp_dir().join(format!("rfind-fmt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("index.db");
        {
            let c = open_with(&path, false).unwrap();
            add(&c, "/plain/x", false, None);
        }
        assert!(!rfind_zvfs::is_compressed(&path));
        assert_eq!(entry_count(&open_readonly(&path).unwrap()).unwrap(), 1);
        {
            let c = open_with(&path, true).unwrap(); // format changed: rebuilt
            assert_eq!(entry_count(&c).unwrap(), 0);
            add(&c, "/zstd/y", false, None);
            c.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);").unwrap();
        }
        assert!(rfind_zvfs::is_compressed(&path));
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);
        let r = open_readonly(&path).unwrap();
        assert!(entry_exists(&r, "/zstd/y").unwrap());
        let _ = std::fs::remove_dir_all(&dir);
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
