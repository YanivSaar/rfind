//! `rfind config …` — view and change /etc/rfindd/config.toml.
//!
//! Changes keep the file's comments, are validated before they are
//! written, and are written atomically. Afterwards rfindd is told to
//! reload (SIGHUP): it re-reads the config and rescans only what changed.
//!
//! `RFIND_CONFIG` overrides the config path (rfindd honors it too).

use crate::{daemon, fail, output::Out, CmdResult, Failed};
use clap::{Args, Subcommand};
use rfind_config::{
    edit::{self, normalize_path, ConfigFile, EditError, KEYS},
    Config, Problem,
};
use serde_json::{json, Value};
use std::{
    io::{self, BufRead, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    process::ExitCode,
};

#[derive(Args, Clone, Copy)]
pub struct ReloadOpt {
    /// Write the file but don't tell rfindd to reload it.
    #[arg(long)]
    no_reload: bool,
}

#[derive(Subcommand)]
pub enum ConfigCmd {
    /// Print the config file (--json: the effective settings).
    Show {
        /// Print the built-in defaults instead.
        #[arg(long)]
        defaults: bool,
    },
    /// Print the config file's path.
    Path,
    /// Validate the config file.
    Check,
    /// List the settings `get`/`set` understand, with current values.
    Keys,
    /// Print one setting, e.g. `rfind config get scan.workers`.
    Get { key: String },
    /// Change one setting, e.g. `rfind config set scan.workers 8`.
    Set {
        key: String,
        value: String,
        #[command(flatten)]
        reload: ReloadOpt,
    },
    /// Remove a setting from the file so its default applies.
    Unset {
        key: String,
        #[command(flatten)]
        reload: ReloadOpt,
    },
    /// Paths never indexed (and never descended into).
    #[command(subcommand)]
    Exclude(ListCmd),
    /// Live roots: scanned, then kept current by fanotify/inotify.
    #[command(subcommand)]
    Root(ListCmd),
    /// Periodic roots: rescanned on an interval, never monitored
    /// (Windows drives under WSL2, network shares).
    #[command(subcommand)]
    Periodic(PeriodicCmd),
    /// Open the config in $VISUAL / $EDITOR; it is validated before saving.
    Edit {
        #[command(flatten)]
        reload: ReloadOpt,
    },
    /// Replace the config with the defaults (keeps a .bak copy).
    Reset {
        /// Confirm.
        #[arg(long)]
        yes: bool,
        #[command(flatten)]
        reload: ReloadOpt,
    },
}

#[derive(Subcommand)]
pub enum ListCmd {
    /// Add one or more paths.
    Add {
        #[arg(required = true)]
        paths: Vec<String>,
        #[command(flatten)]
        reload: ReloadOpt,
    },
    /// Remove one or more paths.
    #[command(alias = "rm")]
    Remove {
        #[arg(required = true)]
        paths: Vec<String>,
        #[command(flatten)]
        reload: ReloadOpt,
    },
    /// List the configured paths.
    #[command(alias = "ls")]
    List,
}

#[derive(Subcommand)]
pub enum PeriodicCmd {
    /// Add periodic roots, or change their interval.
    Add {
        #[arg(required = true)]
        paths: Vec<String>,
        /// Hours between rescans.
        #[arg(long, default_value = "12", value_name = "HOURS")]
        every: u64,
        #[command(flatten)]
        reload: ReloadOpt,
    },
    /// Remove periodic roots.
    #[command(alias = "rm")]
    Remove {
        #[arg(required = true)]
        paths: Vec<String>,
        #[command(flatten)]
        reload: ReloadOpt,
    },
    /// List periodic roots and their intervals.
    #[command(alias = "ls")]
    List,
}

/// Config path for the CLI. Precedence:
///
/// 1. `RFIND_CONFIG` env var — explicit opt-in, used by tests and
///    operators who keep a config outside the standard locations.
/// 2. Per-user config at `$XDG_CONFIG_HOME/rfind/config.toml` if it
///    exists — written by `rfindd --user` on first run.
/// 3. System config `/etc/rfindd/config.toml` — written by the system
///    daemon; only root can usually read it.
///
/// The user-mode fallback was added in v1.10.1 so `rfind name foo`
/// run as a non-root user talks to the user daemon's index without
/// trying (and failing) to read the root-owned system config.
pub fn config_path() -> PathBuf {
    if let Some(v) = std::env::var_os("RFIND_CONFIG") {
        // When the CLI runs as root through sudo (SUDO_USER is set), the
        // RFIND_CONFIG value came from the invoking user's environment —
        // either because sudoers preserved it, or because `sudo -E` was
        // used. The daemon pins RFIND_CONFIG to /etc/rfindd/* for exactly
        // this reason; mirror that here so a value like /etc/shadow or
        // /etc/sudoers cannot be used as a `rfind config reset` target.
        // Interactive non-sudo root is still trusted (dev / CI setups).
        if is_privileged_via_sudo() {
            let p = PathBuf::from(&v);
            if !path_under_etc_rfindd(&p) {
                eprintln!(
                    "rfind: refusing RFIND_CONFIG={} under sudo: must be an absolute path under /etc/rfindd/",
                    p.display()
                );
                std::process::exit(2);
            }
            return p;
        }
        return PathBuf::from(v);
    }
    let user = rfind_config::user_paths::config_path();
    if user.is_file() {
        return user;
    }
    PathBuf::from(rfind_config::DEFAULT_CONFIG_PATH)
}

fn is_privileged_via_sudo() -> bool {
    (unsafe { libc::geteuid() } == 0) && std::env::var_os("SUDO_USER").is_some()
}

fn path_under_etc_rfindd(p: &Path) -> bool {
    p.is_absolute()
        && p.starts_with("/etc/rfindd/")
        && !p
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
}

fn edit_err(e: EditError) -> Failed {
    match &e {
        EditError::Write(_, io) | EditError::Read(_, io)
            if io.kind() == io::ErrorKind::PermissionDenied =>
        {
            fail(format!("{e}\n       Run it with sudo."))
        }
        _ => fail(e),
    }
}

fn open() -> Result<ConfigFile, Failed> {
    ConfigFile::open(&config_path()).map_err(edit_err)
}

fn print_problems(problems: &[Problem]) {
    for p in problems {
        eprintln!(
            "rfind: {}: {}",
            if p.error { "error" } else { "warning" },
            p.message
        );
    }
}

/// Save `file` and reload the daemon; `changes` describe what happened.
fn commit(out: &mut Out, file: &ConfigFile, changes: Vec<String>, reload: ReloadOpt) -> CmdResult {
    if changes.is_empty() {
        out.message("No changes.", json!({ "changed": false }));
        return Ok(ExitCode::SUCCESS);
    }
    let warnings = file.save().map_err(edit_err)?;
    print_problems(&warnings);
    apply(out, changes, reload)
}

/// Report changes and tell rfindd to reload.
fn apply(out: &mut Out, changes: Vec<String>, reload: ReloadOpt) -> CmdResult {
    let reloaded = if reload.no_reload {
        "not reloaded (--no-reload); apply with: sudo rfind reload".to_string()
    } else {
        match daemon::signal(libc::SIGHUP) {
            Ok(pid) => format!("rfindd (pid {pid}) is reloading"),
            Err(e) if daemon::rfindd_pid().is_none() => {
                format!("rfindd is not running; changes apply when it starts ({e})")
            }
            Err(e) => format!("could not reload rfindd: {e}"),
        }
    };
    let text = changes
        .iter()
        .map(|c| format!("✔ {c}"))
        .collect::<Vec<_>>()
        .join("\n")
        + &format!("\n  {reloaded}");
    out.message(
        &text,
        json!({ "changed": true, "changes": changes, "reload": reloaded }),
    );
    Ok(ExitCode::SUCCESS)
}

/// Normalized, de-duplicated paths from the command line.
fn paths(raw: &[String]) -> Result<Vec<PathBuf>, Failed> {
    let mut out: Vec<PathBuf> = Vec::new();
    for p in raw {
        let p = normalize_path(p).map_err(edit_err)?;
        if !out.contains(&p) {
            out.push(p);
        }
    }
    Ok(out)
}

fn warn_missing(p: &Path) {
    if !p.exists() {
        eprintln!("rfind: warning: {} does not exist (yet)", p.display());
    }
}

/// Value of a dotted key in the effective config, as JSON.
fn lookup(cfg: &Config, key: &str) -> Option<Value> {
    let mut v = serde_json::to_value(cfg).ok()?;
    for part in key.split('.') {
        v = v.get(part)?.clone();
    }
    Some(v)
}

fn plain(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(a) => a.iter().map(plain).collect::<Vec<_>>().join("\n"),
        other => other.to_string(),
    }
}

