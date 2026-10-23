#![no_main]
//! Feed arbitrary bytes to the fanotify parsers in rfind-monitor:
//!
//! 1. `__fuzz_resolve` — the DFID_NAME record decoder (unaligned reads
//!    at hard-coded offsets, bounds-checked against `handle_bytes`).
//! 2. `__fuzz_parse_buf` — the outer `event_loop` buffer walk: per-event
//!    metadata parse, version check, event_len lower-bound (added in
//!    v1.12), inner info-record walk.
//!
//! Any panic or non-termination here would mean an attacker-controllable
//! kernel-boundary buffer could crash or hang the daemon. The fuzz-only
//! entry points perform no syscalls and no sink dispatch, so a corpus
//! finding is strictly a parser bug.

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = rfind_monitor::fanotify::__fuzz_resolve(data);
    let _ = rfind_monitor::fanotify::__fuzz_parse_buf(data);
});
