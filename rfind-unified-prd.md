# PRD: Unified rfind — Best of YanivSaar/rfind + daviddl9/rfind

**Author:** Stas (for Yaniv Saar)  
**Date:** 2026-10-20  
**Audience:** Yaniv + Claude Code  
**Status:** Ready for implementation planning  
**Repos analyzed:**  
- https://github.com/YanivSaar/rfind (indexer + daemon)  
- https://github.com/daviddl9/rfind (parallel recursive finder)

This document is written so an LLM coding agent can implement without inventing architecture, file locations, or semantics. Every claim is grounded in the actual source of one or both repositories.

---

## 1. Vision

Deliver a single `rfind` binary that:

1. **Answers “where is X?” in milliseconds** on multi-million-file systems by querying a live SQLite index maintained by a privileged daemon (`rfindd`) that uses fanotify (preferred) or inotify.
2. **Falls back gracefully** to a high-performance parallel tree walk (in the style of daviddl9/rfind) when:
   - The daemon is not running,
   - The user is not root,
   - The query is outside indexed roots, or
   - The user explicitly requests a live walk (`--live` / `--no-index`).
3. Preserves YanivSaar’s strengths (live index, size/mtime, disk-usage from index, systemd unit, compressed VFS, root-only permissions) while absorbing the best CLI ergonomics and filter expressiveness from daviddl9 (time filters with `+1d`/`-2m` syntax, size filters with `+1M`/`-500k`, symlink modes `-P/-H/-L`, `--print0`, type filters including symlink, max-depth, thread control).

**Non-goals (v1 of this unification):**
- Content search / full-text indexing (like Recoll/Baloo).
- Windows native (WSL2 Windows drives remain periodic roots as today).
- Replacing the root requirement for the daemon or for queries against the full-system index.

---

## 2. Current State (Grounded)

### YanivSaar/rfind (the base)
- Multi-crate workspace: `rfindd`, `rfind-cli`, `rfind-core`, `rfind-scanner`, `rfind-monitor`, `rfind-zvfs`, `rfind-config`.
- Daemon builds and maintains a persistent SQLite index (name-interned, directory trie, zstd-compressed pages via custom VFS).
- Live updates via fanotify (FID mode, filesystem marks, kernel ≥5.9) with inotify fallback.
- CLI queries the index directly (read-only, root-required). Supports glob, regex, `--under`, `--largest`, `--recent`, size filters, JSON output, `rfind size` (du-like from index).
- Strong correctness story: WAL snapshots, pidfile flock, scan generations, crash-tested custom VFS.
- Install: `curl | sudo bash` + systemd unit with `UMask=0077` / `StateDirectoryMode=0700`.
- Index is intentionally root-only.

### daviddl9/rfind
- Single binary, `src/main.rs` + `src/filters/`.
- Parallel recursive walk using channels + worker threads (rayon/crossbeam style).
- Pattern: glob if `*`/`?` present, else case-insensitive substring (Boyer-Moore-Horspool via memchr).
- Filters: `--mtime/--atime/--ctime` with `[+-]N[smhd]`, `--size` with `[+-]N[ckMG]`, `-t f|d|l|any`, `-P/-H/-L` symlink modes, `--print0`, `-m` max-depth, `-j` threads.
- No index, no daemon, no root required. Starts at `/` by default.
- Visited-set for symlink loop detection.

### Key gaps to close
| Capability | YanivSaar | daviddl9 | Target |
|------------|-----------|----------|--------|
| Millisecond queries on 10M files | Yes | No | Keep |
| Live updates | Yes | No | Keep |
| Parallel walk fallback | No | Yes | Add |
| `+1d` / `-2m` time syntax | Partial (`--hours`) | Yes | Adopt |
| `+1M` / `-500k` size syntax | Partial | Yes | Adopt |
| `-P/-H/-L` symlink modes | Limited | Yes | Adopt (walk mode) |
| `--print0` | No | Yes | Adopt |
| Max-depth + thread control | Daemon config only | CLI | Adopt for walk mode |
| Non-root usability | No | Yes | Yes (walk mode) |

