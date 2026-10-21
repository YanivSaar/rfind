//! Process-level chaos: spawn the real `rfindd` binary against a tempdir,
//! kill it hard, re-spawn, and assert the daemon's crash-recovery path
//! (missing clean-marker → `PRAGMA integrity_check` → open or rebuild)
//! produces a sound DB.
//!
//! Marked `#[ignore]` because it exec's the daemon binary, writes to tempdirs
//! and takes a few seconds per scenario. Run explicitly:
//!   cargo test -p rfindd --test chaos_proc -- --ignored
//!
//! Requires debug build (`cfg(debug_assertions)`); the env-based path
//! overrides this test relies on are compiled out of release binaries.

use rfind_core::DaemonPhase;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn rfindd_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_rfindd"))
}

/// Populate `root` with `n` small files across a few subdirectories so a
/// scan has something to work with — not so many that SIGKILL always lands
/// after the scan has already finished.
fn seed_tree(root: &Path, n: usize) {
    for i in 0..n {
        let sub = root.join(format!("d{:02}", i % 16));
        std::fs::create_dir_all(&sub).unwrap();
        let f = sub.join(format!("f{i}.txt"));
        std::fs::write(&f, format!("payload {i}\n")).unwrap();
    }
}

fn write_config(path: &Path, root: &Path, db: &Path) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    let toml = format!(
        r#"
[scan]
roots = ["{root}"]
exclude = []
workers = 2
reindex_interval_hours = 24
collect_mtime = true
io_uring = false
max_depth = 64

[index]
db_path = "{db}"
batch_size = 1000
compress = false

[monitor]
prefer_fanotify = false
inotify_fallback = false
watch_new_mounts = false

[search]
mode = "auto"
io_uring = false
threads = 1
"#,
        root = root.display(),
        db = db.display(),
    );
    let mut f = std::fs::File::create(path).unwrap();
    f.write_all(toml.as_bytes()).unwrap();
}

struct Harness {
    _dir: tempfile::TempDir,
    config: PathBuf,
    pidfile: PathBuf,
    progress: PathBuf,
    db: PathBuf,
}

impl Harness {
    fn new(file_count: usize) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        let root = base.join("root");
        let config = base.join("etc/config.toml");
        let pidfile = base.join("run/rfindd.pid");
        let progress = base.join("run/progress.json");
        let db = base.join("var/index.db");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(base.join("run")).unwrap();
        std::fs::create_dir_all(base.join("var")).unwrap();
        seed_tree(&root, file_count);
        write_config(&config, &root, &db);
        Harness {
            _dir: dir,
            config,
            pidfile,
            progress,
            db,
        }
    }

    fn spawn(&self) -> Child {
        Command::new(rfindd_bin())
            .env("RFIND_CONFIG", &self.config)
            .env("RFINDD_PIDFILE_PATH", &self.pidfile)
            .env("RFINDD_PROGRESS_PATH", &self.progress)
            // Keep the daemon quiet; set RUST_LOG=info to debug.
            .env(
                "RUST_LOG",
                std::env::var("RUST_LOG").unwrap_or_else(|_| "warn".into()),
            )
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn rfindd")
    }

    fn wait_for_phase<F>(&self, timeout: Duration, pred: F) -> Option<DaemonPhase>
    where
        F: Fn(&DaemonPhase) -> bool,
    {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if let Ok(text) = std::fs::read_to_string(&self.progress) {
                if let Ok(p) = serde_json::from_str::<serde_json::Value>(&text) {
                    if let Some(ph) = p.get("phase") {
                        let phase = if ph.is_string() {
                            match ph.as_str().unwrap() {
                                "Scanning" => DaemonPhase::Scanning,
                                "Reconciling" => DaemonPhase::Reconciling,
                                "Ready" => DaemonPhase::Ready,
                                _ => continue,
                            }
                        } else if let Some(obj) = ph.as_object() {
                            if obj.contains_key("Error") {
                                DaemonPhase::Error(String::new())
                            } else {
                                continue;
                            }
                        } else {
                            continue;
                        };
                        if pred(&phase) {
                            return Some(phase);
                        }
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        None
    }

    fn integrity_ok(&self) -> bool {
        if !self.db.exists() {
            return false;
        }
        let conn = match rusqlite::Connection::open(&self.db) {
            Ok(c) => c,
            Err(_) => return false,
        };
        let res: rusqlite::Result<String> =
            conn.query_row("PRAGMA integrity_check", [], |r| r.get(0));
        matches!(res, Ok(s) if s == "ok")
    }
}

fn sigkill(child: &mut Child) {
    unsafe {
        libc::kill(child.id() as libc::pid_t, libc::SIGKILL);
    }
    let _ = child.wait();
}

fn sigterm_and_wait(child: &mut Child) {
    unsafe {
        libc::kill(child.id() as libc::pid_t, libc::SIGTERM);
    }
    // Give it a chance to flush and remove the pidfile.
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if let Ok(Some(_)) = child.try_wait() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    sigkill(child);
}

#[test]
#[ignore]
fn sigkill_mid_scan_then_reopen_integrity_ok() {
    let h = Harness::new(4000);

    // First boot: wait for Scanning, then SIGKILL.
    let mut child = h.spawn();
    let phase = h.wait_for_phase(Duration::from_secs(30), |p| {
        matches!(p, DaemonPhase::Scanning | DaemonPhase::Ready)
    });
    assert!(phase.is_some(), "daemon never reached Scanning/Ready");
    sigkill(&mut child);

    // Second boot: missing clean marker triggers integrity_check. The DB
    // may or may not exist depending on whether sweep committed. Either
    // way the daemon must either recover or rebuild and reach Ready.
    let _ = std::fs::remove_file(&h.progress);
    let _ = std::fs::remove_file(&h.pidfile);
    let mut child2 = h.spawn();
    let phase2 = h.wait_for_phase(Duration::from_secs(60), |p| matches!(p, DaemonPhase::Ready));
    assert!(
        phase2.is_some(),
        "daemon did not reach Ready after crash recovery"
    );
    assert!(h.integrity_ok(), "post-recovery integrity_check failed");

    sigterm_and_wait(&mut child2);
}

#[test]
#[ignore]
fn clean_shutdown_leaves_marker_so_next_boot_skips_check() {
    let h = Harness::new(500);

    // Boot, wait for Ready, SIGTERM for clean shutdown.
    let mut child = h.spawn();
    assert!(
        h.wait_for_phase(Duration::from_secs(30), |p| matches!(p, DaemonPhase::Ready))
            .is_some(),
        "did not reach Ready on first boot"
    );
    sigterm_and_wait(&mut child);

    // Second boot: clean marker should be present → integrity_check path
    // skipped. We can't directly observe "skipped", but the DB must still
    // be sound and the daemon must reach Ready.
    let _ = std::fs::remove_file(&h.progress);
    let _ = std::fs::remove_file(&h.pidfile);
    let mut child2 = h.spawn();
    assert!(
        h.wait_for_phase(Duration::from_secs(30), |p| matches!(p, DaemonPhase::Ready))
            .is_some(),
        "did not reach Ready on second boot"
    );
    assert!(h.integrity_ok(), "DB integrity failed after clean restart");

    sigterm_and_wait(&mut child2);
}
