# rfind and FIPS-140

## TL;DR

rfind does no cryptography on the hot path. There is no key material,
no authentication, no digital signature inside the daemon or the CLI
— just filesystem indexing and query. Nothing in rfind therefore
needs a FIPS-validated crypto module.

The one hash we use (CRC32) is for *corruption detection* in the zvfs
page store, not authentication. CRC32 is not a crypto primitive and
is not subject to FIPS validation; using it as a corruption check is
exactly what FIPS 140-3 Implementation Guidance D.K ("Non-security
functions") covers.

If your organization requires that every C library a system daemon
links to be FIPS-validated anyway, use the `system-sqlite` build
feature described below to link your distro's crypto-wrapped SQLite
instead of the bundled copy.

## What rfind links to and why

| Dep | Why | FIPS-relevant? |
|---|---|---|
| `rusqlite` → `libsqlite3` | the index store | yes if your distro ships a FIPS-wrapped build (RHEL does) |
| `zstd` | compressed page storage (zvfs) + an optional compressed index mode | no — compression, not crypto |
| `crc32fast` | zvfs page-corruption check | no — see above |
| `libc` + Linux kernel syscalls (`fanotify`, `inotify`, `statx`, `openat2`, `io_uring`) | monitoring and scanning | no |
| `tracing` + `tracing-subscriber` | log output | no |
| `signal-hook` | signal handling | no |
| `clap` | CLI argument parsing | no |

Nothing above produces, consumes, or stores a secret.

## Linking against a system SQLite

By default rfind builds with `bundled` SQLite — rusqlite vendors a
copy of SQLite's C source and compiles it in. This is portable, needs
no system package, and is required for the musl static release
artifacts. It is *not* a FIPS-validated SQLite.

For environments that need to link the distro's SQLite (RHEL-FIPS
provides one, Ubuntu Pro's `pro enable fips-updates` provides one),
patch each crate's `Cargo.toml` to replace

```toml
rusqlite = { version = "0.40", features = ["bundled"] }
```

with

```toml
rusqlite = { version = "0.40", features = ["modern_sqlite"] }
```

across the five DB-using crates (`rfind-core`, `rfind-zvfs`,
`rfind-monitor`, `rfind-cli`, `rfindd`), install the matching
`-dev` package (`libsqlite3-dev` / `sqlite-devel` / `sqlite-dev`),
and build from source. rusqlite then dynamically links against the
system `libsqlite3`.

A clean `--features system-sqlite` build-time toggle is on the v1.9
roadmap. Until then, the patch above is the recipe and the release
pipeline continues to ship `bundled` static artifacts.

## What FIPS does *not* protect you from

- A system-wide filesystem index is a privacy surface regardless of
  cryptographic posture. See [`threat-model.md`](threat-model.md).
- FIPS does not validate the kernel's `fanotify` or `open_by_handle_at`
  implementations; those are the main non-trivial code paths in the
  monitoring plane.
- CRC32 is not collision-resistant. We use it to detect accidental
  corruption of a zvfs page, not to authenticate it against a
  tamperer. An attacker who can write to `/var/lib/rfindd/index.db`
  can trivially craft a corrupting block that passes CRC32; the
  defense against that is DAC + MAC on the state dir (0700 + the
  SELinux / AppArmor profiles).

## Air-gapped FIPS install

See [`airgapped.md`](airgapped.md). The combined flow is:

1. On a connected build box with your FIPS-validated SQLite installed,
   build with `--no-default-features --features system-sqlite`.
2. Package `target/release/rfind{,d}` + `packaging/` into a tarball.
3. Transfer to the air-gapped host.
4. `install.sh --from-tarball` — the installer does not rebuild, so
   the system-sqlite linking choice carries through unchanged.

## Known gaps

- There is no `cargo-cyclonedx`-produced SBOM yet; CI will emit one
  in v1.9 so FIPS-conscious auditors can verify the dep list above
  mechanically.
- The release pipeline builds with `bundled`. A FIPS-only release
  track (built with `system-sqlite` on a RHEL FIPS builder) is not
  on the roadmap; build it yourself from source for now.