pub fn run(out: &mut Out, cmd: ConfigCmd) -> CmdResult {
    let path = config_path();
    match cmd {
        ConfigCmd::Path => {
            out.message(&path.display().to_string(), json!({ "path": path }));
            Ok(ExitCode::SUCCESS)
        }

        ConfigCmd::Show { defaults } => {
            let file = if defaults { None } else { Some(open()?) };
            let text = match &file {
                None => rfind_config::default_toml(),
                Some(_) if !path.exists() => {
                    format!(
                        "# {} does not exist; these defaults apply:\n{}",
                        path.display(),
                        rfind_config::default_toml()
                    )
                }
                Some(f) => f.text(),
            };
            let cfg = match &file {
                Some(f) => f.config().map_err(edit_err)?,
                None => Config::default(),
            };
            out.text_or_json(&text, &serde_json::to_value(cfg).unwrap_or_default());
            Ok(ExitCode::SUCCESS)
        }

        ConfigCmd::Check => {
            // Distinguish "no file yet" (fall back to defaults) from
            // "can't read it" (ENOENT vs EACCES): otherwise a non-root user
            // whose config file is 0o600 would always see the defaults
            // reported as valid even if the real file is broken.
            let text = match std::fs::read_to_string(&path) {
                Ok(t) => t,
                Err(e) if e.kind() == io::ErrorKind::NotFound => rfind_config::default_toml(),
                Err(e) => {
                    return Err(fail(format!("cannot read {}: {e}", path.display())));
                }
            };
            match edit::check_text(&path, &text) {
                Ok(warnings) => {
                    print_problems(&warnings);
                    out.message(
                        &format!("{} is valid", path.display()),
                        json!({ "valid": true, "warnings": warnings.iter().map(|w| &w.message).collect::<Vec<_>>() }),
                    );
                    Ok(ExitCode::SUCCESS)
                }
                Err(e) => {
                    if out.json {
                        out.message("", json!({ "valid": false, "error": e.to_string() }));
                    }
                    Err(fail(e))
                }
            }
        }

        ConfigCmd::Keys => {
            let cfg = open()?.config().map_err(edit_err)?;
            let mut text = String::new();
            let mut data = serde_json::Map::new();
            for (key, _, desc) in KEYS {
                let v = lookup(&cfg, key).unwrap_or(Value::Null);
                text += &format!("{key:<28} {:<26} {desc}\n", plain(&v));
                data.insert(key.to_string(), json!({ "value": v, "description": desc }));
            }
            text += "\nLists (use the subcommands):\n\
                     scan.roots                   rfind config root add|remove|list\n\
                     scan.exclude                 rfind config exclude add|remove|list\n\
                     scan.periodic_roots          rfind config periodic add|remove|list\n";
            out.text_or_json(&text, &Value::Object(data));
            Ok(ExitCode::SUCCESS)
        }

        ConfigCmd::Get { key } => {
            let cfg = open()?.config().map_err(edit_err)?;
            let v = lookup(&cfg, &key).ok_or_else(|| fail(EditError::UnknownKey(key.clone())))?;
            out.text_or_json(&format!("{}\n", plain(&v)), &v);
            Ok(ExitCode::SUCCESS)
        }

        ConfigCmd::Set { key, value, reload } => {
            let mut f = open()?;
            let before = f.config().map_err(edit_err)?;
            f.set(&key, &value).map_err(edit_err)?;
            let after = f.config().map_err(edit_err)?;
            let changes = if lookup(&before, &key) == lookup(&after, &key) {
                vec![]
            } else {
                vec![format!(
                    "{key} = {}",
                    plain(&lookup(&after, &key).unwrap_or_default())
                )]
            };
            commit(out, &f, changes, reload)
        }

        ConfigCmd::Unset { key, reload } => {
            let mut f = open()?;
            let changes = if f.unset(&key).map_err(edit_err)? {
                vec![format!("{key} reset to its default")]
            } else {
                vec![]
            };
            commit(out, &f, changes, reload)
        }

        ConfigCmd::Exclude(cmd) => list_cmd(out, cmd, "exclude", |p, added| {
            if added {
                format!(
                    "Excluded {} — its entries will be removed from the index",
                    p.display()
                )
            } else {
                format!("{} is no longer excluded — it will be indexed", p.display())
            }
        }),

        ConfigCmd::Root(cmd) => list_cmd(out, cmd, "roots", |p, added| {
            if added {
                format!(
                    "Added live root {} — it will be scanned and monitored",
                    p.display()
                )
            } else {
                format!(
                    "Removed live root {} — entries not under another root will be removed",
                    p.display()
                )
            }
        }),

        ConfigCmd::Periodic(cmd) => periodic_cmd(out, cmd),

        ConfigCmd::Edit { reload } => edit_cmd(out, &path, reload),

        ConfigCmd::Reset { yes, reload } => {
            if !yes {
                return Err(fail(format!(
                    "this replaces {} with the defaults (a copy is kept as {}.bak); re-run with --yes",
                    path.display(),
                    path.display()
                )));
            }
            if path.exists() {
                let bak = PathBuf::from(format!("{}.bak", path.display()));
                std::fs::copy(&path, &bak).map_err(|e| edit_err(EditError::Write(bak, e)))?;
            }
            edit::write_atomic(&path, rfind_config::default_toml().as_bytes()).map_err(edit_err)?;
            apply(
                out,
                vec![format!("{} reset to the defaults", path.display())],
                reload,
            )
        }
    }
}

