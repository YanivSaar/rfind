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

Measured with `scripts/bench.sh` (release build, WSL2, 4 cores) on a real
system with 1.52M indexed entries, against plocate, GNU locate and find.
Every query returned exactly `find`'s results.

| Operation | rfind | plocate | find |
|-----------|------:|--------:|-----:|
| Exact name (`passwd`) | **7 ms** | 13 ms | 1.7 s |
| Glob `*.conf` | 104 ms | **15 ms** | 1.6 s |
| Under a directory (`/etc`, `*.conf`) | **7 ms** | 6 ms | 8 ms |
| Total size of `/usr` (9.8 GB) | **25 ms** | — | 5.0 s |
| 10 largest files under `/usr` | **38 ms** | — | 5.3 s |
| Change visible (create / rename / append / delete) | **≤ 5 s** | at next `updatedb` | immediate |
| Linux side index build (185k entries) | **2.2 s** | 2.3 s | — |

> plocate's database here held only the 185k Linux-side entries while rfind
> searched all 1.52M (including a Windows drive), so plocate's glob and regex
> times are not like-for-like. On equal data (10M below) rfind is faster on
> globs and exact names; plocate keeps the edge on regexes and size.

### Scale: 10 million files

A real 10,000,001-file tree on ext4 (10,201 directories), indexed by an
isolated rfindd:

| | rfind | plocate | find |
|---|---:|---:|---:|
| Initial index build | 70 s (with sizes, io_uring) | **14 s** (names only) | — |
| Index size | 1.03 GB (103 B/entry) | **62 MB** | — |
| Daemon memory (peak) | 84 MB | — | — |
| Exact name | **8–13 ms** | 380 ms | 0.7 s warm / 5.3 s cold |
| Glob `*.md` (1M results) | **1.6 s** | 3.6 s | 1.8 s |
| First 10 matches of a glob | **5 ms** | 32 ms | — |
| Regex over all names | 1.5 s | **0.85 s** | — |
| Total size of all 10M files | **3.1 s** | — | `du`: 61 s |
| Size of a 50k-file subtree | **21 ms** | — | — |
| `largest` / `--larger-than 1M` | **4–7 ms** | — | — |
| Create, rename (50k-entry dir), delete (50k subtree) visible | **≤ 5 s** | at next `updatedb` | — |
| Daemon restart (journal replay) | **0.4 s** | — | — |
| Full rescan | 57 s | 14 s | — |

### io_uring

The scanner batches `statx` through io_uring (`scan.io_uring`, on by default):

| Tree | plain `statx` | io_uring |
|------|--------------:|---------:|
| `/usr` on ext4 (57k entries) | 33–37k entries/s | **62–66k entries/s** |
| `C:\Windows\System32` over 9p (35k) | 4.0k entries/s | **7.4k entries/s** |

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

Downloads a static binary for your architecture (x86_64 or aarch64, checksum
verified) — or builds from source if there's no release for it — and installs
`rfind` + `rfindd`, the man pages and bash/zsh/fish completions, and the
`rfindd` systemd service. On WSL2 a first install configures Windows drives as
periodic roots.

| Task | One-liner |
|------|-----------|
| Install / update | `curl -fsSL https://raw.githubusercontent.com/YanivSaar/rfind/main/scripts/install.sh \| sudo bash` |
| From a clone | `git clone https://github.com/YanivSaar/rfind.git && cd rfind && sudo ./scripts/install.sh` |
| Specific release | `curl -fsSL …/install.sh \| sudo RFIND_VERSION=v0.6.0 bash` |
| Build from source | `curl -fsSL …/install.sh \| sudo RFIND_FROM_SOURCE=1 bash` |
| Uninstall (keep index) | `sudo rfind uninstall` |
| Uninstall + purge | `sudo rfind uninstall --purge` |
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
| Rust | 1.77+ — only to build from source; the installer downloads static binaries |
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

Searching needs root (`sudo`): the index lists every file on the system.
Every command has `--help`, and `man rfind` documents all of them.

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

# By modification time
rfind recent                         # last 24 hours (default)
rfind recent --minutes 30            # last 30 minutes
rfind recent --from 2026-10-01 --to 2026-10-13   # date range (local time)