---

## 3. Target Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                        rfind (CLI)                          │
│  ┌──────────────┐    ┌──────────────┐    ┌──────────────┐  │
│  │ Index path   │    │ Walk path    │    │ Hybrid       │  │
│  │ (root, ms)   │    │ (any user)   │    │ (prefer idx) │  │
│  └──────┬───────┘    └──────┬───────┘    └──────┬───────┘  │
└─────────┼───────────────────┼───────────────────┼──────────┘
          │                   │                   │
          ▼                   ▼                   │
┌─────────────────┐  ┌─────────────────┐          │
│   rfindd        │  │ Parallel walker │          │
│ (fanotify/inotify│  │ (getdents64 +   │◄─────────┘
│  + SQLite WAL)  │  │  statx / io_uring│
└─────────────────┘  │  or rayon-style) │
                     └─────────────────┘
```

**Decision logic (CLI):**
1. If `--live` / `--no-index` → always walk.
2. Else if not root or index DB missing/unreadable or daemon not ready → walk (with warning).
3. Else → query index.
4. Optional `--hybrid`: use index for name/path/size/mtime filters that the index supports; fall back to walk for filters the index cannot answer (e.g. atime/ctime if not stored, or very recent events still in the journal).

**Reuse plan (path of least resistance):**
- Keep the entire YanivSaar workspace as the base.
- Extract or re-implement the parallel walker as a new crate `rfind-walker` (or extend `rfind-scanner` with a “search mode” that emits matches instead of index rows). Prefer extending `rfind-scanner` because it already has high-quality `getdents64` + `statx` + io_uring + panic isolation.
- Add a thin filter layer (time/size parsers) shared between index queries and the walker.
- CLI gains a mode selector and the daviddl9-style flags.

---

## 4. Detailed Design Specs

### 4.1 CLI Surface (target)

```text
rfind [PATTERN] [OPTIONS]

Pattern modes (mutually exclusive where noted):
  (default)          glob on basename if * or ? present, else case-insensitive substring
  --path             match against full path
  --regex            regex (basename by default; --path for full path)

Index-preferring options (work on both modes when possible):
  --under DIR
  --type f|d|l|any
  --larger-than SIZE / --smaller-than SIZE   (existing)
  --size [+-]N[ckMG]                         (new, alias for above)
  --recent / --hours N / --since-flags       (existing)
  --mtime [+-]N[smhd]                        (new)
  --atime [+-]N[smhd]                        (new, walk-only or future index)
  --ctime [+-]N[smhd]                        (new, walk-only or future index)
  --largest --top N
  --print0
  --json
  -l / --long

Walk-mode options:
  --live / --no-index
  -m, --max-depth N
  -j, --threads N
  -P / -H / -L                               (symlink mode)

Daemon / config (unchanged):
  rfind status, config, reload, uninstall, size, ...
```

**Compatibility:**
- Existing YanivSaar scripts (`rfind name ...`, `rfind "*.conf" --under /etc`) continue to work.
- daviddl9-style invocations (`rfind "*.log" --mtime +7d --size +100M --print0`) work in walk mode and, where the index supports the filter, in index mode.

### 4.2 Filter Parsing (shared)

New module (suggested location: `crates/rfind-core/src/filters.rs` or new `rfind-filters` crate):

```rust
pub enum TimeFilter { OlderThan(Duration), NewerThan(Duration), Exactly(Duration) }
pub fn parse_time(s: &str) -> Result<TimeFilter, String>;  // "+1d", "-2m", "3d", "+45s", etc.