fn list_cmd(
    out: &mut Out,
    cmd: ListCmd,
    key: &str,
    describe: impl Fn(&Path, bool) -> String,
) -> CmdResult {
    let mut f = open()?;
    match cmd {
        ListCmd::List => {
            let cfg = f.config().map_err(edit_err)?;
            let v = lookup(&cfg, &format!("scan.{key}")).unwrap_or(Value::Array(vec![]));
            out.text_or_json(&format!("{}\n", plain(&v)).replace("\n\n", "\n"), &v);
            Ok(ExitCode::SUCCESS)
        }
        ListCmd::Add { paths: raw, reload } => {
            // A file without the list uses the defaults: start from them.
            seed_defaults(&mut f, key)?;
            let mut changes = vec![];
            for p in paths(&raw)? {
                warn_missing(&p);
                if f.list_add("scan", key, &p) {
                    changes.push(describe(&p, true));
                }
            }
            commit(out, &f, changes, reload)
        }
        ListCmd::Remove { paths: raw, reload } => {
            seed_defaults(&mut f, key)?;
            let mut changes = vec![];
            for p in paths(&raw)? {
                if f.list_remove("scan", key, &p) {
                    changes.push(describe(&p, false));
                } else {
                    eprintln!("rfind: {} was not in scan.{key}", p.display());
                }
            }
            commit(out, &f, changes, reload)
        }
    }
}

