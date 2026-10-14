//! Scan a directory tree and report throughput; compares statx strategies.
//!
//!   cargo run --release -p rfind-scanner --example scan -- /usr [--io-uring] [--workers N]

use rfind_scanner::{run_scan, ScanConfig};
use std::{
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    sync::Arc,
};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let root = PathBuf::from(args.first().map(String::as_str).unwrap_or("/usr"));
    let io_uring = args.iter().any(|a| a == "--io-uring");
    let workers = args
        .iter()
        .position(|a| a == "--workers")
        .and_then(|i| args.get(i + 1)?.parse().ok())
        .unwrap_or(0);
    let bytes = Arc::new(AtomicU64::new(0));
    let b = Arc::clone(&bytes);
    let cfg = ScanConfig::new(
        vec![root.clone()],
        true,
        |_| {},
        move |e| {
            b.fetch_add(e.size, Ordering::Relaxed);
        },
    )
    .with_workers(workers)
    .with_io_uring(io_uring)
    .with_error_handler(|_, _| {});
    let r = run_scan(cfg);
    println!(
        "{:<28} io_uring={:<5} entries={:<9} dirs={:<8} bytes={:<14} {:>7.2}s {:>9.0}/s",
        root.display(),
        r.io_uring,
        r.entries,
        r.dirs,
        bytes.load(Ordering::Relaxed),
        r.elapsed.as_secs_f64(),
        r.entries as f64 / r.elapsed.as_secs_f64()
    );
}
