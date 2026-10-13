//! rfind — query the rfindd index.
//!
//! Commands:
//!   status   — daemon status
//!   name     — search by filename (glob, exact, regex)
//!   path     — search by full path glob
//!   recent   — recently modified files
//!   health   — index health report
//!   stats    — index statistics
//!   update   — trigger re-index via SIGUSR1

use clap::{Parser, Subcommand, ValueEnum};
use rfind_config::{load as load_config, Config, DEFAULT_CONFIG_PATH};
use rfind_core::{
    db::{self, HealthInfo, IndexStats, RecentEntry},
    db_path, progress_file_path, DaemonPhase, ScanProgress,
};
use std::{
    fs,
    io::{self, Write},
    path::PathBuf,
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

// ---------------------------------------------------------------------------
// CLI definition
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(
    name = "rfind",
    about = "Fast filesystem search backed by rfindd",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
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
            EntryType::Dir  => Some(true),
        }
    }
}

#[derive(Subcommand)]
enum Command {
    /// Show live daemon status.
    Status {
        /// Refresh every second until Ready.
        #[arg(short, long)]
        watch: bool,
    },

    /// Search by filename (exact, glob, or regex).
    #[command(name = "--name", alias = "name")]
    Name {
        /// Pattern (* and ? glob, or regex with --regex).
        pattern: String,
        /// Restrict to this directory tree.
        #[arg(long)]
        under: Option<String>,
        /// Filter by entry type: f=file, d=directory.
        #[arg(long, value_enum)]
        r#type: Option<EntryType>,
        /// Stop after N results.
        #[arg(long)]
        limit: Option<usize>,
        /// Treat pattern as a regular expression.
        #[arg(long)]
        regex: bool,
    },

    /// Search by full path glob.
    #[command(name = "--path", alias = "path")]
    Path {
        pattern: String,
        #[arg(long, value_enum)]
        r#type: Option<EntryType>,
        #[arg(long)]
        limit: Option<usize>,
    },

    /// Show recently modified files.
    Recent {
        /// Files modified in the last N minutes.
        #[arg(long, conflicts_with_all = ["hours", "days", "from"])]
        minutes: Option<i64>,
        /// Files modified in the last N hours.
        #[arg(long, conflicts_with_all = ["minutes", "days", "from"])]
        hours: Option<i64>,
        /// Files modified in the last N days.
        #[arg(long, conflicts_with_all = ["minutes", "hours", "from"])]
        days: Option<i64>,
        /// Start of date range (YYYY-MM-DD).
        #[arg(long, requires = "to")]
        from: Option<String>,
        /// End of date range (YYYY-MM-DD).
        #[arg(long, requires = "from")]
        to: Option<String>,
        /// Filter by type.
        #[arg(long, value_enum)]
        r#type: Option<EntryType>,
        /// Max results.
        #[arg(long, default_value = "50")]
        limit: usize,
    },

    /// Show index health.
    Health,

    /// Show index statistics.
    Stats,

