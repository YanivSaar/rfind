//! Duplicate file detection: SQL phase (same-size candidates) used by the
//! CLI's hash phase.

use rusqlite::{params, Connection, Result};

use super::{find_dir, PathCache, SUBTREE};

/// Return groups of file paths that share the same size.
/// Each inner `Vec<String>` has ≥ 2 paths; size < `min_size` is skipped.
pub fn find_size_candidates(
    conn: &Connection,
    under: Option<&str>,
    min_size: u64,
) -> Result<Vec<Vec<String>>> {
    let min = min_size.min(i64::MAX as u64) as i64;

    // Build the size-candidate query depending on whether --under was given.
    let rows: Vec<(i64, i64, String)>; // (size, dir_id, name)
    if let Some(u) = under {
        let root = match find_dir(conn, u)? {
            Some(d) => d,
            None => return Ok(vec![]),
        };
        let sql = format!(
            "{SUBTREE} SELECT e.size, e.dir_id, n.name
             FROM entries e JOIN names n ON n.id = e.name_id
             WHERE e.is_dir = 0 AND e.size >= ?2 AND e.dir_id IN sub
               AND e.size IN (
                   SELECT size FROM entries WHERE is_dir = 0 AND size >= ?2
                   AND dir_id IN ({SUBTREE} SELECT id FROM sub)
                   GROUP BY size HAVING COUNT(*) > 1
               )
             ORDER BY e.size, e.dir_id, n.name"
        );
        // The nested CTE needs the root param twice.
        let mut stmt = conn.prepare(&sql)?;
        rows = stmt
            .query_map(params![root, min], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<Result<_>>()?;
    } else {
        let sql = "SELECT e.size, e.dir_id, n.name
                   FROM entries e JOIN names n ON n.id = e.name_id
                   WHERE e.is_dir = 0 AND e.size >= ?1
                     AND e.size IN (
                         SELECT size FROM entries
                         WHERE is_dir = 0 AND size >= ?1
                         GROUP BY size HAVING COUNT(*) > 1
                     )
                   ORDER BY e.size, e.dir_id, n.name";
        let mut stmt = conn.prepare(sql)?;
        rows = stmt
            .query_map(params![min], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<Result<_>>()?;
    }

    // Reconstruct full paths and group by size.
    let mut paths = PathCache::default();
    let mut groups: Vec<Vec<String>> = Vec::new();
    let mut current_size: Option<i64> = None;

    for (size, dir_id, name) in rows {
        let full_path = paths.full(conn, dir_id, &name)?;
        if current_size != Some(size) {
            groups.push(Vec::new());
            current_size = Some(size);
        }
        groups.last_mut().unwrap().push(full_path);
    }

    Ok(groups)
}
