# rfind-fuzz

cargo-fuzz targets for the two highest-value attack surfaces:

- `fuzz_zvfs_record` — feeds arbitrary bytes to the zvfs record / page-map
  decoder (`crates/rfind-zvfs/src/store.rs`). Catches panics in map
  parsing, record framing, CRC paths, truncated input, duplicate
  generations.
- `fuzz_zvfs_roundtrip` — writes random pages, truncates at random
  offsets, re-opens the store, asserts no panic and that
  `SQLite PRAGMA integrity_check` returns `ok`.
- `fuzz_fanotify_event` — feeds arbitrary bytes to the fanotify event
  decoder (`crates/rfind-monitor/src/fanotify.rs`): metadata header,
  info records, and handle bytes.

## Requirements

Nightly toolchain (libfuzzer-sys needs sanitizer flags):

```bash
rustup toolchain install nightly
cargo install cargo-fuzz --locked
```

## Running

```bash
cd fuzz
cargo +nightly fuzz run fuzz_zvfs_record     corpus/fuzz_zvfs_record     seeds/fuzz_zvfs_record     -- -max_total_time=60
cargo +nightly fuzz run fuzz_zvfs_roundtrip  corpus/fuzz_zvfs_roundtrip  seeds/fuzz_zvfs_roundtrip  -- -max_total_time=60
cargo +nightly fuzz run fuzz_fanotify_event  corpus/fuzz_fanotify_event  seeds/fuzz_fanotify_event  -- -max_total_time=60
```

`seeds/<target>/` holds the committed starting inputs: hand-written
blobs that reach past the trivial length/magic branches so the fuzzer
spends its budget on deeper coverage instead of rediscovering them.
`corpus/<target>/` is the runtime corpus libFuzzer maintains; it is
gitignored but passed explicitly so repeated local runs accumulate
coverage across invocations.

Any crash lands in `fuzz/artifacts/<target>/`. Treat crashes as release
blockers for the targeted module. Add the reproducer to `seeds/<target>/`
once fixed so future runs regression-test it.

## Not part of the workspace

`fuzz/Cargo.toml` declares its own `[workspace]`, so a plain
`cargo build` in the parent repo on stable does not try to compile
libfuzzer-sys. The nightly CI job builds it explicitly.
