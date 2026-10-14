//! rfind — query the rfindd index and manage its configuration.
//!
//! Searching requires root: the index lists every file on the system.
//! Exit status: 0 = results found / success, 1 = no results, 2 = error.

mod config_cmd;
mod daemon;
mod output;
mod uninstall;

use clap::{CommandFactory, Parser, Subcommand, ValueEnum};
use output::Out;
use rfind_config::load as load_config;
use rfind_core::{db, progress_file_path, DaemonPhase, ScanProgress};
use std::{
    fs,
    process::ExitCode,
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

// ---------------------------------------------------------------------------
// CLI definition
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(
    name = "rfind",
    version,
    about = "Fast filesystem search backed by the rfindd index",
    after_help = "\
Examples:
  sudo rfind name passwd                   exact file name
  sudo rfind name '*.conf' --under /etc    glob, restricted to a tree
  sudo rfind name '^lib.*\\.so\\.[0-9]+$' --regex
  sudo rfind path '/home/*/.ssh/*'         full-path glob
  sudo rfind recent --hours 2 --type f     recently modified files
  sudo rfind name '*.iso' --larger-than 1G -l
  sudo rfind size /home                    disk usage from the index
  sudo rfind largest --under /var --limit 10
  sudo rfind name '*.log' --json | jq -r .path
  sudo rfind config exclude add /home/moshe

Exit status: 0 results found, 1 no results, 2 error.
See rfind(1), rfindd(8) and rfind.conf(5)."
)]
struct Cli {
    /// Machine-readable output: one JSON object per result (JSON Lines),
    /// or a single object for status, health, stats and config.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Command,
}

/// Result filters and display options shared by the search commands.
#[derive(clap::Args, Clone, Default)]
struct FilterArgs {
    /// Only files (f) or only directories (d).
    #[arg(long, value_enum)]
    r#type: Option<EntryType>,
    /// Only files of at least SIZE (e.g. 500K, 10M, 1.5G).
    #[arg(long, value_name = "SIZE", value_parser = parse_size)]
    larger_than: Option<u64>,
    /// Only files of at most SIZE.
    #[arg(long, value_name = "SIZE", value_parser = parse_size)]
    smaller_than: Option<u64>,
    /// Show size and modification time with each path.
    #[arg(short, long)]
    long: bool,
}

impl FilterArgs {
    fn filter(&self) -> db::Filter {
        db::Filter {
            is_dir: self.r#type.as_ref().and_then(|t| t.to_is_dir()),
            min_size: self.larger_than,
            max_size: self.smaller_than,
        }
    }
}

/// Parse a size like 4096, 500K, 10M, 1.5G, 2T (binary units; a trailing
/// "B" or "iB" is accepted).
fn parse_size(s: &str) -> Result<u64, String> {
    let t = s
        .trim()
        .trim_end_matches(['B', 'b'])
        .trim_end_matches(['i', 'I']);
    let (num, mult) = match t.chars().last().map(|c| c.to_ascii_uppercase()) {
        Some('K') => (&t[..t.len() - 1], 1u64 << 10),
        Some('M') => (&t[..t.len() - 1], 1 << 20),
        Some('G') => (&t[..t.len() - 1], 1 << 30),
        Some('T') => (&t[..t.len() - 1], 1 << 40),
        _ => (t, 1),
    };
    let n: f64 = num
        .trim()
        .parse()
        .map_err(|_| format!("invalid size '{s}' (try 500K, 10M, 1.5G)"))?;
    if n < 0.0 {
        return Err(format!("invalid size '{s}'"));
    }
    Ok((n * mult as f64) as u64)
}

#[derive(ValueEnum, Clone, Debug)]
enum EntryType {
    #[value(name = "f")]
    File,
    #[value(name = "d")]
    Dir,
}

impl EntryType {
    fn to_is_dir(&self) -> Option<bool> {
        match self {
            EntryType::File => Some(false),
            EntryType::Dir => Some(true),
        }
    }
}

