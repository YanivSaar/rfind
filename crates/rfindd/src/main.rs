//! rfindd — rfind indexing daemon.
//!
//! # Startup logic
//!
//! On first run (no existing index):
//!   → full scan
//!
//! On restart with existing index:
//!   → if index age < reindex_interval: replay journal only (fast restart)
//!   → if index age > reindex_interval: full rescan
//!   → if the config changed: purge what is no longer indexed, rescan what
//!     is newly included (see config_state)
//!   → always apply unapplied journal events before declaring Ready
//!
//! # Signal handling
//!   SIGUSR1 → force immediate re-index (signals arriving during that
//!             re-index are coalesced into it)
//!   SIGHUP  → reload: stop scans, re-exec with the new configuration
//!   SIGTERM → clean shutdown; running scans are cancelled
//!
//! # Monitoring
//!   fanotify preferred (one mark per filesystem, kernel 5.9+)
//!   inotify fallback (per-directory watches)
//!   index-only if both fail (periodic re-index via reindex_interval)
//!   An event-queue overflow triggers a live re-index.
//!   Filesystems mounted under a live root later are indexed and watched.
//!
//! # Roots
//!   scan.roots          — live: scanned in the foreground, then monitored.
//!   scan.periodic_roots — rescanned in a background thread on their own
//!                         interval, never monitored (e.g. WSL2 /mnt/c).
//!   A root nested in another root is skipped by the outer root's scan.
//!
//! # Scans never empty the index
//!   Each scan upserts rows tagged with a new generation, then sweeps the
//!   rows under its root that it did not see. Searches see the previous
//!   complete index for the whole scan. Cancelled scans sweep nothing.
//!
//! # Event replay
//!   Journaled events are re-checked against the filesystem when applied
//!   (exists → upsert with real metadata, gone → delete subtree), so
//!   replaying an event twice, or after a fresh scan, is harmless.

mod config_state;
mod index;
mod lifecycle;
mod mountwatch;
mod platform;
mod progress;
mod scan;

use lifecycle::{
    acquire_pidfile, clean_marker_path, reexec, restrict_index_permissions, take_clean_marker,
    write_clean_marker,
};

use rfind_config::{load as load_config, write_default, Config, DEFAULT_CONFIG_PATH};
use rfind_core::{db, log::sanitize_path, mounts, BackgroundScan, ScanProgress};
use rfind_monitor::Monitor;
use scan::{now_secs, periodic_paths, Ctx};
use signal_hook::{
    consts::{SIGHUP, SIGINT, SIGTERM, SIGUSR1},
    iterator::Signals,
};
use std::{
    collections::VecDeque,
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant, SystemTime},
};
use tracing::{error, info, warn};

/// Lock a `Mutex`, ignoring poisoning. The daemon has one writer and
/// panic recovery in the scanner can leave the shared connection
/// mutex poisoned even though the DB is still fine; using `.unwrap()`
/// would then crash every subsequent main-loop tick, so we treat
/// poison as "proceed with the inner value" instead.
fn lock<'a, T>(m: &'a Mutex<T>) -> std::sync::MutexGuard<'a, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// Config path: `RFIND_CONFIG` overrides the default.
///
/// The daemon runs as root, so whoever can set its environment can also
/// swap its config. `systemctl edit` and malicious unit drop-ins both do
/// this. Restrict the override to absolute paths under `/etc/rfindd/`:
/// the config dir is already 0640 and only root-writable, so anything an
/// attacker places there needs root first. Anything else logs a warning
/// and falls back to the default. The CLI (`rfind-cli`) stays permissive
/// — it runs as the user and reads a config they already own.
fn config_path() -> PathBuf {
    let default = PathBuf::from(DEFAULT_CONFIG_PATH);
    let Some(v) = std::env::var_os("RFIND_CONFIG") else {
        return default;
    };
    let p = PathBuf::from(&v);
    // Debug builds accept any absolute path so integration tests can point
    // the daemon at a tempdir-backed config. Release (production) builds
    // keep the "/etc/rfindd/" restriction so an attacker who can set env
    // via systemctl edit cannot swap in a config from a world-writable
    // directory.
    #[cfg(debug_assertions)]
    let ok = p.is_absolute()
        && !p
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir));
    #[cfg(not(debug_assertions))]
    let ok = p.is_absolute()
        && p.starts_with("/etc/rfindd/")
        // reject traversal: /etc/rfindd/../foo would still start_with the dir
        && !p.components().any(|c| matches!(c, std::path::Component::ParentDir));
    if ok {
        p
    } else {
        warn!(
            "RFIND_CONFIG={} rejected: must be an absolute path under /etc/rfindd/; using {}",
            p.display(),
            default.display()
        );
        default
    }
}

/// Pidfile path. Precedence:
///
/// 1. `--pidfile=<path>` on the command line (used by OpenRC/runit/s6/dinit
///    units — see `packaging/openrc`, `packaging/runit`, `packaging/s6`,
///    `packaging/dinit`). Must be absolute.
/// 2. `RFINDD_PIDFILE_PATH` env var in debug builds only (test harness).
/// 3. `/run/rfindd/rfindd.pid` (systemd default).
///
/// The env override stays debug-only to preserve the config-path threat
/// model (see `config_path`); a non-systemd init can always set the arg
/// instead.
fn pidfile_path() -> PathBuf {
    if let Some(p) = cli_pidfile() {
        return p;
    }
    #[cfg(debug_assertions)]
    if let Some(v) = std::env::var_os("RFINDD_PIDFILE_PATH") {
        return PathBuf::from(v);
    }
    PathBuf::from("/run/rfindd/rfindd.pid")
}

