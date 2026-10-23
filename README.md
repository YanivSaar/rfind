<div align="center">

# 🔍 rfind

**A high-performance Linux filesystem index and search daemon**

*Find any file on your system in milliseconds, not minutes*

[![CI](https://github.com/YanivSaar/rfind/actions/workflows/ci.yml/badge.svg)](https://github.com/YanivSaar/rfind/actions)
[![Rust](https://img.shields.io/badge/rust-1.77%2B-orange.svg)](https://www.rust-lang.org)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![Platform: Linux](https://img.shields.io/badge/platform-Linux-green.svg)](https://kernel.org)

<video src="https://raw.githubusercontent.com/YanivSaar/rfind/main/docs/media/rfind-brag.mp4"
       poster="https://raw.githubusercontent.com/YanivSaar/rfind/main/docs/media/rfind-brag.jpg"
       controls muted playsinline width="720">
  <a href="https://raw.githubusercontent.com/YanivSaar/rfind/main/docs/media/rfind-brag.mp4">
    <img src="https://raw.githubusercontent.com/YanivSaar/rfind/main/docs/media/rfind-brag.jpg"
         alt="rfind — find any file on your system in milliseconds, not minutes" width="720">
  </a>
</video>

</div>

---

## 🧭 What this is and why it exists

`rfind` is a Linux filesystem indexer and search tool for the case
other tools don't quite cover: **answer any question about any file on
the system in milliseconds, and stay correct as the filesystem changes.**

The daemon (`rfindd`) builds a persistent SQLite index of every file
and keeps it current by subscribing to kernel filesystem events
(`fanotify` on 5.9+, `inotify` elsewhere). The CLI (`rfind`) runs search
queries directly against the index — no daemon round-trip, no re-walk.

### What the usual tools miss

- **`find`** walks the tree every time you ask. On a 10M-entry disk
  that's seconds-to-minutes per query, and it has nothing to say about
  "what changed in the last hour" without walking everything with
  `-newermt`.
- **`locate` / `mlocate` / `updatedb`** are fast but **stale**: the
  index is a nightly cron, so files created today are invisible until
  tomorrow. There's no size filter, no mtime filter, no "largest files
  under /var", and no notion of freshness or incremental update.
- **`plocate`** is the fastest of the locate family, but still updates
  on cron; the database doesn't carry sizes or mtimes, so queries like
  "biggest files under /usr", "files bigger than 1 GB", or "modified
  in the last hour" aren't expressible.
- **GUI indexers** (Baloo, Tracker, Recoll) index content — slow,
  memory-hungry, often per-user, and not what you want for shell /
  scripting workflows.

### What `rfind` does differently

- **Live, not nightly.** fanotify / inotify stream events into a
  durable journal; a change is visible in the next search in ≤ 5 s
  (the reconcile tick), not next `updatedb`.
- **The index knows sizes and mtimes.** Queries like `rfind recent
  --hours 1`, `rfind name "*.iso" --larger-than 1G`, `rfind size /var`,
  and `rfind largest --under /usr` run against the index with no disk
  walk.
- **Scaled for real systems.** 10M entries indexed in ~5 min, kept
  current with < 100 MB RAM; most searches finish in 1–10 ms.
- **Correctness built in.** Searches pin a read snapshot (no path
  stitching across concurrent writes); the daemon holds a pidfile
  flock so two instances cannot race; the scanner catches worker
  panics instead of hanging; crash safety is tested with SIGKILL
  mid-write (append-only, CRC-checked compressed VFS; SQLite in
  `journal_mode=TRUNCATE` to stay crash-safe on 9p/virtiofs shared
  mmap pages).
- **Small on disk.** The index is name-interned, directory-trie
  structured, and zstd page-compressed by default — about 47 bytes
  per entry on real data (68 MB for 1.5M entries, ~400 MB for 10M).
- **Not a service you have to think about.** One systemd unit, one
  config file, `rfind` as a drop-in `find`/`locate` replacement for the
  common questions.

TL;DR: the goal is for `find / -name foo` and `du -sh /home` and
"what was changed today" to all answer **instantly** on the biggest
system you own, with the kernel keeping the index honest.

---

## ⚡ Performance

Measured with `scripts/bench.sh` (release build, WSL2, 4 cores) against
plocate, GNU locate and find on a real system with **1,508,113 entries**
and about **518 GB** of files indexed. Every query returned exactly
`find`'s results (100% accuracy on the Linux-scope `find` baseline).

| Operation | rfind | plocate | GNU locate | find |
|-----------|------:|--------:|-----------:|-----:|
| Exact name (`passwd`) | **13 ms** | 26 ms | 52 ms | 3.05 s |
| Under a directory (`/etc`, `*.conf`) | **9 ms** | 8 ms | 49 ms | 9 ms |
| Glob `*.conf` | 162 ms | **21 ms** | 59 ms | 3.24 s |
| Glob `*.dll` (729 results) | 383 ms | 109 ms | **60 ms** | 3.03 s |
| Regex `^lib.*\.so\.[0-9]+$` | 207 ms | **27 ms** | 214 ms | 3.68 s |
| Total size of `/usr` (9.7 GB) | **25 ms** | — | — | 6.05 s |
| 10 largest files under `/usr` | **24 ms** | — | — | 5.44 s |
| Change visible (create / rename / append / delete) | **≤ 5 s** | at next `updatedb` | at next `updatedb` | immediate |
| Index size | 68 MB (1.51M entries) | **9.6 MB** (299k entries) | 5.3 MB (299k entries) | — |
| Bytes/entry | 47 | 33 | **18** | — |
| Daemon RSS | 124 MB | — | — | — |
| Initial index build (Linux scope) | **2.3 s** | 6.8 s | 2.8 s | — |

> Locate databases only cover the Linux-side scope (299k entries); rfind
> also indexes 1.2M entries on the mounted Windows drives (`/mnt/c`,
> `/mnt/d`) as `scan.periodic_roots`. The locate times are therefore
> optimistic — rfind holds its ground while searching 5× more data.

### Index size and compression

Live from `rfind status`:

```
DB size        : 67.9 MB
Compression    : 2.24× (logical 152.4 MB, saved 84.5 MB, 47.5 B/entry)
```

- **zstd-compressed pages** via `rfind-zvfs` cut the on-disk database
  about in half compared to plain SQLite pages.
- **Name interning + directory-trie schema** stores every basename
  once (half of all names repeat on real systems: `index.js`,
  `__init__.py`, …) and the tree as `(parent_dir_id, name_id)`.
  Renaming a directory rewrites one row.
- **~47 B/entry** on real data, trending toward 45 B/entry as the
  index grows (shared prefixes amortise). Set `index.compress = false`
  to switch to plain SQLite pages if you'd rather trade size for
  slightly faster broad globs.
- Compaction runs after every scan, so the ratio snaps back down as
  dead-page overhead builds up and gets reclaimed — watch it live in
  `rfind status`.

### Scale: 10 million files

10,000,001 files on ext4 in 10,201 directories, half named like a real
system's files and half random, indexed by an isolated rfindd:

| | compressed (default) | plain |
|---|---:|---:|
| **Index size** | **~400 MB** | ~900 MB |
| Daemon memory (peak) | ~95 MB | ~90 MB |
| Initial index build | ~420 s | ~300 s |
| Full rescan | ~55 s | ~45 s |
| Daemon restart (journal replay) | **~0.6 s** | **~0.2 s** |
| Exact name | ~6 ms | **~3 ms** |
| Glob `*.json` (133k results) | ~1.2 s | **~0.6 s** |
| First 10 matches of a glob | ~6 ms | **~4 ms** |
| Regex over all names | ~0.8 s | **~0.5 s** |
| Total size of all 10M files | ~0.9 s | **~0.7 s** |
| Change visible (create / rename / delete 50k) | ≤ 5 s | ≤ 5 s |

### io_uring

The scanner batches `statx` through io_uring (`scan.io_uring`, on by
default). On WSL2 drvfs/9p and libvirt virtiofs the batched path is
automatically disabled (page-cache eviction mid-read SIGBUSes the
read; see [`crates/rfind-core/src/mounts.rs`](crates/rfind-core/src/mounts.rs)
`is_mmap_fragile`):

| Tree | plain `statx` | io_uring |
|------|--------------:|---------:|
| `/usr` on ext4 (57k entries) | 33–37k entries/s | **62–66k entries/s** |
| `C:\Windows\System32` over 9p (35k) | 4.0k entries/s | 4.0k entries/s *(forced plain)* |

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
│                                │   (zvfs + TRUNCATE) │  │
│  ┌──────────────┐    insert    │                     │  │
│  │  getdents64  │─────────────▶│  basename index     │  │
│  │  + statx     │              │  mtime index        │  │
│  │  parallel    │              │  path index         │  │
│  │  scanner     │              └─────────────────────┘  │
│  └──────────────┘                                       │
│                                                         │
│    Scanning ──▶ Reconciling ──▶ Ready                  │
└─────────────────────────────────────────────────────────┘
```

### State Machine

```
  ┌─────────────┐     scan      ┌──────────────┐    apply     ┌────────────┐
  │ INITIALIZING│──────────────▶│   SCANNING   │─────────────▶│RECONCILING│
  └─────────────┘               │              │   queued     └─────┬──────┘
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

Downloads a static binary for your architecture (x86_64 or aarch64) — or
builds from source if there's no release for it — and installs `rfind` +
`rfindd`, the man pages and bash/zsh/fish completions, and the `rfindd`
systemd service. On WSL2 a first install configures Windows drives as
periodic roots.

Each release tarball is signed keyless with [sigstore cosign][cosign] via
GitHub OIDC. If `cosign` is installed, `install.sh` verifies the signature
against this repo's `release.yml` identity before unpacking; otherwise it
falls back to the published SHA-256. For the strongest guarantee, install
cosign first:

```bash
# Debian/Ubuntu example — see sigstore.dev for other distros.
sudo apt install -y cosign
```

[cosign]: https://docs.sigstore.dev/cosign/overview/

| Task | One-liner |
|------|-----------|
| Install / update | `curl -fsSL https://raw.githubusercontent.com/YanivSaar/rfind/main/scripts/install.sh \| sudo bash` |
| From a clone | `git clone https://github.com/YanivSaar/rfind.git && cd rfind && sudo ./scripts/install.sh` |
| Specific release | `curl -fsSL …/install.sh \| sudo RFIND_VERSION=v1.10.2 bash` |
| Build from source | `curl -fsSL …/install.sh \| sudo RFIND_FROM_SOURCE=1 bash` |
| Air-gapped (offline) | `sudo ./install.sh --from-tarball rfind-x86_64-linux.tar.gz` ([`docs/airgapped.md`](docs/airgapped.md)) |
| glibc build (vs musl default) | `curl -fsSL …/install.sh \| sudo RFIND_LIBC=gnu bash` |
| Uninstall (keep index) | `sudo rfind uninstall` |
| Uninstall + purge | `sudo rfind uninstall --purge` |
| Watch first scan | `rfind status --watch` |
| Mount classification | `rfind status --mounts` |
| Benchmark vs find/locate | `sudo ./scripts/bench.sh --quick` |

Updating is the same command: it rebuilds, swaps the binaries, restarts the
daemon and never overwrites `/etc/rfindd/config.toml`. If the index format
changed, rfindd rebuilds the index on its own.

### Search immediately after install

```bash
# Everything is one composable command — pattern + where + sort + top:
rfind passwd                                  # exact or glob name
rfind "*.conf" --under /etc                   # glob, restricted to a tree
rfind "*.py" --larger-than 10M --largest --top 20   # top 20 biggest .py
rfind "*.log" --recent --hours 1 -l           # recent .log files, long
rfind "/home/*/.ssh/*" --path                 # full-path glob
rfind "^lib.*\.so\.[0-9]+$" --regex           # regex (basename)
rfind --largest --under /var --top 10         # pure sort, no pattern
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

### Pinned-toolchain / offline build

For air-gapped or policy-restricted hosts that cannot pipe `rustup` into
a shell:

```bash
# On a build host with network:
rustup toolchain install $(cat rust-toolchain.toml | grep channel | cut -d'"' -f2)
cargo vendor > .cargo/config.toml.vendor      # vendor deps
tar czf rfind-vendored.tar.gz . .cargo/config.toml.vendor vendor/

# On the target host (no network):
tar xzf rfind-vendored.tar.gz
mv .cargo/config.toml.vendor .cargo/config.toml
cargo build --release --offline
sudo ./scripts/install.sh
```

The toolchain pin in `rust-toolchain.toml` is the one CI builds with;
match it on offline hosts to reproduce bits for bits. The installer
itself does not fetch anything when invoked against a locally built
`target/release/` tree.

---

## 🔍 Usage

Index searches need root (`sudo`): the index lists every file on the
system. When the index is not reachable — the caller is not root, the
daemon is not running, or the filter asks for information the index does
not store (atime, ctime, symlink type) — `rfind` falls back to a live
parallel walk that needs no privileges. The walk never opens the index,
and the mode is picked automatically; pass `--live` (or `--no-index`) to
force it. See [Walk mode](#walk-mode) below.

Every command has `--help`, and `man rfind` documents all of them.

### Search commands

A single composable form: `rfind [PATTERN] [--regex|--path] [--under DIR]
[--largest|--recent] [--top N] [--since-flags] [--type f|d] [--larger-than
SIZE] [--smaller-than SIZE] [-l]`. Any combination is valid, and flag
order never matters. The legacy subcommands (`rfind name`, `path`,
`recent`, `largest`) still work — they're thin aliases over the same
logic — so existing scripts don't break.

```bash
# By pattern (default: glob on basename)
rfind passwd                         # exact match
rfind "*.log"                        # glob: all .log files
rfind "app.???.js"                   # glob: ? matches one character
rfind "*.py" --under /home           # scoped to a subtree
rfind "*.conf" --type f              # files only
rfind logs --type d                  # directories only
rfind "*.log" --top 10               # stop after 10 (synonym: --limit)
rfind "^app\." --regex               # regex, matched against basename
rfind "/etc/nginx*" --path           # full-path glob (`*` matches `/`)
rfind "^/home/.*/\.ssh/.*" --regex --path   # regex over full path

# By time (any time flag implies --recent order, newest first)
rfind --recent                       # last 24 hours (default window)
rfind --minutes 30                   # last 30 minutes
rfind --from 2026-10-01 --to 2026-10-13   # date range (local time)
rfind "*.log" --hours 2 -l           # combine: time + pattern + long

# By size (combine with --largest for sorted top-N)
rfind "*.iso" --larger-than 1G
rfind "*.py" --larger-than 10M --largest --top 20
rfind --largest --under /var --top 10   # biggest files in a tree

# All of the above compose freely:
rfind "*.log" --under /var --recent --hours 6 --larger-than 100K --top 5
```

### `find`-style predicates (daviddl9 syntax)

The usual CLI accepts the same `+N`/`-N` prefix syntax `find(1)` uses, so
scripts that already target `daviddl9/rfind` run unmodified:

```bash
rfind "*.log" --mtime +7d --size +100M --print0 | xargs -0 ls -lh
rfind "*.py"  --mtime -1h                             # changed in the last hour
rfind "core.*" --size -1k --top 20                    # tiny core files
rfind "*.conf" --under /etc --ctime -1d               # metadata changed today
```

Both syntaxes are accepted everywhere, and overlapping ones (`--larger-than`
with `--size`, `--hours` with `--mtime`, etc.) are AND-ed by intersection;
a contradictory combination (e.g. `--larger-than 10M --size -1M`) is
reported as a clean error rather than silently returning nothing.

### Disk usage — from the index, no disk walk

```bash
rfind size /home                     # total + 20 largest children (like du -d1)
rfind size / --top 5
rfind --largest --under /var --top 10   # biggest individual files
```

Directory sizes are summed from the indexed file sizes at query time;
nothing extra is stored per directory.

### Scripting

```bash
rfind "*.log" --json                 # one JSON object per result (JSON Lines)
# {"path":"/var/log/syslog","type":"file","size":81234,"mtime":1791900000}
rfind --json status                  # single object for status/stats/config
rfind "*.log" --json | jq -r 'select(.size > 1e8) | .path'
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

### Privacy audit

`rfind privacy-report` cross-checks `scan.exclude` against a built-in
catalog of sensitive trees (SSH keys, GPG, cloud creds, password
managers, browser profiles, mail stores, container state, package
caches) and prints what's covered, what's missing, and what's out of
scope. Exits 1 if anything is missing, so it composes with shell
checks (`rfind privacy-report || notify-admin`). `--json` emits the
same data for scripting. Templates in [`packaging/exclusions.d/`](packaging/exclusions.d/)
hold copy-pasteable `scan.exclude` entries for each category.

### Daemon commands

```bash
rfind status                         # unified daemon + index state:
                                     #   phase, freshness (LIVE/FRESH/STALE),
                                     #   entries, monitoring mode, pending
                                     #   events, DB size, and any background
                                     #   periodic/mount scan in flight
rfind status --watch                 # live refresh until Ready
rfind health                         # alias of `status` (kept for scripts)
rfind stats                          # top file types, largest directories
rfind update                         # rescan now (SIGUSR1)
rfind reload                         # re-read the config (SIGHUP)
rfind uninstall [--purge]            # remove everything (--purge: index + config too)
```

### <a name="walk-mode"></a>Walk mode (live, non-root)

`rfind` falls back to a live parallel walk when the index isn't the right
answer:

- Caller isn't root (walk silently).
- Daemon isn't running or the DB file is missing (one-line stderr warning).
- Filter needs data the index doesn't store: `--atime`, `--ctime`,
  `--type l`, `--type any`, or `-L`/`-H` symlink following.
- `--live` (alias `--no-index`) is passed explicitly.

The walk uses the same parallel scanner the daemon uses to build the
index (getdents64 + statx batched through io_uring where available), so
it's competitive with `find(1)` on cold caches and faster on repeated
runs.

```bash
# Non-root user, no daemon required:
rfind "*.log" --live --mtime +7d --size +100M --print0 | xargs -0 ls -lh

# Follow all symlinks (visited-set breaks cycles automatically):
rfind -L "*.conf" --under /etc

# Depth-limited:
rfind --live --under ~/src -m 2 --type d        # project dirs, two deep

# Walk symlink listing (index can't do this):
rfind --live --type l --under ~/.local
```

Make the choice sticky across invocations via `/etc/rfindd/config.toml`:

```toml
[search]
mode = "auto"       # or "index" (never walk) or "walk" (never query DB)
io_uring = true
threads = 0         # 0 = cores × 2
```

### User mode — one daemon per user, no root

`rfindd --user` runs the daemon as the invoking user with no root privileges.
It uses inotify (fanotify filesystem marks need `CAP_SYS_ADMIN` in the init
userns), defaults its root to `$HOME`, and keeps all of its state under the
user's XDG directories, so each user on a shared box can have their own
index that no other user — including root — needs to know about:

| | System mode (default) | User mode (`rfindd --user`) |
|---|---|---|
| Runs as | root | invoking user |
| Monitor | fanotify (one mark per filesystem) | inotify (one watch per directory) |
| Default roots | `/` | `$HOME` |
| Config | `/etc/rfindd/config.toml` | `$XDG_CONFIG_HOME/rfind/config.toml` |
| Index | `/var/lib/rfindd/index.db` | `$XDG_DATA_HOME/rfind/index.db` |
| Pidfile | `/run/rfindd/rfindd.pid` | `$XDG_RUNTIME_DIR/rfind/rfindd.pid` |

```bash
# Start a user daemon in the background:
rfindd --user &

# From then on, `rfind` as that same user finds the per-user index on its
# own — no --live, no sudo — and only falls back to a walk if the index
# does not exist or does not cover the query:
rfind "*.rs" --under ~/src
```

The CLI picks a per-user index automatically when the invoking user owns one;
system-mode root searches are unaffected. A ready-made user unit ships at
[`packaging/systemd/rfindd.user.service`](packaging/systemd/rfindd.user.service):

```bash
mkdir -p ~/.config/systemd/user
install -m 644 packaging/systemd/rfindd.user.service \
    ~/.config/systemd/user/rfindd.service
systemctl --user daemon-reload
systemctl --user enable --now rfindd
loginctl enable-linger "$USER"   # keep it running across logout (optional)
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
# Pseudo filesystems + package stores + container storage. /mnt is
# intentionally NOT excluded (common place to mount drives, including
# WSL2 Windows drives), snapshot roots are matched via `exclude_dir_names`
# below.
exclude = [
    "/proc", "/sys", "/dev", "/run", "/media",
    "/var/lib/flatpak", "/var/lib/snapd", "/snap",
    "/nix/store", "/gnu/store",
    "/var/lib/machines", "/var/lib/libvirt",
    "/var/lib/docker/overlay2",
    "/var/lib/containers/storage/overlay",
]
# Match these basenames anywhere in the tree (**/<name> semantics).
# Catches btrfs/Snapper snapshots, ZFS snapshot auto-mounts, Timeshift
# snapshot roots — all of which otherwise multiply the live tree many
# times over in the index.
exclude_dir_names = [".snapshots", "@snapshots", ".zfs"]
workers = 0                    # 0 = auto-detect CPU count
reindex_interval_hours = 6     # auto re-index every 6h in index-only mode
collect_mtime = true           # modification times and sizes (recommended)
io_uring = true                # batch statx through io_uring when available
max_depth = 4096               # 0 = unbounded

[index]
db_path = "/var/lib/rfindd/index.db"
batch_size = 10000
compress = true                # zstd-compressed pages (~2.2× smaller); changing it rebuilds

[monitor]
prefer_fanotify = true         # one mark per filesystem (kernel 5.9+, root)
inotify_fallback = true        # per-directory watches if fanotify is unavailable
# Off by default: on kernels with unprivileged FUSE, an attacker-created
# mount under a live root would otherwise trigger a root-owned walk of
# attacker-controlled contents.
watch_new_mounts = false

[search]
mode = "auto"                  # auto | index (never walk) | walk (never query DB)
io_uring = true                # io_uring for walk-mode statx batching
threads = 0                    # 0 = cores × 2 for walk mode
# On shared boxes with multiple sudoers: re-stat each result through the
# invoking user's fsuid/fsgid and drop paths they could not have reached
# themselves. Off by default (one extra stat per result). See
# docs/threat-model.md § 6a.
filter_by_dac = false

[log]
level = "info"                 # error | warn | info | debug | trace
                               # RFIND_LOG env takes precedence
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
│   ├── rfind-zvfs/       # SQLite VFS storing pages zstd-compressed
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

**New-mount / replug detection.** `/proc/self/mountinfo` is polled; mounts
that appear under a live root are indexed and marked, and mounts that
disappear have their old paths swept from the index. The mount identity is
the triple `(dev, mount_point, source)` so a USB drive unplugged and a
different drive plugged in at the same mount point is recognised as a
replug (the kernel can reuse `major:minor`, so `(dev, point)` alone would
miss it and the index would keep serving stale paths from the old drive).
`monitor.watch_new_mounts` is off by default on hosts where unprivileged
FUSE can create mounts.

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

Schema v6 stores nothing twice:

```sql
-- Every distinct file or directory name, once (half of all names repeat:
-- index.js, __init__.py, ...). Looked up by an 8-byte hash, not a text index.
CREATE TABLE names (id INTEGER PRIMARY KEY, name TEXT NOT NULL, hash INTEGER NOT NULL);
CREATE INDEX idx_names_hash ON names (hash);

-- The directory tree as (parent, name). Paths are built from this chain,
-- so renaming a directory rewrites one row. The root is id 1.
CREATE TABLE dirs (id INTEGER PRIMARY KEY, parent INTEGER, name_id INTEGER);
CREATE UNIQUE INDEX idx_dirs_parent_name ON dirs (parent, name_id);

-- Every entry, clustered on its key: the table is its own unique index.
CREATE TABLE entries (
    dir_id INTEGER NOT NULL, name_id INTEGER NOT NULL,
    is_dir INTEGER NOT NULL, mtime_sec INTEGER NOT NULL, mtime_nsec INTEGER NOT NULL,
    size INTEGER NOT NULL,          -- bytes; 0 for directories
    scan_gen INTEGER NOT NULL,      -- last scan generation that saw it
    PRIMARY KEY (dir_id, name_id)
) WITHOUT ROWID;
CREATE INDEX idx_entries_name  ON entries (name_id);
CREATE INDEX idx_entries_mtime ON entries (mtime_sec);
CREATE INDEX idx_entries_big   ON entries (size) WHERE size >= 1048576;  -- partial

-- Durable event journal and daemon state
CREATE TABLE pending_events (id INTEGER PRIMARY KEY, event_type TEXT, path TEXT,
    new_path TEXT, is_dir INTEGER, timestamp INTEGER, applied INTEGER);
CREATE TABLE scan_metadata (key TEXT PRIMARY KEY, value TEXT);
```

An index with an older schema version (`PRAGMA user_version`) is dropped and
rebuilt by a full scan.

### Compressed storage (`index.compress`, default on)

`rfind-zvfs` is a SQLite VFS that keeps the database's pages
zstd-compressed — about 2× smaller on real data (2.24× on a typical
Linux-side index; see Performance for the live `rfind status` numbers):

- `index.db` stays an empty stub that carries SQLite's locks; it runs
  in `journal_mode=TRUNCATE` rather than WAL. WAL's `-shm` file is a
  shared mmap, and 9p / virtiofs can evict a page from under the mmap
  mid-read and SIGBUS the daemon — truncate-mode journaling avoids
  the shared mmap entirely while keeping crash safety.
- Pages live in an append-only `index.db-zdata.N` file of checksummed
  records; `index.db-zmap` is a page-map snapshot replaced atomically.
  Readers load the map when a read transaction starts and replay only the
  records appended since.
- A torn record at the end of `-zdata.N` fails its CRC and is ignored:
  the next write re-appends past it, and the rollback journal keeps the
  main database consistent. Tested by SIGKILLing a writer 45 times
  mid-write — `PRAGMA integrity_check` was `ok` every time.
- Old page versions are reclaimed by compaction (a new generation file);
  rfindd compacts after every scan, and readers still on the old file keep
  reading it until they reload.

The tradeoff is CPU: every page a search reads is decompressed, so
scan-heavy searches (broad globs) are slower than uncompressed — see
Performance. `rfind config set index.compress false` switches to plain
SQLite (the index is rebuilt automatically).

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

| Entries | Compressed (default) | Plain (`index.compress = false`) |
|--------:|---------------------:|---------------------------------:|
| 150k | ~7 MB | ~16 MB |
| 1.5M | ~70 MB | ~165 MB |
| 10M | ~400–500 MB | ~0.9–1.1 GB |
| 100M | ~4–5 GB | ~9–11 GB |

Measured: 1.51M entries (WSL2, Linux + mounted Windows drives) → 68 MB
compressed (~47 B/entry), 163 MB plain; 10M synthetic entries with
realistic names → ~400 MB compressed, ~900 MB plain. Scaling is linear
in the number of entries; name interning and the directory trie keep the
per-entry bytes roughly flat as the index grows.

---

## 🔒 Security

- Daemon runs as **root** (required for fanotify/inotify on system directories), but the
  systemd unit confines it: `CapabilityBoundingSet=CAP_SYS_ADMIN CAP_DAC_READ_SEARCH`,
  `NoNewPrivileges`, `ProtectSystem=strict`, `ProtectHome=read-only`, `PrivateTmp/Devices`,
  the `Protect*` kernel/clock/hostname set, `LockPersonality`, `MemoryDenyWriteExecute`,
  `RestrictAddressFamilies=AF_UNIX`, `IPAddressDeny=any`, `SystemCallFilter=@system-service`
  plus `fanotify_*`/`*_handle_at` minus `@mount @reboot @swap @module @obsolete`.
  `systemd-analyze security rfindd`: **2.1 OK** (unconfined would score ~9.5).
- Release tarballs are signed **keyless with sigstore cosign** via GitHub OIDC;
  `install.sh` verifies the signature against this repo's `release.yml` identity when
  `cosign` is installed, falling back to the published SHA-256 otherwise.
- `RFIND_CONFIG` on the **daemon** must be an absolute path under `/etc/rfindd/`; any
  other value is logged and ignored. (The CLI stays permissive.)
- The index is **root-only** (`/var/lib/rfindd` is `0700`, the database `0600`): it lists every
  file name on the system, including other users' files, so `rfind` searches need `sudo`
- Index at `/var/lib/rfindd/index.db` — root-owned
- Config at `/etc/rfindd/config.toml` — `0640` (root-owned; group-readable so a `wheel`-style
  group can audit the exclude list without needing read on the index itself)
- Progress file at `/run/rfindd/progress.json` — root-only; the systemd unit sets
  `RuntimeDirectoryMode=0700`, so the sanitized path list and scan timings are not
  readable by unprivileged users
- Search queries read the index directly — no daemon IPC required
- The zvfs page store verifies the per-page CRC on **every read**, not only at startup
  replay — defence-in-depth against on-disk corruption of `index.db-zdata.<gen>`.
  CRC32 is a **corruption** check, not a cryptographic MAC: an attacker who already has
  write access to `/var/lib/rfindd/` can craft a page that passes it. The index is
  `0700` and root-owned precisely so no non-root process has that access. See
  [`docs/fips.md`](docs/fips.md) and [`docs/threat-model.md`](docs/threat-model.md)
  for the full integrity-vs-authenticity story.
- Symlinks are **not followed** during scan (prevents cycles)
- Excluded paths (default `/proc`, `/sys`, `/dev`, `/run`, `/media`) are never indexed
- `rfind config edit` uses a private `0700` tmp subdir with `O_CREAT|O_EXCL|O_NOFOLLOW`
  so a local user cannot win a `/tmp` symlink race against a `sudo rfind config edit`
- `$EDITOR` is executed as a program path (via `sh -c 'exec "$0" "$@"'`), never as a shell
  fragment — a hostile `EDITOR=…` cannot inject commands under `sudo -E`
- `rfind uninstall --purge` only removes dirs whose basename is `rfindd` or `rfind`; a
  misconfigured `index.db_path = /var/foo.db` can no longer take out `/var`
- The daemon takes `flock(LOCK_EX|LOCK_NB)` on `/run/rfindd/rfindd.pid` at startup, so a
  second instance refuses to run
- When a filesystem is unmounted the fanotify mark and its cached mount fd are
  dropped immediately, so a reused `fsid` cannot resolve against the wrong mount.
  The mount watcher identifies mounts by `(dev, point, source)`, not `(dev, point)`
  alone, so an unplug/replug at the same mount point is recognised even when the
  kernel reuses the `major:minor` of the removed device
- `rfindd --user` runs the daemon as the invoking user with no root and no
  shared state: config, index, pidfile and progress file all live under the
  user's XDG directories (`$XDG_CONFIG_HOME/rfind/`, `$XDG_DATA_HOME/rfind/`,
  `$XDG_RUNTIME_DIR/rfind/`), each mode-0600 under a mode-0700 directory, so
  one user's per-user index is not readable by another
- `search.filter_by_dac` (off by default) re-stats each result through the
  invoking user's `fsuid`/`fsgid` and drops paths they could not have reached
  themselves, for shared boxes with multiple sudoers (see
  [`docs/threat-model.md`](docs/threat-model.md) § 6a)
- **Search results are an index snapshot, not a live filesystem check.** Like
  every indexed finder (`locate`, `plocate`, …), a path that was present when
  the index last reconciled may have been renamed, deleted, or replaced by a
  symlink by the time your pipeline opens it. For destructive automation,
  re-validate each path before acting on it:

  ```bash
  rfind "*.log" --print0 | while IFS= read -r -d '' p; do
      [ -f "$p" ] && [ ! -L "$p" ] && rm -f -- "$p"
  done
  ```

  For higher-assurance scripts, open the parent directory once and use
  `openat()` / `statx()` with `AT_SYMLINK_NOFOLLOW`, or `O_NOFOLLOW` when
  opening the final component — the index is the planner, not the executor.

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

Roughly 170 `#[test]` cases across the workspace. The non-ignored suites
run in a few seconds on a cold `cargo test`:

| Area | What's covered | Needs a running rfindd |
|------|---------------|:---:|
| `rfind-core` | DB schema upgrades, upserts, subtrees, renames, sweeps, sizes, disk usage, mount table, bind-mount aliases, read-only open | no |
| `rfind-config` | TOML parsing, validation, comment-preserving edits, XDG user paths | no |
| `rfind-zvfs` | Compressed VFS pages, CRC, compaction, SIGKILL chaos | no |
| `rfind-cli` | Search filter composition, walk-mode, size parsing, mode selection | no |
| `rfindd` | Scanner, journal replay, mount watcher, pidfile flock, chaos under `/proc` noise | no |
| `tests/integration` | Searches and invariants against the live index | yes |
| `fuzz/` | libFuzzer harnesses: fanotify DFID_NAME decoder, zvfs record parser, zvfs round-trip | optional |

End to end, the daemon is exercised in an isolated mount namespace
(scanning, live events with fanotify and inotify, config reloads, new /
removed mounts, cancellation, SIGHUP reexec), and `scripts/bench.sh`
checks every query against `find` on the real system.

---

## 🛣️ Roadmap

- [x] Parallel scanner with `getdents64` + `statx`, batched through io_uring
- [x] SQLite persistent index (WAL): names stored once, directory tree, clustered entries
- [x] zstd page compression via a custom crash-safe SQLite VFS (−77% on real data)
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
- [x] Cache resolved directory handles (per-fsid mount-fd cache in fanotify)
- [x] Hardened `rfind config edit` (symlink race, empty-file guard, EDITOR injection)
- [x] Whitelisted `rfind uninstall --purge`; daemon pidfile + flock
- [x] Searches pin a read snapshot; live-event GC runs in bulk; stats use the right dot
- [x] Scanner work-queue uses mutex-protected notify (no lost-wakeup hang); worker panics
      are caught; io_uring buffer lifetime is sound on error paths
- [x] WSL2 `/mnt/c` SIGBUS fix: `journal_mode=TRUNCATE` to avoid the WAL shared mmap on 9p
- [x] `rfindd --user`: XDG per-user daemon, inotify-only, CLI auto-picks the user index
- [x] Mount watcher: new-mount pickup and USB replug detection via `(dev, point, source)`
- [x] `rfind status --mounts`: classify every mount (live / periodic / excluded / aliased)
- [x] Expanded default excludes: snapshot roots, package stores, container overlays
- [x] `search.filter_by_dac`: per-user DAC filter for shared sudoers hosts
- [x] Signed releases: sigstore cosign keyless signing + SHA-256 fallback in `install.sh`
- [x] Air-gapped install (`--from-tarball`) and glibc build artefacts alongside musl
- [x] Containers / non-systemd release: OpenRC, runit, s6, dinit units; Containerfile
- [x] Hardening docs: threat model, SELinux + AppArmor profiles, FIPS note
- [x] libFuzzer harnesses: fanotify DFID_NAME decoder, zvfs record parser, zvfs round-trip
- [ ] Extension index for `*.ext` globs
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
