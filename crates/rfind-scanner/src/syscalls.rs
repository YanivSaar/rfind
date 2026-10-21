//! Linux syscalls the scanner issues directly: `getdents64` layout,
//! `statx` (both the one-shot syscall and the `io_uring` batched path),
//! and the `Stat` summary struct the worker consumes. Kept separate so
//! the raw-FFI + leak-on-timeout logic doesn't drown out the scanner's
//! control flow.

use libc::{syscall, SYS_statx, EINTR};
use std::ffi::{CStr, CString};
use std::os::unix::io::RawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use tracing::warn;

// ---------------------------------------------------------------------------
// Linux kernel structs for getdents64
// ---------------------------------------------------------------------------

#[repr(C)]
pub(crate) struct LinuxDirent64 {
    pub(crate) d_ino: u64,
    pub(crate) d_off: i64,
    pub(crate) d_reclen: u16,
    pub(crate) d_type: u8,
    // d_name follows: variable-length null-terminated string
}

pub(crate) const DT_DIR: u8 = 4;
pub(crate) const DT_REG: u8 = 8;
pub(crate) const DT_LNK: u8 = 10;
pub(crate) const DT_UNKNOWN: u8 = 0;

// ---------------------------------------------------------------------------
// statx structures
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Default)]
pub(crate) struct StatxTimestamp {
    pub(crate) tv_sec: i64,
    pub(crate) tv_nsec: u32,
    pub(crate) _pad: i32,
}

#[repr(C)]
#[derive(Default)]
pub(crate) struct Statx {
    pub(crate) stx_mask: u32,
    pub(crate) stx_blksize: u32,
    pub(crate) stx_attributes: u64,
    pub(crate) stx_nlink: u32,
    pub(crate) stx_uid: u32,
    pub(crate) stx_gid: u32,
    pub(crate) stx_mode: u16,
    pub(crate) _pad1: u16,
    pub(crate) stx_ino: u64,
    pub(crate) stx_size: u64,
    pub(crate) stx_blocks: u64,
    pub(crate) stx_attributes_mask: u64,
    pub(crate) stx_atime: StatxTimestamp,
    pub(crate) stx_btime: StatxTimestamp,
    pub(crate) stx_ctime: StatxTimestamp,
    pub(crate) stx_mtime: StatxTimestamp,
    pub(crate) stx_rdev_major: u32,
    pub(crate) stx_rdev_minor: u32,
    pub(crate) stx_dev_major: u32,
    pub(crate) stx_dev_minor: u32,
    pub(crate) stx_mnt_id: u64,
    pub(crate) _pad2: [u64; 13],
}

pub const STATX_TYPE: u32 = 0x1;
pub const STATX_MTIME: u32 = 0x40;
pub const STATX_SIZE: u32 = 0x200;
pub const STATX_ATIME: u32 = 0x20;
pub const STATX_CTIME: u32 = 0x80;
/// The indexer's statx mask: type + mtime + size.
pub const STATX_MASK: u32 = STATX_TYPE | STATX_MTIME | STATX_SIZE;
const S_IFMT: u16 = 0o170000;
const S_IFDIR: u16 = 0o040000;
const S_IFLNK: u16 = 0o120000;

/// What the scanner needs from statx. `atime`/`ctime` are populated only
/// when the caller requests them via a wider mask; otherwise they are 0.
/// `(dev, ino)` identify an inode uniquely across bind mounts — used by
/// the `-L` symlink mode to break traversal cycles.
#[derive(Clone, Copy)]
pub(crate) struct Stat {
    pub(crate) is_dir: bool,
    pub(crate) is_symlink: bool,
    pub(crate) mtime_sec: i64,
    pub(crate) mtime_nsec: i64,
    pub(crate) atime_sec: i64,
    pub(crate) ctime_sec: i64,
    pub(crate) size: u64,
    pub(crate) dev: u64,
    pub(crate) ino: u64,
}

impl Stat {
    fn from_statx(stx: &Statx) -> Self {
        let mode = stx.stx_mode & S_IFMT;
        Self {
            is_dir: mode == S_IFDIR,
            is_symlink: mode == S_IFLNK,
            mtime_sec: stx.stx_mtime.tv_sec,
            mtime_nsec: stx.stx_mtime.tv_nsec as i64,
            atime_sec: stx.stx_atime.tv_sec,
            ctime_sec: stx.stx_ctime.tv_sec,
            size: stx.stx_size,
            dev: ((stx.stx_dev_major as u64) << 32) | stx.stx_dev_minor as u64,
            ino: stx.stx_ino,
        }
    }
}

/// statx(dirfd, name, flags, mask) as a plain syscall. `flags` typically
/// includes `AT_SYMLINK_NOFOLLOW`; omit it to follow a symlink target.
fn statx_one(dirfd: RawFd, name: &CStr, mask: u32, flags: i32) -> Option<Stat> {
    let mut stx = Statx::default();
    loop {
        let ret = unsafe {
            syscall(
                SYS_statx, // per-architecture number (332 on x86_64, 291 on aarch64)
                dirfd as libc::c_long,
                name.as_ptr() as libc::c_long,
                flags as libc::c_long,
                mask as libc::c_long,
                &mut stx as *mut Statx as libc::c_long,
            )
        };
        if ret == 0 {
            return Some(Stat::from_statx(&stx));
        }
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() == Some(EINTR) {
            continue;
        }
        return None;
    }
}

