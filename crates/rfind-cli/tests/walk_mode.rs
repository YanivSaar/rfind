//! End-to-end walk-mode tests. Drive the built `rfind` binary; no daemon,
//! no root required. Each test builds a tiny temp tree and asserts output.

use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

/// Build:
///   root/
///     a.txt         (file: "hello")
///     notes.log     (file: "log")
///     sub/
///       inner.log   (file: "deep")
///     link_rel -> sub
///     dangling -> nowhere
fn fixture() -> TempDir {
    let d = TempDir::new().unwrap();
    let r = d.path();
    fs::write(r.join("a.txt"), b"hello").unwrap();
    fs::write(r.join("notes.log"), b"log").unwrap();
    fs::create_dir(r.join("sub")).unwrap();
    fs::write(r.join("sub/inner.log"), b"deep").unwrap();
    symlink("sub", r.join("link_rel")).unwrap();
    symlink("nowhere", r.join("dangling")).unwrap();
    d
}

/// The debug `rfind` binary this test crate ships with.
fn bin() -> PathBuf {
    // $CARGO_BIN_EXE_<name> is set by cargo for integration tests when
    // the crate declares the binary.
    PathBuf::from(env!("CARGO_BIN_EXE_rfind"))
}

/// Run `rfind` and return (stdout bytes, stderr string, exit code).
fn run(args: &[&str], under: &Path) -> (Vec<u8>, String, i32) {
    let mut cmd = Command::new(bin());
    cmd.arg("--live");
    cmd.arg("--under").arg(under);
    for a in args {
        cmd.arg(a);
    }
    let output = cmd.output().expect("spawn rfind");
    let code = output.status.code().unwrap_or(-1);
    (
        output.stdout,
        String::from_utf8_lossy(&output.stderr).into_owned(),
        code,
    )
}

fn stdout_lines(bytes: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(bytes)
        .lines()
        .map(|s| s.to_string())
        .collect()
}

#[test]
fn plain_glob_matches_files_in_subtree() {
    let d = fixture();
    let (stdout, _err, code) = run(&["*.log"], d.path());
    let hits = stdout_lines(&stdout);
    assert!(hits.iter().any(|p| p.ends_with("notes.log")));
    assert!(hits.iter().any(|p| p.ends_with("sub/inner.log")));
    assert_eq!(code, 0);
}

#[test]
fn print0_separates_hits_with_nul() {
    let d = fixture();
    let (stdout, _err, _code) = run(&["*.log", "--print0"], d.path());
    // No newlines, trailing NUL on every record.
    assert!(!stdout.contains(&b'\n'));
    assert!(stdout.ends_with(b"\0"));
    let records: Vec<&[u8]> = stdout.split(|&b| b == 0).filter(|s| !s.is_empty()).collect();
    assert!(records
        .iter()
        .any(|r| std::str::from_utf8(r).unwrap().ends_with("notes.log")));
}

#[test]
fn type_l_finds_symlinks_including_dangling() {
    let d = fixture();
    let (stdout, _err, _code) = run(&["--type", "l"], d.path());
    let hits = stdout_lines(&stdout);
    assert!(hits.iter().any(|p| p.ends_with("link_rel")));
    assert!(hits.iter().any(|p| p.ends_with("dangling")));
}

#[test]
fn max_depth_one_hides_deeper_files() {
    let d = fixture();
    let (stdout, _err, _code) = run(&["*.log", "-m", "1"], d.path());
    let hits = stdout_lines(&stdout);
    assert!(hits.iter().any(|p| p.ends_with("notes.log")));
    assert!(!hits.iter().any(|p| p.ends_with("sub/inner.log")));
}

#[test]
fn top_n_limits_output() {
    let d = fixture();
    let (stdout, _err, _code) = run(&["*.log", "--top", "1"], d.path());
    assert_eq!(stdout_lines(&stdout).len(), 1);
}

#[test]
fn mtime_filter_honored() {
    let d = fixture();
    // Everything in the fixture was just created, so --mtime -1h must
    // match them all; --mtime +365d must match none.
    let (recent, _, _) = run(&["*.log", "--mtime", "-1h"], d.path());
    let (ancient, _, _) = run(&["*.log", "--mtime", "+365d"], d.path());
    assert!(!stdout_lines(&recent).is_empty());
    assert!(stdout_lines(&ancient).is_empty());
}

#[test]
fn size_filter_honored_both_syntaxes() {
    let d = fixture();
    // The three test files are tiny; +1k matches none, -1k matches all.
    for sz in &["+1k", "+1K"] {
        let (big, _, _) = run(&["*.log", "--size", sz], d.path());
        assert!(stdout_lines(&big).is_empty(), "size {} should match 0", sz);
    }
    let (small, _, _) = run(&["*.log", "--size", "-1k"], d.path());
    assert!(!stdout_lines(&small).is_empty());
    // Legacy syntax should still work.
    let (small_legacy, _, _) = run(&["*.log", "--smaller-than", "1k"], d.path());
    assert_eq!(
        stdout_lines(&small_legacy),
        stdout_lines(&small),
        "--smaller-than 1k and --size -1k must agree"
    );
}

#[test]
fn l_mode_terminates_on_cycle() {
    let d = fixture();
    // Add a cycle: sub/cycle -> ../sub
    symlink("../sub", d.path().join("sub/cycle")).unwrap();
    let (stdout, _err, code) = run(&["*.log", "-L"], d.path());
    // Deterministic completion is the real assertion; the test timeout
    // enforces that. Must find at least the direct hits.
    assert!(stdout_lines(&stdout)
        .iter()
        .any(|p| p.ends_with("inner.log")));
    assert_eq!(code, 0);
}

#[test]
fn json_output_shape_matches_index_mode() {
    let d = fixture();
    let (stdout, _err, _code) = run(&["*.log", "--json"], d.path());
    for line in stdout_lines(&stdout) {
        if line.is_empty() {
            continue;
        }
        let v: serde_json::Value = serde_json::from_str(&line).expect("json line");
        for key in &["path", "type", "size", "mtime"] {
            assert!(v.get(*key).is_some(), "missing {} in {}", key, line);
        }
    }
}

#[test]
fn empty_result_exits_with_code_one() {
    let d = fixture();
    let (stdout, _err, code) = run(&["no-such-pattern-exists"], d.path());
    assert!(stdout_lines(&stdout).is_empty());
    assert_eq!(code, 1);
}
