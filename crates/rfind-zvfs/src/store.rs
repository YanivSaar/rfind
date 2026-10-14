//! Compressed page store: the contents of one SQLite main-database file,
//! kept as zstd-compressed pages in an append-only data file.
//!
//! # Files (next to the database path `<db>`)
//! - `<db>-zdata.<gen>`: records `[magic u32][pgno u32][clen u32][rawlen u32]
//!   [crc32 u32][clen bytes of zstd]`. A newer record for a page replaces
//!   older ones; old versions are never overwritten.
//! - `<db>-zmap`: a snapshot of page → record offset for `<gen>`, covering
//!   the data file up to `covered`. Written to a temporary file and renamed,
//!   so it is always complete. Readers load it, then replay the records
//!   appended after `covered`.
//!
//! # Why this is crash-safe
//! SQLite (in WAL mode) writes the main database only while checkpointing,
//! and keeps the WAL until the checkpoint is synced. Records are only ever
//! appended; a crash can leave a torn record at the end, which fails its
//! CRC and is ignored — and SQLite redoes the checkpoint from the WAL.
//!
//! # Why concurrent readers are safe
//! A reader loads the page map when it starts and reads records by offset.
//! Records are never modified, and compaction writes a new generation file
//! instead of rewriting the old one, so a reader always sees complete pages.

use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    os::unix::fs::{FileExt, MetadataExt},
    path::{Path, PathBuf},
};

const REC_MAGIC: u32 = 0x5246_5A31; // "RFZ1"
const REC_HDR: usize = 20;
const SNAP_MAGIC: &[u8; 8] = b"RFZMAP01";
/// Level 1: about twice as fast as the default 3 for a slightly lower
/// ratio; pages are recompressed at every checkpoint, so speed matters.
const ZSTD_LEVEL: i32 = 1;
/// Rewrite the snapshot once this many bytes were appended after it, so a
/// reader never replays more than this.
const SNAPSHOT_EVERY: u64 = 8 << 20;
/// While writing, compact only when the data file is this big and old
/// versions take over two thirds of it (each compaction copies the live
/// data, so a high bar keeps write amplification low). Callers compact
/// after bulk work with [`Store::compact_if_garbage`].
const COMPACT_MIN: u64 = 64 << 20;

/// Where a page's newest record lives.
#[derive(Clone, Copy, Default, Debug, PartialEq)]
struct Loc {
    off: u64,
    /// Compressed length; 0 = page absent.
    clen: u32,
}

pub struct Store {
    base: PathBuf,
    readonly: bool,
    page_size: u32,
    /// Index: page number - 1.
    map: Vec<Loc>,
    gen: u64,
    data: Option<File>,
    /// End of the last valid record.
    data_len: u64,
    /// Bytes of the data file the snapshot covers.
    covered: u64,
    /// Bytes held by current page versions (record headers included).
    live: u64,
    /// Identity of the snapshot this map was built from.
    snap_id: Option<(u64, i64, i64)>,
    dirty: bool,
    /// Reused for every page read: record bytes and a zstd context.
    scratch: Vec<u8>,
    dctx: Option<zstd::bulk::Decompressor<'static>>,
    /// Reused for every page write: a single zstd compressor + scratch
    /// buffer instead of a fresh context per page (hot during bulk scans).
    cctx: Option<zstd::bulk::Compressor<'static>>,
    comp_scratch: Vec<u8>,
    /// Pages were dropped since the last snapshot: replaying the log would
    /// bring them back, so the next sync must publish a snapshot.
    truncated: bool,
}

fn snap_path(base: &Path) -> PathBuf {
    PathBuf::from(format!("{}-zmap", base.display()))
}

fn data_path(base: &Path, gen: u64) -> PathBuf {
    PathBuf::from(format!("{}-zdata.{gen}", base.display()))
}

/// The files a store keeps for `base` (for deleting a database).
pub fn store_files(base: &Path) -> Vec<PathBuf> {
    let mut v = vec![snap_path(base)];
    let prefix = format!(
        "{}-zdata.",
        base.file_name().unwrap_or_default().to_string_lossy()
    );
    if let Ok(rd) = fs::read_dir(base.parent().unwrap_or(Path::new("/"))) {
        for e in rd.flatten() {
            if e.file_name().to_string_lossy().starts_with(&prefix) {
                v.push(e.path());
            }
        }
    }
    v
}

/// Is `base` stored in this format (has a snapshot or a data file)?
pub fn exists(base: &Path) -> bool {
    store_files(base).iter().any(|p| p.exists())
}

