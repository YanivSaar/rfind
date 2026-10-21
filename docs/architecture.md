# rfind architecture

This is a pointer, not the manual. The README has the full picture
(diagrams, state machine, schema, backends). This file documents the
crate boundaries that CLAUDE.md tells agents to check first.

## Crate layout

```
crates/
  rfind-config/   # TOML parsing, comment-preserving edits, validation
  rfind-core/     # Shared types, SQLite schema, all search / write SQL,
                  # mount-table parsing (bind-mount aliases, fanotify
                  # mark points), journal replay helpers.
  rfind-scanner/  # Parallel scanner: getdents64 + statx (+ io_uring),
                  # catch_unwind-isolated worker threads, work-queue
                  # synchronised through its own mutex (no lost wakeups).
  rfind-monitor/  # fanotify (FID mode, filesystem marks) + inotify
                  # (per-directory watches, used on WSL2 / containers)
                  # with a bounded event sink (sync_channel + overflow
                  # flag mirroring FAN_Q_OVERFLOW). Subtree walks on
                  # watch-new-tree / moved-in run on a background
                  # thread so the event drain is never blocked.
  rfind-zvfs/     # SQLite VFS that keeps DB pages zstd-compressed
                  # in an append-only, checksummed data file.
                  # truncate() publishes a snapshot + fsync so a crash
                  # cannot replay-resurrect dropped pages.
  rfindd/         # Daemon: state machine, signal handling, config
                  # reload, mount watcher, periodic roots, pidfile +
                  # flock on /run/rfindd/rfindd.pid.
  rfind-cli/      # `rfind` binary: searches, status/health/stats,
                  # config CLI, uninstall (purge is whitelisted),
                  # json / long / human output.
tests/
  integration/    # Live-index integration tests (gated behind
                  # `--ignored`; need a running rfindd).
```

## Where to make common changes

| Task | Primary files |
|---|---|
| Add a new search mode | `rfind-core/src/db.rs` (new `search_*_streaming`), then wire a subcommand in `rfind-cli/src/main.rs` |
| Add a new config key | `rfind-config/src/lib.rs` (struct + `validate`), `rfind-config/src/edit.rs` (comment-preserving set), `KEYS` table |
| Change the schema | `rfind-core/src/db.rs` (`SCHEMA_VERSION`, DDL in `open_with`); older schemas are dropped and rebuilt by a full scan |
| Change what counts as a reload | `rfindd/src/config_state.rs` (`State::of`, `apply`) |
| Change fanotify/inotify behaviour | `rfind-monitor/src/{fanotify,inotify}.rs` + the shared `sink.rs` |
| Change compressed storage | `rfind-zvfs/src/{store,vfs}.rs` (VFS methods pass through for everything but the main DB file) |
| Change scanner syscalls | `rfind-scanner/src/lib.rs` (`statx_batch`, `worker_loop`) |

## Invariants worth preserving

- **One writer, many readers.** Only the daemon writes to the index.
  CLI processes open the DB read-only. The daemon serialises writes
  with a `Mutex<Connection>`. The DB uses `journal_mode=TRUNCATE`
  (rollback journal, not WAL): on WSL2 the WAL's `-shm` file is always
  mmap'd and faults as SIGBUS under /mnt/c scan memory pressure. Readers
  set `busy_timeout=5000` so a reader that lands during a write batch
  waits for the lock to drop instead of returning `SQLITE_BUSY`; scan
  batches commit well under that window.
- **Reads pin a snapshot.** `db::stream_hits` opens a `DEFERRED` txn so
  the parent-chain walk through `dirs` cannot stitch together two
  snapshots across writes (a rename mid-walk otherwise returns `NoRows`).
- **Scanner work queue.** `WorkQueue::finish_item` decrements and notifies
  under the queue mutex, otherwise a reader can observe `in_flight > 0`
  and then slip into `condvar.wait` after the final notify fires.
- **Scan generations are monotonic.** Each scan reserves `scan_gen+1`,
  upserts rows tagged with it, then sweeps `scan_gen < current`.
  Rebuilding is lossy without this.
- **Live events do GC in bulk, not per event.** `delete_entry_by_path`,
  `delete_entries_under`, `rename_entry` increment a counter and only run
  `collect_garbage` once it crosses the threshold.
- **Index is root-only.** `/var/lib/rfindd` is 0700, `index.db` is 0600,
  the daemon's umask is 077. The config at `/etc/rfindd/config.toml` is
  0640. Searches need root.
- **Pidfile + flock.** The daemon holds an exclusive flock on
  `/run/rfindd/rfindd.pid` for its lifetime; a second instance refuses
  to start.

## Testing

- `cargo test --all --lib` — unit tests, no daemon needed.
- `cargo test -- --ignored` — integration suite; needs a running rfindd
  with a completed scan.
- `scripts/bench.sh --quick` — compare against `find` on this machine.
