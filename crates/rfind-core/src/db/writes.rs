//! Entry inserts, deletes, renames, generation-tagged sweeps, and the
//! deferred GC that reclaims orphan `names`/`dirs` rows after live
//! events delete entries.

use rusqlite::{params, Connection, Result};
use std::{
    collections::HashMap,
    sync::atomic::{AtomicU64, Ordering},
};

use super::{child_dir, find_dir, locate, name_or_create, split_path, ROOT, SUBTREE};

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

    pub(super) fn name(&mut self, name: &str) -> Result<i64> {
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
    pub(super) fn dir(&mut self, path: &str) -> Result<i64> {
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

/// Count of live-event deletions since the last incremental GC. Garbage
/// collection is deferred until this crosses GC_THRESHOLD so inotify /
/// fanotify event floods don't pay for a full-table anti-join per event,
/// but a long-running daemon still eventually reclaims orphan `names` and
/// `dirs` rows.
static DELETES_SINCE_GC: AtomicU64 = AtomicU64::new(0);
const GC_THRESHOLD: u64 = 1000;

fn maybe_gc(conn: &Connection, deletes: usize) -> Result<()> {
    if deletes == 0 {
        return Ok(());
    }
    let prev = DELETES_SINCE_GC.fetch_add(deletes as u64, Ordering::Relaxed);
    if prev + deletes as u64 >= GC_THRESHOLD {
        DELETES_SINCE_GC.store(0, Ordering::Relaxed);
        collect_garbage(conn)?;
    }
    Ok(())
}

pub fn delete_entry_by_path(conn: &Connection, path: &str) -> Result<usize> {
    let n = match locate(conn, path)? {
        Some((did, nid)) => conn.execute(
            "DELETE FROM entries WHERE dir_id = ?1 AND name_id = ?2",
            params![did, nid],
        )?,
        None => 0,
    };
    maybe_gc(conn, n)?;
    Ok(n)
}

/// Run `f` inside a SQLite SAVEPOINT so a mid-op failure rolls back the
/// partial writes. Savepoints nest cleanly inside an outer transaction
/// (reconcile wraps a tx around a batch) AND work on an auto-commit
/// connection (startup config apply), so callers don't need to care.
fn with_savepoint<F, T>(conn: &Connection, name: &str, f: F) -> Result<T>
where
    F: FnOnce(&Connection) -> Result<T>,
{
    conn.execute_batch(&format!("SAVEPOINT {name}"))?;
    match f(conn) {
        Ok(v) => {
            conn.execute_batch(&format!("RELEASE {name}"))?;
            Ok(v)
        }
        Err(e) => {
            // Best-effort rollback: a release failure here would mask
            // the real error, which is already being returned.
            let _ = conn.execute_batch(&format!("ROLLBACK TO {name}; RELEASE {name}"));
            Err(e)
        }
    }
}

/// Delete `path` itself and everything beneath it. The two subtree
/// deletes (entries then dirs) run inside a SAVEPOINT so a failure
/// midway cannot leave orphan `dirs` whose entries are gone.
pub fn delete_entries_under(conn: &Connection, path: &str) -> Result<usize> {
    let Some((did, nid)) = locate(conn, path)? else {
        return Ok(0);
    };
    let n = with_savepoint(conn, "del_sub", |c| {
        let mut n = c.execute(
            "DELETE FROM entries WHERE dir_id = ?1 AND name_id = ?2",
            params![did, nid],
        )?;
        if let Some(sub) = child_dir(c, did, nid)? {
            n += c.execute(
                &format!("{SUBTREE} DELETE FROM entries WHERE dir_id IN sub"),
                params![sub],
            )?;
            c.execute(
                &format!("{SUBTREE} DELETE FROM dirs WHERE id IN sub"),
                params![sub],
            )?;
        }
        Ok(n)
    })?;
    maybe_gc(conn, n)?;
    Ok(n)
}

/// Move `old` (and its subtree) to `new`. Anything already indexed at
/// `new` is replaced, matching rename(2) semantics. Returns the number of
/// entries moved (0 if `old` was not indexed). The nested delete + updates
/// run inside a SAVEPOINT so a mid-op failure cannot split the move.
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
    let moved = with_savepoint(conn, "ren_sub", |c| {
        delete_entries_under(c, new)?;
        let (new_dir, new_name) = split_path(new);
        let mut ins = Inserter::new(c);
        let new_did = ins.dir(new_dir)?;
        let new_nid = ins.name(new_name)?;
        c.execute(
            "UPDATE entries SET dir_id = ?1, name_id = ?2 WHERE dir_id = ?3 AND name_id = ?4",
            params![new_did, new_nid, old_did, old_nid],
        )?;
        let mut moved = 1;
        // A directory: re-parent its row; everything beneath follows by id.
        if let Some(d) = child_dir(c, old_did, old_nid)? {
            c.execute(
                "UPDATE dirs SET parent = ?1, name_id = ?2 WHERE id = ?3",
                params![new_did, new_nid, d],
            )?;
            moved += c.query_row(
                &format!("{SUBTREE} SELECT COUNT(*) FROM entries WHERE dir_id IN sub"),
                params![d],
                |r| r.get::<_, i64>(0),
            )? as usize;
        }
        Ok(moved)
    })?;
    // A rename can orphan the old (dir, name) pair; count it toward GC.
    maybe_gc(conn, 1)?;
    Ok(moved)
}

/// Drop `dirs` rows that no longer hold entries or lead to any, and
/// `names` nothing refers to.
///
/// A single bottom-up reachability pass: a dir is reachable iff it holds
/// an entry, or is the ancestor of a reachable dir. Starting from the set
/// of dirs that still hold entries, walk ancestors to a transitive closure
/// and delete every other dir in one shot. Previously this looped on full
/// anti-joins, which was quadratic in the number of orphan chains and
/// ran >100 ms per pass on a 10M-entry index.
fn collect_garbage(conn: &Connection) -> Result<()> {
    // Previously wrapped in `catch_unwind` to tolerate a REGEXP callback
    // panic. That is UB: rusqlite callbacks run inside the SQLite C
    // library, and a Rust unwind through non-`extern "C-unwind"` C
    // frames is undefined behavior. Instead the REGEXP binding below
    // converts its own panics to `UserFunctionError`, so a bad regex
    // comes back as a normal `Result::Err` and `with_savepoint` rolls
    // back cleanly without an unwind ever reaching SQLite.
    conn.execute_batch(
        "
        DROP TABLE IF EXISTS _reachable;
        CREATE TEMP TABLE _reachable (id INTEGER PRIMARY KEY);
        WITH RECURSIVE anc(id) AS (
            SELECT DISTINCT dir_id FROM entries
            UNION
            SELECT d.parent FROM dirs d JOIN anc ON d.id = anc.id
                WHERE d.parent IS NOT NULL
        )
        INSERT INTO _reachable (id) SELECT id FROM anc;
        INSERT OR IGNORE INTO _reachable (id) VALUES (1);
        DELETE FROM dirs WHERE id NOT IN (SELECT id FROM _reachable);
        DROP TABLE _reachable;
        DELETE FROM names
          WHERE id NOT IN (SELECT name_id FROM entries)
            AND id NOT IN (SELECT name_id FROM dirs WHERE name_id IS NOT NULL);
        ",
    )
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
    // SAVEPOINT so the delete + follow-up GC are atomic.
    with_savepoint(conn, "sweep_stale", |c| {
        let n = c.execute(
            &format!(
                "WITH RECURSIVE sub(id) AS (SELECT ?1 UNION ALL SELECT d.id FROM dirs d
                     JOIN sub ON d.parent = sub.id WHERE d.id NOT IN ({}))
                 DELETE FROM entries WHERE dir_id IN sub AND scan_gen < ?2{keep_entries}",
                id_list(&keep_dirs)
            ),
            params![rid, scan_gen],
        )?;
        if n > 0 {
            collect_garbage(c)?;
        }
        Ok(n)
    })
}