fn crc(pgno: u32, rawlen: u32, payload: &[u8]) -> u32 {
    let mut h = crc32fast::Hasher::new();
    h.update(&pgno.to_le_bytes());
    h.update(&rawlen.to_le_bytes());
    h.update(payload);
    h.finalize()
}

fn sync_dir(path: &Path) {
    if let Some(dir) = path.parent() {
        if let Ok(d) = File::open(dir) {
            let _ = d.sync_all();
        }
    }
}

impl Store {
    /// Open the store for database `base`. A writer also discards a torn
    /// record at the end of the data file.
    pub fn open(base: &Path, readonly: bool) -> io::Result<Self> {
        let mut s = Store {
            base: base.to_path_buf(),
            readonly,
            page_size: 0,
            map: Vec::new(),
            gen: 0,
            data: None,
            data_len: 0,
            covered: 0,
            live: 0,
            snap_id: None,
            dirty: false,
            scratch: Vec::new(),
            dctx: None,
            cctx: None,
            comp_scratch: Vec::new(),
            truncated: false,
        };
        s.load()?;
        if !readonly {
            if let Some(f) = &s.data {
                if f.metadata()?.len() > s.data_len {
                    f.set_len(s.data_len)?;
                }
            }
        }
        Ok(s)
    }

    fn snap_identity(&self) -> Option<(u64, i64, i64)> {
        fs::metadata(snap_path(&self.base)).ok().map(|m| {
            (
                m.ino(),
                m.mtime_nsec() + m.mtime() * 1_000_000_000,
                m.size() as i64,
            )
        })
    }

    /// (Re)build the map from the snapshot plus the records after it.
    fn load(&mut self) -> io::Result<()> {
        self.map.clear();
        self.gen = 0;
        self.covered = 0;
        self.page_size = 0;
        self.snap_id = self.snap_identity();
        if let Ok(bytes) = fs::read(snap_path(&self.base)) {
            self.parse_snapshot(&bytes);
        }
        self.data = self.open_data(self.gen)?;
        self.data_len = self.covered;
        self.replay_tail()?;
        self.recount_live();
        Ok(())
    }

