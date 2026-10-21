//! rfind — query the rfindd index and manage its configuration.
//!
//! Searching requires root: the index lists every file on the system.
//! Exit status: 0 = results found / success, 1 = no results, 2 = error.

mod config_cmd;
mod daemon;
mod mode;
mod output;
mod privacy_report;
mod uninstall;

use clap::{CommandFactory, Parser, Subcommand, ValueEnum};
use output::Out;
use rfind_config::load as load_config;
use rfind_core::{
    db,
    filters::{parse_size as parse_size_spec, parse_time, SizeFilter, SizeOp, TimeFilter},
    progress_file_path, DaemonPhase, ScanProgress,
};
use std::{
    fs,
    path::Path,
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
    // Previously `args_conflicts_with_subcommands = true` — tidy for the
    // bare-pattern form, but clap then rejects `rfind --json name foo`
    // (and anything else combining the global `--json` flag with a
    // subcommand), because the top-level `search: SearchArgs` conflicts
    // with every subcommand even though --json is `global = true`.
    // Dropping the conflict lets --json compose with subcommands; `main`
    // already routes on `command.is_some()` first, so a user giving both
    // a top-level pattern and a subcommand gets the subcommand's result.
    after_help = "\
Examples:
  sudo rfind passwd                        exact or glob name
  sudo rfind '*.conf' --under /etc         glob, restricted to a tree
  sudo rfind '^lib.*\\.so\\.[0-9]+$' --regex
  sudo rfind '/home/*/.ssh/*' --path       full-path glob
  sudo rfind --recent --hours 2 --type f   recently modified files
  sudo rfind '*.py' --largest --top 10     ten biggest python files
  sudo rfind '*.log' --recent --top 20 -l  twenty most recent logs, long
  sudo rfind --largest --under /var --top 10
  sudo rfind size /home                    disk usage from the index
  sudo rfind status                        daemon + index state
  sudo rfind config exclude add /home/moshe

Exit status: 0 results found, 1 no results, 2 error.
See rfind(1), rfindd(8) and rfind.conf(5)."
)]
struct Cli {
    /// Machine-readable output: one JSON object per result (JSON Lines),
    /// or a single object for status, stats and config.
    #[arg(long, global = true)]
    json: bool,

    /// Top-level search (used when no subcommand is given).
    #[command(flatten)]
    search: SearchArgs,

    #[command(subcommand)]
    command: Option<Command>,
}

/// Result filters and display options shared by the search commands.
#[derive(clap::Args, Clone, Default)]
struct FilterArgs {
    /// Only files (f), directories (d), symlinks (l), or any (any).
    /// Symlink and any filtering only applies in walk mode.
    #[arg(long, value_enum)]
    r#type: Option<EntryType>,
    /// Only files of at least SIZE (e.g. 500K, 10M, 1.5G).
    #[arg(long, value_name = "SIZE", value_parser = parse_size)]
    larger_than: Option<u64>,
    /// Only files of at most SIZE.
    #[arg(long, value_name = "SIZE", value_parser = parse_size)]
    smaller_than: Option<u64>,
    /// Size predicate in daviddl9 form: +N (larger), -N (smaller), N (exact).
    /// Units: c=byte, k/K=KiB, M=MiB, G=GiB, T=TiB; trailing B/iB accepted.
    #[arg(long, value_name = "SPEC", allow_hyphen_values = true, value_parser = parse_size_filter)]
    size: Option<SizeFilter>,
    /// mtime predicate: +N (older), -N (newer), N (exact). Units: s, m, h, d.
    #[arg(long, value_name = "SPEC", allow_hyphen_values = true, value_parser = parse_time_filter)]
    mtime: Option<TimeFilter>,
    /// atime predicate (walk mode only; not stored in the index).
    #[arg(long, value_name = "SPEC", allow_hyphen_values = true, value_parser = parse_time_filter)]
    atime: Option<TimeFilter>,
    /// ctime predicate (walk mode only; not stored in the index).
    #[arg(long, value_name = "SPEC", allow_hyphen_values = true, value_parser = parse_time_filter)]
    ctime: Option<TimeFilter>,
    /// Show size and modification time with each path.
    #[arg(short, long)]
    long: bool,
}

/// Merge legacy `--larger-than` / `--smaller-than` with `--size`:
/// AND their bounds. Returns an error only when bounds are contradictory
/// (e.g. min > max).
fn merge_size_bounds(
    larger_than: Option<u64>,
    smaller_than: Option<u64>,
    size: Option<SizeFilter>,
) -> Result<(Option<u64>, Option<u64>), Failed> {
    let (mut lo, mut hi) = (larger_than, smaller_than);
    if let Some(sf) = size {
        let (sf_lo, sf_hi) = sf.to_bounds();
        if let Some(x) = sf_lo {
            lo = Some(lo.map_or(x, |cur| cur.max(x)));
        }
        if let Some(x) = sf_hi {
            hi = Some(hi.map_or(x, |cur| cur.min(x)));
        }
    }
    if let (Some(l), Some(h)) = (lo, hi) {
        if l > h {
            return Err(fail(format!("size filters contradict: min {l} > max {h}")));
        }
    }
    Ok((lo, hi))
}