/// If the file doesn't set `scan.<key>`, write the effective (default)
/// list first, so adding one path doesn't drop the defaults.
fn seed_defaults(f: &mut ConfigFile, key: &str) -> Result<(), Failed> {
    if !f.list("scan", key).is_empty() {
        return Ok(());
    }
    let cfg = f.config().map_err(edit_err)?;
    let current = if key == "exclude" {
        cfg.scan.exclude
    } else {
        cfg.scan.roots
    };
    for p in current {
        f.list_add("scan", key, &p);
    }
    Ok(())
}

fn periodic_cmd(out: &mut Out, cmd: PeriodicCmd) -> CmdResult {
    let mut f = open()?;
    match cmd {
        PeriodicCmd::List => {
            let cfg = f.config().map_err(edit_err)?;
            let text: String = cfg
                .scan
                .periodic_roots
                .iter()
                .map(|r| format!("{}  every {}h\n", r.path.display(), r.interval_hours))
                .collect();
            out.text_or_json(
                &text,
                &serde_json::to_value(&cfg.scan.periodic_roots).unwrap_or_default(),
            );
            Ok(ExitCode::SUCCESS)
        }
        PeriodicCmd::Add {
            paths: raw,
            every,
            reload,
        } => {
            let mut changes = vec![];
            for p in paths(&raw)? {
                warn_missing(&p);
                if f.periodic_set(&p, every) {
                    changes.push(format!(
                        "{} will be rescanned every {every}h (first scan runs in the background)",
                        p.display()
                    ));
                }
            }
            commit(out, &f, changes, reload)
        }
        PeriodicCmd::Remove { paths: raw, reload } => {
            let mut changes = vec![];
            for p in paths(&raw)? {
                if f.periodic_remove(&p) {
                    changes.push(format!("{} is no longer a periodic root", p.display()));
                } else {
                    eprintln!("rfind: {} is not a periodic root", p.display());
                }
            }
            commit(out, &f, changes, reload)
        }
    }
}

