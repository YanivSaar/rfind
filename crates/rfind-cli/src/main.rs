//! rfind — query the rfindd index.

use clap::{Parser, Subcommand};
use rfind_core::{db, db_path, progress_file_path, DaemonPhase, ScanProgress};
use std::{
    fs,
    io::{self, Write},
    thread,
    time::Duration,
};

#[derive(Parser)]
#[command(name = "rfind", about = "Fast filesystem search backed by rfindd", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show live daemon status and scan progress.
    Status {
        #[arg(short, long)]
        watch: bool,
    },
    /// Search by filename (exact or glob: * and ? supported, case-sensitive).
    #[command(name = "--name", alias = "name")]
    Name {
        pattern: String,
        #[arg(long)]
        under: Option<String>,
    },
    /// Search by full path glob (case-sensitive).
    #[command(name = "--path", alias = "path")]
    Path {
        pattern: String,
    },
}

fn load_progress() -> Option<ScanProgress> {
    let text = fs::read_to_string(progress_file_path()).ok()?;
    serde_json::from_str(&text).ok()
}

fn print_progress(p: &ScanProgress) {
    let phase = match &p.phase {
        DaemonPhase::Scanning     => "SCANNING",
        DaemonPhase::Reconciling  => "RECONCILING",
        DaemonPhase::Ready        => "READY",
        DaemonPhase::Error(_)     => "ERROR",
    };
    println!("Phase          : {phase}");
    println!("Entries        : {}", p.entries_found);
    println!("Dirs done      : {}", p.dirs_completed);
    println!("Dirs pending   : {}", p.dirs_pending);
    println!("Rate           : {:.0} entries/s", p.entries_per_sec);
    println!("Errors         : {}", p.errors);
    println!("Events pending : {}", p.events_pending);
    println!("Events applied : {}", p.events_applied);
    println!("Status         : {}", p.status_line);
}

fn open_db() -> Option<rusqlite::Connection> {
    match db::open(&db_path()) {
        Ok(c) => Some(c),
        Err(e) => {
            eprintln!("Cannot open index database: {e}");
            None
        }
    }
}

fn print_result(path: &str) -> bool {
    writeln!(io::stdout().lock(), "{path}").is_ok()
}

fn print_results(paths: Vec<String>) {
    let mut count = 0usize;
    for p in &paths {
        if !print_result(p) { break; }
        count += 1;
    }
    eprintln!("\n{count} result(s)");
}

fn main() {
    let cli = Cli::parse();
    match cli.command {
        Command::Status { watch } => {
            if !watch {
                match load_progress() {
                    Some(p) => print_progress(&p),
                    None => eprintln!("No progress data. Is rfindd running?"),
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
        Command::Name { pattern, under } => {
            let conn = match open_db() { Some(c) => c, None => return };
            let results = if let Some(ref prefix) = under {
                db::search_under(&conn, prefix, Some(&pattern))
            } else {
                db::search_by_glob(&conn, &pattern)
            };
            match results {
                Ok(paths) => print_results(paths),
                Err(e) => eprintln!("Search error: {e}"),
            }
        }
        Command::Path { pattern } => {
            let conn = match open_db() { Some(c) => c, None => return };
            match db::search_by_path_glob(&conn, &pattern) {
                Ok(paths) => print_results(paths),
                Err(e) => eprintln!("Search error: {e}"),
            }
        }
    }
}