impl FilterArgs {
    /// Build a `db::Filter` honoring both the legacy and new size/time flags.
    /// Returns an error if size or mtime bounds are contradictory.
    fn filter(&self, now: i64) -> Result<db::Filter, Failed> {
        let (min_size, max_size) =
            merge_size_bounds(self.larger_than, self.smaller_than, self.size)?;
        let mtime_range = self.mtime.map(|tf| tf.to_range(now));
        Ok(db::Filter {
            is_dir: self.r#type.as_ref().and_then(|t| t.to_is_dir()),
            min_size,
            max_size,
            mtime_range,
        })
    }

    /// True iff a filter is set that the index can't currently answer
    /// (atime, ctime, type=l, type=any) — the mode selector routes these
    /// to walk mode.
    fn requires_walk(&self) -> bool {
        self.atime.is_some()
            || self.ctime.is_some()
            || matches!(self.r#type, Some(EntryType::Symlink) | Some(EntryType::Any))
    }
}

fn parse_size_filter(s: &str) -> Result<SizeFilter, String> {
    parse_size_spec(s)
}

fn parse_time_filter(s: &str) -> Result<TimeFilter, String> {
    parse_time(s)
}

/// Composable search: pattern + where + sort + top-N + time + filters.
/// The one source of truth for `rfind <PATTERN>` and the legacy
/// `name/path/recent/largest` subcommands (which build this and dispatch
/// to the same handler).
#[derive(clap::Args, Clone, Default)]
struct SearchArgs {
    /// Name or path pattern. Default: glob matched against the basename.
    /// Use --path to match the full path, --regex to use a regular
    /// expression. Omit when selecting with --largest, --recent, or a
    /// time filter alone.
    pattern: Option<String>,

    /// Treat PATTERN as a regular expression.
    #[arg(long)]
    regex: bool,

    /// Match PATTERN against the full path (`*` matches `/` too),
    /// not just the basename.
    #[arg(long = "path")]
    full_path: bool,

    /// Only search inside this directory tree.
    #[arg(long, value_name = "DIR")]
    under: Option<String>,

    /// Sort by size, biggest first.
    #[arg(long, conflicts_with = "recent")]
    largest: bool,

    /// Sort by modification time, newest first (implied by any time filter).
    #[arg(long)]
    recent: bool,

    /// Keep the N best (or first) results (synonym: --limit).
    #[arg(long, visible_alias = "limit", value_name = "N")]
    top: Option<usize>,

    /// Only entries modified in the last N minutes.
    #[arg(long, conflicts_with_all = ["hours", "days", "from"], value_name = "N")]
    minutes: Option<i64>,
    /// Only entries modified in the last N hours.
    #[arg(long, conflicts_with_all = ["minutes", "days", "from"], value_name = "N")]
    hours: Option<i64>,
    /// Only entries modified in the last N days.
    #[arg(long, conflicts_with_all = ["minutes", "hours", "from"], value_name = "N")]
    days: Option<i64>,
    /// Start of a date range (YYYY-MM-DD, local time).
    #[arg(long, requires = "to", value_name = "DATE")]
    from: Option<String>,
    /// End of a date range, inclusive (YYYY-MM-DD).
    #[arg(long, requires = "from", value_name = "DATE")]
    to: Option<String>,

    /// Walk the filesystem instead of querying the index.
    #[arg(long, visible_alias = "no-index")]
    live: bool,
    /// Terminate records with NUL (`\0`) instead of newline (xargs -0, jq -0).
    #[arg(long)]
    print0: bool,
    /// Walk-mode: maximum directory depth to descend (0 = roots only).
    #[arg(short = 'm', long, value_name = "N")]
    max_depth: Option<u32>,
    /// Walk-mode: worker thread count. 0 = cores × 2.
    #[arg(short = 'j', long, value_name = "N")]
    threads: Option<usize>,

    /// Walk-mode: never follow symlinks (default).
    #[arg(short = 'P', group = "symlinks")]
    no_follow: bool,
    /// Walk-mode: follow symlinks given as roots only.
    #[arg(short = 'H', group = "symlinks")]
    follow_roots: bool,
    /// Walk-mode: follow all symlinks (uses a visited set to break cycles).
    #[arg(short = 'L', group = "symlinks")]
    follow_all: bool,

