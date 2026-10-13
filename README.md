<div align="center">

# 🔍 rfind

**A high-performance Linux filesystem index and search daemon**

*Find any file on your system in milliseconds, not minutes*

[![CI](https://github.com/YanivSaar/rfind/actions/workflows/ci.yml/badge.svg)](https://github.com/YanivSaar/rfind/actions)
[![Rust](https://img.shields.io/badge/rust-1.99%2B-orange.svg)](https://www.rust-lang.org)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![Platform: Linux](https://img.shields.io/badge/platform-Linux-green.svg)](https://kernel.org)

</div>

---

## ⚡ Performance

| Operation | rfind | GNU find | Speedup |
|-----------|------:|--------:|--------:|
| Search by exact name | **6 ms** | 1.8 s | ~300× |
| Search by glob `*.conf` | **116 ms** | 1.9 s | ~16× |
| Regex `^lib.*\.so\.[0-9]+$` | **189 ms** | 2.0 s | ~10× |
| Under a directory (`/etc`, `*.conf`) | **5 ms** | 8 ms | ~1.6× |
| Change visible after create / rename / delete | **≤ 5 s** | immediate | — |
| Initial index build, Linux side (146k entries) | **2.0 s** | — | — |

> Release build, WSL2 (4 cores), 1.46M indexed entries (Linux + a Windows
> drive). `find` timings cover the Linux side only (146k entries); rfind
> searched all 1.46M. Every query returned exactly `find`'s results.
> Reproduce on your machine: `sudo ./scripts/bench.sh`.

---

## 🧠 How It Works

rfind maintains a **persistent SQLite index** of your filesystem, built once
and kept current via kernel filesystem notifications. No more waiting for
`find` to traverse millions of directories.

```
┌─────────────────────────────────────────────────────────┐
│                        rfindd                           │
│                                                         │
│  ┌──────────────┐    events    ┌─────────────────────┐  │
│  │   fanotify   │─────────────▶│   pending_events    │  │
│  │  (bare metal)│              │   (durable journal) │  │
│  │      or      │              └──────────┬──────────┘  │
│  │   inotify    │                         │ reconcile   │
│  │   (WSL2)     │              ┌──────────▼──────────┐  │
│  └──────────────┘              │   entries index     │  │
│                                │   (SQLite WAL)      │  │
│  ┌──────────────┐    insert    │                     │  │
│  │  getdents64  │─────────────▶│  basename index     │  │
│  │  + statx     │              │  mtime index        │  │
│  │  parallel    │              │  path index         │  │
│  │  scanner     │              └─────────────────────┘  │
│  └──────────────┘                                       │
│                                                         │
│    Scanning ──▶ Reconciling ──▶ Ready                   │
└─────────────────────────────────────────────────────────┘
```

### State Machine

```
  ┌─────────────┐     scan      ┌──────────────┐    apply     ┌───────────┐
  │ INITIALIZING│──────────────▶│   SCANNING   │─────────────▶│RECONCILING│
  └─────────────┘               │              │   queued     └─────┬─────┘
                                │ fanotify/    │   events           │
                                │ inotify      │                    ▼
                                │ queuing      │              ┌───────────┐
                                │ events       │              │   READY   │
                                └──────────────┘              └─────┬─────┘
                                                                     │
                                                       live events   │
                                                       applied every │
                                                       5 seconds  ◀──┘
```

---

## 🚀 Quick Start

### One-liner install

```bash
curl -fsSL https://raw.githubusercontent.com/YanivSaar/rfind/main/scripts/install.sh | sudo bash
```

Builds from source (installs a C compiler and Rust if missing), installs
`rfind` + `rfindd` to `/usr/local/bin`, and enables the `rfindd` systemd
service. On WSL2 a first install configures Windows drives as periodic roots.

| Task | One-liner |
|------|-----------|
| Install / update | `curl -fsSL https://raw.githubusercontent.com/YanivSaar/rfind/main/scripts/install.sh \| sudo bash` |
| From a clone | `git clone https://github.com/YanivSaar/rfind.git && cd rfind && sudo ./scripts/install.sh` |
| Specific branch/tag | `curl -fsSL …/install.sh \| sudo RFIND_REF=<branch-or-tag> bash` |
| Uninstall (keep index) | `curl -fsSL …/install.sh \| sudo bash -s -- --uninstall` |
| Uninstall + purge | `curl -fsSL …/install.sh \| sudo bash -s -- --uninstall --purge` |
| Watch first scan | `rfind status --watch` |
| Benchmark vs find/locate | `sudo ./scripts/bench.sh --quick` |

Updating is the same command: it rebuilds, swaps the binaries, restarts the
daemon and never overwrites `/etc/rfindd/config.toml`. If the index format
changed, rfindd rebuilds the index on its own.

### Search immediately after install

```bash
# Find a file by exact name
rfind name passwd

# Find all config files
rfind name "*.conf"

# Find configs under /etc only
rfind name "*.conf" --under /etc

# Find only directories named "cache"
rfind name cache --type d

# Find recently modified files (last hour)
rfind recent --hours 1

# Full path glob
rfind path "/usr/lib/python*/*.py"
```

---

## 📦 Installation

### Requirements

| Requirement | Version |
|-------------|---------|
| Linux kernel | 5.9+ for fanotify (5.17+ for single-event renames); older kernels use inotify |
| Rust | 1.77+ (for building; the installer sets it up) |
| Architecture | x86_64, aarch64 (any Linux target with `statx`) |
| Root access | Required: the daemon needs it to monitor, and searches need it because the index lists every file name |
| WSL2 | ✅ fanotify on `/`; Windows drives as periodic roots |
| Bare metal / VM / VPS | ✅ fanotify, one mark per filesystem |
| Container | ⚠️ inotify or index-only, depending on capabilities |

### Manual build

```bash
# Install Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"

# Clone and build
git clone https://github.com/YanivSaar/rfind.git
cd rfind
cargo build --release

# Install (reuses the build above)
sudo ./scripts/install.sh
```

---

## 🔍 Usage

### Search commands

```bash
# By filename
rfind name passwd                    # exact match (case-sensitive)
rfind name "*.log"                   # glob: all .log files
rfind name "app.???.js"              # glob: ? matches one character
rfind name "*.py" --under /home      # scoped to directory tree
rfind name "*.conf" --type f         # files only
rfind name logs --type d             # directories only
rfind name "*.log" --limit 10        # stop after 10 results
rfind name "^app\." --regex          # regular expression

# By path
rfind path "/etc/nginx*"             # full path glob
rfind path "/home/*/.*"              # dotfiles in home dirs
rfind path "/usr/lib/*.so.*"         # shared libraries

# By modification time
rfind recent                         # last 24 hours (default)
rfind recent --minutes 30            # last 30 minutes
rfind recent --hours 6               # last 6 hours
rfind recent --days 7                # last 7 days
rfind recent --from 2026-10-01 --to 2026-10-13   # date range
rfind recent --days 1 --type f       # files only, last day
rfind recent --hours 1 --limit 20    # cap results
```

### Daemon commands

```bash
# Status
rfind status                         # current state snapshot
rfind status --watch                 # live refresh until Ready

# Health and stats
rfind health                         # index freshness, monitoring mode, DB size
rfind stats                          # entry counts, top file types, largest dirs

# Control
rfind update                         # trigger immediate re-index (SIGUSR1)
```

### systemd integration

```bash
systemctl status rfindd              # status + recent log lines
systemctl restart rfindd             # restart (smart: replays journal if fresh)
journalctl -u rfindd -f              # follow live logs
journalctl -u rfindd --since "1h ago"
```

---

## ⚙️ Configuration

Default config is written to `/etc/rfindd/config.toml` on first run:

```toml
[scan]
roots = ["/"]
exclude = ["/proc", "/sys", "/dev", "/run", "/media"]
workers = 0                    # 0 = auto-detect CPU count
reindex_interval_hours = 6     # auto re-index every 6h in index-only mode
collect_mtime = true           # collect file modification times (recommended)

[index]
db_path = "/var/lib/rfindd/index.db"
batch_size = 10000

[monitor]
prefer_fanotify = true         # one mark per filesystem (kernel 5.9+, root)
inotify_fallback = true        # per-directory watches if fanotify is unavailable
```

### Periodic roots (WSL2 Windows drives, network mounts)

Some mounts can't be monitored: on WSL2, inotify never sees changes made
from Windows to `/mnt/c`, and watching a whole drive can exhaust
`fs.inotify.max_user_watches`. Index them with periodic rescans instead:

```toml
[[scan.periodic_roots]]
path = "/mnt/c"
interval_hours = 12            # default 12
```

Periodic roots are rescanned in a background thread, so live events for
other roots keep being applied during a long scan. A periodic root nested
inside a live root (here `/mnt/c` under `/`) is skipped by the live scan.
`rfind update` rescans live roots immediately and schedules periodic roots.
At startup rfindd warns about any Windows drive under a live root that is
neither excluded nor periodic.

---

## 🏗️ Architecture

### Crate structure

```
rfind/
├── crates/
│   ├── rfind-config/     # Configuration parsing (TOML)
│   ├── rfind-core/       # Shared types, DB schema, all search queries
│   ├── rfind-scanner/    # Parallel scanner: getdents64 + statx
│   ├── rfind-monitor/    # fanotify + inotify filesystem monitoring
│   ├── rfindd/           # Daemon binary
│   └── rfind-cli/        # rfind CLI binary
├── tests/
│   └── integration/      # Integration tests against the live index
├── scripts/
│   ├── install.sh        # Build + install / update / uninstall
│   └── bench.sh          # Benchmark vs find and locate
├── packaging/
│   └── systemd/          # rfindd.service unit file
└── .github/
    └── workflows/
        └── ci.yml        # CI: fmt + clippy + test + release build
```

### Scanner design

The scanner uses raw Linux syscalls for maximum performance:

**Traditional approach (slow):**

```
std::fs::read_dir  →  one PathBuf + syscall per entry
fs::metadata(full_path)  →  full path resolution per stat call
```

**rfind approach (fast):**

```
open(dir, O_DIRECTORY)           →  one fd per directory
getdents64(fd, buf, 65536)       →  bulk: ~256 entries per syscall
statx(dirfd, "filename", MTIME)  →  relative: no full path resolution
push child dirs to work queue    →  parallel: N workers × N subtrees
```

Result: 3–10× faster than `std::fs::read_dir` with mtime collection essentially free.

### Monitoring backends

| Backend | Coverage | Kernel objects | Used when |
|---------|----------|----------------|-----------|
| **fanotify** | Whole filesystem | **1 mark per filesystem** | Kernel 5.9+ with root (bare metal, VM, VPS, WSL2) |
| **inotify** | Per directory | 1 watch per directory | fanotify unavailable (older kernels, restricted containers) |
| **index-only** | None (periodic rescan) | 0 | Neither is available |

fanotify runs in FID mode (`FAN_REPORT_DFID_NAME`): each event carries the
parent directory's file handle and the entry name, which rfindd resolves to a
path with `open_by_handle_at()` against that filesystem's mount. Directory
create, delete and move events are only reported in this mode — requesting
them on a plain mount mark is what makes `fanotify_mark` fail with `EINVAL`.
On kernel 5.17+ a rename arrives as one `FAN_RENAME` event carrying both
paths; on 5.9–5.16 it arrives as a move-from/move-to pair and a moved
directory is re-walked.

rfindd marks each local filesystem under the live roots once. Pseudo
filesystems (`proc`, `sysfs`, …), bind-mount aliases and excluded paths are
skipped; network, overlay and read-only filesystems are indexed but not
monitored live (the log suggests `scan.periodic_roots` for them).

**Fallback chain:**

```
fanotify_init(FID mode) + FAN_MARK_FILESYSTEM on each filesystem
        │
        ├── OK  →  fanotify active, no per-directory watches
        │
        └── fails (old kernel, no CAP_SYS_ADMIN)
                │
                ├── inotify_init1 OK  →  inotify active, one watch per directory,
                │                        watches added before each directory is read
                │
                └── inotify fails  →  index-only mode
                                       periodic re-index every N hours
```

### Database schema

Schema v4 is normalized: each directory path is stored once, and each entry
stores only its name and its parent directory's id. A full path is
`dirs.path || '/' || entries.name` (the root directory is stored as `''`).
This halves the index compared with storing every full path (which SQLite
also duplicated in the path index), and renaming a directory only rewrites
`dirs` rows.

```sql
-- One row per directory that has entries
CREATE TABLE dirs (
    id   INTEGER PRIMARY KEY,
    path TEXT NOT NULL UNIQUE        -- no trailing '/'; root is ''
);

-- Every filesystem entry
CREATE TABLE entries (
    id         INTEGER PRIMARY KEY,
    dir_id     INTEGER NOT NULL,     -- parent directory (dirs.id)
    name       TEXT    NOT NULL,     -- basename
    is_dir     INTEGER NOT NULL,
    mtime_sec  INTEGER NOT NULL,     -- modification time (Unix seconds)
    mtime_nsec INTEGER NOT NULL,
    scan_gen   INTEGER NOT NULL      -- last scan generation that saw it
);
CREATE UNIQUE INDEX idx_entries_dir_name ON entries (dir_id, name);  -- upserts
CREATE INDEX        idx_entries_name     ON entries (name);          -- name search
CREATE INDEX        idx_entries_mtime    ON entries (mtime_sec);     -- rfind recent

-- Durable event journal: survives daemon crashes and restarts
CREATE TABLE pending_events (
    id         INTEGER PRIMARY KEY,
    event_type TEXT,      -- CREATE / DELETE / RENAME / MODIFY
    path       TEXT,
    new_path   TEXT,      -- for RENAME events
    is_dir     INTEGER,
    timestamp  INTEGER,
    applied    INTEGER    -- 0=pending, 1=applied
);

-- Daemon state: last scan time, scan generation, monitoring mode
CREATE TABLE scan_metadata (key TEXT PRIMARY KEY, value TEXT);
```

The index is a cache of the filesystem: an index with an older schema
version (`PRAGMA user_version`) is dropped and rebuilt by a full scan.

**Consistency:** scans upsert rows tagged with a new generation, then delete
the rows under their root that they didn't see — searches never see an
empty index mid-scan. Journaled events are applied by re-checking each path
on disk, in transactions that also mark them applied, so replays are
harmless. Bind mounts of an already-indexed filesystem (e.g. WSL2's
`/mnt/wslg/distro`) are detected from `/proc/self/mountinfo` and skipped.

### Restart behaviour

```
On daemon restart:
  ├── Read last_scan_completed from scan_metadata
  │
  ├── Index age < reindex_interval_hours?
  │     └── YES → Fast restart: replay unapplied journal events only (~<1s)
  │
  └── Index age > reindex_interval_hours (or first run)?
        └── YES → Full rescan (~3s for 136k entries)
```

---

## 📊 Storage estimates

| Entries | Index size (approx) |
|--------:|--------------------:|
| 150k | ~30 MB |
| 1.5M | ~300 MB |
| 10M | ~2 GB |
| 100M | ~20 GB |

Measured: 1.46M entries (WSL2, Linux + a Windows drive) → 296 MB, about
215 bytes/entry including the name, directory and mtime indexes.

--------:|--------------------:|
| 136k | ~48 MB |
| 1M | ~350 MB |
| 10M | ~3.5 GB |
| 100M | ~35 GB |

Storage scales linearly. Includes mtime and discovered_at columns.
Normalized parent-ID schema avoids storing full paths redundantly.

---

## 🔒 Security

- Daemon runs as **root** (required for fanotify/inotify on system directories)
- The index is **root-only** (`/var/lib/rfindd` is `0700`, the database `0600`): it lists every
  file name on the system, including other users' files, so `rfind` searches need `sudo`
- Index at `/var/lib/rfindd/index.db` — root-owned
- Progress file at `/run/rfindd/progress.json` — world-readable (filenames only, no content)
- Search queries read the index directly — no daemon IPC required
- Symlinks are **not followed** during scan (prevents cycles)
- Excluded paths (default `/proc`, `/sys`, `/dev`, `/run`, `/media`) are never indexed

---

## ✅ Testing

```bash
# Run all tests (requires running rfindd with completed scan)
cargo test -- --ignored

# Run unit tests only (no daemon required)
cargo test

# Compare against find and locate on this machine (latency, accuracy, freshness)
sudo ./scripts/bench.sh --quick     # Linux side only, ~1 min
sudo ./scripts/bench.sh             # include periodic roots such as /mnt/c
```

### Test coverage (15 tests)

| Category | Tests |
|----------|------:|
| Exact name match | 2 |
| Case sensitivity | 2 |
| Glob patterns (`*` and `?`) | 3 |
| Scoped search (`--under`) | 3 |
| Excluded paths | 1 |
| Database integrity | 4 |
| **Total** | **15** |

All tests pass in ~0.2s.

---

## 🛣️ Roadmap

- [x] Parallel scanner with `getdents64` + `statx`
- [x] SQLite persistent index with WAL mode
- [x] fanotify monitoring: per-filesystem marks in FID mode (kernel 5.9+)
- [x] inotify fallback (WSL2 / containers)
- [x] Durable event journal with reconciliation
- [x] Smart restart (journal-only when index is fresh)
- [x] `rfind recent` with time range queries (minutes/hours/days/date range)
- [x] `rfind health` — index freshness and monitoring status
- [x] `rfind stats` — entry counts, top file types, largest directories
- [x] `rfind update` — force re-index via SIGUSR1
- [x] `--type f/d`, `--limit N`, `--regex` search flags
- [x] Configurable roots, excludes, worker count
- [x] Periodic re-index in index-only mode
- [x] systemd service with proper lifecycle management
- [x] CI pipeline (fmt + clippy + test + release build)
- [ ] Scale validation at 10M+ entries
- [ ] Release build benchmarks vs `find` and `plocate`
- [ ] `io_uring` batched statx for even faster scanning
- [ ] JSON output mode (`--json`)
- [ ] Man page (`rfind.1`)

---

## 🤝 Contributing

Issues and PRs welcome.

```bash
# Before submitting
cargo fmt --all
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
```

---

## 📄 License

MIT — see [LICENSE](LICENSE)

---

<div align="center">

Built with ❤️ in Rust &nbsp;•&nbsp; Designed for Linux &nbsp;•&nbsp; Fast by design

*From `find / -name passwd` taking seconds to minutes to `rfind name passwd` taking 6 ms*

</div>