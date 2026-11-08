//! Lookups and search: `entry_exists` / `entry_count` / `get_all_dirs`,
//! the streaming search family (glob / path-glob / regex / recent), and
//! the shared `Hit` / `Filter` types + `stream_hits` helper.
//!
//! `register_regexp` is `pub(super)` and installed by the connection
//! openers in `mod.rs`.

use rusqlite::{params, Connection, Result};

use super::{find_dir, find_name, locate, PathCache, BIG_FILE, ROOT, SUBTREE};

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
pub(super) const HIT_COLS: &str = "e.dir_id, n.name, e.is_dir, e.mtime_sec, e.size";

/// Result filters shared by all searches.
#[derive(Debug, Clone, Copy, Default)]
pub struct Filter {
    /// Some(true) = directories only, Some(false) = files only.
    pub is_dir: Option<bool>,
    /// Files of at least this many bytes (implies files only).
    pub min_size: Option<u64>,
    /// Files of at most this many bytes (implies files only).
    pub max_size: Option<u64>,
    /// Inclusive (from_sec, to_sec) epoch window on mtime. Uses i64::MIN /
    /// i64::MAX for one-sided bounds. None = no mtime constraint.
    pub mtime_range: Option<(i64, i64)>,
}

impl Filter {
    pub fn files() -> Self {
        Self {
            is_dir: Some(false),
            ..Self::default()
        }
    }

    /// Only files of at least BIG_FILE bytes match.
    pub(super) fn big_only(&self) -> bool {
        self.min_size.is_some_and(|n| n >= BIG_FILE)
    }

    /// SQL conditions to append to a WHERE clause. Only integers from typed
    /// fields are inlined, so this is injection-safe.
    pub(super) fn clause(&self) -> String {
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
        if let Some((lo, hi)) = self.mtime_range {
            if lo != i64::MIN {
                c.push_str(&format!(" AND e.mtime_sec >= {lo}"));
            }
            if hi != i64::MAX {
                c.push_str(&format!(" AND e.mtime_sec <= {hi}"));
            }
        }
        c
    }
}

/// `entries e`, forced onto the big-file partial index when the query is
/// restricted to files of 1 MiB+ (SQLite's planner otherwise walks
/// directories: 2.5 s instead of <1 ms at 10M entries).
pub(super) fn entries_from(_conn: &Connection, big_only: bool) -> &'static str {
    if big_only {
        "entries e INDEXED BY idx_entries_big"
    } else {
        "entries e"
    }
}

/// What a row filter decides beyond SQL (path globs, path regexes).
type RowCheck<'a> = Option<&'a dyn Fn(&str) -> bool>;

/// Run a query selecting [`HIT_COLS`], build each path, and stream hits to
/// `on_result` until it returns false or `limit` is reached.
///
/// `sort`: order by path. That means reading every match before returning
/// the first, so callers sort only when no limit is set; a limited search
/// returns the first matches found.
pub(super) fn stream_hits(
    conn: &Connection,
    sql: &str,
    params: &[&dyn rusqlite::ToSql],
    limit: Option<usize>,
    sort: bool,
    check: RowCheck,
    on_result: &mut dyn FnMut(&Hit) -> bool,
) -> Result<u64> {
    let tx = conn.unchecked_transaction()?;
    let conn: &Connection = &tx;
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
    pattern.contains(['*', '?'])
}

