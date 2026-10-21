High-value target. Always-on root daemon that indexes the entire filesystem via fanotify/inotify and serves searches from a SQLite index. Designed carefully in places, but the privilege model is inherently dangerous.

### Critical / High

*1. Persistent privileged daemon (single point of failure)*  
rfindd runs as root indefinitely. fanotify filesystem marks + open_by_handle_at require CAP_SYS_ADMIN + CAP_DAC_READ_SEARCH. No privilege drop after init. Any memory-corruption, logic, or parser bug in the event path, scanner, or custom SQLite VFS is instant root.

*2. Supply-chain via curl | sudo bash*  
Primary install path downloads a static binary (checksum verified) and installs a root systemd unit. Compromise of the GitHub release assets, raw install script, or a MITM that bypasses the checksum check = persistent rootkit. Classic high-ROI vector.

*3. Complete filesystem oracle*  
Index holds every path, size, and mtime (except exclusions). Correctly locked down (UMask=0077, StateDirectoryMode=0700, explicit 0700/0600 on DB + WAL/SHM, daemon umask 077). Any bug that lets a non-root process read /var/lib/rfindd is catastrophic information disclosure (SSH keys, secrets, interesting files without triggering FS audit).

*4. fanotify path resolution surface*  
FID-mode events carry parent handle + name, resolved via open_by_handle_at against a cached mount fd. Classic TOCTOU window between event and resolution. ESTALE handling and mount-fd caching exist but are complex; races around umount, bind mounts, and rapid renames are likely. Overflow forces full re-index (DoS primitive).

*5. Custom compressed SQLite VFS (rfind-zvfs)*  
zstd + CRC + append-only design. Extra attack surface. Currently hard to reach (root-only files), but a write primitive on the DB files or a bug in page handling becomes memory corruption in the root process.

### Medium

- Event flood / queue overflow → forced expensive full rescan. Easy local DoS.
- RFIND_CONFIG env override. Influencing the daemon’s environment (service misconfig, local attack) controls what gets indexed/monitored.
- Systemd unit is minimal: no ProtectSystem=, PrivateTmp=, NoNewPrivileges=, capability bounding set after setup, or SystemCallFilter.
- Inotify fallback (WSL2/containers) has watch-limit and different race characteristics.
- Parallel scanner + catch_unwind workers. Panics are contained, resource exhaustion is not.

### Positive controls observed

- Index permissions are rigorously enforced and documented as an invariant.
- Pidfile + exclusive flock prevents dual writers.
- Searches explicitly require root.
- WAL + read snapshots to avoid path stitching across concurrent writes.
- Scanner claims panic isolation; crash safety tested with SIGKILL mid-write.

### Adversary priorities

1. Supply chain (install script / release binaries).  
2. Daemon bugs in fanotify event parsing, handle resolution, or the custom VFS.  
3. Any path that leaks the index to non-root.  
4. Event-flood DoS or forced rescans that burn CPU/IO.

*Bottom line:* Treat as high-risk root software. Prefer building from source with pinned checksums. The design is more careful than most “index everything” tools, but a root daemon that watches the entire filesystem with kernel hooks will always be a prime target