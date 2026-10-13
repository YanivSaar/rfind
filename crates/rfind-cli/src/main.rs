//! rfind — query the rfindd index.
//!
//! Currently implements the `status` subcommand which reads the live
//! progress snapshot written by the daemon.

use clap::{Parser, Subcommand};
use rfind_core::{progress_file_path, DaemonPhase, ScanProgress};
use std::{fs, thread, time::Duration};

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

#[derive(Subcommand)]
enum Command {
    /// Show live daemon status and scan progress.
    Status {
        /// Watch mode: refresh every second until scan completes.
        #[arg(short, long)]
        watch: bool,
    },
}

fn load_progress() -> Option<ScanProgress> {
    let path = progress_file_path();
    let text = fs::read_to_string(&path).ok()?;
    serde_json::from_str(&text).ok()
}

fn print_progress(p: &ScanProgress) {
    let phase = match &p.phase {
        DaemonPhase::Scanning => "SCANNING".to_string(),
        DaemonPhase::Ready => "READY   ".to_string(),
        DaemonPhase::Error(e) => format!("ERROR: {e}"),
    };

    println!("Phase       : {phase}");
    println!("Entries     : {}", p.entries_found);
    println!("Dirs done   : {}", p.dirs_completed);
    println!("Rate        : {:.0} entries/s", p.entries_per_sec);
    println!("Errors      : {}", p.errors);
    println!("Status      : {}", p.status_line);
}

fn main() {
    let cli = Cli::parse();

    match cli.command {
        Command::Status { watch } => {
            if !watch {
                match load_progress() {
                    Some(p) => print_progress(&p),
                    None => eprintln!(
                        "No progress data found at {:?}\nIs rfindd running?",
                        progress_file_path()
                    ),
                }
            } else {
                loop {
                    // Clear screen for watch mode.
                    print!("\x1B[2J\x1B[1;1H");
                    match load_progress() {
                        Some(ref p) => {
                            print_progress(p);
                            if p.phase == DaemonPhase::Ready {
                                println!("\nScan complete.");
                                break;
                            }
                        }
                        None => eprintln!(
                            "Waiting for rfindd... ({:?})",
                            progress_file_path()
                        ),
                    }
                    thread::sleep(Duration::from_secs(1));
                }
            }
        }
    }
}
