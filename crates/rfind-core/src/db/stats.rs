//! Health and stats readouts used by `rfind status`, `rfind health` and
//! `rfind stats`. Pure read side; mutates nothing.

use rusqlite::{Connection, Result};
use std::{collections::HashMap, path::Path};

use super::events::pending_event_count;
use super::{get_metadata, PathCache};

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

/// Database file plus its WAL. Compressed stores sum every page file.
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

    // Top 10 extensions by count. Compute the extension in Rust so we can
    // use the LAST dot: SQLite has no REVERSE, and `INSTR` finds the FIRST
    // dot, which would wrongly bucket `foo.tar.gz` under `tar.gz`. Leading
    // dots (`.bashrc`) and trailing dots give no extension.
    let mut name_stmt = conn.prepare(
        "SELECT n.name, COUNT(*) FROM entries e JOIN names n ON n.id = e.name_id
         WHERE e.is_dir = 0 GROUP BY n.name",
    )?;
    let mut ext_counts: HashMap<String, i64> = HashMap::new();
    let mut name_rows = name_stmt.query([])?;
    while let Some(r) = name_rows.next()? {
        let name: String = r.get(0)?;
        let count: i64 = r.get(1)?;
        if let Some(dot) = name.rfind('.') {
            if dot > 0 && dot + 1 < name.len() {
                let ext = name[dot + 1..].to_ascii_lowercase();
                *ext_counts.entry(ext).or_insert(0) += count;
            }
        }
    }
    let mut top: Vec<(String, i64)> = ext_counts.into_iter().collect();
    top.sort_unstable_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    top.truncate(10);
    let top_extensions: Vec<ExtStat> = top
        .into_iter()
        .map(|(ext, count)| ExtStat { ext, count })
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
