# rfind threat model

rfindd is a privileged root daemon that indexes the entire filesystem.
This document names the trust boundaries, what each side is trusted to do,
and what rfind does *not* defend against. It is deliberately short; the
code is the source of truth.

## Trust boundaries

### 1. Index database (`/var/lib/rfindd/index.db` + zdata files)

- **Writer:** rfindd only. The daemon holds `flock(LOCK_EX)` on the
  pidfile for its lifetime; a second daemon refuses to start.
- **Readers:** `rfind` CLI processes, opened `SQLITE_OPEN_READ_ONLY`.
  Searches require root (`/var/lib/rfindd` is 0700, `index.db` is 0600).
- **Trusted to be well-formed?** No. The custom zvfs decoder treats the
  on-disk bytes as untrusted: CRC-per-read, bounded record parsing, and
  on abnormal boot a `PRAGMA integrity_check` runs before serving reads
  (`rfindd/src/main.rs`). On failure the index is unlinked and rebuilt.
- **Not defended:** an attacker with root can corrupt the database;
  rfind reports and rebuilds, it does not detect tampering by a peer
  root process.

### 2. Config (`/etc/rfindd/config.toml`)

- **Writers:** root only (file is 0640, directory 0755). The CLI
  `rfind config edit` opens the file through a 0700 private tempdir
  with `O_CREAT|O_EXCL|O_NOFOLLOW` to prevent `/tmp` symlink races
  against `sudo`.
- **Trusted to be well-formed?** Validated on load. The daemon
  does *not* exec a new config it would reject (see `load_effective_config`).
- **Not defended:** a root-controlled config can direct rfindd to index
  anywhere root can read, including sensitive trees. Exclusions are the
  mechanism; see `docs/privacy.md`.

### 3. Release artifacts (installer path)

- **Trusted root:** GitHub release signed keyless with sigstore cosign
  via this repo's `release.yml` OIDC identity.
- **Verification:** `install.sh` requires cosign by default; the
  SHA-256 is a fallback for cosign-unavailable systems and is not a
  provenance claim. See `README.md` for the pinned-toolchain / offline
  build path for air-gapped installs.
- **Not defended:** a maintainer-account compromise. Rely on the
  normal GitHub security surface (2FA, protected branches, release
  attestations).

### 4. Fanotify / inotify events

- **Source:** the kernel. Treated as untrusted *data*: path components
  are resolved via `open_by_handle_at` under the mount FD recorded at
  mark time, never by stitching strings. FID-mode marks are mount-scoped
  so events from a different filesystem cannot be mis-attributed.
- **Trusted to be timely?** No. Overflow (`FAN_Q_OVERFLOW`) and
  monitor-thread death both trigger a debounced full re-index.
- **Not defended against:** adversarial filesystems designed to defeat
  statx/getdents semantics (hostile FUSE); such filesystems are
  excluded by the mount allowlist.

### 5. Mount table (`/proc/self/mountinfo`)

- **Trusted to list:** every mount currently visible to the daemon.
- **Allowlist:** only mounts on filesystems in the configured allowlist
  are marked for fanotify; see `rfind-core/src/mounts.rs` and
  `rfind-monitor/src/fanotify.rs`.
- **Not defended against:** a root-set up bind-mount designed to make
  rfindd index a path twice; the sweep is per-mount and the duplicate
  is a quality-of-service issue, not a correctness one.

### 6. Local non-root users

- **Index exposure:** none by design. Index and runtime dirs are 0700,
  files 0600. Progress file lives in a 0700 RuntimeDirectory.
- **Search:** requires root.
- **Not defended against:** side channels (disk-usage, timing) that
  reveal rough index size; these are considered acceptable.

## What is explicitly *not* in scope

- Preventing root from seeing sensitive filenames: rfind indexes
  everything root can `getdents`; privacy templates (`docs/privacy.md`)
  are the mitigation.
- Multi-tenant hosts where "root" is not a single trust principal.
- Rowhammer, firmware-level, or hypervisor-level attackers.
- Supply-chain compromise of upstream crates: `cargo audit` +
  `cargo deny` + Dependabot are the first line, not the only line.

## Signal-handling startup window

Between `execve` (SIGHUP reload re-execs the daemon in place) and the
`signal_hook::Signals::new()` registration in `main()`, the default
disposition for SIGHUP/SIGUSR1 is "terminate the process". A client
that bursts SIGHUPs during startup — a reload loop under `systemctl`,
a stress test, or hostile churn — could otherwise kill the new daemon
mid-pidfile-acquire. `main()` blocks both signals via `pthread_sigmask`
as its very first act; the signal_hook handler drains them normally
once registered. SIGTERM/SIGINT are intentionally left unblocked so
an admin stop still works during the startup window. Discovered by
the `sighup_flood_does_not_crash_the_daemon` chaos test.

## Test-only env overrides (debug builds only)

The chaos-test harness (`crates/rfindd/tests/chaos_proc.rs`) spawns the
real daemon binary against a tempdir using three environment variables:
`RFINDD_PIDFILE_PATH`, `RFINDD_PROGRESS_PATH`, and a loosened
`RFIND_CONFIG` that accepts any absolute non-traversal path. All three
are gated by `cfg(debug_assertions)` and compile out of `cargo build
--release`, so the production binary ignores them. The restriction on
`RFIND_CONFIG` (must be under `/etc/rfindd/`) and the hard-coded
pidfile / progress paths are intact in release builds. A reviewer who
greps for the env var names should know they are not an exposed
attack surface in shipped binaries.

## Change control

A change that moves a boundary (new file written outside
`/var/lib/rfindd`, new network dependency, new privileged syscall,
new input trusted to be well-formed) updates this document in the same
commit as the code.
