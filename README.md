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
| Search by exact name | **40ms** | ~3 min | ~4,500× |
| Search by glob `*.conf` | **120ms** | ~3 min | ~1,500× |
| Initial index build (136k entries) | **3s** | — | — |
| Re-index after restart (journal only) | **<1s** | — | — |

> Benchmarks on Ubuntu 26.04, WSL2, ~136k indexed entries, debug build.
> Release build (`--release`) is significantly faster.

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
git clone https://github.com/YanivSaar/rfind.git && cd rfind && bash scripts/install.sh
```

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
| Linux kernel | 5.1+ (6.x recommended) |
| Rust | 1.70+ (for building) |
| Root access | Required (fanotify/inotify) |
| WSL2 | ✅ Fully supported (kernel 5.15+) |
| Bare metal | ✅ Full fanotify support |
| VM (KVM/VMware) | ✅ Full fanotify support |
| Container | ⚠️ Index-only mode |

### Manual build

```bash
# Install Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"

# Clone and build
git clone https://github.com/YanivSaar/rfind.git
cd rfind
cargo build --release

# Install
sudo bash scripts/install.sh
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
exclude = ["/proc", "/sys", "/dev", "/run", "/mnt", "/media"]
workers = 0                    # 0 = auto-detect CPU count
reindex_interval_hours = 6     # auto re-index every 6h in index-only mode
collect_mtime = true           # collect file modification times (recommended)

[index]
db_path = "/var/lib/rfindd/index.db"
batch_size = 10000

[monitor]
prefer_fanotify = true         # try fanotify first (bare metal/VM)
inotify_fallback = true        # fall back to inotify (WSL2/containers)
```

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
│   └── integration/      # 15 integration tests
├── scripts/
│   └── install.sh        # Build + install script
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

| Backend | Coverage | Watches needed | Environment |
|---------|----------|----------------|-------------|
| **fanotify** | Entire mount point | **1** | Bare metal, full VM |
| **inotify** | Per-directory | ~9,000 (all indexed dirs) | WSL2, containers |
| **index-only** | None (periodic rescan) | 0 | Restricted containers |

### Why fanotify fails on WSL2

WSL2's Microsoft-built kernel restricts `FAN_MARK_MOUNT` on the root filesystem.
`fanotify_init` succeeds but `fanotify_mark(FAN_MARK_MOUNT)` returns `EINVAL` (errno 22).

rfind detects this immediately and falls back to inotify automatically — no configuration needed.
On a production Linux server or full VM, fanotify activates instead, covering the entire
filesystem with a single watch descriptor.

**Fallback chain:**

```
fanotify_mark(FAN_MARK_MOUNT, "/")
        │
        ├── OK  →  fanotify active, one watch covers everything
        │
        └── EINVAL/EPERM
                │
                ├── inotify_init1 OK  →  inotify active, one watch per directory
                │                        auto-registers new directories on CREATE
                │
                └── inotify fails  →  index-only mode
                                       periodic re-index every N hours
```

### Database schema

```sql
-- Core index: every filesystem entry
CREATE TABLE entries (
    id            INTEGER PRIMARY KEY,
    parent_id     INTEGER,           -- directory relationship
    basename      TEXT NOT NULL,     -- filename only (fast name search)
    full_path     TEXT NOT NULL,     -- absolute path (fast path search)
    is_dir        INTEGER NOT NULL,
    mtime_sec     INTEGER NOT NULL,  -- modification time (Unix seconds)
    mtime_nsec    INTEGER NOT NULL,  -- nanosecond precision
    discovered_at INTEGER NOT NULL   -- when indexed
);

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

-- Daemon state: last scan time, entry count, monitoring mode
CREATE TABLE scan_metadata (key TEXT PRIMARY KEY, value TEXT);
```

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
| 136k | ~48 MB |
| 1M | ~350 MB |
| 10M | ~3.5 GB |
| 100M | ~35 GB |

Storage scales linearly. Includes mtime and discovered_at columns.
Normalized parent-ID schema avoids storing full paths redundantly.

---

## 🔒 Security

- Daemon runs as **root** (required for fanotify/inotify on system directories)
- Index at `/var/lib/rfindd/index.db` — root-owned
- Progress file at `/run/rfindd/progress.json` — world-readable (filenames only, no content)
- Search queries read the index directly — no daemon IPC required
- Symlinks are **not followed** during scan (prevents cycles)
- Excluded paths (`/proc`, `/sys`, `/dev`, `/run`, `/mnt`, `/media`) are never indexed

---

## ✅ Testing

```bash
# Run all tests (requires running rfindd with completed scan)
cargo test -- --ignored

# Run unit tests only (no daemon required)
cargo test
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
- [x] fanotify monitoring (bare metal / VM)
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

*From `find / -name passwd` taking 3 minutes to `rfind name passwd` taking 40ms*

</div>