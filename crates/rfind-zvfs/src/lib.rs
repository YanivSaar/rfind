//! rfind-zvfs — SQLite with zstd-compressed database pages.
//!
//! A VFS that keeps the main database file as compressed pages in an
//! append-only store (see [`store`]) while WAL, shared memory and locking
//! work exactly as in plain SQLite: one writer, any number of concurrent
//! readers, crash-safe through SQLite's own WAL.
//!
//! ```no_run
//! let conn = rfind_zvfs::open(std::path::Path::new("/var/lib/rfindd/index.db"),
//!     rusqlite::OpenFlags::default())?;
//! # Ok::<(), rusqlite::Error>(())
//! ```

pub mod store;
mod vfs;

use rusqlite::{Connection, OpenFlags};
use std::path::{Path, PathBuf};

pub use store::{exists as is_compressed, store_files};
pub use vfs::{register, VFS_NAME};

/// Open `path` through the compressing VFS.
pub fn open(path: &Path, flags: OpenFlags) -> rusqlite::Result<Connection> {
    register().map_err(|e| {
        rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_ERROR),
            Some(e),
        )
    })?;
    Connection::open_with_flags_and_vfs(path, flags, VFS_NAME)
}

/// Compact the store behind `conn` if old page versions take a significant
/// share of it — call after bulk writes and a WAL checkpoint. Does nothing
/// (and succeeds) for databases not using this VFS.
pub fn compact(conn: &Connection) -> rusqlite::Result<()> {
    let rc = unsafe {
        rusqlite::ffi::sqlite3_file_control(
            conn.handle(),
            c"main".as_ptr(),
            vfs::FCNTL_COMPACT,
            std::ptr::null_mut(),
        )
    };
    match rc {
        rusqlite::ffi::SQLITE_OK | rusqlite::ffi::SQLITE_NOTFOUND => Ok(()),
        rc => Err(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rc),
            None,
        )),
    }
}

/// Bytes the database occupies on disk: compressed store plus WAL.
pub fn disk_bytes(path: &Path) -> u64 {
    let size = |p: &Path| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
    // SQLite appends `-wal` to the full path, not after a split on extension:
    // `/foo/bar.db` → `/foo/bar.db-wal`, `/foo/bar` → `/foo/bar-wal`. The
    // old `with_extension` form produced `bar.-wal` for extension-less paths.
    let wal = PathBuf::from(format!("{}-wal", path.display()));
    store_files(path).iter().map(|p| size(p)).sum::<u64>() + size(&wal)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;

    fn fill(c: &Connection, n: i64) {
        c.execute_batch(
            "CREATE TABLE IF NOT EXISTS t (id INTEGER PRIMARY KEY, name TEXT, v INTEGER);
             CREATE INDEX IF NOT EXISTS t_name ON t (name);",
        )
        .unwrap();
        let tx = c.unchecked_transaction().unwrap();
        for i in 0..n {
            tx.execute(
                "INSERT INTO t (name, v) VALUES (?1, ?2)",
                params![
                    format!(
                        "/usr/lib/python3/site-packages/module_{}/file_{}.py",
                        i % 977,
                        i
                    ),
                    i
                ],
            )
            .unwrap();
        }
        tx.commit().unwrap();
    }

    #[test]
    fn database_round_trips_compressed_with_wal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        {
            let c = open(&path, OpenFlags::default()).unwrap();
            c.execute_batch("PRAGMA journal_mode=WAL;").unwrap();
            fill(&c, 50_000);
            c.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);").unwrap();
        }
        // The stub stays empty; the pages live compressed beside it.
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);
        assert!(is_compressed(&path));
        let c = open(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let (n, ok): (i64, String) = (
            c.query_row("SELECT COUNT(*) FROM t", [], |r| r.get(0))
                .unwrap(),
            c.query_row("PRAGMA integrity_check", [], |r| r.get(0))
                .unwrap(),
        );
        assert_eq!((n, ok.as_str()), (50_000, "ok"));
        let logical: i64 = c
            .query_row(
                "SELECT page_count * page_size FROM pragma_page_count, pragma_page_size",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let on_disk = disk_bytes(&path) as i64;
        assert!(
            on_disk * 2 < logical,
            "{on_disk} on disk for {logical} logical bytes"
        );
    }

    #[test]
    fn readers_see_checkpointed_data_while_the_writer_keeps_writing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        let w = open(&path, OpenFlags::default()).unwrap();
        w.execute_batch("PRAGMA journal_mode=WAL;").unwrap();
        fill(&w, 10_000);
        w.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);").unwrap();
        let r = open(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let count = |c: &Connection| {
            c.query_row("SELECT COUNT(*) FROM t", [], |r| r.get::<_, i64>(0))
                .unwrap()
        };
        assert_eq!(count(&r), 10_000);
        for round in 1..=5 {
            fill(&w, 10_000);
            w.execute_batch("PRAGMA wal_checkpoint(PASSIVE);").unwrap();
            assert_eq!(count(&r), 10_000 * (round + 1));
        }
        w.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);").unwrap();
        assert_eq!(count(&r), 60_000);
        let ok: String = r
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))
            .unwrap();
        assert_eq!(ok, "ok");
    }

    /// Child process for the crash test: write until killed.
    #[test]
    fn crash_writer_child() {
        let Ok(path) = std::env::var("RFIND_ZVFS_CRASH_DB") else {
            return;
        };
        let c = open(Path::new(&path), OpenFlags::default()).unwrap();
        c.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=100;")
            .unwrap();
        loop {
            fill(&c, 2_000);
        }
    }

    #[test]
    fn survives_being_killed_mid_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        let exe = std::env::current_exe().unwrap();
        for round in 0..5 {
            let mut child = std::process::Command::new(&exe)
                .args(["--exact", "tests::crash_writer_child", "--nocapture"])
                .env("RFIND_ZVFS_CRASH_DB", &path)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap();
            std::thread::sleep(std::time::Duration::from_millis(400 + 150 * round));
            child.kill().unwrap(); // SIGKILL: no cleanup at all
            child.wait().unwrap();
            let c = open(&path, OpenFlags::default()).unwrap();
            let ok: String = c
                .query_row("PRAGMA integrity_check", [], |r| r.get(0))
                .unwrap();
            assert_eq!(ok, "ok", "round {round}");
            let n: i64 = c
                .query_row("SELECT COUNT(*) FROM t", [], |r| r.get(0))
                .unwrap();
            assert!(
                n > 0 && n % 2_000 == 0,
                "round {round}: {n} rows (transactions are all-or-nothing)"
            );
        }
    }
}