/// Parse argv once, latch results. Supported flags: `--pidfile=PATH`,
/// `--help`, `--version`. Anything else exits 2.
fn parse_argv_once() {
    let mut args = std::env::args().skip(1);
    let mut pidfile = None;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--pidfile" => match args.next() {
                Some(v) => pidfile = Some(PathBuf::from(v)),
                None => die_usage(),
            },
            s if s.starts_with("--pidfile=") => {
                pidfile = Some(PathBuf::from(&s["--pidfile=".len()..]));
            }
            "--help" | "-h" => {
                print_usage();
                std::process::exit(0);
            }
            "--version" | "-V" => {
                println!("rfindd {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            _ => die_usage(),
        }
    }
    if let Some(p) = pidfile.as_ref() {
        if !p.is_absolute() {
            eprintln!("rfindd: --pidfile must be an absolute path");
            std::process::exit(2);
        }
    }
    let _ = CLI_PIDFILE.set(pidfile);
}

static CLI_PIDFILE: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();

fn cli_pidfile() -> Option<PathBuf> {
    CLI_PIDFILE.get().cloned().unwrap_or(None)
}

fn print_usage() {
    let bin = env!("CARGO_PKG_NAME");
    println!("Usage: {bin} [--pidfile=PATH]");
    println!();
    println!("Options:");
    println!("  --pidfile=PATH   Write the pidfile at PATH (default /run/rfindd/rfindd.pid).");
    println!("                   Used by non-systemd init scripts.");
    println!("  -h, --help       Show this message.");
    println!("  -V, --version    Print version.");
}

fn die_usage() -> ! {
    print_usage();
    std::process::exit(2);
}

/// Warn about Windows drives (WSL2 drvfs) that would be monitored live:
/// inotify never sees changes made from Windows, and watching a whole
/// drive can exhaust the inotify watch limit.
/// On very small images (minimal containers, embedded systems) the
/// tmpfs backing /run can be sized tight enough that progress.json
/// writes will eventually ENOSPC. Warn once at startup rather than
/// surprise the operator mid-scan.
fn warn_small_runtime_dir() {
    use std::ffi::CString;
    use std::mem::MaybeUninit;
    let Ok(path) = CString::new("/run/rfindd") else {
        return;
    };
    let mut st: MaybeUninit<libc::statvfs> = MaybeUninit::uninit();
    let rc = unsafe { libc::statvfs(path.as_ptr(), st.as_mut_ptr()) };
    if rc != 0 {
        return;
    }
    let st = unsafe { st.assume_init() };
    let bytes_free = (st.f_bsize as u64).saturating_mul(st.f_bavail as u64);
    const MIN: u64 = 16 * 1024 * 1024;
    if bytes_free < MIN {
        warn!(
            "/run/rfindd has only {} KiB free; progress.json writes may fail. \
             Resize the tmpfs backing /run or raise RuntimeDirectorySize.",
            bytes_free / 1024
        );
    }
}

