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

    // Search filters are stitched into SQL text (big_only, under, scoped,
    // size-filter clauses), so the number of distinct prepared statements
    // easily exceeds rusqlite's default 16-entry cache under a mixed CLI
    // mix. Bump it so each plan is reused.
    conn.set_prepared_statement_cache_capacity(64);

    // Register REGEXP once per connection so searches don't pay for
    // `create_scalar_function` on every call.
    reads::register_regexp(&conn)?;

    Ok(conn)
}

/// After bulk writes: checkpoint the WAL and let a compressed store drop
/// old page versions.
/// Run `PRAGMA integrity_check` and return `Ok(())` iff SQLite reports the
/// single row `"ok"`. Any other result (including I/O errors) returns an
/// `Err` whose message is suitable for logging. The call is cheap on a
/// small index and bounded by WAL/page count on a large one; callers run
/// it only after an abnormal exit, not on every start.
pub fn integrity_check(conn: &Connection) -> std::result::Result<(), String> {
    let mut stmt = conn
        .prepare("PRAGMA integrity_check")
        .map_err(|e| format!("prepare integrity_check: {e}"))?;
    let rows: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .map_err(|e| format!("query integrity_check: {e}"))?
        .filter_map(|r| r.ok())
        .collect();
    match rows.as_slice() {
        [s] if s == "ok" => Ok(()),
        _ => Err(rows.join("; ")),
    }
}

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
    // search_by_regex_streaming uses REGEXP; open_with registers it on the
    // daemon's connection, read-only openers need it too.
    reads::register_regexp(&conn).map_err(|e| format!("registering REGEXP: {e}"))?;
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

/// (parent dir id, name id) of an indexed path, if both exist. Lives in
/// `mod.rs` because both `writes` (delete/rename) and `reads`
/// (`entry_exists`) need it.
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

mod writes;
pub use writes::*;

mod reads;
pub use reads::*;

mod sizes;
pub use sizes::*;

mod stats;
pub use stats::*;

mod events;
pub use events::*;

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
    fn delete_under_runs_inside_outer_transaction() {
        // The caller (reconcile) wraps a batch in a tx; delete_entries_under
        // uses a SAVEPOINT internally so it nests cleanly inside that tx.
        // Regression check: before the SAVEPOINT fix this still ran, but
        // the two multi-statement deletes were not atomic across a mid-op
        // failure. This test just confirms it still works inside a tx.
        let c = mem();
        for p in ["/a/b", "/a/b/c", "/a/b/c/d"] {
            add(&c, p, false, None);
        }
        let tx = c.unchecked_transaction().unwrap();
        delete_entries_under(&tx, "/a/b").unwrap();
        tx.commit().unwrap();
        assert_eq!(paths(&c), Vec::<String>::new());
    }

    #[test]
    fn rename_runs_inside_outer_transaction() {
        let c = mem();
        for p in ["/a/d", "/a/d/f", "/b/d"] {
            add(&c, p, false, None);
        }
        let tx = c.unchecked_transaction().unwrap();
        assert_eq!(rename_entry(&tx, "/a/d", "/b/d").unwrap(), 2);
        tx.commit().unwrap();
        assert_eq!(paths(&c), vec!["/b/d", "/b/d/f"]);
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
    fn delete_outside_roots_skips_when_a_root_is_unresolved() {
        // If one of two configured roots has no `dirs` row yet (NFS unmount
        // race, mid-rename, startup lag), the old code would prune everything
        // under the missing root using only the resolved root as the keep-set.
        let c = mem();
        for p in ["/home/a/x", "/mnt/c/w"] {
            add(&c, p, false, None);
        }
        // "/mnt/data" never resolves; must not wipe /home.
        let n = delete_outside_roots(&c, &["/home".to_string(), "/mnt/data".to_string()]).unwrap();
        assert_eq!(n, 0);
        assert_eq!(paths(&c), vec!["/home/a/x", "/mnt/c/w"]);
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
