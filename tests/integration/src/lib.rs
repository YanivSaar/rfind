//! Integration tests for rfind search correctness.
//!
//! These tests require a running rfindd index at /var/lib/rfindd/index.db
//! They are marked #[ignore] by default and run explicitly with:
//!   cargo test -- --ignored
//! or in CI after the daemon has completed a scan.

#![cfg(test)]

use rfind_core::{db, db_path};

fn open() -> rusqlite::Connection {
    db::open_readonly(&db_path()).expect("Cannot open index database — is rfindd done scanning?")
}

fn search_by_glob(conn: &rusqlite::Connection, pattern: &str) -> rusqlite::Result<Vec<String>> {
    let mut v = Vec::new();
    db::search_by_glob_streaming(conn, pattern, &db::Filter::default(), None, |p| {
        v.push(p.path.to_string());
        true
    })?;
    Ok(v)
}

fn search_under(
    conn: &rusqlite::Connection,
    prefix: &str,
    name: Option<&str>,
) -> rusqlite::Result<Vec<String>> {
    let mut v = Vec::new();
    db::search_under_streaming(conn, prefix, name, &db::Filter::default(), None, |p| {
        v.push(p.path.to_string());
        true
    })?;
    Ok(v)
}

// -------------------------------------------------------------------------
// Exact match
// -------------------------------------------------------------------------

#[test]
#[ignore]
fn exact_match_passwd_returns_results() {
    let conn = open();
    let results = search_by_glob(&conn, "passwd").unwrap();
    assert!(!results.is_empty(), "Expected at least one 'passwd' entry");
}

#[test]
#[ignore]
fn exact_match_passwd_all_basenames_correct() {
    let conn = open();
    let results = search_by_glob(&conn, "passwd").unwrap();
    for path in &results {
        let basename = std::path::Path::new(path)
            .file_name()
            .unwrap()
            .to_str()
            .unwrap();
        assert_eq!(basename, "passwd", "Unexpected basename in: {path}");
    }
}

// -------------------------------------------------------------------------
// Case sensitivity
// -------------------------------------------------------------------------

#[test]
#[ignore]
fn case_sensitive_uppercase_returns_nothing() {
    let conn = open();
    let upper = search_by_glob(&conn, "PASSWD").unwrap();
    let mixed = search_by_glob(&conn, "Passwd").unwrap();
    assert!(
        upper.is_empty(),
        "PASSWD should return 0 results on Linux, got: {upper:?}"
    );
    assert!(
        mixed.is_empty(),
        "Passwd should return 0 results on Linux, got: {mixed:?}"
    );
}

#[test]
#[ignore]
fn case_sensitive_lowercase_returns_results() {
    let conn = open();
    let results = search_by_glob(&conn, "passwd").unwrap();
    assert!(!results.is_empty(), "passwd should return results");
}

// -------------------------------------------------------------------------
// Glob patterns
// -------------------------------------------------------------------------

#[test]
#[ignore]
fn glob_star_matches_extension() {
    let conn = open();
    let results = search_by_glob(&conn, "*.conf").unwrap();
    assert!(!results.is_empty(), "*.conf should match files");
    for path in &results {
        assert!(
            path.ends_with(".conf"),
            "Result does not end with .conf: {path}"
        );
    }
}

#[test]
#[ignore]
fn glob_question_mark_matches_single_char() {
    let conn = open();
    // "pass??" should match "passwd" (5 chars after "pass" — wait, passwd
    // is 6 chars: p-a-s-s-w-d, so "pass??" matches it)
    let results = search_by_glob(&conn, "pass??").unwrap();
    assert!(
        results.iter().any(|p| p.ends_with("/passwd")),
        "pass?? should match passwd, got: {results:?}"
    );
}

#[test]
#[ignore]
fn glob_no_match_returns_empty() {
    let conn = open();
    let results = search_by_glob(&conn, "this_file_xyz_does_not_exist_abc123").unwrap();
    assert!(
        results.is_empty(),
        "Expected no results for nonexistent name"
    );
}

// -------------------------------------------------------------------------
// Scoped search
// -------------------------------------------------------------------------

#[test]
#[ignore]
fn search_under_restricts_to_prefix() {
    let conn = open();
    let results = search_under(&conn, "/etc", Some("*.conf")).unwrap();
    assert!(!results.is_empty(), "Expected .conf files under /etc");
    for path in &results {
        assert!(path.starts_with("/etc/"), "Result outside /etc: {path}");
    }
}

#[test]
#[ignore]
fn search_under_nonexistent_dir_returns_empty() {
    let conn = open();
    let results = search_under(&conn, "/nonexistent_dir_xyz", None).unwrap();
    assert!(
        results.is_empty(),
        "Expected no results under nonexistent dir"
    );
}

#[test]
#[ignore]
fn search_under_case_sensitive() {
    let conn = open();
    let lower = search_under(&conn, "/etc", Some("passwd")).unwrap();
    let upper = search_under(&conn, "/etc", Some("PASSWD")).unwrap();
    assert!(!lower.is_empty(), "/etc/passwd should exist");
    assert!(
        upper.is_empty(),
        "PASSWD under /etc should not exist on Linux"
    );
}

// -------------------------------------------------------------------------
// Excluded paths never appear
// -------------------------------------------------------------------------

#[test]
#[ignore]
fn excluded_paths_not_indexed() {
    let conn = open();
    let cfg = rfind_config::load(std::path::Path::new(rfind_config::DEFAULT_CONFIG_PATH))
        .expect("Cannot read rfindd config");
    for prefix in &cfg.scan.exclude {
        let prefix = prefix.to_string_lossy();
        let results = search_under(&conn, &prefix, None).unwrap();
        assert!(
            results.is_empty(),
            "Excluded path {prefix} has {} indexed entries: {:?}",
            results.len(),
            &results[..results.len().min(3)]
        );
    }
}

// -------------------------------------------------------------------------
// Database integrity
// -------------------------------------------------------------------------

#[test]
#[ignore]
fn entry_count_is_nonzero() {
    let conn = open();
    let count = db::entry_count(&conn).unwrap();
    assert!(count > 0, "Index should have entries");
    println!("Total indexed entries: {count}");
}

#[test]
#[ignore]
fn all_entries_have_nonempty_name() {
    let conn = open();
    let bad: i64 = conn
        .query_row("SELECT COUNT(*) FROM entries WHERE name = ''", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(bad, 0, "{bad} entries have an empty name");
}

#[test]
#[ignore]
fn every_entry_has_a_parent_directory_row() {
    let conn = open();
    let orphans: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM entries e LEFT JOIN dirs d ON d.id = e.dir_id WHERE d.id IS NULL",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        orphans, 0,
        "{orphans} entries point at a missing directory row"
    );
}

#[test]
#[ignore]
fn directory_paths_are_normalized() {
    let conn = open();
    let bad: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM dirs WHERE path LIKE '%/' OR (path <> '' AND path NOT LIKE '/%')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        bad, 0,
        "{bad} directory paths have a trailing or missing leading '/'"
    );
}
