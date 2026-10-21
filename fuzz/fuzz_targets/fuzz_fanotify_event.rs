#![no_main]
//! Feed arbitrary bytes to the fanotify DFID_NAME record decoder in
//! rfind-monitor. The parser does unaligned reads at hard-coded offsets
//! and bounds-checks against the `handle_bytes` field; any panic here
//! would mean an attacker-controllable fanotify event could crash the
//! daemon. The fuzz-only entry point uses an empty marks list, so the
//! `find(fsid)` lookup short-circuits before any syscall.

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = rfind_monitor::fanotify::__fuzz_resolve(data);
});