    #[command(flatten)]
    filter: FilterArgs,
}

impl SearchArgs {
    /// Did the caller ask for anything? Bare `rfind` without args or
    /// subcommand prints help instead of listing every indexed file.
    fn is_empty(&self) -> bool {
        self.pattern.is_none()
            && !self.largest
            && !self.recent
            && self.minutes.is_none()
            && self.hours.is_none()
            && self.days.is_none()
            && self.from.is_none()
            && self.under.is_none()
            && self.filter.r#type.is_none()
            && self.filter.larger_than.is_none()
            && self.filter.smaller_than.is_none()
            && self.filter.size.is_none()
            && self.filter.mtime.is_none()
            && self.filter.atime.is_none()
            && self.filter.ctime.is_none()
    }

    /// Was any mtime-based time constraint given (legacy or new)?
    fn has_time_filter(&self) -> bool {
        self.minutes.is_some()
            || self.hours.is_some()
            || self.days.is_some()
            || (self.from.is_some() && self.to.is_some())
            || self.filter.mtime.is_some()
    }

    /// Resolve the mtime time window; None when no mtime filter is in play.
    /// Legacy flags (--minutes/--hours/--days/--from/--to) and the new
    /// --mtime are AND'd — the result is the intersection of the two
    /// ranges, with an error on a contradictory window.
    fn time_range(&self) -> Result<Option<(i64, i64)>, Failed> {
        let now = now();
        let mut window: Option<(i64, i64)> = None;
        let mut narrow = |next: (i64, i64)| {
            window = Some(match window {
                None => next,
                Some((lo, hi)) => (lo.max(next.0), hi.min(next.1)),
            });
        };
        if let (Some(f), Some(t)) = (&self.from, &self.to) {
            match (parse_date(f), parse_date_end(t)) {
                (Some(fs), Some(ts)) => narrow((fs, ts)),
                _ => return Err(fail("invalid date; use YYYY-MM-DD")),
            }
        } else if let Some(m) = self.minutes {
            narrow((now - m * 60, now));
        } else if let Some(h) = self.hours {
            narrow((now - h * 3600, now));
        } else if let Some(d) = self.days {
            narrow((now - d * 86400, now));
        } else if self.recent {
            narrow((now - 86400, now));
        }
        if let Some(mt) = self.filter.mtime {
            narrow(mt.to_range(now));
        }
        if let Some((lo, hi)) = window {
            if lo > hi {
                return Err(fail("time filters contradict (empty window)"));
            }
        }
        Ok(window)
    }

    /// Which symlink mode the user picked, or `P` (nofollow) by default.
    /// Only one of `-P`/`-H`/`-L` can be set thanks to the clap group.
    fn symlink_mode(&self) -> mode::SymlinkMode {
        if self.follow_all {
            mode::SymlinkMode::L
        } else if self.follow_roots {
            mode::SymlinkMode::H
        } else {
            mode::SymlinkMode::P
        }
    }
}

/// Parse a plain size like 4096, 500K, 10M, 1.5G, 2T (binary units; a
/// trailing "B"/"iB" is accepted). Delegates to the shared parser and
/// rejects a leading `+`/`-` — those belong to `--size`, not
/// `--larger-than` / `--smaller-than`.
fn parse_size(s: &str) -> Result<u64, String> {
    let sf = parse_size_spec(s)?;
    match sf.op {
        SizeOp::Exact => Ok(sf.bytes),
        SizeOp::Larger | SizeOp::Smaller => Err(format!(
            "'{s}' has a +/- prefix; use --size for that, not --larger-than / --smaller-than"
        )),
    }
}

#[derive(ValueEnum, Clone, Debug, PartialEq)]
enum EntryType {
    #[value(name = "f")]
    File,
    #[value(name = "d")]
    Dir,
    /// Symlink. Walk-mode only (the index does not store symlink type today).
    #[value(name = "l")]
    Symlink,
    /// Match any entry type. Walk-mode only.
    #[value(name = "any")]
    Any,
}

impl EntryType {
    /// Map to the index's `is_dir` filter. `Symlink` / `Any` return None and
    /// require walk mode (handled by the mode selector).
    fn to_is_dir(&self) -> Option<bool> {
        match self {
            EntryType::File => Some(false),
            EntryType::Dir => Some(true),
            EntryType::Symlink | EntryType::Any => None,
        }
    }
}

#[derive(Subcommand)]
enum Command {
    /// Legacy: `rfind name PAT` → `rfind PAT`. Same flags still work.
    #[command(name = "name", alias = "--name")]
    Name {
        pattern: String,
        #[arg(long, value_name = "DIR")]
        under: Option<String>,
        #[arg(long, visible_alias = "top", value_name = "N")]
        limit: Option<usize>,
        #[arg(long)]
        regex: bool,
        #[command(flatten)]
        filter: FilterArgs,
    },

    /// Legacy: `rfind path PAT` → `rfind PAT --path`.
    #[command(name = "path", alias = "--path")]
    Path {
        pattern: String,
        #[arg(long, visible_alias = "top", value_name = "N")]
        limit: Option<usize>,
        #[command(flatten)]
        filter: FilterArgs,
    },

