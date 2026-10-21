//! Chaos tests at the DB layer: simulate the state an abnormal rfindd
//! exit would leave, re-open, and assert invariants hold.
//!
//! Process-level SIGKILL tests (killing rfindd mid-scan) need runtime
//! path overrides the daemon doesn't expose yet; those scenarios are
//! queued for a later slice. The invariants exercised here are the
//! same ones that would be checked at the end of such a harness.
//!
//! Each test:
//!   1. Opens a fresh DB.
//!   2. Writes partial state (uncommitted tx / mid-scan / mid-apply).
//!   3. Drops the handle to simulate crash.
//!   4. Re-opens and asserts no partial state leaked and recovery is clean.

use rfind_core::db;
use rusqlite::Connection;

fn fresh_db() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("index.db");
    // Not compressed: SQLite's own WAL is enough for crash-recovery
    // invariants; zvfs crash recovery has its own test suite.
    let _ = db::open_with(&path, false).unwrap();
    (dir, path)
}

fn open(path: &std::path::Path) -> Connection {
    db::open_with(path, false).unwrap()
}

fn paths(conn: &Connection) -> Vec<String> {
    let mut v = Vec::new();
    db::search_under_streaming(conn, "/", None, &db::Filter::default(), None, |h| {
        v.push(h.path.to_string());
        true
    })
    .unwrap();
    v.sort();
    v
}

/// SIGKILL mid-scan: an uncommitted transaction that inserted some rows
/// at generation N must leave no trace after re-open. SQLite's WAL
/// handles this; the test pins down the invariant so a future
/// transaction-handling refactor can't silently regress it.
#[test]
fn uncommitted_scan_tx_leaves_no_rows_after_reopen() {
    let (_dir, path) = fresh_db();

    {
        let conn = open(&path);
        conn.execute_batch("BEGIN").unwrap();
        for p in ["/a/x", "/a/y", "/b/z"] {
            db::insert_entry(&conn, p, false, 0, 0, 0, Some(1)).unwrap();
        }
        // Drop without COMMIT — simulates SIGKILL.
        drop(conn);
    }

    let conn = open(&path);
    assert!(db::integrity_check(&conn).is_ok());
    assert!(
        paths(&conn).is_empty(),
        "uncommitted inserts must not survive"
    );
}

/// A committed scan generation that was partially swept by the next
/// scan before the daemon died: re-opening must leave the half-swept
/// state visible (we don't re-run the sweep on boot), AND re-running
/// the sweep must then finish the job without side effects.
#[test]
fn partial_sweep_can_be_resumed_cleanly() {
    let (_dir, path) = fresh_db();

    let conn = open(&path);
    for p in ["/a/x", "/a/y", "/b/z"] {
        db::insert_entry(&conn, p, false, 0, 0, 0, Some(1)).unwrap();
    }
    // Simulate the next scan starting: it would upsert the still-present
    // files at gen=2 and leave gone ones at gen=1. /a/y is gone.
    db::insert_entry(&conn, "/a/x", false, 0, 0, 0, Some(2)).unwrap();
    db::insert_entry(&conn, "/b/z", false, 0, 0, 0, Some(2)).unwrap();
    drop(conn);

    // Crash-and-reopen before the sweep ran.
    let conn = open(&path);
    assert!(db::integrity_check(&conn).is_ok());
    // Stale /a/y still visible (the sweep hadn't run yet). This is the
    // expected "slightly stale, never missing" invariant.
    let before = paths(&conn);
    assert!(before.contains(&"/a/y".to_string()));

    // Sweep: /a/y should drop; /a/x and /b/z stay.
    let n = db::sweep_stale(&conn, "/", &[], 2).unwrap();
    assert_eq!(n, 1, "exactly one stale row");
    let after = paths(&conn);
    assert_eq!(after, vec!["/a/x".to_string(), "/b/z".to_string()]);

    // Idempotent: a second sweep at the same gen is a no-op.
    let n2 = db::sweep_stale(&conn, "/", &[], 2).unwrap();
    assert_eq!(n2, 0);
    assert_eq!(paths(&conn), after);
}

/// Mid-apply crash in the event replay path. The journal records
/// pending events; the daemon walks `load_unapplied_events`, applies
/// each, and marks them applied. A SIGKILL between apply and mark must
/// leave the event in the "unapplied" set, and the next replay must
/// not duplicate the already-applied effect.
#[test]
fn event_replay_is_idempotent_across_a_crash() {
    use db::EventType;
    let (_dir, path) = fresh_db();

    let conn = open(&path);
    // Journal three creates.
    let ids: Vec<i64> = ["/a/x", "/a/y", "/b/z"]
        .iter()
        .map(|p| db::insert_pending_event(&conn, &EventType::Create, p, None, false, 0).unwrap())
        .collect();

    // Replay: apply all three, but mark only the first two before the
    // simulated crash.
    for p in ["/a/x", "/a/y", "/b/z"] {
        db::insert_entry(&conn, p, false, 0, 0, 0, None).unwrap();
    }
    db::mark_event_applied(&conn, ids[0]).unwrap();
    db::mark_event_applied(&conn, ids[1]).unwrap();
    // ids[2]: insert happened, mark did not — the "crash window".
    drop(conn);

    let conn = open(&path);
    assert!(db::integrity_check(&conn).is_ok());
    // All three entries present from the pre-crash apply.
    assert_eq!(paths(&conn), vec!["/a/x", "/a/y", "/b/z"]);
    // The third event is still unapplied in the journal.
    let unapplied = db::load_unapplied_events(&conn, 100).unwrap();
    assert_eq!(unapplied.len(), 1);
    assert_eq!(unapplied[0].id, ids[2]);
    assert_eq!(unapplied[0].path, "/b/z");

    // Replay the leftover: inserting /b/z again must not duplicate it
    // (upsert-on-conflict in insert_entry), and marking must drain the
    // unapplied set.
    db::insert_entry(&conn, &unapplied[0].path, false, 0, 0, 0, None).unwrap();
    db::mark_event_applied(&conn, unapplied[0].id).unwrap();
    assert_eq!(paths(&conn), vec!["/a/x", "/a/y", "/b/z"]);
    assert!(db::load_unapplied_events(&conn, 100).unwrap().is_empty());
}

/// Clean-shutdown-marker contract: a run that leaves no marker should
/// be treated as abnormal and the integrity_check path should pass on
/// a clean DB. Verifies the daemon's boot logic has something safe to
/// branch on (the actual branching is tested in rfindd::lifecycle).
#[test]
fn integrity_check_on_clean_db_passes() {
    let (_dir, path) = fresh_db();
    let conn = open(&path);
    for p in ["/a/x", "/a/y", "/b/z"] {
        db::insert_entry(&conn, p, false, 0, 0, 0, Some(1)).unwrap();
    }
    assert!(db::integrity_check(&conn).is_ok());
}