    fn open_data(&self, gen: u64) -> io::Result<Option<File>> {
        let p = data_path(&self.base, gen);
        let r = if self.readonly {
            File::open(&p)
        } else {
            OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&p)
        };
        match r {
            Ok(f) => Ok(Some(f)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn parse_snapshot(&mut self, b: &[u8]) {
        let u32_at = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
        let u64_at = |o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap());
        if b.len() < 44 || &b[..8] != SNAP_MAGIC {
            return;
        }
        let body = &b[..b.len() - 4];
        if crc32fast::hash(body) != u32_at(b.len() - 4) {
            return; // never happens with rename-based writes; be safe anyway
        }
        let n = u64_at(32) as usize;
        if 40 + n * 12 + 4 != b.len() {
            return;
        }
        self.page_size = u32_at(8);
        self.gen = u64_at(16);
        self.covered = u64_at(24);
        self.map = (0..n)
            .map(|i| Loc {
                off: u64_at(40 + i * 12),
                clen: u32_at(40 + i * 12 + 8),
            })
            .collect();
    }

    /// Apply valid records from `data_len` to the end of the data file.
    fn replay_tail(&mut self) -> io::Result<()> {
        let Some(f) = &self.data else { return Ok(()) };
        let end = f.metadata()?.len();
        let mut off = self.data_len;
        let mut hdr = [0u8; REC_HDR];
        let mut found = Vec::new();
        while off + REC_HDR as u64 <= end {
            f.read_exact_at(&mut hdr, off)?;
            let field = |i: usize| u32::from_le_bytes(hdr[i * 4..i * 4 + 4].try_into().unwrap());
            let (magic, pgno, clen, rawlen, sum) =
                (field(0), field(1), field(2), field(3), field(4));
            if magic != REC_MAGIC || pgno == 0 || off + (REC_HDR as u64) + clen as u64 > end {
                break;
            }
            let mut payload = vec![0u8; clen as usize];
            f.read_exact_at(&mut payload, off + REC_HDR as u64)?;
            if crc(pgno, rawlen, &payload) != sum {
                break; // torn write: the rest is garbage
            }
            found.push((pgno, rawlen, Loc { off, clen }));
            off += REC_HDR as u64 + clen as u64;
        }
        for (pgno, rawlen, loc) in found {
            if self.page_size == 0 {
                self.page_size = rawlen;
            }
            self.set_loc(pgno, loc);
        }
        self.data_len = off;
        Ok(())
    }

    fn set_loc(&mut self, pgno: u32, loc: Loc) {
        let i = pgno as usize - 1;
        if self.map.len() <= i {
            self.map.resize(i + 1, Loc::default());
        }
        self.map[i] = loc;
    }

    fn recount_live(&mut self) {
        self.live = self
            .map
            .iter()
            .filter(|l| l.clen > 0)
            .map(|l| REC_HDR as u64 + l.clen as u64)
            .sum();
    }

    /// Reload if another process published a new snapshot or appended
    /// records. Readers call this when they start a transaction.
    pub fn refresh(&mut self) -> io::Result<()> {
        if self.dirty {
            return Ok(()); // we are the writer; our map is newest
        }
        if self.snap_identity() != self.snap_id {
            return self.load();
        }
        let grown = match &self.data {
            Some(f) => f.metadata()?.len() > self.data_len,
            None => data_path(&self.base, self.gen).exists(),
        };
        if grown {
            if self.data.is_none() {
                self.data = self.open_data(self.gen)?;
            }
            self.replay_tail()?;
            self.recount_live();
        }
        Ok(())
    }

    pub fn page_size(&self) -> u32 {
        self.page_size
    }

    /// Logical size of the database file.
    pub fn size(&self) -> u64 {
        let pages = self
            .map
            .iter()
            .rposition(|l| l.clen > 0)
            .map_or(0, |i| i + 1);
        pages as u64 * self.page_size as u64
    }

    /// Read page `pgno` (1-based) into `buf` (page_size bytes). Returns
    /// false if the page doesn't exist (buf zeroed).
    pub fn read_page(&mut self, pgno: u32, buf: &mut [u8]) -> io::Result<bool> {
        let loc = self.map.get(pgno as usize - 1).copied().unwrap_or_default();
        let (Some(f), true) = (&self.data, loc.clen > 0) else {
            buf.fill(0);
            return Ok(false);
        };
        self.scratch.resize(REC_HDR + loc.clen as usize, 0);
        f.read_exact_at(&mut self.scratch, loc.off)?;
        if self.dctx.is_none() {
            self.dctx = Some(zstd::bulk::Decompressor::new()?);
        }
        let n = self
            .dctx
            .as_mut()
            .unwrap()
            .decompress_to_buffer(&self.scratch[REC_HDR..], buf)?;
        if n != buf.len() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "short page"));
        }
        Ok(true)
    }

    /// Append a new version of page `pgno`.
    pub fn write_page(&mut self, pgno: u32, page: &[u8]) -> io::Result<()> {
        if self.readonly {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "read-only store",
            ));
        }
        if self.page_size == 0 {
            self.page_size = page.len() as u32;
        }
        if self.data.is_none() {
            self.data = self.open_data(self.gen)?;
        }
        // Reuse one zstd context + scratch buffer across every page write.
        if self.cctx.is_none() {
            self.cctx = Some(zstd::bulk::Compressor::new(ZSTD_LEVEL)?);
        }
        let cap = zstd::zstd_safe::compress_bound(page.len());
        if self.comp_scratch.capacity() < cap {
            self.comp_scratch
                .reserve(cap - self.comp_scratch.capacity());
        }
        self.comp_scratch.clear();
        self.comp_scratch.resize(cap, 0);
        let cctx = self.cctx.as_mut().unwrap();
        let n = cctx.compress_to_buffer(page, &mut self.comp_scratch)?;
        let payload = &self.comp_scratch[..n];
        let mut rec = Vec::with_capacity(REC_HDR + payload.len());
        for v in [REC_MAGIC, pgno, payload.len() as u32, page.len() as u32] {
            rec.extend_from_slice(&v.to_le_bytes());
        }
        rec.extend_from_slice(&crc(pgno, page.len() as u32, payload).to_le_bytes());
        rec.extend_from_slice(payload);
        let f = self.data.as_ref().unwrap();
        f.write_all_at(&rec, self.data_len)?;
        let old = self.map.get(pgno as usize - 1).copied().unwrap_or_default();
        if old.clen > 0 {
            self.live -= REC_HDR as u64 + old.clen as u64;
        }
        self.live += rec.len() as u64;
        self.set_loc(
            pgno,
            Loc {
                off: self.data_len,
                clen: payload.len() as u32,
            },
        );
        self.data_len += rec.len() as u64;
        self.dirty = true;
        Ok(())
    }

    /// Drop pages beyond `size` bytes.
    ///
    /// This must publish a durable snapshot immediately: without one, a
    /// crash between the in-memory shrink and the next `sync()` would let
    /// `replay_tail` resurrect the pages we just dropped from the appended
    /// records. Writing the snapshot now (and fsyncing) commits the
    /// truncation to disk.
    pub fn truncate(&mut self, size: u64) -> io::Result<()> {
        if self.page_size == 0 {
            return Ok(());
        }
        let pages = size.div_ceil(self.page_size as u64) as usize;
        if pages < self.map.len() {
            self.map.truncate(pages);
            self.recount_live();
            self.dirty = true;
            self.truncated = true;
            if !self.readonly {
                self.write_snapshot()?;
                if let Some(f) = &self.data {
                    f.sync_data()?;
                }
                self.truncated = false;
            }
        }
        Ok(())
    }

    /// Make appended records durable; publish a snapshot or compact when
    /// worthwhile.
    pub fn sync(&mut self) -> io::Result<()> {
        if !self.dirty || self.readonly {
            return Ok(());
        }
        if let Some(f) = &self.data {
            f.sync_data()?;
        }
        if self.data_len >= COMPACT_MIN && self.data_len > 3 * self.live {
            self.compact()?;
        } else if self.truncated || self.data_len - self.covered >= SNAPSHOT_EVERY {
            self.write_snapshot()?;
        }
        self.truncated = false;
        self.dirty = false;
        Ok(())
    }

    /// Compact if old versions take more than a fifth of the data file
    /// (after a scan or other bulk writes). Returns whether it compacted.
    pub fn compact_if_garbage(&mut self) -> io::Result<bool> {
        if self.readonly
            || self.data.is_none()
            || self.data_len * 4 <= self.live * 5 + (1 << 20) * 4
        {
            return Ok(false);
        }
        if let Some(f) = &self.data {
            f.sync_data()?;
        }
        self.compact()?;
        self.dirty = false;
        Ok(true)
    }

    fn write_snapshot(&mut self) -> io::Result<()> {
        let mut b = Vec::with_capacity(44 + self.map.len() * 12);
        b.extend_from_slice(SNAP_MAGIC);
        b.extend_from_slice(&self.page_size.to_le_bytes());
        b.extend_from_slice(&0u32.to_le_bytes());
        b.extend_from_slice(&self.gen.to_le_bytes());
        b.extend_from_slice(&self.data_len.to_le_bytes());
        b.extend_from_slice(&(self.map.len() as u64).to_le_bytes());
        for l in &self.map {
            b.extend_from_slice(&l.off.to_le_bytes());
            b.extend_from_slice(&l.clen.to_le_bytes());
        }
        b.extend_from_slice(&crc32fast::hash(&b).to_le_bytes());
        let path = snap_path(&self.base);
        let tmp = PathBuf::from(format!("{}.tmp", path.display()));
        let mut f = File::create(&tmp)?;
        f.write_all(&b)?;
        f.sync_all()?;
        fs::rename(&tmp, &path)?;
        sync_dir(&path);
        self.covered = self.data_len;
        self.snap_id = self.snap_identity();
        Ok(())
    }

    /// Copy the current page versions into a new generation file, publish
    /// a snapshot pointing at it, then remove the old file. Readers still
    /// holding the old file keep reading it until they reload.
    fn compact(&mut self) -> io::Result<()> {
        let old_gen = self.gen;
        let new_gen = old_gen + 1;
        let path = data_path(&self.base, new_gen);
        let out = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)?;
        let src = self.data.as_ref().unwrap();
        let mut pos = 0u64;
        let mut buf = Vec::new();
        for loc in self.map.iter_mut().filter(|l| l.clen > 0) {
            let len = REC_HDR + loc.clen as usize;
            buf.resize(len, 0);
            src.read_exact_at(&mut buf, loc.off)?;
            out.write_all_at(&buf, pos)?;
            loc.off = pos;
            pos += len as u64;
        }
        out.sync_all()?;
        self.gen = new_gen;
        self.data = Some(out);
        self.data_len = pos;
        self.live = pos;
        self.write_snapshot()?;
        let _ = fs::remove_file(data_path(&self.base, old_gen));
        Ok(())
    }

    /// Bytes on disk (snapshot + data file).
    pub fn disk_bytes(&self) -> u64 {
        let size = |p: PathBuf| fs::metadata(p).map(|m| m.len()).unwrap_or(0);
        size(snap_path(&self.base)) + size(data_path(&self.base, self.gen))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(fill: u8, ps: usize) -> Vec<u8> {
        let mut p = vec![fill; ps];
        p[0] = 1; // something non-uniform
        p
    }

    #[test]
    fn pages_round_trip_and_survive_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("db");
        let mut s = Store::open(&base, false).unwrap();
        for i in 1..=50u32 {
            s.write_page(i, &page(i as u8, 4096)).unwrap();
        }
        s.write_page(7, &page(99, 4096)).unwrap(); // newer version wins
        s.sync().unwrap();
        assert_eq!(s.size(), 50 * 4096);
        let mut r = Store::open(&base, true).unwrap();
        let mut buf = vec![0u8; 4096];
        assert!(r.read_page(7, &mut buf).unwrap());
        assert_eq!(buf, page(99, 4096));
        assert!(r.read_page(50, &mut buf).unwrap());
        assert_eq!(buf, page(50, 4096));
        assert!(!r.read_page(51, &mut buf).unwrap());
        // Compressed: 50 pages of 4 KiB take far less than 200 KiB.
        assert!(r.disk_bytes() < 10_000, "{}", r.disk_bytes());
    }

    #[test]
    fn torn_tail_is_ignored_and_truncated_by_the_writer() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("db");
        let mut s = Store::open(&base, false).unwrap();
        s.write_page(1, &page(1, 4096)).unwrap();
        s.write_page(2, &page(2, 4096)).unwrap();
        s.sync().unwrap();
        let good_len = s.data_len;
        drop(s);
        // Simulate a crash mid-append: half a record of garbage.
        let f = OpenOptions::new()
            .append(true)
            .open(data_path(&base, 0))
            .unwrap();
        (&f).write_all(&[0x31, 0x5A, 0x46, 0x52, 3, 0, 0, 0, 200, 0])
            .unwrap();
        let r = Store::open(&base, true).unwrap();
        assert_eq!(r.size(), 2 * 4096);
        let w = Store::open(&base, false).unwrap();
        assert_eq!(fs::metadata(data_path(&base, 0)).unwrap().len(), good_len);
        assert_eq!(w.size(), 2 * 4096);
    }

    #[test]
    fn readers_pick_up_appended_records_and_snapshots() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("db");
        let mut w = Store::open(&base, false).unwrap();
        w.write_page(1, &page(1, 4096)).unwrap();
        w.sync().unwrap();
        let mut r = Store::open(&base, true).unwrap();
        assert_eq!(r.size(), 4096);
        w.write_page(2, &page(2, 4096)).unwrap();
        w.sync().unwrap();
        r.refresh().unwrap();
        assert_eq!(r.size(), 2 * 4096);
        w.write_snapshot().unwrap();
        r.refresh().unwrap();
        let mut buf = vec![0u8; 4096];
        assert!(r.read_page(2, &mut buf).unwrap());
    }

    #[test]
    fn compaction_drops_old_versions_and_keeps_old_readers_working() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("db");
        let mut w = Store::open(&base, false).unwrap();
        // Incompressible-ish pages so the data file grows past COMPACT_MIN.
        let mut seed = 1u64;
        let mut noisy = |ps: usize| {
            (0..ps)
                .map(|_| {
                    seed ^= seed << 13;
                    seed ^= seed >> 7;
                    seed ^= seed << 17;
                    seed as u8
                })
                .collect::<Vec<u8>>()
        };
        let last = noisy(4096);
        for _ in 0..(COMPACT_MIN / 4096 / 4 + 10) {
            for p in 1..=4u32 {
                w.write_page(p, &noisy(4096)).unwrap();
            }
        }
        w.write_page(1, &last).unwrap();
        let mut reader_before = Store::open(&base, true).unwrap(); // gen 0, pre-compaction
        w.sync().unwrap(); // triggers compaction
        assert_eq!(w.gen, 1);
        assert!(w.disk_bytes() < 64 * 1024, "{}", w.disk_bytes());
        let mut buf = vec![0u8; 4096];
        // A reader opened before compaction still reads its (old) file.
        assert!(reader_before.read_page(1, &mut buf).unwrap());
        assert_eq!(buf, last);
        // A new reader sees the compacted generation.
        let mut r = Store::open(&base, true).unwrap();
        assert!(r.read_page(1, &mut buf).unwrap());
        assert_eq!(buf, last);
        assert_eq!(r.size(), 4 * 4096);
    }

    #[test]
    fn truncate_shrinks_the_logical_size() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("db");
        let mut w = Store::open(&base, false).unwrap();
        for p in 1..=10 {
            w.write_page(p, &page(p as u8, 1024)).unwrap();
        }
        w.truncate(4 * 1024).unwrap();
        w.sync().unwrap(); // publishes a snapshot: replay must not resurrect pages
        assert_eq!(Store::open(&base, true).unwrap().size(), 4 * 1024);
    }
}