#[derive(Subcommand)]
enum Command {
    /// Search by file name: exact, glob (* ?), or regex.
    #[command(name = "name", alias = "--name")]
    Name {
        /// Name, glob, or (with --regex) regular expression.
        pattern: String,
        /// Only search inside this directory tree.
        #[arg(long, value_name = "DIR")]
        under: Option<String>,
        /// Stop after N results.
        #[arg(long, value_name = "N")]
        limit: Option<usize>,
        /// Treat PATTERN as a regular expression matched against names.
        #[arg(long)]
        regex: bool,
        #[command(flatten)]
        filter: FilterArgs,
    },

    /// Search by full path glob (* also matches '/').
    #[command(name = "path", alias = "--path")]
    Path {
        pattern: String,
        /// Stop after N results.
        #[arg(long, value_name = "N")]
        limit: Option<usize>,
        #[command(flatten)]
        filter: FilterArgs,
    },

    /// Recently modified entries, newest first (default: last 24 hours).
    Recent {
        /// Modified in the last N minutes.
        #[arg(long, conflicts_with_all = ["hours", "days", "from"])]
        minutes: Option<i64>,
        /// Modified in the last N hours.
        #[arg(long, conflicts_with_all = ["minutes", "days", "from"])]
        hours: Option<i64>,
        /// Modified in the last N days.
        #[arg(long, conflicts_with_all = ["minutes", "hours", "from"])]
        days: Option<i64>,
        /// Start of a date range (YYYY-MM-DD, local time).
        #[arg(long, requires = "to")]
        from: Option<String>,
        /// End of a date range, inclusive (YYYY-MM-DD).
        #[arg(long, requires = "from")]
        to: Option<String>,
        /// Maximum results.
        #[arg(long, default_value = "50", value_name = "N")]
        limit: usize,
        #[command(flatten)]
        filter: FilterArgs,
    },

    /// Disk usage of a directory tree, from the index (no disk walk).
    #[command(alias = "du")]
    Size {
        /// Directory (default: /).
        #[arg(default_value = "/")]
        path: String,
        /// Show the N largest children (0 = total only).
        #[arg(long, default_value = "20", value_name = "N")]
        top: usize,
    },

    /// Largest files, biggest first.
    Largest {
        /// Only inside this directory tree.
        #[arg(long, value_name = "DIR")]
        under: Option<String>,
        /// Number of files.
        #[arg(long, default_value = "20", value_name = "N")]
        limit: usize,
        #[command(flatten)]
        filter: FilterArgs,
    },

    /// Daemon status: scan phase and progress.
    Status {
        /// Refresh every second until the daemon is ready.
        #[arg(short, long)]
        watch: bool,
    },

    /// Index health: freshness, size, monitoring mode.
    Health,

    /// Index statistics: top file types, largest directories.
    Stats,

    /// Rescan now (live roots immediately, periodic roots in the background).
    Update,

    /// Make rfindd re-read its configuration (after editing it by hand).
    Reload,

    /// Show or change the configuration (/etc/rfindd/config.toml).
    #[command(subcommand)]
    Config(config_cmd::ConfigCmd),

    /// Remove rfind: service, binaries, man pages, completions.
    Uninstall {
        /// Also delete the index and the configuration.
        #[arg(long)]
        purge: bool,
        /// Don't ask for confirmation.
        #[arg(short, long)]
        yes: bool,
    },

