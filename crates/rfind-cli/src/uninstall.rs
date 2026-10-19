//! `rfind uninstall` — remove what scripts/install.sh installed.
//!
//! Paths are derived from where this binary lives (`<prefix>/bin/rfind`),
//! so a custom PREFIX is handled. `--purge` also removes the index and the
//! configuration.

use crate::{config_cmd::config_path, daemon, fail, output::Out, require_root, CmdResult};
use serde_json::json;
use std::{
    io::{self, BufRead, IsTerminal, Write},
    path::{Path, PathBuf},
    process::{Command, ExitCode},
    thread,
    time::Duration,
};

const UNIT: &str = "/etc/systemd/system/rfindd.service";

fn prefix() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent()?.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("/usr/local"))
}

/// Everything the installer may have put on the system.
fn installed_files(prefix: &Path) -> Vec<PathBuf> {
    let share = prefix.join("share");
    vec![
        PathBuf::from(UNIT),
        prefix.join("bin/rfindd"),
        prefix.join("bin/rfind"),
        share.join("man/man1/rfind.1"),
        share.join("man/man5/rfind.conf.5"),
        share.join("man/man8/rfindd.8"),
        share.join("bash-completion/completions/rfind"),
        share.join("zsh/site-functions/_rfind"),
        share.join("fish/vendor_completions.d/rfind.fish"),
        PathBuf::from("/etc/bash_completion.d/rfind"),
    ]
}

/// Only directories whose basename is `rfindd` or `rfind` are eligible for
/// purge: a misconfigured `index.db_path = /var/foo.db` or
/// `RFIND_CONFIG=/etc/myrfind.toml` must not let `--purge` take out `/var`
/// or `/etc`.
fn purge_paths() -> Vec<PathBuf> {
    let is_rfind_dir = |p: &Path| {
        p.file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n == "rfindd" || n == "rfind")
    };
    let cfg = rfind_config::load(&config_path()).unwrap_or_default();
    let mut v = vec![];
    if let Some(dir) = cfg.index.db_path.parent() {
        if is_rfind_dir(dir) {
            v.push(dir.to_path_buf());
        } else {
            eprintln!(
                "rfind: refusing to purge {} (not an rfind directory); keeping it",
                dir.display()
            );
        }
    }
    if let Some(dir) = config_path().parent() {
        if is_rfind_dir(dir) {
            v.push(dir.to_path_buf());
        } else {
            eprintln!(
                "rfind: refusing to purge {} (not an rfind directory); keeping it",
                dir.display()
            );
        }
    }
    v.push(PathBuf::from("/run/rfindd"));
    v
}

/// Poll /proc for rfindd exit, up to `secs` seconds (100 ms ticks).
/// Returns true once the daemon is gone.
fn wait_for_exit(secs: u64) -> bool {
    let ticks = secs * 10;
    for _ in 0..ticks {
        if daemon::rfindd_pid().is_none() {
            return true;
        }
        thread::sleep(Duration::from_millis(100));
    }
    daemon::rfindd_pid().is_none()
}

/// Drive the daemon to a stopped state no matter what, so file removal
/// below never races a live writer. See the ladder documented at the call
/// site in [`run`].
fn stop_daemon_with_escalation() {
    if wait_for_exit(10) {
        return;
    }
    if let Some(pid) = daemon::rfindd_pid() {
        eprintln!("rfind: rfindd (pid {pid}) still running after 10s; sending SIGTERM");
        unsafe {
            libc::kill(pid, libc::SIGTERM);
        }
    }
    if wait_for_exit(5) {
        return;
    }
    if let Some(pid) = daemon::rfindd_pid() {
        eprintln!("rfind: rfindd (pid {pid}) did not respond to SIGTERM; sending SIGKILL");
        unsafe {
            libc::kill(pid, libc::SIGKILL);
        }
    }
    if wait_for_exit(2) {
        return;
    }
    if let Some(pid) = daemon::rfindd_pid() {
        eprintln!(
            "rfind: WARNING: rfindd (pid {pid}) survived SIGKILL (likely stuck in a kernel \
             syscall); continuing with uninstall anyway"
        );
    }
}

fn confirm(prompt: &str) -> bool {
    if !io::stdin().is_terminal() {
        return false;
    }
    eprint!("{prompt} [y/N] ");
    let _ = io::stderr().flush();
    let mut answer = String::new();
    let _ = io::stdin().lock().read_line(&mut answer);
    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

pub fn run(out: &mut Out, purge: bool, yes: bool) -> CmdResult {
    require_root()?;
    let files: Vec<PathBuf> = installed_files(&prefix())
        .into_iter()
        .filter(|p| p.exists())
        .collect();
    let dirs: Vec<PathBuf> = if purge {
        purge_paths().into_iter().filter(|p| p.exists()).collect()
    } else {
        vec![]
    };

    if !yes {
        eprintln!("This will stop rfindd and remove:");
        for p in files.iter().chain(&dirs) {
            eprintln!("  {}", p.display());
        }
        if !purge {
            eprintln!("The index and config are kept (use --purge to remove them too).");
        }
        if !confirm("Continue?") {
            return Err(fail("cancelled (use --yes to skip this question)"));
        }
    }

    // Stop the daemon. Escalation ladder (so uninstall never hangs on a
    // wedged rfindd):
    //
    //   1. `systemctl disable` to remove the enablement (fast; does not stop).
    //   2. `systemctl stop --no-block` so the systemctl command returns
    //      immediately rather than waiting up to TimeoutStopSec (90s default)
    //      for the unit to deactivate.
    //   3. Poll /proc for exit with a 10 s grace period — if systemd (or the
    //      daemon's own signal handler) brings it down, we're done.
    //   4. Still alive → send SIGTERM directly, wait 5 s.
    //   5. Still alive → send SIGKILL directly, wait 2 s.
    //   6. Still alive after SIGKILL → warn and continue; the kernel can't
    //      kill it (D-state), which is a kernel-level problem the user needs
    //      to know about, not something uninstall can fix.
    let systemd = Path::new("/run/systemd/system").is_dir();
    if systemd && Path::new(UNIT).exists() {
        let _ = Command::new("systemctl")
            .args(["disable", "rfindd"])
            .status();
        let _ = Command::new("systemctl")
            .args(["stop", "--no-block", "rfindd"])
            .status();
    }
    stop_daemon_with_escalation();

    let mut removed = vec![];
    for p in &files {
        match std::fs::remove_file(p) {
            Ok(()) => removed.push(p.display().to_string()),
            Err(e) => eprintln!("rfind: could not remove {}: {e}", p.display()),
        }
    }
    for d in &dirs {
        match std::fs::remove_dir_all(d) {
            Ok(()) => removed.push(d.display().to_string()),
            Err(e) => eprintln!("rfind: could not remove {}: {e}", d.display()),
        }
    }
    if systemd {
        let _ = Command::new("systemctl").arg("daemon-reload").status();
    }

    let text = format!(
        "{}\n✔ rfind uninstalled{}",
        removed
            .iter()
            .map(|p| format!("  removed {p}"))
            .collect::<Vec<_>>()
            .join("\n"),
        if purge {
            " (index and config purged)"
        } else {
            "; index and config kept"
        }
    );
    out.message(&text, json!({ "removed": removed, "purged": purge }));
    Ok(ExitCode::SUCCESS)
}
