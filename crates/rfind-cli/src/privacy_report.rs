//! `rfind privacy-report`: audit the daemon's configured excludes against
//! a built-in catalog of sensitive trees (same categories as
//! `packaging/exclusions.d/`). Pure-read, no privilege change, no writes.
//!
//! Output: a table of (category, covered?, missing paths). `--json` emits
//! the same data for scripting.

use rfind_config::{load as load_config, Config, DEFAULT_CONFIG_PATH};
use serde::Serialize;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use crate::{fail, CmdResult, Out};

/// A sensitive-tree category: a human-readable name plus the home-relative
/// suffixes (joined under each user's home) and the absolute paths to
/// check for in the config excludes.
struct Category {
    name: &'static str,
    /// Suffixes joined under each discovered home (e.g. ".ssh").
    home_suffixes: &'static [&'static str],
    /// Absolute paths (checked as-is).
    abs_paths: &'static [&'static str],
}

const CATEGORIES: &[Category] = &[
    Category {
        name: "ssh keys",
        home_suffixes: &[".ssh"],
        abs_paths: &[],
    },
    Category {
        name: "gpg / keyring",
        home_suffixes: &[".gnupg", ".local/share/keyrings", ".config/keyringer"],
        abs_paths: &[],
    },
    Category {
        name: "cloud credentials",
        home_suffixes: &[".aws", ".config/gcloud", ".azure", ".kube"],
        abs_paths: &[],
    },
    Category {
        name: "password managers",
        home_suffixes: &[
            ".password-store",
            ".config/pass",
            ".config/Bitwarden",
            ".config/1Password",
        ],
        abs_paths: &[],
    },
    Category {
        name: "browser profiles",
        home_suffixes: &[
            ".mozilla",
            ".config/google-chrome",
            ".config/chromium",
            ".config/BraveSoftware",
        ],
        abs_paths: &[],
    },
    Category {
        name: "mail",
        home_suffixes: &[
            ".thunderbird",
            ".betterbird",
            ".config/akonadi",
            ".local/share/akonadi",
            ".local/share/evolution",
            ".cache/evolution",
            ".local/share/aerc",
            ".mu",
            ".notmuch",
            ".offlineimap",
            ".mail",
            "Mail",
            "Maildir",
            "mail",
        ],
        abs_paths: &[],
    },
    Category {
        name: "containers / sandboxes",
        home_suffixes: &[".local/share/containers", ".local/share/flatpak", "snap"],
        abs_paths: &[
            "/var/lib/docker",
            "/var/lib/containerd",
            "/var/lib/containers",
            "/var/lib/lxc",
            "/var/lib/lxd",
            "/var/lib/incus",
            "/var/lib/snapd",
            "/var/lib/flatpak",
        ],
    },
    Category {
        name: "package caches",
        home_suffixes: &[],
        abs_paths: &[
            "/var/cache/apt",
            "/var/cache/dnf",
            "/var/cache/pacman",
            "/var/cache/yum",
        ],
    },
];

/// Parse /etc/passwd for home directories we should audit. We take any
/// entry whose home is under /home or is exactly /root. UIDs are ignored:
/// the point is to catch sensitive trees that the daemon would walk,
/// which is a function of what lives on disk, not who owns it.
fn discover_homes() -> Vec<PathBuf> {
    let mut homes: BTreeSet<PathBuf> = BTreeSet::new();
    if let Ok(text) = std::fs::read_to_string("/etc/passwd") {
        for line in text.lines() {
            let fields: Vec<&str> = line.split(':').collect();
            if fields.len() >= 6 {
                let home = fields[5];
                if home == "/root" || home.starts_with("/home/") {
                    homes.insert(PathBuf::from(home));
                }
            }
        }
    }
    // Fall back to $HOME if /etc/passwd was unavailable or empty.
    if homes.is_empty() {
        if let Some(h) = std::env::var_os("HOME") {
            homes.insert(PathBuf::from(h));
        }
    }
    homes.into_iter().collect()
}

/// Does the config exclude `path` directly or via an ancestor exclude?
fn is_excluded(excludes: &[PathBuf], path: &Path) -> bool {
    excludes.iter().any(|ex| path.starts_with(ex))
}

/// Would this path even be reachable by the daemon? If no scanned root
/// is an ancestor (and the path itself is not a root), the daemon never
/// looks at it, so "missing exclude" is not actually a leak.
fn is_in_scope(roots: &[PathBuf], path: &Path) -> bool {
    roots
        .iter()
        .any(|r| path.starts_with(r) || r.starts_with(path))
}

#[derive(Serialize)]
struct CategoryReport {
    name: String,
    covered: Vec<String>,
    missing: Vec<String>,
    out_of_scope: Vec<String>,
}