/// If `pattern` is of the form `*.ext` where `ext` contains no `.`, `*`,
/// `?`, or `[`, return `ext`. These patterns are answered by the extension
/// index instead of a full names-table GLOB scan.
fn pure_extension(pattern: &str) -> Option<&str> {
    let ext = pattern.strip_prefix("*.")?;
    if ext.is_empty() || ext.contains(['.', '*', '?', '[']) {
        return None;
    }
    Some(ext)
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
    // Fast path: `*.ext` patterns use the extension index — a single B-tree
    // probe instead of a full names-table GLOB scan.
    if let Some(ext) = pure_extension(pattern) {
        let sql = if filter.big_only() {
            format!(
                "SELECT {HIT_COLS} FROM {} JOIN names n ON n.id = e.name_id
                 WHERE n.extension = ?1{}",
                entries_from(conn, true),
                filter.clause()
            )
        } else {
            format!(
                "SELECT {HIT_COLS} FROM names n CROSS JOIN entries e ON e.name_id = n.id
                 WHERE n.extension = ?1{}",
                filter.clause()
            )
        };
        return stream_hits(
            conn,
            &sql,
            &[&ext],
            limit,
            limit.is_none(),
            None,
            &mut on_result,
        );
    }
    let sql = if filter.big_only() {
        format!(
            "SELECT {HIT_COLS} FROM {} JOIN names n ON n.id = e.name_id
             WHERE n.name GLOB ?1{}",
            entries_from(conn, true),
            filter.clause()
        )
    } else {
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
    // REGEXP is registered once at open_with; no need to re-register here.
    let sql = match (root, by_path) {
        (None, false) => format!(
            "SELECT {HIT_COLS} FROM names n CROSS JOIN entries e ON e.name_id = n.id
                 WHERE n.name REGEXP ?1{}",
            filter.clause()
        ),
        (Some(_), false) => format!(
            "{SUBTREE} SELECT {HIT_COLS} FROM sub JOIN entries e ON e.dir_id = sub.id
                 JOIN names n ON n.id = e.name_id WHERE n.name REGEXP ?2{}",
            filter.clause()
        ),
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

// ---------------------------------------------------------------------------
// Diff
// ---------------------------------------------------------------------------

/// Kind of change reported by [`diff_since`].
#[derive(Debug, Clone, PartialEq)]
pub enum DiffChange {
    /// Entry has a mtime newer than the cutoff (added or modified).
    Changed,
    /// Entry was deleted (sourced from `pending_events` — only recent
    /// deletions are available before the events are pruned).
    Deleted,
}

/// One entry in a diff result.
#[derive(Debug, Clone)]
pub struct DiffEntry {
    pub path: String,
    pub change: DiffChange,
    pub mtime_sec: i64,
}

/// Stream entries whose mtime > `since_epoch` (added/modified) plus
/// recent deletions from `pending_events`.
pub fn diff_since(
    conn: &Connection,
    since_epoch: i64,
    filter: &Filter,
    mut on_result: impl FnMut(&DiffEntry) -> bool,
) -> Result<u64> {
    let mut count = 0u64;
    // Phase 1: changed/added entries (mtime > cutoff).
    let sql = format!(
        "SELECT e.dir_id, n.name, e.mtime_sec FROM entries e
         JOIN names n ON n.id = e.name_id
         WHERE e.mtime_sec > ?1{}
         ORDER BY e.mtime_sec DESC",
        filter.clause()
    );
    let mut paths = PathCache::default();
    let mut stmt = conn.prepare_cached(&sql)?;
    let mut rows = stmt.query(params![since_epoch])?;
    while let Some(row) = rows.next()? {
        let path = paths.full(conn, row.get(0)?, row.get_ref(1)?.as_str()?)?;
        let entry = DiffEntry { path, change: DiffChange::Changed, mtime_sec: row.get(2)? };
        if !on_result(&entry) {
            return Ok(count);
        }
        count += 1;
    }
    // Phase 2: recent deletions still in pending_events (pruned after apply).
    let mut stmt = conn.prepare_cached(
        "SELECT path, timestamp FROM pending_events
         WHERE applied = 1 AND event_type = 'DELETE' AND timestamp > ?1
         ORDER BY timestamp DESC",
    )?;
    let mut rows = stmt.query(params![since_epoch])?;
    while let Some(row) = rows.next()? {
        let entry = DiffEntry {
            path: row.get(0)?,
            change: DiffChange::Deleted,
            mtime_sec: row.get(1)?,
        };
        if !on_result(&entry) {
            return Ok(count);
        }
        count += 1;
    }
    Ok(count)
}

// ---------------------------------------------------------------------------
// Watch
// ---------------------------------------------------------------------------

/// One event returned by [`poll_applied_events_since`].
#[derive(Debug, Clone)]
pub struct WatchEvent {
    pub id: i64,
    pub event_type: String,
    pub path: String,
    pub new_path: Option<String>,
}

/// Poll for applied events newer than `after_id`. Returns events in id order.
pub fn poll_applied_events_since(
    conn: &Connection,
    after_id: i64,
) -> Result<Vec<WatchEvent>> {
    let mut stmt = conn.prepare_cached(
        "SELECT id, event_type, path, new_path FROM pending_events
         WHERE applied = 1 AND id > ?1 ORDER BY id ASC",
    )?;
    let rows = stmt.query_map(params![after_id], |r| {
        Ok(WatchEvent {
            id: r.get(0)?,
            event_type: r.get(1)?,
            path: r.get(2)?,
            new_path: r.get(3)?,
        })
    })?;
    rows.collect()
}

/// Largest applied event id, or 0 if no events exist.
pub fn max_applied_event_id(conn: &Connection) -> Result<i64> {
    conn.query_row(
        "SELECT COALESCE(MAX(id), 0) FROM pending_events WHERE applied = 1",
        [],
        |r| r.get(0),
    )
}

pub(super) fn register_regexp(conn: &Connection) -> Result<()> {
    conn.create_scalar_function(
        "REGEXP",
        2,
        rusqlite::functions::FunctionFlags::SQLITE_UTF8
            | rusqlite::functions::FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
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