/// Delete entries that are not under any of `roots` (after roots were
/// removed from the config), then drop rows left unused.
pub fn delete_outside_roots(conn: &Connection, roots: &[String]) -> Result<usize> {
    // Safety: an empty root list would make the recursive CTE produce an
    // empty `sub`, so `DELETE ... WHERE dir_id NOT IN sub` would wipe the
    // entire index. Refuse rather than silently truncate.
    if roots.is_empty() {
        return Ok(0);
    }
    let mut ids = Vec::new();
    for r in roots {
        if let Some(d) = find_dir(conn, r)? {
            ids.push(d);
        }
    }
    // Partial resolution is dangerous: with roots=["/home","/mnt/data"] and
    // /home temporarily unresolved (NFS unmount race, mid-rename, startup
    // lag), pruning against only /mnt/data wipes the entire /home subtree.
    // Skip the tick rather than lose data; the next pass picks it up.
    if ids.len() != roots.len() {
        return Ok(0);
    }
    with_savepoint(conn, "del_outside", |c| {
        let n = c.execute(
            &format!(
                "WITH RECURSIVE sub(id) AS (SELECT id FROM dirs WHERE id IN ({})
                     UNION ALL SELECT d.id FROM dirs d JOIN sub ON d.parent = sub.id)
                 DELETE FROM entries WHERE dir_id NOT IN sub",
                id_list(&ids)
            ),
            [],
        )?;
        if n > 0 {
            collect_garbage(c)?;
        }
        Ok(n)
    })
}