    /// Legacy: `rfind recent` → `rfind --recent`. Same time flags apply.
    Recent {
        #[arg(long, conflicts_with_all = ["hours", "days", "from"])]
        minutes: Option<i64>,
        #[arg(long, conflicts_with_all = ["minutes", "days", "from"])]
        hours: Option<i64>,
        #[arg(long, conflicts_with_all = ["minutes", "hours", "from"])]
        days: Option<i64>,
        #[arg(long, requires = "to")]
        from: Option<String>,
        #[arg(long, requires = "from")]
        to: Option<String>,
        #[arg(long, visible_alias = "top", default_value = "50", value_name = "N")]
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

    /// Legacy: `rfind largest` → `rfind --largest`.
    Largest {
        #[arg(long, value_name = "DIR")]
        under: Option<String>,
        #[arg(long, visible_alias = "top", default_value = "20", value_name = "N")]
        limit: usize,
        #[command(flatten)]
        filter: FilterArgs,
    },

    /// Daemon + index state: phase, progress, freshness, monitoring,
    /// pending events, DB size, any background scan in flight.
    #[command(alias = "health")]
    Status {
        /// Refresh every second until the daemon is ready.
        #[arg(short, long)]
        watch: bool,
    },

    /// Index statistics: top file types, largest directories.
    Stats,

    /// Rescan now (live roots immediately, periodic roots in the background).
    Update,

    /// Make rfindd re-read its configuration (after editing it by hand).
    Reload,

    /// Show or change the configuration (/etc/rfindd/config.toml).
    #[command(subcommand)]
    Config(config_cmd::ConfigCmd),

    /// Audit scan.exclude against a built-in catalog of sensitive trees
    /// (SSH keys, keyrings, cloud creds, browser profiles, mail stores,
    /// container state, package caches). Exit 1 if any are missing.
    #[command(name = "privacy-report")]
    PrivacyReport,

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

/// Same as [`open_db`] but reuses an already-loaded `Config` to skip the
/// second file read. The index path is still enforced root-only.
fn open_db_with_cfg(cfg: &rfind_config::Config) -> Result<rusqlite::Connection, Failed> {
    require_root()?;
    db::open_readonly(&cfg.index.db_path).map_err(fail)
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

/// End-of-day for `--to`, DST-correct. Previously the caller added 86399 s
/// to local midnight; on a "spring forward" day the real end-of-day sits
/// at 86399 - 3600 seconds past midnight and the +86399 fudge rolls into
/// the next day.
fn parse_date_end(s: &str) -> Option<i64> {
    use chrono::TimeZone;
    let day = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()?;
    let eod = day.and_hms_opt(23, 59, 59)?;
    Some(
        chrono::Local
            .from_local_datetime(&eod)
            .latest()?
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

fn basename(path: &str) -> &str {
    Path::new(path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(path)
}

// ---------------------------------------------------------------------------
// Unified search
// ---------------------------------------------------------------------------

/// A compiled matcher derived from --regex/--path and the pattern.
enum PatternKind {
    None,
    NameGlob(String),
    PathGlob(String),
    NameRegex(regex_lite::Regex),
    PathRegex(regex_lite::Regex),
}

impl PatternKind {
    fn build(a: &SearchArgs) -> Result<Self, Failed> {
        match (&a.pattern, a.regex, a.full_path) {
            (None, _, _) => Ok(Self::None),
            (Some(p), true, true) => regex_lite::Regex::new(p)
                .map(Self::PathRegex)
                .map_err(|e| fail(format!("bad regex: {e}"))),
            (Some(p), true, false) => regex_lite::Regex::new(p)
                .map(Self::NameRegex)
                .map_err(|e| fail(format!("bad regex: {e}"))),
            (Some(p), false, true) => Ok(Self::PathGlob(p.clone())),
            (Some(p), false, false) => Ok(Self::NameGlob(p.clone())),
        }
    }

    /// True when `path` matches the pattern (or no pattern was given).
    fn matches(&self, path: &str) -> bool {
        match self {
            Self::None => true,
            Self::NameGlob(p) => db::glob_match(p, basename(path)),
            Self::PathGlob(p) => db::glob_match(p, path),
            Self::NameRegex(r) => r.is_match(basename(path)),
            Self::PathRegex(r) => r.is_match(path),
        }
    }
}

/// `rfind <PATTERN> [--largest|--recent] [filters] [--top N]`.
///
/// Dispatch strategy:
/// - time filter or --recent: `search_recent_streaming` + in-callback
///   pattern/under check; stops after `top` matches.
/// - --largest: `largest_files_streaming` + in-callback pattern check;
///   also stops after `top`. Unlimited at the SQL level so a tight
///   pattern does not miss big files past the SQL LIMIT.
/// - otherwise: the matching glob/regex/path streamer, which already
///   honours --under at the SQL level.
///
/// A thin shim dispatches between index mode and walk mode via the
/// mode selector; the walk branch never opens the index.
fn cmd_search(out: &mut Out, a: &SearchArgs) -> CmdResult {
    // --long is implied by any sort flag — the sort key is what the user
    // wants to see. Same with --print0.
    out.long = a.filter.long || a.largest;
    out.print0 = a.print0;

    let cfg = config()?;
    let request = mode::Request {
        live_flag: a.live,
        walk_only_filter: a.filter.requires_walk(),
        symlinks: a.symlink_mode(),
    };
    let env = mode::Env {
        caller_is_root: unsafe { libc::geteuid() } == 0,
        cfg: &cfg,
        index_available: mode::index_available(&cfg),
        daemon_ready: daemon::rfindd_pid().is_some(),
    };
    let (chosen, warn) = mode::choose_mode(&request, &env).map_err(fail)?;
    if let Some(msg) = warn {
        if !out.json {
            eprintln!("rfind: {msg}");
        }
    }
    match chosen {
        mode::Mode::Index => cmd_search_index(out, a, &cfg),
        mode::Mode::Walk => cmd_search_walk(out, a),
    }
}

/// Query the SQLite index (requires root). Previously the body of
/// `cmd_search`; split out so the walk branch can skip `open_db` entirely.
fn cmd_search_index(out: &mut Out, a: &SearchArgs, cfg: &rfind_config::Config) -> CmdResult {
    let conn = open_db_with_cfg(cfg)?;
    let filter = a.filter.filter(now())?;
    let kind = PatternKind::build(a)?;
    if a.recent || a.has_time_filter() {
        return search_by_time(out, &conn, a, &kind, &filter);
    }
    if a.largest {
        return search_largest(out, &conn, a, &kind, &filter);
    }
    search_by_pattern(out, &conn, a, &filter)
}

/// Walk the live filesystem instead of the index. Never opens the index
/// DB; works for any user. The predicate is composed from the usual
/// pattern/under/filter inputs plus the walk-only extras (atime, ctime,
/// --type l/any). `--top N` sets the scanner's cancel after N hits.
fn cmd_search_walk(out: &mut Out, a: &SearchArgs) -> CmdResult {
    use rfind_scanner::{
        run_search, EntryInfo, SearchConfig, STATX_ATIME, STATX_CTIME, STATX_MASK, STATX_MTIME,
        STATX_SIZE, STATX_TYPE,
    };
    use std::sync::{
        atomic::{AtomicBool, Ordering as AtomicOrdering},
        mpsc::sync_channel,
        Arc,
    };

    let now_s = now();
    let filter = a.filter.filter(now_s)?;
    let time_window = a.time_range()?;
    let kind = PatternKind::build(a)?;
    let under = a
        .under
        .clone()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let roots = vec![under];

    // Walk-only time bounds (atime/ctime) separate from the mtime window.
    let atime_range = a.filter.atime.map(|tf| tf.to_range(now_s));
    let ctime_range = a.filter.ctime.map(|tf| tf.to_range(now_s));

    // Narrow or widen the statx mask to only what the predicate reads.
    let needs_mtime = time_window.is_some();
    let needs_size = filter.min_size.is_some() || filter.max_size.is_some();
    let needs_atime = atime_range.is_some();
    let needs_ctime = ctime_range.is_some();
    let mut mask = STATX_TYPE;
    if needs_mtime {
        mask |= STATX_MTIME;
    }
    if needs_size {
        mask |= STATX_SIZE;
    }
    if needs_atime {
        mask |= STATX_ATIME;
    }
    if needs_ctime {
        mask |= STATX_CTIME;
    }
    // No filter actually needs mtime/size/atime/ctime → pure name query.
    // STATX_TYPE alone is enough; the scanner's needs_stat fast-path will
    // skip statx on known-type DT_REG entries.
    if mask == STATX_TYPE && !needs_mtime && !needs_size {
        // keep mask = STATX_TYPE
    } else if mask == STATX_TYPE {
        // Caller asked only for atime or ctime without mtime/size — unlikely
        // but fall back to the indexer's wide mask so the scanner always
        // has valid metadata to hand to the predicate.
        mask = STATX_MASK;
    }

    let want_type = a.filter.r#type.clone();
    let pattern = Arc::new(kind);
    let under_prefix_str = a.under.clone();
    let predicate_filter = filter;
    let predicate = {
        let pattern = Arc::clone(&pattern);
        move |e: &EntryInfo| {
            // --type: files, dirs, symlinks, or anything.
            if let Some(t) = &want_type {
                let ok = match t {
                    EntryType::File => !e.is_dir && !e.is_symlink,
                    EntryType::Dir => e.is_dir,
                    EntryType::Symlink => e.is_symlink,
                    EntryType::Any => true,
                };
                if !ok {
                    return false;
                }
            } else if e.is_dir {
                // Default (no --type): hide directories from walk output;
                // a pattern query usually means "find files named X". The
                // predicate still lets the scanner descend — this only
                // filters what the sink sees.
                return false;
            }
            // --under keeps paths within the subtree even after -L jumps.
            if let Some(u) = &under_prefix_str {
                if !under_prefix(e.path.to_string_lossy().as_ref(), u) {
                    return false;
                }
            }
            if !pattern.matches(&e.path.to_string_lossy()) {
                return false;
            }
            if !e.is_dir {
                if let Some(min) = predicate_filter.min_size {
                    if e.size < min {
                        return false;
                    }
                }
                if let Some(max) = predicate_filter.max_size {
                    if e.size > max {
                        return false;
                    }
                }
            }
            if let Some((lo, hi)) = time_window {
                if e.mtime_sec < lo || e.mtime_sec > hi {
                    return false;
                }
            }
            if let Some((lo, hi)) = atime_range {
                if e.atime_sec < lo || e.atime_sec > hi {
                    return false;
                }
            }
            if let Some((lo, hi)) = ctime_range {
                if e.ctime_sec < lo || e.ctime_sec > hi {
                    return false;
                }
            }
            true
        }
    };

    out.long = a.filter.long || a.largest;
    out.print0 = a.print0;

    // Streaming producer/consumer:
    //   workers → mpsc::sync_channel → main thread (owns `out`)
    // Bounded buffer gives backpressure if printing lags.
    let (tx, rx) = sync_channel::<EntryInfo>(1024);
    let cancel = Arc::new(AtomicBool::new(false));
    let hit_sink = move |e: EntryInfo| {
        let _ = tx.send(e);
    };

    let workers_opt = a.threads.unwrap_or(0);
    let workers = if workers_opt > 0 {
        workers_opt
    } else {
        num_cpus_default()
    };
    let cfg = SearchConfig {
        workers,
        max_depth: a.max_depth,
        symlinks: match a.symlink_mode() {
            mode::SymlinkMode::P => rfind_scanner::SymlinkMode::P,
            mode::SymlinkMode::H => rfind_scanner::SymlinkMode::H,
            mode::SymlinkMode::L => rfind_scanner::SymlinkMode::L,
        },
        statx_mask: mask,
        cancel: Arc::clone(&cancel),
        ..SearchConfig::new(roots, predicate, hit_sink)
    };

    // Scanner runs on a dedicated thread so the main thread can own `out`
    // and drain the receiver without lifetime acrobatics.
    let scan_handle = std::thread::spawn(move || run_search(cfg));

    let want = a.top.unwrap_or(usize::MAX);
    let mut n: u64 = 0;
    while let Ok(e) = rx.recv() {
        let path_str = e.path.to_string_lossy();
        let hit = db::Hit {
            path: path_str.as_ref(),
            is_dir: e.is_dir,
            mtime_sec: e.mtime_sec,
            size: e.size,
        };
        let keep = out.hit(&hit);
        n += 1;
        if !keep || n as usize >= want {
            cancel.store(true, AtomicOrdering::Relaxed);
            break;
        }
    }
    // Keep draining until the senders close (post-cancel). Discarded; the
    // sink still ran their predicate work but we don't print them.
    while rx.recv().is_ok() {}
    let _ = scan_handle.join();

    out.count(n);
    found(n)
}

/// Walk-mode default: cores × 2 (I/O-bound).
fn num_cpus_default() -> usize {
    std::thread::available_parallelism()
        .map(|p| p.get() * 2)
        .unwrap_or(4)
}

fn search_by_time(
    out: &mut Out,
    conn: &rusqlite::Connection,
    a: &SearchArgs,
    kind: &PatternKind,
    filter: &db::Filter,
) -> CmdResult {
    let (from_sec, to_sec) = a.time_range()?.unwrap_or_else(|| {
        let n = now();
        (n - 86400, n)
    });
    let want = a.top.unwrap_or(50);
    let under = a.under.as_deref();
    let mut matched = 0u64;
    db::search_recent_streaming(conn, from_sec, to_sec, filter, None, |e| {
        if !kind.matches(&e.full_path) {
            return true;
        }
        if let Some(u) = under {
            if !under_prefix(&e.full_path, u) {
                return true;
            }
        }
        matched += 1;
        let keep = out.recent(e);
        keep && (matched as usize) < want
    })
    .map_err(|e| fail(format!("search error: {e}")))?;
    out.count(matched);
    found(matched)
}

fn search_largest(
    out: &mut Out,
    conn: &rusqlite::Connection,
    a: &SearchArgs,
    kind: &PatternKind,
    filter: &db::Filter,
) -> CmdResult {
    let want = a.top.unwrap_or(20);
    // --type d switches to directory ranking: subtree size (du-style),
    // not the always-zero size field on dir rows.
    if filter.is_dir == Some(true) {
        return search_largest_dirs(out, conn, a, kind, filter, want);
    }
    let matches_everything = matches!(kind, PatternKind::None);
    // With no pattern we can let SQL do the LIMIT. With a pattern we
    // stream unbounded and stop after `want` matches.
    let sql_limit = if matches_everything { Some(want) } else { None };
    let mut matched = 0u64;
    db::largest_files_streaming(conn, a.under.as_deref(), filter, sql_limit, |h| {
        if !kind.matches(h.path) {
            return true;
        }
        matched += 1;
        let keep = out.hit(h);
        keep && (matched as usize) < want
    })
    .map_err(|e| fail(format!("search error: {e}")))?;
    out.count(matched);
    found(matched)
}

fn search_largest_dirs(
    out: &mut Out,
    conn: &rusqlite::Connection,
    a: &SearchArgs,
    kind: &PatternKind,
    filter: &db::Filter,
    want: usize,
) -> CmdResult {
    // When a pattern is given we rank first (cheap), then take the top
    // `want` matches by walking the ranked list.
    let matches_everything = matches!(kind, PatternKind::None);
    let sql_limit = if matches_everything { Some(want) } else { None };
    let mut matched = 0u64;
    db::largest_dirs_streaming(conn, a.under.as_deref(), filter, sql_limit, |h| {
        if !kind.matches(h.path) {
            return true;
        }
        matched += 1;
        let keep = out.hit(h);
        keep && (matched as usize) < want
    })
    .map_err(|e| fail(format!("search error: {e}")))?;
    out.count(matched);
    found(matched)
}

fn search_by_pattern(
    out: &mut Out,
    conn: &rusqlite::Connection,
    a: &SearchArgs,
    filter: &db::Filter,
) -> CmdResult {
    let p = a.pattern.as_deref();
    let under = a.under.as_deref();
    let limit = a.top;
    let emit = |h: &db::Hit| out.hit(h);

    let n = match (p, a.regex, a.full_path) {
        (None, _, _) => {
            // No pattern, no sort, no filter window — pointless and
            // potentially multi-million rows. Guard the user.
            return Err(fail(
                "nothing to search for; give a pattern or --largest/--recent/--since",
            ));
        }
        (Some(p), true, false) => {
            db::search_by_regex_streaming(conn, p, "basename", under, filter, limit, emit)
        }
        (Some(p), true, true) => {
            db::search_by_regex_streaming(conn, p, "full_path", under, filter, limit, emit)
        }
        (Some(p), false, true) => db::search_by_path_glob_streaming(conn, p, filter, limit, emit),
        (Some(p), false, false) => match under {
            Some(prefix) => db::search_under_streaming(conn, prefix, Some(p), filter, limit, emit),
            None => db::search_by_glob_streaming(conn, p, filter, limit, emit),
        },
    }
    .map_err(|e| fail(format!("search error: {e}")))?;
    out.count(n);
    found(n)
}

/// True when `path` starts with `prefix` on a path boundary.
fn under_prefix(path: &str, prefix: &str) -> bool {
    let p = prefix.trim_end_matches('/');
    if p.is_empty() {
        return true;
    }
    if let Some(rest) = path.strip_prefix(p) {
        rest.is_empty() || rest.starts_with('/')
    } else {
        false
    }
}

// ---------------------------------------------------------------------------
// Non-search commands
// ---------------------------------------------------------------------------

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

/// Combined daemon + index state. One command replaces the former
/// `status` (daemon progress file) and `health` (DB freshness).
fn cmd_status(out: &mut Out, watch: bool) -> CmdResult {
    require_root()?;
    let cfg = config()?;
    loop {
        if watch && !out.json {
            print!("\x1B[2J\x1B[1;1H");
        }
        let progress = load_progress();
        let health = db::open_readonly(&cfg.index.db_path)
            .ok()
            .and_then(|c| db::get_health(&c, &cfg.index.db_path).ok());
        let pid = daemon::rfindd_pid();
        if progress.is_none() && health.is_none() && pid.is_none() {
            if !watch {
                return Err(fail("no daemon state — is rfindd running?"));
            }
            eprintln!("Waiting for rfindd...");
        } else {
            out.status(progress.as_ref(), health.as_ref(), pid, now());
            let done = progress
                .as_ref()
                .map(|p| p.phase == DaemonPhase::Ready)
                .unwrap_or(true);
            if !watch || done {
                return Ok(ExitCode::SUCCESS);
            }
        }
        thread::sleep(Duration::from_secs(1));
    }
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

// ---------------------------------------------------------------------------
// Legacy subcommand adapters: build a SearchArgs and dispatch.
// ---------------------------------------------------------------------------

fn legacy_name(
    out: &mut Out,
    pattern: String,
    under: Option<String>,
    limit: Option<usize>,
    regex: bool,
    filter: FilterArgs,
) -> CmdResult {
    cmd_search(
        out,
        &SearchArgs {
            pattern: Some(pattern),
            regex,
            under,
            top: limit,
            filter,
            ..Default::default()
        },
    )
}

fn legacy_path(
    out: &mut Out,
    pattern: String,
    limit: Option<usize>,
    filter: FilterArgs,
) -> CmdResult {
    cmd_search(
        out,
        &SearchArgs {
            pattern: Some(pattern),
            full_path: true,
            top: limit,
            filter,
            ..Default::default()
        },
    )
}

#[allow(clippy::too_many_arguments)]
fn legacy_recent(
    out: &mut Out,
    minutes: Option<i64>,
    hours: Option<i64>,
    days: Option<i64>,
    from: Option<String>,
    to: Option<String>,
    limit: usize,
    filter: FilterArgs,
) -> CmdResult {
    cmd_search(
        out,
        &SearchArgs {
            recent: true,
            minutes,
            hours,
            days,
            from,
            to,
            top: Some(limit),
            filter,
            ..Default::default()
        },
    )
}

fn legacy_largest(
    out: &mut Out,
    under: Option<String>,
    limit: usize,
    filter: FilterArgs,
) -> CmdResult {
    cmd_search(
        out,
        &SearchArgs {
            largest: true,
            under,
            top: Some(limit),
            filter,
            ..Default::default()
        },
    )
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

fn main() -> ExitCode {
    let cli = Cli::parse();
    let mut out = Out::new(cli.json);
    let result = match cli.command {
        None => {
            if cli.search.is_empty() {
                // Bare `rfind` or `rfind --json`: show help on stderr,
                // exit 2 (clap-help convention).
                let _ = Cli::command().print_help();
                eprintln!();
                return ExitCode::from(2);
            }
            cmd_search(&mut out, &cli.search)
        }
        Some(cmd) => match cmd {
            Command::Name {
                pattern,
                under,
                limit,
                regex,
                filter,
            } => legacy_name(&mut out, pattern, under, limit, regex, filter),
            Command::Path {
                pattern,
                limit,
                filter,
            } => legacy_path(&mut out, pattern, limit, filter),
            Command::Recent {
                minutes,
                hours,
                days,
                from,
                to,
                limit,
                filter,
            } => legacy_recent(&mut out, minutes, hours, days, from, to, limit, filter),
            Command::Size { path, top } => cmd_size(&mut out, &path, top),
            Command::Largest {
                under,
                limit,
                filter,
            } => legacy_largest(&mut out, under, limit, filter),
            Command::Uninstall { purge, yes } => uninstall::run(&mut out, purge, yes),
            Command::Status { watch } => cmd_status(&mut out, watch),
            Command::Stats => cmd_stats(&mut out),
            Command::Update => cmd_signal(
                &mut out,
                libc::SIGUSR1,
                "Re-index triggered; follow it with: sudo rfind status --watch",
            ),
            Command::Reload => cmd_signal(&mut out, libc::SIGHUP, "Configuration reloaded"),
            Command::Config(cmd) => config_cmd::run(&mut out, cmd),
            Command::PrivacyReport => privacy_report::run(&mut out),
            Command::Completions { shell } => {
                clap_complete::generate(
                    shell,
                    &mut Cli::command(),
                    "rfind",
                    &mut std::io::stdout(),
                );
                Ok(ExitCode::SUCCESS)
            }
        },
    };
    result.unwrap_or(ExitCode::from(2))
}

#[cfg(test)]
mod tests {
    use super::{parse_size, under_prefix, Cli, Command};
    use clap::Parser;

    /// Regression: `rfind --json name <pat>` used to be rejected by clap
    /// because `args_conflicts_with_subcommands = true` made the top-level
    /// `--json` conflict with every subcommand, even though `--json` is
    /// `global = true`. The bench harness relied on this combination and
    /// silently reported "not within 20s" on every append — reads like a
    /// freshness gap, but was a CLI parse error.
    #[test]
    fn global_json_flag_composes_with_name_subcommand() {
        let cli = Cli::try_parse_from(["rfind", "--json", "name", "passwd"])
            .expect("--json + name subcommand must parse");
        assert!(cli.json);
        assert!(matches!(cli.command, Some(Command::Name { .. })));
    }

    #[test]
    fn global_json_flag_composes_with_path_subcommand() {
        let cli = Cli::try_parse_from(["rfind", "--json", "path", "/etc/*"])
            .expect("--json + path subcommand must parse");
        assert!(cli.json);
        assert!(matches!(cli.command, Some(Command::Path { .. })));
    }

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

    #[test]
    fn under_prefix_respects_path_boundary() {
        assert!(under_prefix("/etc/passwd", "/etc"));
        assert!(under_prefix("/etc/passwd", "/etc/"));
        assert!(under_prefix("/etc", "/etc"));
        assert!(!under_prefix("/etcetera/x", "/etc"));
        assert!(under_prefix("/anywhere", ""));
    }
}