pub enum SizeFilter { Larger(u64), Smaller(u64), Exact(u64) }
pub fn parse_size(s: &str) -> Result<SizeFilter, String>;  // "+1M", "-500k", "1G", "1024c"
```

Map these onto existing index query parameters where possible (`mtime_sec`, `size`). For atime/ctime, document as walk-only in v1 (index does not currently store them).

### 4.3 Parallel Walker

**Preferred implementation path:** Extend `rfind-scanner` rather than rewrite from scratch.

- Reuse existing `getdents64` + `statx` (+ io_uring) path.
- Add a “match mode” that applies the shared filter predicates and emits paths (or structured hits) instead of (or in addition to) index upserts.
- Preserve `AT_SYMLINK_NOFOLLOW` by default; implement `-L` by following (with visited set to prevent loops, as daviddl9 does).
- Support max-depth and thread count via the existing worker pool.
- Output: streaming, supports `--print0` and JSON.

**Alternative (higher cost):** Port daviddl9’s channel-based walker. Not recommended — the scanner already solves the hard parts (syscall efficiency, panic isolation, work-queue correctness).

### 4.4 Index Mode Enhancements

- Map `--mtime` / `--size` onto existing SQL (already has mtime and size indexes).
- Add `--type l` support if the index distinguishes symlinks (check schema; currently `is_dir`). If not stored, either:
  - Add a `is_symlink` column (schema bump + rebuild), or
  - Document that symlink filtering is walk-only.
- Keep all existing index queries (`largest`, `recent`, `size`, regex, under) unchanged.

### 4.5 Daemon / Security (no regressions)

- Do **not** weaken the root-only index model.
- Keep `UMask=0077`, `StateDirectoryMode=0700`, daemon umask, and `restrict_index_permissions`.
- Walk mode must never open the index DB when the caller is non-root.
- Optional hardening (from prior security review): add systemd sandbox directives (`ProtectSystem`, `PrivateTmp`, `NoNewPrivileges`, capability bounding). Not required for the feature but recommended in the same release if low effort.

### 4.6 Schema / Storage

No mandatory schema change for v1. Optional later:
- Store `atime` / `ctime` if desired (adds columns, rebuild).
- Store raw bytes for non-UTF-8 names (currently lossy via `to_string_lossy`).

---

## 5. Path of Least Resistance & Effort

**Recommended order (lowest risk, highest value):**

1. **Shared filter parsers** (1–2 days)  
   Add `parse_time` / `parse_size` in `rfind-core`. Unit tests. Map onto existing index queries.  
   Files: new `crates/rfind-core/src/filters.rs`, wire into `rfind-cli` and `rfind-core/src/db.rs` query builders.

2. **CLI flag plumbing + mode selector** (2–3 days)  
   Add `--live`, `--print0`, `-P/-H/-L`, `-m`, `-j`, `--mtime/--atime/--ctime/--size`.  
   Decision logic in `crates/rfind-cli/src/main.rs`.  
   Index path uses existing search functions; walk path is a stub that prints “not yet”.

3. **Walker integration** (4–7 days)  
   Extend `rfind-scanner` with a search entry point that applies filters and streams matches.  
   Implement symlink modes + visited set.  
   Wire to CLI. Add integration tests (temp dir, known files).

4. **Polish & docs** (1–2 days)  
   Man page updates, README examples covering both modes, `rfind --help` consistency, JSON schema stability.

5. **Optional hardening** (1 day)  
   Systemd unit improvements.

**Total estimated effort:** 9–15 engineer-days for a solid v1.  
**Risk hotspots:**  
- Symlink loop detection correctness.  
- Ensuring non-root users never touch the index.  
- Performance regression on the pure walk path (must stay competitive with daviddl9 and classic `find`).

**What not to do:**
- Rewrite the daemon or VFS.
- Force a schema migration for v1.
- Make the daemon drop privileges (it needs them for fanotify).

---

## 6. Acceptance Criteria

- [ ] `sudo rfind passwd` still returns in <20 ms on a 1M+ entry index.
- [ ] `rfind "*.log" --mtime +7d --size +100M --print0` works without root (walk mode) and produces null-delimited output.
- [ ] `sudo rfind "*.conf" --under /etc --mtime -1d` uses the index when the daemon is ready.
- [ ] Non-root user cannot read `/var/lib/rfindd/index.db` (permissions unchanged).
- [ ] Existing scripts using `rfind name`, `rfind size`, `rfind config` continue to work.
- [ ] Walk mode respects `-P/-H/-L` and does not infinite-loop on cyclic symlinks.
- [ ] `--json` and `-l` work in both modes with stable field names.
- [ ] Man page and `--help` document the mode selection rules clearly.

---

## 7. Performance Targets

| Mode | Target | Notes |
|------|--------|-------|
| Index query (exact name) | <10 ms | Existing |
| Index query (glob + under) | <50 ms typical | Existing |
| Walk mode (1M files, SSD) | ≤2× classic `find`, ideally competitive with daviddl9 | Reuse scanner’s getdents64/io_uring |
| Daemon memory | <150 MB steady | Existing |
| Index size | ~45–70 bytes/entry compressed | Existing |

---

## 8. Risks & Mitigations

| Risk | Mitigation |
|------|------------|
| TOCTOU in fanotify resolve (existing) | Document; optional stronger consistency check later. Do not regress. |
| Non-UTF-8 name lossiness | Document; optional raw-bytes schema later. |
| Supply-chain of install script | Keep checksum; document source-build path. |
| Walk mode accidentally opens index as non-root | Explicit `require_root` only on index path; unit test. |
| Symlink loops | Visited set (device+inode or canonical path). |
| Claude Code inventing new crates | This PRD specifies exact crates and “extend scanner” preference. |

---

## 9. Implementation Notes for Claude Code

**Start here:**
1. Read `crates/rfind-scanner/src/lib.rs` (work queue, getdents64, statx).
2. Read `crates/rfind-cli/src/main.rs` (argument parsing, `open_db`, search dispatch).
3. Read `crates/rfind-core/src/db.rs` (search functions, schema).
4. Look at daviddl9’s `src/main.rs` and `src/filters/` only for the filter syntax and symlink-mode semantics — do not copy its walker.

**Do not:**
- Create a parallel walker from scratch if the scanner can be extended.
- Change index permissions or drop the root requirement for the daemon.
- Break the existing composable CLI (`rfind PATTERN --under --largest --top`).

**Suggested first PR:**
- Add filter parsers + map `--mtime`/`--size` onto existing index queries.
- Add `--live` flag that currently just errors with “walker not yet implemented”.
- This unblocks the CLI surface while the walker is built.

---

## 10. Open Questions for Yaniv

1. Should atime/ctime be added to the index in a later phase, or stay walk-only forever?
2. Preferred default when both index and walk could answer: always prefer index, or allow a config toggle?
3. Do you want the binary name to stay `rfind` (and possibly conflict with other tools) or introduce `rfind` + `rfind-walk`?
4. Any hard requirement that walk mode must match daviddl9’s exact output format for scripting compatibility?

---

## Appendix: Key File References

**YanivSaar/rfind**
- Daemon entry: `crates/rfindd/src/main.rs`
- Fanotify resolve: `crates/rfind-monitor/src/fanotify.rs` (`resolve`, ~L323)
- Scanner: `crates/rfind-scanner/src/lib.rs`
- Schema & searches: `crates/rfind-core/src/db.rs`
- CLI: `crates/rfind-cli/src/main.rs`
- Systemd: `packaging/systemd/rfindd.service`
- Architecture notes: `docs/architecture.md`

**daviddl9/rfind**
- Main: `src/main.rs` (Args, PatternMatcher, symlink modes, work distribution)
- Filters: `src/filters/{time,filesize,filetype}.rs`

This PRD is complete enough for an implementation agent to begin without inventing core design decisions.