# By size (any search; K/M/G/T are powers of 1024)
rfind name "*.iso" --larger-than 1G
rfind name "*.log" --under /var --smaller-than 1K
rfind recent --hours 2 -l            # -l / --long: size and time per result
```

### Disk usage — from the index, no disk walk

```bash
rfind size /home                     # total + 20 largest children (like du -d1)
rfind size / --top 5
rfind largest --under /var --limit 10
```

Directory sizes are summed from the indexed file sizes at query time;
nothing extra is stored per directory.

### Scripting

```bash
rfind name "*.log" --json            # one JSON object per result (JSON Lines)
# {"path":"/var/log/syslog","type":"file","size":81234,"mtime":1791900000}
rfind --json health                  # single objects for status/health/stats/config
rfind name "*.log" --json | jq -r 'select(.size > 1e8) | .path'
```

Exit status: `0` results found, `1` no results, `2` error.

### Configuration commands

`rfind config` edits `/etc/rfindd/config.toml` in place — comments are kept,
every change is validated before it's written, and rfindd reloads
automatically (it purges newly excluded entries and scans what was added,
without rebuilding the index):

```bash
rfind config exclude add /home/moshe        # stop indexing a directory
rfind config exclude list
rfind config root add /data                 # another live (monitored) root
rfind config periodic add /mnt/nas --every 24   # rescan a share daily
rfind config set scan.workers 8
rfind config get scan.exclude
rfind config keys                           # every setting, value and meaning
rfind config edit                           # $EDITOR, validated on save
rfind config check
rfind config reset --yes                    # back to defaults (keeps a .bak)
```

Add `--no-reload` to only write the file; `rfind reload` applies it later.

### Daemon commands

```bash
rfind status                         # current state snapshot
rfind status --watch                 # live refresh until Ready
rfind health                         # LIVE/FRESH/STALE, entries, bytes, DB size
rfind stats                          # top file types, largest directories
rfind update                         # rescan now (SIGUSR1)
rfind reload                         # re-read the config (SIGHUP)
rfind uninstall [--purge]            # remove everything (--purge: index + config too)
```

### Shell completions

Installed for bash, zsh and fish by the installer; for other shells:
`rfind completions <bash|zsh|fish|elvish|powershell>`.

### systemd integration

```bash
systemctl status rfindd              # status + recent log lines
systemctl reload rfindd              # re-read the config in place
systemctl restart rfindd             # restart (smart: replays journal if fresh)
journalctl -u rfindd -f              # follow live logs
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
| 10M | ~1–2 GB |
| 100M | ~10–20 GB |

Measured: 1.52M real entries → 302 MB (~200 B/entry, long Windows paths);
10M synthetic entries with short names → 1.03 GB (103 B/entry). Includes the
name, directory, mtime and big-file indexes.

--------:|--------------------:|
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

### Test coverage

| Suite | Tests | Needs a running rfindd |
|-------|------:|:---:|
| Index: upserts, subtrees, renames, sweeps, sizes, disk usage, read-only open | 17 | no |
| Mount table: bind-mount aliases, fanotify mark points | 8 | no |
| Config: parsing, validation, comment-preserving edits | 10 | no |
| CLI: size parsing | 1 | no |
| Integration: searches and invariants on the live index | 15 | yes |

End to end, the daemon is exercised in an isolated mount namespace
(scanning, live events with fanotify and inotify, config reloads, new
mounts, cancellation), and `scripts/bench.sh` checks every query against
`find` on the real system.

----------|------:|
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

- [x] Parallel scanner with `getdents64` + `statx`, batched through io_uring
- [x] SQLite persistent index (WAL), normalized schema
- [x] fanotify monitoring: per-filesystem marks in FID mode (kernel 5.9+)
- [x] inotify fallback (older kernels / containers)
- [x] Durable event journal with idempotent replay
- [x] Smart restart, config-change aware (purge / rescan only what changed)
- [x] Periodic roots (WSL2 Windows drives, network shares)
- [x] Bind-mount alias detection; new mounts indexed and watched
- [x] File sizes, `rfind size` (du from the index), `rfind largest`, size filters
- [x] `rfind config` (comment-preserving edits, validation, live reload)
- [x] JSON output (`--json`), grep-style exit codes, shell completions
- [x] Man pages: `rfind(1)`, `rfindd(8)`, `rfind.conf(5)`
- [x] Static x86_64 / aarch64 release binaries; one-line install and uninstall
- [x] Scale validation at 10M entries; benchmarks vs find, locate, plocate
- [ ] Cache resolved directory handles (fanotify CPU under event floods)
- [ ] Extension index for `*.ext` globs
- [ ] Per-directory size totals for instant whole-disk `rfind size`
- [ ] Debian / RPM packages

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