fn warn_drvfs_under_live_roots(cfg: &Config, mounts: &[mounts::MountEntry]) {
    let handled: Vec<PathBuf> = cfg
        .scan
        .exclude
        .iter()
        .cloned()
        .chain(periodic_paths(cfg))
        .collect();
    for m in mounts.iter().filter(|m| m.is_drvfs()) {
        let live = cfg.scan.roots.iter().any(|r| m.point.starts_with(r));
        if live && !handled.iter().any(|h| m.point.starts_with(h)) {
            warn!(
                "{} is a Windows drive under a live root: inotify cannot see changes \
                 made from Windows. Add it to scan.periodic_roots (or scan.exclude).",
                sanitize_path(&m.point)
            );
        }
    }
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod tests {
    use super::*;

    #[test]
    fn config_path_rejects_traversal_and_relative() {
        // SAFETY: single-threaded test; cargo test runs module tests sequentially
        // within a `mod tests` unless `test-threads` is overridden. The two config
        // tests touch the same env var and must not run concurrently; we serialise
        // via a mutex to make that explicit.
        //
        // Tests always compile with debug_assertions on, so the "must be under
        // /etc/rfindd/" restriction (release-only, see `config_path`) is not
        // exercised here — only the invariants that hold in both profiles:
        // the override must be absolute and must not contain `..`.
        let _g = ENV_LOCK.lock().unwrap();
        let prev = std::env::var_os("RFIND_CONFIG");

        unsafe { std::env::set_var("RFIND_CONFIG", "/etc/rfindd/../tmp/evil.toml") };
        assert_eq!(config_path(), PathBuf::from(DEFAULT_CONFIG_PATH));

        unsafe { std::env::set_var("RFIND_CONFIG", "relative.toml") };
        assert_eq!(config_path(), PathBuf::from(DEFAULT_CONFIG_PATH));

        match prev {
            Some(v) => unsafe { std::env::set_var("RFIND_CONFIG", v) },
            None => unsafe { std::env::remove_var("RFIND_CONFIG") },
        }
    }

    #[test]
    fn config_path_accepts_paths_under_etc_rfindd() {
        let _g = ENV_LOCK.lock().unwrap();
        let prev = std::env::var_os("RFIND_CONFIG");

        unsafe { std::env::set_var("RFIND_CONFIG", "/etc/rfindd/alt.toml") };
        assert_eq!(config_path(), PathBuf::from("/etc/rfindd/alt.toml"));

        match prev {
            Some(v) => unsafe { std::env::set_var("RFIND_CONFIG", v) },
            None => unsafe { std::env::remove_var("RFIND_CONFIG") },
        }
    }

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
}

/// Load the config, write the default on first start, and exclude
/// bind-mount aliases. Errors are fatal: a broken config must not silently
/// fall back to defaults that could index excluded paths.
fn load_effective_config(table: &[mounts::MountEntry]) -> Result<Config, String> {
    let path = config_path();
    if !path.exists() {
        match write_default(&path) {
            Ok(()) => info!("Wrote default config to {}", path.display()),
            Err(e) => warn!("Could not write default config: {e}"),
        }
    }
    let mut cfg = load_config(&path).map_err(|e| e.to_string())?;
    if let Some(p) = cfg.validate().into_iter().find(|p| p.error) {
        return Err(p.message);
    }
    for (alias, canonical) in mounts::alias_mounts(table) {
        if index::is_excluded(&cfg, &alias) {
            continue;
        }
        info!(
            "Skipping {} (bind mount; same files as {})",
            alias.display(),
            canonical.display()
        );
        cfg.scan.exclude.push(alias);
    }
    Ok(cfg)
}

/// Decide what to do with a filesystem that was mounted or unmounted.
fn on_mount_change(
    ctx: &Ctx,
    monitor: Option<&Monitor>,
    changes: &mountwatch::MountChanges,
    queue: &mut VecDeque<PathBuf>,
    in_flight: Option<&PathBuf>,
) {
    let in_flight_match = |p: &PathBuf| in_flight == Some(p);
    let aliases: Vec<PathBuf> = mounts::alias_mounts(&changes.table)
        .into_iter()
        .map(|(a, _)| a)
        .collect();
    // Allowlist of fstypes safe to auto-index. Unprivileged users with
    // user-namespaces can mount tmpfs/ramfs/ext*-via-loop (not just FUSE),
    // and a root-owned walk of attacker-controlled contents is exactly
    // what we are trying to avoid. Only local block-backed filesystems
    // get the automatic treatment; anything else must be opted in via
    // `scan.periodic_roots` so the admin takes responsibility explicitly.
    let relevant = |m: &mounts::MountEntry| {
        ctx.cfg.scan.roots.iter().any(|r| m.point.starts_with(r))
            && !index::is_excluded(&ctx.cfg, &m.point)
            && !periodic_paths(&ctx.cfg)
                .iter()
                .any(|p| m.point.starts_with(p))
            && m.is_local_block_fs()
            && m.point.is_dir()
    };
    for m in changes.added.iter().filter(|m| relevant(m)) {
        if aliases.contains(&m.point) {
            info!(
                "New mount {} is a bind mount of indexed files; skipping",
                sanitize_path(&m.point)
            );
            continue;
        }
        if m.is_unmonitorable() {
            info!(
                "New mount {} ({}): indexing once; not monitored live (consider scan.periodic_roots)",
                sanitize_path(&m.point),
                m.fstype
            );
        } else if let Some(mon) = monitor {
            match mon.add_filesystem(&m.point) {
                Ok(true) => info!(
                    "New mount {} ({}): watching and indexing",
                    sanitize_path(&m.point),
                    m.fstype
                ),
                Ok(false) => info!(
                    "New mount {} ({}): indexing",
                    sanitize_path(&m.point),
                    m.fstype
                ),
                Err(e) => warn!(
                    "New mount {}: {e}; indexing without live updates",
                    sanitize_path(&m.point)
                ),
            }
        }
        if !queue.contains(&m.point) && !in_flight_match(&m.point) {
            queue.push_back(m.point.clone());
        }
    }
    let periodic = periodic_paths(&ctx.cfg);
    for m in changes
        .removed
        .iter()
        .filter(|m| relevant(m) || m.point.is_dir())
    {
        // Drop the fanotify mark and its cached mount fd before anything
        // else: otherwise the fd pins the (now-dead) mount and a later
        // event whose fsid gets reused resolves against the wrong mount.
        // Call this for every removed mount, including ones under
        // excludes or periodic roots — the mark was taken when they went
        // in, so it must go when they leave.
        if let Some(mon) = monitor {
            if mon.remove_filesystem(&m.point) {
                info!(
                    "Unmounted {}: dropped fanotify mark",
                    sanitize_path(&m.point)
                );
            }
        }
        if !ctx.cfg.scan.roots.iter().any(|r| m.point.starts_with(r))
            || index::is_excluded(&ctx.cfg, &m.point)
            || periodic.iter().any(|p| m.point.starts_with(p))
        {
            continue;
        }
        info!(
            "Unmounted {}: rescanning the mount point",
            sanitize_path(&m.point)
        );
        if !queue.contains(&m.point) && !in_flight_match(&m.point) {
            queue.push_back(m.point.clone());
        }
    }
}

/// Overlay the current background-scan state onto a Ready-phase progress
/// write. The daemon's status file is otherwise only touched during live
/// scans and reconcile; keeping it current here lets `rfind status` tell
/// the user that a periodic/mount scan is in flight.
fn publish_bg_scan(conn: &Mutex<rusqlite::Connection>, mode: &str, bg: Option<BackgroundScan>) {
    let (entries, pending) = {
        let c = lock(conn);
        (
            db::entry_count(&c).unwrap_or(0) as u64,
            db::pending_event_count(&c).unwrap_or(0) as u64,
        )
    };
    let mut p = progress::status(rfind_core::DaemonPhase::Ready, entries, pending, 0, mode);
    p.background_scan = bg;
    progress::write(&p);
}

fn main() {
    // Parse argv first so flags are latched before path helpers read them.
    // --help/--version exit here; unknown flags die here.
    parse_argv_once();

    // Install SIG_IGN for SIGHUP / SIGUSR1 as the very first act of
    // main(). The default disposition for both is "terminate"; a signal
    // arriving between kernel exec-return and signal_hook's handler
    // install would otherwise kill the new daemon.
    //
    // SIG_IGN survives execve (per sigaction(2) "dispositions of ignored
    // signals are left unchanged"); installed handlers are reset to
    // default. `reexec()` therefore restores SIG_IGN immediately before
    // calling execve so the new image inherits the ignore disposition —
    // closing the race across every reload-reexec cycle, not only on
    // first boot. pthread_sigmask was tried first (v1.6.3) but blocks
    // signal_hook 0.4's pipe-based delivery entirely in production.
    //
    // SIGTERM/SIGINT are intentionally not blocked / ignored so admin-
    // stop during the pidfile-acquire window still works.
    unsafe {
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
        libc::signal(libc::SIGUSR1, libc::SIG_IGN);
    }

    // SIGBUS / SIGSEGV logger: on WSL2 the 9p/drvfs translator can
    // SIGBUS the process during /mnt/c scans. Previously the daemon
    // disappeared with only systemd's `status=7/BUS` as a hint; this
    // handler logs the fault address + pid before letting the default
    // disposition (core dump + terminate) take over, so journalctl
    // carries a diagnostic the operator can file.
    unsafe {
        extern "C" fn fatal_signal_handler(
            signo: libc::c_int,
            info: *mut libc::siginfo_t,
            _ctx: *mut libc::c_void,
        ) {
            // Can't use tracing from a signal handler (allocation,
            // mutexes). Write a single short line via write(2) to
            // stderr; systemd captures it as a journal entry.
            let addr = if !info.is_null() {
                unsafe { (*info).si_addr() as usize }
            } else {
                0
            };
            let name = match signo {
                libc::SIGBUS => "SIGBUS",
                libc::SIGSEGV => "SIGSEGV",
                _ => "FATAL",
            };
            let pid = unsafe { libc::getpid() };
            let msg = format!("rfindd: {name} at addr {addr:#x} (pid {pid})\n");
            let _ = unsafe {
                libc::write(2, msg.as_ptr() as *const _, msg.len())
            };
            // Dump the /proc/self/maps line covering the fault address, so
            // the journal tells us whether the fault was in a file-backed
            // mmap (and which file) vs anonymous memory. async-signal-safe:
            // open/read/write only, no allocation beyond stack bufs.
            unsafe {
                let fd = libc::open(c"/proc/self/maps".as_ptr(), libc::O_RDONLY);
                if fd >= 0 {
                    let mut buf = [0u8; 4096];
                    let mut carry = 0usize;
                    loop {
                        let n = libc::read(
                            fd,
                            buf.as_mut_ptr().add(carry) as *mut _,
                            buf.len() - carry,
                        );
                        if n <= 0 { break; }
                        let end = carry + n as usize;
                        // Scan complete lines.
                        let mut line_start = 0usize;
                        for i in 0..end {
                            if buf[i] == b'\n' {
                                let line = &buf[line_start..i];
                                // Parse "start-end ..." hex.
                                let mut lo: usize = 0;
                                let mut hi: usize = 0;
                                let mut j = 0;
                                while j < line.len() && line[j] != b'-' {
                                    let d = match line[j] {
                                        b'0'..=b'9' => (line[j] - b'0') as usize,
                                        b'a'..=b'f' => (line[j] - b'a' + 10) as usize,
                                        _ => break,
                                    };
                                    lo = lo * 16 + d;
                                    j += 1;
                                }
                                if j < line.len() && line[j] == b'-' {
                                    j += 1;
                                    while j < line.len() && line[j] != b' ' {
                                        let d = match line[j] {
                                            b'0'..=b'9' => (line[j] - b'0') as usize,
                                            b'a'..=b'f' => (line[j] - b'a' + 10) as usize,
                                            _ => break,
                                        };
                                        hi = hi * 16 + d;
                                        j += 1;
                                    }
                                    if addr >= lo && addr < hi {
                                        let prefix = b"rfindd: map: ";
                                        let _ = libc::write(2, prefix.as_ptr() as *const _, prefix.len());
                                        let _ = libc::write(2, line.as_ptr() as *const _, line.len());
                                        let _ = libc::write(2, b"\n".as_ptr() as *const _, 1);
                                    }
                                }
                                line_start = i + 1;
                            }
                        }
                        // Carry trailing partial line.
                        carry = end - line_start;
                        if carry > 0 && line_start > 0 {
                            std::ptr::copy(
                                buf.as_ptr().add(line_start),
                                buf.as_mut_ptr(),
                                carry,
                            );
                        } else if carry >= buf.len() {
                            break;
                        }
                    }
                    libc::close(fd);
                }
            }
            // Chain to default: reinstall default handler, re-raise.
            unsafe {
                libc::signal(signo, libc::SIG_DFL);
                libc::raise(signo);
            }
        }
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = fatal_signal_handler as *const () as usize;
        sa.sa_flags = libc::SA_SIGINFO;
        libc::sigemptyset(&mut sa.sa_mask);
        libc::sigaction(libc::SIGBUS, &sa, std::ptr::null_mut());
        libc::sigaction(libc::SIGSEGV, &sa, std::ptr::null_mut());
    }

    // Logging: RFIND_LOG env wins (tracing_subscriber EnvFilter syntax,
    // e.g. "rfindd=debug,rfind_monitor=trace"). Falls back to "info"
    // for the pre-config bootstrap window; once the config loads we
    // raise/lower via the reloadable layer below so operators can set
    // `log.level = "debug"` without rebuilding or touching env vars.
    use tracing_subscriber::{fmt, layer::SubscriberExt, reload, util::SubscriberInitExt, EnvFilter};
    let have_env = std::env::var("RFIND_LOG").is_ok();
    let initial = EnvFilter::try_from_env("RFIND_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    let (filter, filter_handle) = reload::Layer::new(initial);
    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_target(false).with_level(true))
        .init();
    let log_reload: &'static reload::Handle<EnvFilter, tracing_subscriber::Registry> =
        Box::leak(Box::new(filter_handle));
    info!("rfindd {} starting", env!("CARGO_PKG_VERSION"));

    // Everything rfindd creates (index, WAL, progress file) is root-only.
    unsafe { libc::umask(0o077) };

    // Pidfile + exclusive flock on /run/rfindd/rfindd.pid: a second rfindd
    // started by hand next to the systemd service would otherwise race on
    // the SQLite WAL and the event journal. We leak the lockfile so the
    // flock is held for the lifetime of the process (dropping would close
    // the fd and release the lock).
    let _lockfile: &'static std::fs::File =
        Box::leak(Box::new(match acquire_pidfile(&pidfile_path()) {
            Ok(f) => f,
            Err(e) => {
                error!("{e}");
                std::process::exit(1);
            }
        }));

    let mut mount_table = mounts::read_mountinfo();
    let cfg = match load_effective_config(&mount_table) {
        Ok(c) => {
            info!("Config loaded from {}", config_path().display());
            c
        }
        Err(e) => {
            error!("Config error in {}: {e}", config_path().display());
            std::process::exit(1);
        }
    };

    // Apply config.log.level unless RFIND_LOG was set (env wins).
    if !have_env {
        let desired = cfg.log.level.as_filter_str();
        if let Ok(new) = EnvFilter::try_new(desired) {
            let _ = log_reload.modify(|f| *f = new);
            info!("Log level set to {} from config.log.level", desired);
        }
    }

    info!("Roots: {:?}", cfg.scan.roots);
    for r in &cfg.scan.periodic_roots {
        info!(
            "Periodic root: {} (every {}h)",
            r.path.display(),
            r.interval_hours
        );
    }
    info!("Excludes: {:?}", cfg.scan.exclude);
    info!(
        "Workers: {} | collect mtime/size: {} | io_uring: {} | compressed index: {} | reindex interval: {}h",
        if cfg.scan.workers == 0 {
            num_cpus::get()
        } else {
            cfg.scan.workers
        },
        cfg.scan.collect_mtime,
        cfg.scan.io_uring,
        cfg.index.compress,
        cfg.scan.reindex_interval_hours
    );
    warn_drvfs_under_live_roots(&cfg, &mount_table);
    warn_small_runtime_dir();

    // Detect runtime (host / Docker / Podman / LXC / k8s / WSL / nspawn)
    // once at startup. If the environment can't support fanotify FS
    // marks, skip the attempt rather than spam fanotify_mark EPERM per
    // filesystem — go straight to inotify.
    let runtime_env = platform::RuntimeEnv::detect();
    runtime_env.log_summary();
    let prefer_fanotify_effective = cfg.monitor.prefer_fanotify
        && !runtime_env.should_skip_fanotify();
    if cfg.monitor.prefer_fanotify && !prefer_fanotify_effective {
        info!(
            "Skipping fanotify (runtime {}, caps insufficient) — using inotify",
            runtime_env.runtime.name()
        );
    }

    let db_file = cfg.index.db_path.clone();
    let Some(db_parent) = db_file.parent() else {
        error!(
            "index.db_path = {} has no parent directory; set it to an absolute path",
            db_file.display()
        );
        std::process::exit(1);
    };
    if let Err(e) = fs::create_dir_all(db_parent) {
        error!("cannot create {}: {e}", db_parent.display());
        std::process::exit(1);
    }
    let marker = clean_marker_path(&db_file);
    let prior_was_clean = take_clean_marker(&marker);
    let conn = match db::open_with(&db_file, cfg.index.compress) {
        Ok(c) => Arc::new(Mutex::new(c)),
        Err(e) => {
            error!("cannot open index at {}: {e}", db_file.display());
            std::process::exit(1);
        }
    };
    restrict_index_permissions(&db_file);

    // After an abnormal exit (SIGKILL, OOM, power loss) run SQLite's
    // integrity_check. On failure the index is unlinked so the next
    // open path rebuilds via a full scan; we do not try to serve reads
    // from a corrupt database.
    if !prior_was_clean {
        info!("No clean-shutdown marker; running index integrity_check");
        let bad = {
            let c = lock(&conn);
            match db::integrity_check(&c) {
                Ok(()) => {
                    info!("integrity_check: ok");
                    false
                }
                Err(msg) => {
                    error!("integrity_check failed: {msg}; dropping index for rebuild");
                    true
                }
            }
        };
        if bad {
            // Drop the open handle before unlinking so SQLite's -wal/-shm
            // files are released. The re-exec restarts rfindd with no
            // index, triggering a full scan via the normal path.
            drop(conn);
            db::remove_index_files(&db_file);
            write_clean_marker(&marker);
            info!("Re-executing after index wipe to rebuild via full scan");
            reexec();
        }
    }

    // ── Signal handling ──────────────────────────────────────────────────────
    let force_reindex = Arc::new(AtomicBool::new(false));
    let shutdown = Arc::new(AtomicBool::new(false));
    let reload = Arc::new(AtomicBool::new(false));
    let cancel = Arc::new(AtomicBool::new(false));
    {
        let (fr, sd, rl, cn) = (
            Arc::clone(&force_reindex),
            Arc::clone(&shutdown),
            Arc::clone(&reload),
            Arc::clone(&cancel),
        );
        let mut signals = match Signals::new([SIGUSR1, SIGTERM, SIGINT, SIGHUP]) {
            Ok(s) => s,
            Err(e) => {
                error!("cannot install signal handlers: {e}");
                std::process::exit(1);
            }
        };
        thread::spawn(move || {
            for sig in signals.forever() {
                match sig {
                    SIGUSR1 => {
                        info!("SIGUSR1: forcing re-index");
                        fr.store(true, Ordering::Relaxed);
                    }
                    SIGHUP => {
                        info!("SIGHUP: reloading configuration");
                        rl.store(true, Ordering::Relaxed);
                        cn.store(true, Ordering::Relaxed);
                    }
                    _ => {
                        info!("Shutting down");
                        sd.store(true, Ordering::Relaxed);
                        cn.store(true, Ordering::Relaxed);
                    }
                }
            }
        });
    }

    // ── Decide: full scan or journal-only restart ────────────────────────────
    let config_changes = config_state::apply(&lock(&conn), &cfg);
    let last_scan = db::get_metadata(&lock(&conn), "last_scan_completed")
        .ok()
        .flatten()
        .and_then(|v| v.parse::<i64>().ok());
    let reindex_interval_secs = cfg.scan.reindex_interval_hours as i64 * 3600;
    let age_secs = last_scan.map(|t| now_secs() - t).unwrap_or(i64::MAX);
    let needs_full_scan = last_scan.is_none()
        || config_changes.rescan_live
        || (reindex_interval_secs > 0 && age_secs > reindex_interval_secs);
    if !needs_full_scan {
        info!("Index age {age_secs}s < interval {reindex_interval_secs}s — fast restart, journal only");
    } else if last_scan.is_none() {
        info!("Full scan needed (first run)");
    } else if !config_changes.rescan_live {
        info!("Full scan needed (index age {age_secs}s > interval {reindex_interval_secs}s)");
    }

    // ── Start monitor ────────────────────────────────────────────────────────
    progress::write(&ScanProgress::new_scanning());

    // Periodic roots are never monitored.
    let mut monitor_excludes: Vec<PathBuf> = cfg.scan.exclude.clone();
    monitor_excludes.extend(periodic_paths(&cfg));

    // inotify fast restart: watch every indexed live directory up front.
    // A full scan registers watches as it discovers directories, and
    // fanotify needs none.
    let dirs_for_inotify: Vec<String> = if !needs_full_scan && !cfg.monitor.prefer_fanotify {
        db::get_all_dirs(&lock(&conn))
            .unwrap_or_default()
            .into_iter()
            .filter(|d| {
                !monitor_excludes
                    .iter()
                    .any(|ex| Path::new(d).starts_with(ex))
            })
            .collect()
    } else {
        Vec::new()
    };

    let (mut mark_points, unmonitored) =
        mounts::fanotify_mark_points(&mount_table, &cfg.scan.roots, &monitor_excludes);
    // Files bind-mounted over files (e.g. WSL2's /init) can't be marked.
    mark_points.retain(|p| p.is_dir());
    for m in &unmonitored {
        if !m.is_drvfs() && m.point.is_dir() {
            info!(
                "{} ({}) is indexed but not monitored live; consider scan.periodic_roots",
                sanitize_path(&m.point),
                m.fstype
            );
        }
    }

    let monitor_result = if cfg.scan.roots.is_empty() {
        Err(rfind_monitor::MonitorError::NotSupported)
    } else {
        let r = Monitor::start(
            &cfg.scan.roots,
            &mark_points,
            dirs_for_inotify,
            Arc::clone(&conn),
            monitor_excludes.clone(),
            prefer_fanotify_effective,
            cfg.monitor.inotify_fallback,
        );
        // With fanotify preferred, a fast restart skips loading the inotify
        // watch list; if fanotify failed and inotify took over, load it now.
        if let Ok((Monitor::Inotify(m), _)) = &r {
            if !needs_full_scan && cfg.monitor.prefer_fanotify {
                let dirs = db::get_all_dirs(&lock(&conn)).unwrap_or_default();
                for d in &dirs {
                    if !monitor_excludes
                        .iter()
                        .any(|ex| Path::new(d).starts_with(ex))
                    {
                        m.add_watch(Path::new(d));
                    }
                }
            }
        }
        r
    };

    let (monitor, mode) = match monitor_result {
        Ok((m, mode)) => {
            info!("Monitoring: {mode}");
            (Some(m), mode)
        }
        Err(_) if cfg.scan.roots.is_empty() => {
            info!("No live roots — periodic roots only");
            (None, "periodic-only".to_string())
        }
        Err(e) => {
            warn!("Monitoring unavailable: {e}");
            warn!("Running in index-only mode.");
            (None, "index-only".to_string())
        }
    };
    let ctx = Ctx {
        conn: Arc::clone(&conn),
        cfg: Arc::new(cfg),
        mode,
        watcher: monitor.as_ref().and_then(|m| m.dir_watcher()),
        cancel: Arc::clone(&cancel),
    };

    // ── Scan or fast-restart, then reconcile ─────────────────────────────────
    // Clear the force-reindex flag AFTER the scan, mirroring the steady-state
    // loop below: a SIGUSR1 arriving during this initial scan is coalesced
    // into it instead of triggering an immediate second full scan.
    let ready = if needs_full_scan {
        let ok = scan::live_reindex(&ctx);
        force_reindex.store(false, Ordering::Relaxed);
        ok
    } else {
        scan::reconcile_and_mark_ready(&ctx, false);
        true
    };
    if ready {
        info!("Daemon ready | monitoring: {}", ctx.mode);
    }

    // ── Steady state loop ────────────────────────────────────────────────────
    // Background jobs: periodic roots and new/removed mounts, one at a time.
    let mut queue: VecDeque<PathBuf> = VecDeque::new();
    // (path currently being scanned in the background, join handle)
    // (path, handle, start_ts, live-entry counter — see "Background jobs").
    let mut bg: Option<(PathBuf, thread::JoinHandle<()>, u64, Arc<AtomicU64>)> = None;
    let mut mount_watch = mountwatch::MountWatch::new(&mount_table);
    let mut last_tick = Instant::now();
    let mut tick = 0u64;
    // Debounce overflow-triggered re-indexes: a local attacker can fill the
    // monitor sink or trigger FAN_Q_OVERFLOW with sustained touch/rm churn
    // under any writable scanned path, which without this guard pins root
    // I/O in a re-index loop. SIGUSR1 (forced) is admin-initiated and is
    // never debounced.
    //
    // Track BOTH an Instant (monotonic, drops suspend time) and a
    // SystemTime wallclock. If the suspend-aware wall clock says more
    // than the debounce has elapsed, we re-arm even if the monotonic
    // clock disagrees — otherwise a laptop suspend/resume stretches the
    // admin-blind window indefinitely.
    const OVERFLOW_REINDEX_DEBOUNCE: Duration = Duration::from_secs(60);
    let mut last_overflow_reindex: Option<(Instant, SystemTime)> = None;
    loop {
        // Short sleeps so shutdown and reload are prompt; work runs every 5s.
        thread::sleep(Duration::from_millis(200));

        let stopping = shutdown.load(Ordering::Relaxed);
        if stopping || reload.load(Ordering::Relaxed) {
            if !stopping {
                // Don't exec into a config the new process would reject.
                if let Err(e) = load_effective_config(&mounts::read_mountinfo()) {
                    error!("Not reloading: config error: {e}");
                    reload.store(false, Ordering::Relaxed);
                    cancel.store(false, Ordering::Relaxed);
                    continue;
                }
            }
            if let Some((_, h, _, _)) = bg.take() {
                let _ = h.join(); // cancelled: returns promptly
            }
            if let Some(m) = monitor {
                m.stop();
            }
            drop(ctx);
            drop(conn);
            write_clean_marker(&marker);
            if stopping {
                info!("Stopped");
                return;
            }
            info!("Re-executing to apply the new configuration");
            reexec();
        }

        if last_tick.elapsed() < Duration::from_secs(5) {
            continue;
        }
        last_tick = Instant::now();
        tick += 1;

        // Monitor liveness: the event-reading thread can exit on an
        // unexpected read() error (lazy unmount, EIO) or panic. If so,
        // no further events arrive and the index silently freezes.
        // Treat it the same as an overflow (force a re-index) and
        // re-exec so a fresh monitor comes up; systemd keeps the pid.
        if monitor.as_ref().is_some_and(|m| m.is_dead()) {
            error!("Monitor event thread exited; re-executing to restart it");
            write_clean_marker(&marker);
            reexec();
        }

        let overflowed_raw = monitor.as_ref().is_some_and(|m| m.take_overflow());
        let forced = force_reindex.swap(false, Ordering::Relaxed);
        // Suppress overflow-only triggers that arrive within the debounce
        // window; a forced trigger always wins. Debounce only when BOTH
        // monotonic AND wall-clock agree we're still inside the window —
        // this prevents a suspend/resume from stretching the window and
        // creating an unbounded admin-blind period.
        let overflow_debounced = overflowed_raw
            && !forced
            && last_overflow_reindex.is_some_and(|(mono, wall)| {
                mono.elapsed() < OVERFLOW_REINDEX_DEBOUNCE
                    && wall
                        .elapsed()
                        .map(|d| d < OVERFLOW_REINDEX_DEBOUNCE)
                        .unwrap_or(false)
            });
        if overflow_debounced {
            warn!(
                "Event overflow during debounce window ({}s); dropping and will reconcile at next tick",
                OVERFLOW_REINDEX_DEBOUNCE.as_secs()
            );
        }
        let overflowed = overflowed_raw && !overflow_debounced;
        if forced {
            // Mark every periodic root due; the background queue picks
            // them up after the live re-index.
            let c = lock(&conn);
            for r in &ctx.cfg.scan.periodic_roots {
                let _ = db::set_metadata(&c, &scan::root_scan_key(&r.path), "0");
            }
        }
        if forced || overflowed {
            info!(
                "Re-index triggered by {}",
                if forced { "SIGUSR1" } else { "event overflow" }
            );
            if overflowed {
                last_overflow_reindex = Some((Instant::now(), SystemTime::now()));
            }
            scan::live_reindex(&ctx);
            // This re-index already covers any SIGUSR1 that arrived while it ran.
            force_reindex.store(false, Ordering::Relaxed);
            continue;
        }

        // Apply pending events.
        let pending = db::pending_event_count(&lock(&conn)).unwrap_or(0);
        if pending > 0 {
            index::reconcile(&conn, &ctx.cfg);
        }

        // Periodic re-index in index-only mode.
        if monitor.is_none()
            && !ctx.cfg.scan.roots.is_empty()
            && ctx.cfg.scan.reindex_interval_hours > 0
            && tick.is_multiple_of(ctx.cfg.scan.reindex_interval_hours * 720)
        {
            info!(
                "Periodic re-index (index-only mode, interval {}h)",
                ctx.cfg.scan.reindex_interval_hours
            );
            scan::live_reindex(&ctx);
        }

        // Filesystems mounted / unmounted under live roots.
        if ctx.cfg.monitor.watch_new_mounts {
            if let Some(changes) = mount_watch.poll(&mount_table) {
                let in_flight = bg.as_ref().map(|(p, _, _, _)| p);
                on_mount_change(&ctx, monitor.as_ref(), &changes, &mut queue, in_flight);
                mount_table = changes.table;
            }
        }

        // Background jobs, one at a time, so live events keep being applied
        // while a slow drive is walked. Each start/finish updates the
        // progress file so `rfind status` can show the running scan.
        // bg carries (root, join_handle, start_ts, shared counter) so
        // the main loop can republish live progress each tick.
        if bg
            .as_ref()
            .is_some_and(|(_, h, _, _): &(PathBuf, thread::JoinHandle<()>, u64, Arc<AtomicU64>)| {
                h.is_finished()
            })
        {
            bg = None;
            publish_bg_scan(&conn, &ctx.mode, None);
        }
        if bg.is_none() {
            if queue.is_empty() {
                queue.extend(scan::due_periodic_roots(&ctx));
            }
            if let Some(root) = queue.pop_front() {
                let ctx = ctx.clone();
                let root_clone = root.clone();
                let started_at = now_secs() as u64;
                let counter = Arc::new(AtomicU64::new(0));
                let counter_for_thread = Arc::clone(&counter);
                publish_bg_scan(
                    &conn,
                    &ctx.mode,
                    Some(BackgroundScan {
                        // World-readable (0644), so any attacker-created
                        // mount point's bytes land in `rfind status` output
                        // and admin terminals. Sanitize before serializing.
                        path: sanitize_path(&root_clone),
                        entries: 0,
                        started_at,
                    }),
                );
                let handle = thread::spawn(move || {
                    info!("Background scan of {}", sanitize_path(&root_clone));
                    scan::scan_root_with_counter(
                        &ctx,
                        &root_clone,
                        false,
                        Some(counter_for_thread),
                    );
                });
                bg = Some((root, handle, started_at, counter));
            }
        } else if let Some((root, _, started_at, counter)) = &bg {
            // Live progress: republish BackgroundScan with the current
            // entry count on every tick (5s cadence — the main loop's
            // steady-state work interval). Keeps `rfind status` from
            // showing a static "0 entries" through a multi-minute walk
            // of a WSL2 Windows drive.
            publish_bg_scan(
                &conn,
                &ctx.mode,
                Some(BackgroundScan {
                    path: sanitize_path(root),
                    entries: counter.load(Ordering::Relaxed),
                    started_at: *started_at,
                }),
            );
        }

        // Heartbeat every 5 minutes.
        if tick.is_multiple_of(60) {
            let cnt = db::entry_count(&lock(&conn)).unwrap_or(0);
            info!("Heartbeat | {cnt} entries | monitoring: {}", ctx.mode);
        }
    }
}
