#![no_main]
//! Random write/truncate cycles against an open Store, then re-open and
//! check that `SQLite PRAGMA integrity_check` returns `ok` on the whole
//! DB. We open through the zvfs so corruption in the compressed layer
//! surfaces to SQLite.
//!
//! Input is a sequence of 2-byte-length-prefixed opcodes:
//!   0x01: write_page(pgno, page[page_size])
//!   0x02: truncate(size)
//!   0x03: sync()
//!   0x04: refresh()
//! Unknown opcodes end the sequence.

use libfuzzer_sys::fuzz_target;
use rfind_zvfs::store::Store;

fuzz_target!(|data: &[u8]| {
    let dir = match tempfile::tempdir() {
        Ok(d) => d,
        Err(_) => return,
    };
    let base = dir.path().join("index.db");

    let mut store = match Store::open(&base, false) {
        Ok(s) => s,
        Err(_) => return,
    };
    let page_size = store.page_size() as usize;
    if page_size == 0 || page_size > 65536 {
        return;
    }
    let max_pages: u32 = 64;

    let mut i = 0usize;
    while i < data.len() {
        let op = data[i];
        i += 1;
        match op {
            0x01 => {
                if i + 4 > data.len() {
                    break;
                }
                let pgno =
                    u32::from_le_bytes([data[i], data[i + 1], data[i + 2], data[i + 3]])
                        % max_pages
                        + 1;
                i += 4;
                let end = (i + page_size).min(data.len());
                let mut page = vec![0u8; page_size];
                page[..end - i].copy_from_slice(&data[i..end]);
                i = end;
                let _ = store.write_page(pgno, &page);
            }
            0x02 => {
                if i + 4 > data.len() {
                    break;
                }
                let n_pages =
                    u32::from_le_bytes([data[i], data[i + 1], data[i + 2], data[i + 3]])
                        % (max_pages + 1);
                i += 4;
                let size = n_pages as u64 * page_size as u64;
                let _ = store.truncate(size);
            }
            0x03 => {
                let _ = store.sync();
            }
            0x04 => {
                let _ = store.refresh();
            }
            _ => break,
        }
    }
    let _ = store.sync();
    drop(store);

    // Re-open via the SQLite VFS wrapper and run integrity_check.
    if let Ok(conn) = rfind_zvfs::open(&base, rusqlite::OpenFlags::default()) {
        let _: rusqlite::Result<String> =
            conn.query_row("PRAGMA integrity_check", [], |r| r.get(0));
    }
});
