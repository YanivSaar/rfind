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
    args_conflicts_with_subcommands = true,
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
    }

    /// Was any time constraint given?
    fn has_time_filter(&self) -> bool {
        self.minutes.is_some()
            || self.hours.is_some()
            || self.days.is_some()
            || (self.from.is_some() && self.to.is_some())
    }

    /// Resolve the time window; None when no time filter is in play.
    fn time_range(&self) -> Result<Option<(i64, i64)>, Failed> {
        let now = now();
        if let (Some(f), Some(t)) = (&self.from, &self.to) {
            match (parse_date(f), parse_date_end(t)) {
                (Some(fs), Some(ts)) => Ok(Some((fs, ts))),
                _ => Err(fail("invalid date; use YYYY-MM-DD")),
            }
        } else if let Some(m) = self.minutes {
            Ok(Some((now - m * 60, now)))
        } else if let Some(h) = self.hours {
            Ok(Some((now - h * 3600, now)))
        } else if let Some(d) = self.days {
            Ok(Some((now - d * 86400, now)))
        } else if self.recent {
            Ok(Some((now - 86400, now))) // default: last 24h
        } else {
            Ok(None)
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
    let bytes = n * mult as f64;
    if !bytes.is_finite() || bytes > u64::MAX as f64 {
        return Err(format!("size '{s}' is too large"));
    }
    Ok(bytes as u64)
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
fn cmd_search(out: &mut Out, a: &SearchArgs) -> CmdResult {
    let conn = open_db()?;
    let filter = a.filter.filter();
    let kind = PatternKind::build(a)?;

    // --long is implied by any sort flag — the sort key is what the user
    // wants to see.
    out.long = a.filter.long || a.largest;

    if a.recent || a.has_time_filter() {
        return search_by_time(out, &conn, a, &kind, &filter);
    }
    if a.largest {
        return search_largest(out, &conn, a, &kind, &filter);
    }
    search_by_pattern(out, &conn, a, &filter)
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
    let sql_limit = if matches_everything {
        Some(want)
    } else {
        None
    };
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
            Some(prefix) => {
                db::search_under_streaming(conn, prefix, Some(p), filter, limit, emit)
            }
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
    use super::{parse_size, under_prefix};

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