fn edit_cmd(out: &mut Out, path: &Path, reload: ReloadOpt) -> CmdResult {
    let original = std::fs::read_to_string(path).unwrap_or_else(|_| rfind_config::default_toml());

    // Random-name 0700 subdir in TMPDIR — mkdtemp atomic + unpredictable
    // so an unprivileged attacker can neither pre-create our dir (DoS)
    // nor win a symlink race on the editor's working file. tempdir
    // defaults to 0o700. Dropped on scope exit (deleted via Drop).
    let dir_guard = tempfile::Builder::new()
        .prefix("rfind-edit-")
        .tempdir()
        .map_err(|e| fail(format!("cannot create tempdir: {e}")))?;
    let dir = dir_guard.path().to_path_buf();
    let tmp = dir.join("config.toml");
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&tmp)
        .map_err(|e| fail(format!("cannot create {}: {e}", tmp.display())))?;
    f.write_all(original.as_bytes())
        .map_err(|e| fail(format!("cannot write {}: {e}", tmp.display())))?;
    drop(f);

    // Under sudo, follow the sudoedit convention: SUDO_EDITOR beats
    // VISUAL/EDITOR. SUDO_EDITOR is sanitized by sudo and names the
    // tool the admin configured for privileged edits, so a hostile
    // VISUAL/EDITOR carried from the invoking user's shell cannot
    // choose the root-executed binary.
    let editor = if is_privileged_via_sudo() {
        std::env::var("SUDO_EDITOR")
            .or_else(|_| std::env::var("VISUAL"))
            .or_else(|_| std::env::var("EDITOR"))
            .unwrap_or_else(|_| "vi".into())
    } else {
        std::env::var("VISUAL")
            .or_else(|_| std::env::var("EDITOR"))
            .unwrap_or_else(|_| "vi".into())
    };
    let result = loop {
        // Invoke the editor without shell re-parsing: `sh -c 'exec "$0" "$@"'`
        // runs $0 as a plain program argument, so a hostile EDITOR
        // (`sh -c "…" #`) cannot be smuggled through `sudo -E rfind config
        // edit` to run arbitrary shell as root.
        let status = std::process::Command::new("sh")
            .arg("-c")
            .arg(r#"exec "$0" "$@""#)
            .arg(&editor)
            .arg(&tmp)
            .status();
        if !status.is_ok_and(|s| s.success()) {
            break Err(fail(format!("editor '{editor}' failed; nothing saved")));
        }
        let text = std::fs::read_to_string(&tmp).unwrap_or_default();
        if text == original {
            out.message("No changes.", json!({ "changed": false }));
            break Ok(ExitCode::SUCCESS);
        }
        // Every Config field has #[serde(default)], so an empty TOML parses
        // as a valid default — an editor crash / `> file` typo / out-of-space
        // would silently wipe the admin's roots and excludes. Refuse.
        if text.trim().is_empty() && !original.trim().is_empty() {
            break Err(fail(format!(
                "editor returned an empty file; refusing to replace {} with defaults",
                path.display()
            )));
        }
        match edit::check_text(path, &text) {
            Ok(warnings) => {
                print_problems(&warnings);
                if let Err(e) = edit::write_atomic(path, text.as_bytes()) {
                    break Err(edit_err(e));
                }
                break apply(out, vec![format!("{} updated", path.display())], reload);
            }
            Err(e) => {
                eprintln!("rfind: {e}");
                eprint!("Edit again? [Y/n] ");
                let _ = io::stderr().flush();
                let mut answer = String::new();
                let _ = io::stdin().lock().read_line(&mut answer);
                if answer.trim().eq_ignore_ascii_case("n") {
                    break Err(fail("discarded; the config file was not changed"));
                }
            }
        }
    };
    drop(dir_guard); // explicit for clarity; TempDir drops the whole subtree
    result
}