    /// Trigger a re-index (sends SIGUSR1 to rfindd).
    Update,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn load_progress() -> Option<ScanProgress> {
    let text = fs::read_to_string(progress_file_path()).ok()?;
    serde_json::from_str(&text).ok()
}

fn print_progress(p: &ScanProgress) {
    let phase = match &p.phase {
        DaemonPhase::Scanning    => "SCANNING",
        DaemonPhase::Reconciling => "RECONCILING",
        DaemonPhase::Ready       => "READY",
        DaemonPhase::Error(_)    => "ERROR",
    };
    println!("Phase          : {phase}");
    println!("Entries        : {}", p.entries_found);
    println!("Dirs done      : {}", p.dirs_completed);
    println!("Dirs pending   : {}", p.dirs_pending);
    println!("Rate           : {:.0} entries/s", p.entries_per_sec);
    println!("Errors         : {}", p.errors);
    println!("Events pending : {}", p.events_pending);
    println!("Events applied : {}", p.events_applied);
    println!("Monitoring     : {}", p.monitoring_mode);
    println!("Status         : {}", p.status_line);
}

fn open_db() -> Option<rusqlite::Connection> {
    let cfg = load_config(&PathBuf::from(DEFAULT_CONFIG_PATH))
        .unwrap_or_default();
    match db::open(&cfg.index.db_path) {
        Ok(c) => Some(c),
        Err(e) => {
            eprintln!("Cannot open index: {e}");
            eprintln!("Is rfindd running and has the initial scan completed?");
            None
        }
    }
}

/// Write a result line. Returns false if stdout is closed (pipe).
fn emit(path: &str) -> bool {
    writeln!(io::stdout().lock(), "{path}").is_ok()
}

fn print_count(n: u64) {
    eprintln!("\n{n} result(s)");
}

fn format_duration(secs: i64) -> String {
    if secs < 60      { format!("{}s", secs) }
    else if secs < 3600 { format!("{}m {}s", secs/60, secs%60) }
    else if secs < 86400 { format!("{}h {}m", secs/3600, (secs%3600)/60) }
    else               { format!("{}d {}h", secs/86400, (secs%86400)/3600) }
}

fn format_bytes(b: u64) -> String {
    if b < 1024        { format!("{b} B") }
    else if b < 1<<20  { format!("{:.1} KB", b as f64 / 1024.0) }
    else if b < 1<<30  { format!("{:.1} MB", b as f64 / (1<<20) as f64) }
    else               { format!("{:.2} GB", b as f64 / (1<<30) as f64) }
}

/// Parse YYYY-MM-DD into a Unix timestamp at start of day UTC.
fn parse_date(s: &str) -> Option<i64> {
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() != 3 { return None; }
    let y: i64 = parts[0].parse().ok()?;
    let m: i64 = parts[1].parse().ok()?;
    let d: i64 = parts[2].parse().ok()?;
    // Rough Unix timestamp (ignores leap seconds, good enough for date ranges).
    let days_since_epoch = {
        let y = y - 1970;
        y * 365 + y/4 - y/100 + y/400
        + [0i64,31,59,90,120,151,181,212,243,273,304,334][(m-1) as usize]
        + d - 1
    };
    Some(days_since_epoch * 86400)
}

fn format_mtime(sec: i64) -> String {
    // Simple timestamp formatting without chrono for lightweight display.
    let dt = chrono::DateTime::from_timestamp(sec, 0)
        .unwrap_or_default()
        .with_timezone(&chrono::Local);
    dt.format("%Y-%m-%d %H:%M:%S").to_string()
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn main() {
    let cli = Cli::parse();

    match cli.command {
        // ── Status ──────────────────────────────────────────────────────────
        Command::Status { watch } => {
            if !watch {
                match load_progress() {
                    Some(p) => print_progress(&p),
                    None    => eprintln!("No status. Is rfindd running?"),
                }
            } else {
                loop {
                    print!("\x1B[2J\x1B[1;1H");
                    match load_progress() {
                        Some(ref p) => {
                            print_progress(p);
                            if p.phase == DaemonPhase::Ready {
                                println!("\nReady.");
                                break;
                            }
                        }
                        None => eprintln!("Waiting for rfindd..."),
                    }
                    thread::sleep(Duration::from_secs(1));
                }
            }
        }

        // ── Name search ─────────────────────────────────────────────────────
        Command::Name { pattern, under, r#type, limit, regex } => {
            let conn = match open_db() { Some(c) => c, None => return };
            let type_filter = r#type.as_ref().and_then(|t| t.to_is_dir());
            let mut count = 0u64;

            let result = if regex {
                let field = if under.is_some() { "full_path" } else { "basename" };
                db::search_by_regex_streaming(
                    &conn, &pattern, field, type_filter, limit,
                    |p| { count += 1; emit(p) },
                )
            } else if let Some(ref prefix) = under {
                db::search_under_streaming(
                    &conn, prefix, Some(&pattern), type_filter, limit,
                    |p| { count += 1; emit(p) },
                )
            } else {
                db::search_by_glob_streaming(
                    &conn, &pattern, type_filter, limit,
                    |p| { count += 1; emit(p) },
                )
            };

            match result {
                Ok(n)  => print_count(n),
                Err(e) => eprintln!("Search error: {e}"),
            }
        }

        // ── Path search ─────────────────────────────────────────────────────
        Command::Path { pattern, r#type, limit } => {
            let conn = match open_db() { Some(c) => c, None => return };
            let type_filter = r#type.as_ref().and_then(|t| t.to_is_dir());
            match db::search_by_path_glob_streaming(
                &conn, &pattern, type_filter, limit,
                |p| emit(p),
            ) {
                Ok(n)  => print_count(n),
                Err(e) => eprintln!("Search error: {e}"),
            }
        }

        // ── Recent ───────────────────────────────────────────────────────────
        Command::Recent { minutes, hours, days, from, to, r#type, limit } => {
            let conn = match open_db() { Some(c) => c, None => return };
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64;

            let (from_sec, to_sec) = if let (Some(f), Some(t)) = (from, to) {
                match (parse_date(&f), parse_date(&t)) {
                    (Some(fs), Some(ts)) => (fs, ts + 86399), // inclusive end day
                    _ => { eprintln!("Invalid date format. Use YYYY-MM-DD"); return; }
                }
            } else if let Some(m) = minutes {
                (now - m * 60, now)
            } else if let Some(h) = hours {
                (now - h * 3600, now)
            } else if let Some(d) = days {
                (now - d * 86400, now)
            } else {
                // Default: last 24 hours.
                (now - 86400, now)
            };

            let type_filter = r#type.as_ref().and_then(|t| t.to_is_dir());
            let mut count = 0u64;

            match db::search_recent_streaming(
                &conn, from_sec, to_sec, type_filter, Some(limit),
                |entry| {
                    count += 1;
                    let ts = format_mtime(entry.mtime_sec);
                    writeln!(io::stdout().lock(), "{ts}  {}", entry.full_path).is_ok()
                },
            ) {
                Ok(_)  => print_count(count),
                Err(e) => eprintln!("Recent error: {e}"),
            }
        }

        // ── Health ───────────────────────────────────────────────────────────
        Command::Health => {
            let cfg = load_config(&PathBuf::from(DEFAULT_CONFIG_PATH))
                .unwrap_or_default();
            let conn = match open_db() { Some(c) => c, None => return };
            match db::get_health(&conn, &cfg.index.db_path) {
                Ok(h) => {
                    let now = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs() as i64;

                    let age = h.last_scan_completed
                        .map(|t| format_duration(now - t))
                        .unwrap_or_else(|| "never".to_string());

                    let status = if h.pending_events > 0 {
                        format!("UPDATING ({} events pending)", h.pending_events)
                    } else {
                        match h.last_scan_completed {
                            None    => "NO INDEX".to_string(),
                            Some(t) if now - t < 3600 => "FRESH".to_string(),
                            Some(t) if now - t < 86400 => "RECENT".to_string(),
                            _       => "STALE".to_string(),
                        }
                    };

                    println!("Status         : {status}");
                    println!("Index age      : {age}");
                    println!("Entries        : {}", h.entry_count);
                    println!("Directories    : {}", h.dir_count);
                    println!("Files          : {}", h.file_count);
                    println!("Monitoring     : {}", h.monitoring_mode);
                    println!("Events pending : {}", h.pending_events);
                    println!("DB size        : {}", format_bytes(h.db_size_bytes));
                }
                Err(e) => eprintln!("Health error: {e}"),
            }
        }

        // ── Stats ────────────────────────────────────────────────────────────
        Command::Stats => {
            let cfg = load_config(&PathBuf::from(DEFAULT_CONFIG_PATH))
                .unwrap_or_default();
            let conn = match open_db() { Some(c) => c, None => return };
            match db::get_stats(&conn, &cfg.index.db_path) {
                Ok(s) => {
                    println!("Total entries  : {}", s.entry_count);
                    println!("Directories    : {}", s.dir_count);
                    println!("Files          : {}", s.file_count);
                    println!("DB size        : {}", format_bytes(s.db_size_bytes));
                    if let Some(age) = s.index_age_secs {
                        println!("Index age      : {}", format_duration(age));
                    }
                    if !s.top_extensions.is_empty() {
                        println!("\nTop file types:");
                        for e in &s.top_extensions {
                            println!("  .{:<12} {}", e.ext, e.count);
                        }
                    }
                    if !s.largest_dirs.is_empty() {
                        println!("\nLargest directories:");
                        for d in &s.largest_dirs {
                            println!("  {:>6} entries  {}", d.count, d.path);
                        }
                    }
                }
                Err(e) => eprintln!("Stats error: {e}"),
            }
        }

        // ── Update ───────────────────────────────────────────────────────────
        Command::Update => {
            // Find rfindd PID and send SIGUSR1.
            match find_rfindd_pid() {
                Some(pid) => {
                    let ret = unsafe { libc::kill(pid, libc::SIGUSR1) };
                    if ret == 0 {
                        println!("Sent SIGUSR1 to rfindd (pid {pid}) — re-index triggered.");
                        println!("Run 'rfind status --watch' to monitor progress.");
                    } else {
                        eprintln!("Failed to signal rfindd (pid {pid}): {}",
                                  std::io::Error::last_os_error());
                    }
                }
                None => eprintln!("rfindd is not running."),
            }
        }
    }
}

fn find_rfindd_pid() -> Option<libc::pid_t> {
    // Read /proc/*/comm to find rfindd.
    let proc = std::fs::read_dir("/proc").ok()?;
    for entry in proc.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.chars().all(|c| c.is_ascii_digit()) { continue; }
        let comm_path = entry.path().join("comm");
        if let Ok(comm) = std::fs::read_to_string(&comm_path) {
            if comm.trim() == "rfindd" {
                if let Ok(pid) = name.parse::<libc::pid_t>() {
                    return Some(pid);
                }
            }
        }
    }
    None
}
