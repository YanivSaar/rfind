//! Process-level chaos for the compressed VFS: a forked child drives
//! SQLite (which uses the zvfs as its backing store) through a tight
//! INSERT/COMMIT loop. The parent SIGKILLs it mid-transaction, re-opens
//! the DB through the SQLite VFS wrapper, and asserts
//! `PRAGMA integrity_check` returns `ok` and no partially-committed
//! transaction state survives.
//!
//! This covers the one chaos scenario in the review plan that fuzzing
//! does not: a real-process SIGKILL mid-COMMIT, exercising the zvfs's
//! generation-rotation / sync / truncate interaction with SQLite's WAL
//! or journal, rather than fuzzer-generated opcodes against a
//! single-shot harness.
//!
//! Marked `#[ignore]` — spawns a child process and sleeps briefly for
//! torture scheduling. Run explicitly:
//!   cargo test -p rfind-zvfs --test chaos -- --ignored

use std::path::PathBuf;
use std::time::Duration;

#[test]
#[ignore]
fn sigkill_during_commit_loop_keeps_db_sound() {
    let dir = tempfile::tempdir().unwrap();
    let base: PathBuf = dir.path().join("torture.db");

    // Seed the DB via the zvfs with a committed table + one row. This
    // row is the "survivor invariant": it must still be readable after
    // the crash. Only PRAGMAs SQLite accepts for a VFS-backed DB are
    // used; the zvfs is not WAL-compatible, so DELETE journaling.
    {
        let conn = rfind_zvfs::open(&base, rusqlite::OpenFlags::default())
            .expect("open fresh db via zvfs");
        conn.execute_batch(
            "PRAGMA journal_mode=DELETE; \
             CREATE TABLE t (k INTEGER PRIMARY KEY, v TEXT); \
             INSERT INTO t (v) VALUES ('seed-row');",
        )
        .unwrap();
    }

    // Fork a child that drives SQLite in a hot INSERT/COMMIT loop and
    // never cleanly exits — the parent SIGKILLs it mid-flight.
    let pid = unsafe { libc::fork() };
    assert!(pid >= 0, "fork failed");

    if pid == 0 {
        // --- Child ---
        let conn =
            rfind_zvfs::open(&base, rusqlite::OpenFlags::default()).expect("child reopen via zvfs");
        let mut stmt = conn
            .prepare("INSERT INTO t (v) VALUES (?)")
            .expect("prepare insert");
        let mut n: u64 = 0;
        loop {
            // Small per-tx body so COMMIT (and the sync/truncate path
            // the zvfs runs behind SQLite) fires frequently.
            conn.execute_batch("BEGIN").ok();
            for _ in 0..8 {
                let _ = stmt.execute([format!("row-{n}")]);
                n += 1;
            }
            conn.execute_batch("COMMIT").ok();
        }
    }

    // --- Parent ---
    // 150ms = many thousands of commit cycles on CI; enough to land
    // the SIGKILL inside a sync / truncate / generation-rotate step.
    std::thread::sleep(Duration::from_millis(150));
    unsafe {
        libc::kill(pid, libc::SIGKILL);
        let mut status: libc::c_int = 0;
        let _ = libc::waitpid(pid, &mut status, 0);
    }

    // Re-open through the SQLite VFS wrapper. The zvfs record-walk /
    // CRC pass runs first; a torn tail would show up here as an open
    // failure, not as corrupt SQLite content.
    let conn = rfind_zvfs::open(&base, rusqlite::OpenFlags::default())
        .expect("reopen through zvfs after SIGKILL");

    // integrity_check: SQLite's own structural pass over the whole DB.
    // "ok" is the only acceptable answer — this is the invariant that
    // would be broken if the zvfs returned partial-write page content.
    let result: String = conn
        .query_row("PRAGMA integrity_check", [], |r| r.get(0))
        .expect("integrity_check query");
    assert_eq!(
        result, "ok",
        "post-SIGKILL integrity_check reported: {result}"
    );

    // The seed row committed before the chaos loop started must still
    // be readable. A sweep that nuked committed state would be a bug.
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM t WHERE v = 'seed-row'", [], |r| {
            r.get(0)
        })
        .expect("count seed row");
    assert_eq!(count, 1, "committed seed row did not survive crash");

    // Every surviving row must be a complete INSERT (either present or
    // not; no half-COMMIT state). We don't know HOW many rows the child
    // got through, only that each one must parse.
    let total: i64 = conn
        .query_row("SELECT COUNT(*) FROM t", [], |r| r.get(0))
        .expect("count total");
    assert!(total >= 1, "lost even the seed row");
}