#[derive(Serialize)]
struct Report {
    config_path: String,
    scan_roots: Vec<String>,
    homes_audited: Vec<String>,
    categories: Vec<CategoryReport>,
}

fn build_report(cfg: &Config, cfg_path: &Path) -> Report {
    let homes = discover_homes();
    let excludes = &cfg.scan.exclude;
    let roots = &cfg.scan.roots;

    let categories: Vec<CategoryReport> = CATEGORIES
        .iter()
        .map(|cat| {
            let mut covered = BTreeSet::new();
            let mut missing = BTreeSet::new();
            let mut oos = BTreeSet::new();
            // Per-home suffixes.
            for h in &homes {
                for suf in cat.home_suffixes {
                    let p = h.join(suf);
                    let s = p.display().to_string();
                    if !is_in_scope(roots, &p) {
                        oos.insert(s);
                    } else if is_excluded(excludes, &p) {
                        covered.insert(s);
                    } else {
                        missing.insert(s);
                    }
                }
            }
            // Absolute paths.
            for abs in cat.abs_paths {
                let p = PathBuf::from(abs);
                let s = p.display().to_string();
                if !is_in_scope(roots, &p) {
                    oos.insert(s);
                } else if is_excluded(excludes, &p) {
                    covered.insert(s);
                } else {
                    missing.insert(s);
                }
            }
            CategoryReport {
                name: cat.name.to_string(),
                covered: covered.into_iter().collect(),
                missing: missing.into_iter().collect(),
                out_of_scope: oos.into_iter().collect(),
            }
        })
        .collect();

    Report {
        config_path: cfg_path.display().to_string(),
        scan_roots: roots.iter().map(|p| p.display().to_string()).collect(),
        homes_audited: homes.iter().map(|p| p.display().to_string()).collect(),
        categories,
    }
}

fn status(c: &CategoryReport) -> &'static str {
    if !c.missing.is_empty() {
        "MISSING"
    } else if !c.covered.is_empty() {
        "COVERED"
    } else {
        "N/A"
    }
}

fn print_text(r: &Report) {
    println!("config:  {}", r.config_path);
    println!("roots:   {}", r.scan_roots.join(", "));
    println!("homes:   {}", r.homes_audited.join(", "));
    println!();
    println!("{:<24}  {:<8}  details", "category", "status");
    println!("{:-<24}  {:-<8}  {:-<40}", "", "", "");
    for c in &r.categories {
        println!(
            "{:<24}  {:<8}  {} covered, {} missing, {} out-of-scope",
            c.name,
            status(c),
            c.covered.len(),
            c.missing.len(),
            c.out_of_scope.len()
        );
        for m in &c.missing {
            println!("    ! {m}");
        }
    }
    println!();
    println!("Categories flagged MISSING have at least one sensitive path");
    println!("that lives inside a scan root and is not excluded. Templates");
    println!("in /usr/share/rfindd/exclusions.d/ (or packaging/exclusions.d/");
    println!("in the source tree) show the expected paths per category.");
}

pub fn run(out: &mut Out) -> CmdResult {
    let cfg_path = PathBuf::from(DEFAULT_CONFIG_PATH);
    let cfg = match load_config(&cfg_path) {
        Ok(c) => c,
        Err(e) => return Err(fail(format!("cannot read {}: {e}", cfg_path.display()))),
    };
    let report = build_report(&cfg, &cfg_path);
    let any_missing = report.categories.iter().any(|c| !c.missing.is_empty());

    if out.json {
        let s = serde_json::to_string_pretty(&report).unwrap_or_default();
        println!("{s}");
    } else {
        print_text(&report);
    }

    // Exit 1 if anything is missing, so this subcommand composes with
    // shell checks: `rfind privacy-report || notify-admin`.
    Ok(if any_missing {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_scope_matches_descendants_and_ancestors_of_roots() {
        let roots = vec![PathBuf::from("/home")];
        assert!(is_in_scope(&roots, Path::new("/home/alice/.ssh")));
        assert!(is_in_scope(&roots, Path::new("/")));
        assert!(!is_in_scope(&roots, Path::new("/var/lib/docker")));
    }

    #[test]
    fn excluded_matches_ancestor_rule() {
        let excludes = vec![PathBuf::from("/home/alice/.ssh")];
        assert!(is_excluded(&excludes, Path::new("/home/alice/.ssh")));
        assert!(is_excluded(
            &excludes,
            Path::new("/home/alice/.ssh/id_ed25519")
        ));
        assert!(!is_excluded(&excludes, Path::new("/home/alice")));
    }
}