/// Ring size for batched statx; one batch per getdents64 buffer chunk.
const URING_ENTRIES: u32 = 256;

/// Latches when io_uring turned out to be unusable at least once; only used
/// to suppress repeated warnings and to report the result honestly. Each
/// scan still tries to open a ring so a transient hiccup doesn't disable
/// io_uring for the whole life of a long-running daemon.
pub(crate) static URING_UNAVAILABLE: AtomicBool = AtomicBool::new(false);

pub(crate) fn new_ring() -> Option<io_uring::IoUring> {
    match io_uring::IoUring::new(URING_ENTRIES) {
        Ok(r) => Some(r),
        Err(e) => {
            if !URING_UNAVAILABLE.swap(true, Ordering::Relaxed) {
                warn!("io_uring unavailable ({e}); using plain statx syscalls");
            }
            None
        }
    }
}

/// statx for every name in `names` (relative to `dirfd`). With a ring the
/// calls are submitted in batches: one syscall per batch instead of one
/// per entry, and on slow filesystems (network, WSL2 drvfs) the kernel
/// runs them concurrently.
///
/// Safety: `bufs` AND the name allocations are handed to the kernel by raw
/// pointer. We must not drop them (or the ring) until every submitted SQE
/// has completed — closing the uring fd does not synchronously cancel
/// in-flight ops, so a late kernel read of the name or write to the buf
/// would otherwise be a use-after-free. The caller's CStrings go away on
/// the next getdents iteration (`batch.clear()`), so the ring path owns
/// its own copies for the duration of the submission: on a clean path
/// they drop normally; on the leak path they are leaked alongside the
/// ring and bufs.
pub(crate) fn statx_batch(
    ring: &mut Option<io_uring::IoUring>,
    dirfd: RawFd,
    names: &[&CStr],
    mask: u32,
    flags: i32,
) -> Vec<Option<Stat>> {
    let Some(r) = ring.as_mut() else {
        return names
            .iter()
            .map(|n| statx_one(dirfd, n, mask, flags))
            .collect();
    };
    let owned_names: Vec<CString> = names.iter().map(|n| CString::from(*n)).collect();
    let mut out = vec![None; names.len()];
    let mut bufs: Vec<Statx> = (0..names.len()).map(|_| Statx::default()).collect();
    for (chunk_start, chunk) in owned_names.chunks(URING_ENTRIES as usize).enumerate() {
        let base = chunk_start * URING_ENTRIES as usize;
        let mut submitted = 0usize;
        for (i, name) in chunk.iter().enumerate() {
            let sqe = io_uring::opcode::Statx::new(
                io_uring::types::Fd(dirfd),
                name.as_ptr(),
                &mut bufs[base + i] as *mut Statx as *mut io_uring::types::statx,
            )
            .flags(flags)
            .mask(mask)
            .build()
            .user_data((base + i) as u64);
            if unsafe { r.submission().push(&sqe) }.is_err() {
                warn!(
                    "io_uring submission queue full ({submitted}/{}); falling back to statx syscall",
                    chunk.len()
                );
                URING_UNAVAILABLE.store(true, Ordering::Relaxed);
                if submitted > 0 {
                    let _ = r.submit_and_wait(submitted);
                    for cqe in r.completion() {
                        let i = cqe.user_data() as usize;
                        if cqe.result() >= 0 {
                            out[i] = Some(Stat::from_statx(&bufs[i]));
                        }
                    }
                }
                for (i, name) in names.iter().enumerate() {
                    if out[i].is_none() {
                        out[i] = statx_one(dirfd, name, mask, flags);
                    }
                }
                return out;
            }
            submitted += 1;
        }
        let mut done = 0usize;
        while done < submitted {
            match r.submit_and_wait(submitted - done) {
                Ok(_) => {}
                Err(e) if e.raw_os_error() == Some(EINTR) => {
                    // Drain whatever came back and retry.
                }
                Err(e) => {
                    warn!("io_uring submit_and_wait failed ({e}); draining and giving up");
                    for cqe in r.completion() {
                        let i = cqe.user_data() as usize;
                        if cqe.result() >= 0 {
                            out[i] = Some(Stat::from_statx(&bufs[i]));
                        }
                        done += 1;
                    }
                    if done < submitted {
                        warn!(
                            "io_uring: {} ops still outstanding; leaking ring+buffers+names",
                            submitted - done
                        );
                        if let Some(old) = ring.take() {
                            std::mem::forget(old);
                        }
                        std::mem::forget(bufs);
                        std::mem::forget(owned_names);
                        URING_UNAVAILABLE.store(true, Ordering::Relaxed);
                        return names
                            .iter()
                            .map(|n| statx_one(dirfd, n, mask, flags))
                            .collect();
                    }
                    URING_UNAVAILABLE.store(true, Ordering::Relaxed);
                    *ring = None;
                    return names
                        .iter()
                        .map(|n| statx_one(dirfd, n, mask, flags))
                        .collect();
                }
            }
            for cqe in r.completion() {
                let i = cqe.user_data() as usize;
                if cqe.result() >= 0 {
                    out[i] = Some(Stat::from_statx(&bufs[i]));
                }
                done += 1;
            }
        }
    }
    out
}