    /// Print a shell completion script (bash, zsh, fish, elvish, powershell).
    #[command(after_help = "\
Install, e.g.:
  rfind completions bash | sudo tee /usr/share/bash-completion/completions/rfind
  rfind completions zsh  | sudo tee /usr/local/share/zsh/site-functions/_rfind
  rfind completions fish > ~/.config/fish/completions/rfind.fish")]
    Completions { shell: clap_complete::Shell },
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Failure that has already been reported to the user.
pub struct Failed;

pub type CmdResult = Result<ExitCode, Failed>;

pub fn fail(msg: impl std::fmt::Display) -> Failed {
    eprintln!("rfind: {msg}");
    Failed
}

pub fn require_root() -> Result<(), Failed> {
    if unsafe { libc::geteuid() } != 0 {
        eprintln!("rfind: this needs root — the index lists every file on the system.");
        eprintln!("       Run it with sudo, e.g.  sudo rfind name passwd");
        return Err(Failed);
    }
    Ok(())
}

/// The config, or a reported error: falling back to the defaults would
/// silently point rfind at a different index.
fn config() -> Result<rfind_config::Config, Failed> {
    let path = config_cmd::config_path();
    load_config(&path).map_err(|e| fail(format!("{e}\n       Check it with: rfind config check")))
}

/// Open the index read-only: searching never modifies it.
fn open_db() -> Result<rusqlite::Connection, Failed> {
    require_root()?;
    db::open_readonly(&config()?.index.db_path).map_err(fail)
}

fn load_progress() -> Option<ScanProgress> {
    let text = fs::read_to_string(progress_file_path()).ok()?;
    serde_json::from_str(&text).ok()
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// Parse YYYY-MM-DD into the Unix timestamp of local midnight that day
/// (results are displayed in local time too). None for invalid dates.
fn parse_date(s: &str) -> Option<i64> {
    use chrono::TimeZone;
    let day = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()?;
    let midnight = day.and_hms_opt(0, 0, 0)?;
    Some(
        chrono::Local
            .from_local_datetime(&midnight)
            .earliest()?
            .timestamp(),
    )
}

fn found(n: u64) -> CmdResult {
    Ok(if n > 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

fn cmd_name(
    out: &mut Out,
    pattern: &str,
    under: Option<&str>,
    f: &FilterArgs,
    limit: Option<usize>,
    regex: bool,
) -> CmdResult {
    let conn = open_db()?;
    let ty = &f.filter();
    out.long = f.long;
    let emit = |h: &db::Hit| out.hit(h);
    let result = if regex {
        db::search_by_regex_streaming(&conn, pattern, "basename", under, ty, limit, emit)
    } else if let Some(prefix) = under {
        db::search_under_streaming(&conn, prefix, Some(pattern), ty, limit, emit)
    } else {
        db::search_by_glob_streaming(&conn, pattern, ty, limit, emit)
    };
    let n = result.map_err(|e| fail(format!("search error: {e}")))?;
    out.count(n);
    found(n)
}

fn cmd_path(out: &mut Out, pattern: &str, f: &FilterArgs, limit: Option<usize>) -> CmdResult {
    let conn = open_db()?;
    out.long = f.long;
    let n = db::search_by_path_glob_streaming(&conn, pattern, &f.filter(), limit, |h| out.hit(h))
        .map_err(|e| fail(format!("search error: {e}")))?;
    out.count(n);
    found(n)
}

#[allow(clippy::too_many_arguments)]
fn cmd_recent(
    out: &mut Out,
    minutes: Option<i64>,
    hours: Option<i64>,
    days: Option<i64>,
    from: Option<String>,
    to: Option<String>,
    f: &FilterArgs,
    limit: usize,
) -> CmdResult {
    let conn = open_db()?;
    let now = now();
    let (from_sec, to_sec) = if let (Some(f), Some(t)) = (from, to) {
        match (parse_date(&f), parse_date(&t)) {
            (Some(fs), Some(ts)) => (fs, ts + 86399),
            _ => return Err(fail("invalid date; use YYYY-MM-DD")),
        }
    } else if let Some(m) = minutes {
        (now - m * 60, now)
    } else if let Some(h) = hours {
        (now - h * 3600, now)
    } else if let Some(d) = days {
        (now - d * 86400, now)
    } else {
        (now - 86400, now)
    };
    let n = db::search_recent_streaming(&conn, from_sec, to_sec, &f.filter(), Some(limit), |e| {
        out.recent(e)
    })
    .map_err(|e| fail(format!("search error: {e}")))?;
    out.count(n);
    found(n)
}

fn cmd_size(out: &mut Out, path: &str, top: usize) -> CmdResult {
    let conn = open_db()?;
    let (total, children) = db::usage(&conn, path, top).map_err(|e| fail(format!("size: {e}")))?;
    if total.files == 0 && total.dirs == 0 {
        return Err(fail(format!(
            "nothing indexed under {path} (excluded, or not a directory?)"
        )));
    }
    out.usage(&total, &children);
    Ok(ExitCode::SUCCESS)
}

fn cmd_largest(out: &mut Out, under: Option<&str>, f: &FilterArgs, limit: usize) -> CmdResult {
    let conn = open_db()?;
    out.long = true; // the size is the point here
    let n = db::largest_files_streaming(&conn, under, &f.filter(), Some(limit), |h| out.hit(h))
        .map_err(|e| fail(format!("search error: {e}")))?;
    out.count(n);
    found(n)
}

fn cmd_status(out: &mut Out, watch: bool) -> CmdResult {
    require_root()?;
    loop {
        if watch && !out.json {
            print!("\x1B[2J\x1B[1;1H");
        }
        match load_progress() {
            Some(p) => {
                out.progress(&p);
                if !watch || p.phase == DaemonPhase::Ready {
                    return Ok(ExitCode::SUCCESS);
                }
            }
            None if !watch => return Err(fail("no status — is rfindd running?")),
            None => eprintln!("Waiting for rfindd..."),
        }
        thread::sleep(Duration::from_secs(1));
    }
}

fn cmd_health(out: &mut Out) -> CmdResult {
    let conn = open_db()?;
    let cfg = config()?;
    let h = db::get_health(&conn, &cfg.index.db_path).map_err(|e| fail(format!("health: {e}")))?;
    out.health(&h, now(), daemon::rfindd_pid());
    Ok(ExitCode::SUCCESS)
}

fn cmd_stats(out: &mut Out) -> CmdResult {
    let conn = open_db()?;
    let cfg = config()?;
    let s = db::get_stats(&conn, &cfg.index.db_path).map_err(|e| fail(format!("stats: {e}")))?;
    out.stats(&s);
    Ok(ExitCode::SUCCESS)
}

fn cmd_signal(out: &mut Out, sig: libc::c_int, what: &str) -> CmdResult {
    require_root()?;
    let pid = daemon::signal(sig).map_err(fail)?;
    out.message(
        &format!("{what} (rfindd pid {pid})"),
        serde_json::json!({ "pid": pid }),
    );
    Ok(ExitCode::SUCCESS)
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let mut out = Out::new(cli.json);
    let result = match cli.command {
        Command::Name {
            pattern,
            under,
            limit,
            regex,
            filter,
        } => cmd_name(&mut out, &pattern, under.as_deref(), &filter, limit, regex),
        Command::Path {
            pattern,
            limit,
            filter,
        } => cmd_path(&mut out, &pattern, &filter, limit),
        Command::Recent {
            minutes,
            hours,
            days,
            from,
            to,
            limit,
            filter,
        } => {
            out.long = filter.long;
            cmd_recent(&mut out, minutes, hours, days, from, to, &filter, limit)
        }
        Command::Size { path, top } => cmd_size(&mut out, &path, top),
        Command::Largest {
            under,
            limit,
            filter,
        } => cmd_largest(&mut out, under.as_deref(), &filter, limit),
        Command::Uninstall { purge, yes } => uninstall::run(&mut out, purge, yes),
        Command::Status { watch } => cmd_status(&mut out, watch),
        Command::Health => cmd_health(&mut out),
        Command::Stats => cmd_stats(&mut out),
        Command::Update => cmd_signal(
            &mut out,
            libc::SIGUSR1,
            "Re-index triggered; follow it with: sudo rfind status --watch",
        ),
        Command::Reload => cmd_signal(&mut out, libc::SIGHUP, "Configuration reloaded"),
        Command::Config(cmd) => config_cmd::run(&mut out, cmd),
        Command::Completions { shell } => {
            clap_complete::generate(shell, &mut Cli::command(), "rfind", &mut std::io::stdout());
            Ok(ExitCode::SUCCESS)
        }
    };
    result.unwrap_or(ExitCode::from(2))
}

#[cfg(test)]
mod tests {
    use super::parse_size;

    #[test]
    fn sizes_parse_with_binary_units() {
        assert_eq!(parse_size("4096"), Ok(4096));
        assert_eq!(parse_size("500K"), Ok(500 * 1024));
        assert_eq!(parse_size("10M"), Ok(10 << 20));
        assert_eq!(parse_size("1.5G"), Ok(3 << 29));
        assert_eq!(parse_size("2TiB"), Ok(2 << 40));
        assert_eq!(parse_size("10mb"), Ok(10 << 20));
        assert!(parse_size("lots").is_err());
    }
}
