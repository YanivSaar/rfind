#![no_main]
//! Feed arbitrary bytes to the zvfs store opener. The opener parses the
//! snapshot file (page-size + map) and then re-walks every generation's
//! records to validate framing + CRC. Any panic in that path is a bug.
//!
//! Input layout: first 2 bytes = snap length (little-endian, capped), then
//! snap bytes, then data bytes. We refuse 0-byte inputs so libfuzzer has
//! something to mutate.

use libfuzzer_sys::fuzz_target;
use std::io::Write;

fuzz_target!(|data: &[u8]| {
    if data.len() < 2 {
        return;
    }
    let snap_len = u16::from_le_bytes([data[0], data[1]]) as usize;
    let snap_len = snap_len.min(data.len() - 2);
    let (snap, zdata) = data[2..].split_at(snap_len);

    let dir = match tempfile::tempdir() {
        Ok(d) => d,
        Err(_) => return,
    };
    let base = dir.path().join("index.db");

    // Snapshot file lives at <base>-zdata.snap; data files at <base>-zdata.<gen>.
    // Write gen 0 unconditionally so Store::open has something to walk.
    if let Ok(mut f) = std::fs::File::create(format!("{}-zdata.snap", base.display())) {
        let _ = f.write_all(snap);
    }
    if let Ok(mut f) = std::fs::File::create(format!("{}-zdata.0", base.display())) {
        let _ = f.write_all(zdata);
    }

    // Opening the store exercises every decoder: snap header parse, page-map
    // parse, record framing walk, CRC checks, truncation detection.
    let _ = rfind_zvfs::store::Store::open(&base, true);
});
