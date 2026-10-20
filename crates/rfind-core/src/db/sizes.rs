//! Disk usage: `du`-style totals, child breakdowns, and ranked largest
//! dirs/files. Shares `Filter` / `Hit` / `entries_from` / `stream_hits`
//! with the reads module.

use rusqlite::{params, Connection, Result};
use std::collections::{HashMap, HashSet};

use super::reads::{entries_from, stream_hits, Filter, Hit, HIT_COLS};
use super::{find_dir, PathCache, BIG_FILE, ROOT, SUBTREE};

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

/// Directories ranked by the total size of their subtree (sum of every
/// file beneath them, recursive — like `du`), biggest first.
pub fn largest_dirs_streaming(
    conn: &Connection,
    under: Option<&str>,
    filter: &Filter,
    limit: Option<usize>,
    mut on_result: impl FnMut(&Hit) -> bool,
) -> Result<u64> {
    let scope_root = match under {
        Some(u) => match find_dir(conn, u)? {
            Some(d) => d,
            None => return Ok(0),
        },
        None => ROOT,
    };

    let mut parent_of: HashMap<i64, i64> = HashMap::new();
    let mut in_scope: HashSet<i64> = HashSet::new();
    {
        let (sql, params): (String, Vec<&dyn rusqlite::ToSql>) = if scope_root == ROOT {
            ("SELECT id, parent FROM dirs".to_string(), vec![])
        } else {
            (
                format!("{SUBTREE} SELECT d.id, d.parent FROM dirs d WHERE d.id IN sub"),
                vec![&scope_root],
            )
        };
        let mut stmt = conn.prepare_cached(&sql)?;
        let mut rows = stmt.query(rusqlite::params_from_iter(params))?;
        while let Some(r) = rows.next()? {
            let id: i64 = r.get(0)?;
            let parent: Option<i64> = r.get(1)?;
            in_scope.insert(id);
            if let Some(p) = parent {
                parent_of.insert(id, p);
            }
        }
    }

    let mut direct: HashMap<i64, u64> = HashMap::new();
    {
        let (sql, params): (String, Vec<&dyn rusqlite::ToSql>) = if scope_root == ROOT {
            (
                "SELECT dir_id, SUM(size) FROM entries WHERE is_dir = 0 GROUP BY dir_id"
                    .to_string(),
                vec![],
            )
        } else {
            (
                format!(
                    "{SUBTREE} SELECT dir_id, SUM(size) FROM entries
                     WHERE is_dir = 0 AND dir_id IN sub GROUP BY dir_id"
                ),
                vec![&scope_root],
            )
        };
        let mut stmt = conn.prepare_cached(&sql)?;
        let mut rows = stmt.query(rusqlite::params_from_iter(params))?;
        while let Some(r) = rows.next()? {
            let did: i64 = r.get(0)?;
            let sum: i64 = r.get(1)?;
            direct.insert(did, sum as u64);
        }
    }

    let mut cumul: HashMap<i64, u64> = in_scope.iter().map(|&id| (id, 0u64)).collect();
    for (&id, &bytes) in &direct {
        if !in_scope.contains(&id) {
            continue;
        }
        let mut cur = id;
        loop {
            if let Some(v) = cumul.get_mut(&cur) {
                *v += bytes;
            }
            match parent_of.get(&cur) {
                Some(&p) if in_scope.contains(&p) => cur = p,
                _ => break,
            }
        }
    }

    let mut ranked: Vec<(i64, u64)> = cumul.into_iter().collect();
    if let Some(min) = filter.min_size {
        ranked.retain(|(_, s)| *s >= min);
    }
    if let Some(max) = filter.max_size {
        ranked.retain(|(_, s)| *s <= max);
    }
    ranked.sort_unstable_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let want = limit.unwrap_or(usize::MAX);
    if ranked.len() > want {
        ranked.truncate(want);
    }

    let mut paths = PathCache::default();
    let mut count = 0u64;
    for (id, bytes) in ranked {
        let path = if id == ROOT {
            "/".to_string()
        } else {
            paths.dir(conn, id)?
        };
        let hit = Hit {
            path: &path,
            is_dir: true,
            mtime_sec: 0,
            size: bytes,
        };
        if !on_result(&hit) {
            break;
        }
        count += 1;
    }
    Ok(count)
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
