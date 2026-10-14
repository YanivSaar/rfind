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
    ]
}

fn purge_paths() -> Vec<PathBuf> {
    let cfg = rfind_config::load(&config_path()).unwrap_or_default();
    let mut v = vec![];
    if let Some(dir) = cfg.index.db_path.parent() {
        v.push(dir.to_path_buf());
    }
    if let Some(dir) = config_path().parent() {
        v.push(dir.to_path_buf());
    }
    v.push(PathBuf::from("/run/rfindd"));
    v
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

    // Stop the daemon.
    let systemd = Path::new("/run/systemd/system").is_dir();
    if systemd && Path::new(UNIT).exists() {
        let _ = Command::new("systemctl")
            .args(["disable", "--now", "rfindd"])
            .status();
    }
    if daemon::rfindd_pid().is_some() {
        let _ = daemon::signal(libc::SIGTERM);
        for _ in 0..50 {
            if daemon::rfindd_pid().is_none() {
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
    }

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
