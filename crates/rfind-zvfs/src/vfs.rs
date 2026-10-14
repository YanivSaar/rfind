//! The SQLite VFS: SQLite's default Unix VFS, except that the main
//! database file's contents live in a compressed [`Store`].
//!
//! The main database path itself stays an empty stub: SQLite's byte-range
//! locks, the `-wal` file and the `-shm` file all keep working unchanged
//! (they belong to the wrapped Unix file), so WAL mode and concurrent
//! readers behave exactly as with plain SQLite. Every other file (WAL,
//! journals, temp files) passes straight through.

use crate::store::Store;
use rusqlite::ffi;
use std::{
    ffi::{c_char, c_int, c_void, CStr},
    mem,
    path::PathBuf,
    ptr,
    sync::OnceLock,
};

pub const VFS_NAME: &str = "rfind-zstd";

/// File-control opcode: compact the store if worthwhile (see
/// [`crate::compact`]). Outside SQLite's own opcode range.
pub(crate) const FCNTL_COMPACT: c_int = 0x5246_5A01;
const VFS_NAME_C: &CStr = c"rfind-zstd";

/// A file opened through this VFS. `base` must come first: SQLite hands
/// us a pointer to it.
#[repr(C)]
struct ZFile {
    base: ffi::sqlite3_file,
    /// The Unix VFS file this wraps (allocated with its szOsFile).
    inner: *mut ffi::sqlite3_file,
    /// Main database only; null for pass-through files.
    store: *mut Store,
}

struct Registered {
    inner: *mut ffi::sqlite3_vfs,
}
unsafe impl Send for Registered {}
unsafe impl Sync for Registered {}

static VFS: OnceLock<Result<Registered, String>> = OnceLock::new();

fn inner_vfs() -> *mut ffi::sqlite3_vfs {
    VFS.get()
        .and_then(|r| r.as_ref().ok())
        .map_or(ptr::null_mut(), |r| r.inner)
}

/// Register the VFS (once per process). Open databases with
/// `Connection::open_with_flags_and_vfs(path, flags, VFS_NAME)`.
pub fn register() -> Result<(), String> {
    VFS.get_or_init(|| unsafe {
        let inner = ffi::sqlite3_vfs_find(ptr::null());
        if inner.is_null() {
            return Err("SQLite has no default VFS".into());
        }
        // Start from a copy of the Unix VFS: every call except xOpen goes
        // straight to it (those functions don't use pAppData).
        let mut v: ffi::sqlite3_vfs = *inner;
        v.zName = VFS_NAME_C.as_ptr();
        v.szOsFile = mem::size_of::<ZFile>() as c_int;
        v.pNext = ptr::null_mut();
        v.xOpen = Some(z_open);
        let ours = Box::into_raw(Box::new(v));
        let rc = ffi::sqlite3_vfs_register(ours, 0);
        if rc != ffi::SQLITE_OK {
            return Err(format!("sqlite3_vfs_register failed ({rc})"));
        }
        Ok(Registered { inner })
    })
    .as_ref()
    .map(|_| ())
    .map_err(Clone::clone)
}

unsafe fn zf<'a>(f: *mut ffi::sqlite3_file) -> &'a mut ZFile {
    &mut *(f as *mut ZFile)
}

unsafe fn inner_methods<'a>(f: &ZFile) -> &'a ffi::sqlite3_io_methods {
    &*(*f.inner).pMethods
}

unsafe extern "C" fn z_open(
    _vfs: *mut ffi::sqlite3_vfs,
    name: *const c_char,
    file: *mut ffi::sqlite3_file,
    flags: c_int,
    out_flags: *mut c_int,
) -> c_int {
    let f = zf(file);
    f.base.pMethods = ptr::null();
    f.inner = ptr::null_mut();
    f.store = ptr::null_mut();
    let iv = inner_vfs();
    let inner = ffi::sqlite3_malloc((*iv).szOsFile) as *mut ffi::sqlite3_file;
    if inner.is_null() {
        return ffi::SQLITE_NOMEM;
    }
    ptr::write_bytes(inner as *mut u8, 0, (*iv).szOsFile as usize);
    let rc = ((*iv).xOpen.unwrap())(iv, name, inner, flags, out_flags);
    if rc != ffi::SQLITE_OK {
        ffi::sqlite3_free(inner as *mut c_void);
        return rc;
    }
    f.inner = inner;
    if flags & ffi::SQLITE_OPEN_MAIN_DB != 0 && !name.is_null() {
        let path = PathBuf::from(CStr::from_ptr(name).to_string_lossy().into_owned());
        let readonly = flags & ffi::SQLITE_OPEN_READONLY != 0;
        match Store::open(&path, readonly) {
            Ok(s) => f.store = Box::into_raw(Box::new(s)),
            Err(_) => {
                if let Some(close) = (*(*inner).pMethods).xClose {
                    close(inner);
                }
                ffi::sqlite3_free(inner as *mut c_void);
                f.inner = ptr::null_mut();
                return ffi::SQLITE_CANTOPEN;
            }
        }
        f.base.pMethods = &Z_METHODS;
    } else {
        f.base.pMethods = &PASS_METHODS;
    }
    ffi::SQLITE_OK
}

// --- Compressed main database ---------------------------------------------

static Z_METHODS: ffi::sqlite3_io_methods = ffi::sqlite3_io_methods {
    // Version 2: shared memory (WAL) yes, xFetch/mmap no — the stub file
    // must never be memory-mapped as if it held the pages.
    iVersion: 2,
    xClose: Some(z_close),
    xRead: Some(z_read),
    xWrite: Some(z_write),
    xTruncate: Some(z_truncate),
    xSync: Some(z_sync),
    xFileSize: Some(z_file_size),
    xLock: Some(p_lock),
    xUnlock: Some(p_unlock),
    xCheckReservedLock: Some(p_check_reserved),
    xFileControl: Some(z_file_control),
    xSectorSize: Some(p_sector_size),
    xDeviceCharacteristics: Some(p_device_characteristics),
    xShmMap: Some(p_shm_map),
    xShmLock: Some(z_shm_lock),
    xShmBarrier: Some(p_shm_barrier),
    xShmUnmap: Some(p_shm_unmap),
    xFetch: None,
    xUnfetch: None,
};

unsafe fn store<'a>(f: *mut ffi::sqlite3_file) -> &'a mut Store {
    &mut *zf(f).store
}

unsafe extern "C" fn z_close(file: *mut ffi::sqlite3_file) -> c_int {
    let f = zf(file);
    if !f.store.is_null() {
        drop(Box::from_raw(f.store));
        f.store = ptr::null_mut();
    }
    p_close(file)
}

unsafe extern "C" fn z_read(
    file: *mut ffi::sqlite3_file,
    buf: *mut c_void,
    amt: c_int,
    off: i64,
) -> c_int {
    let s = store(file);
    let out = std::slice::from_raw_parts_mut(buf as *mut u8, amt as usize);
    let ps = s.page_size() as u64;
    if ps == 0 {
        out.fill(0);
        return ffi::SQLITE_IOERR_SHORT_READ;
    }
    let mut done = 0usize;
    let mut page = Vec::new();
    while done < out.len() {
        let abs = off as u64 + done as u64;
        let pgno = (abs / ps + 1) as u32;
        let within = (abs % ps) as usize;
        let n = (ps as usize - within).min(out.len() - done);
        let found = if within == 0 && n == ps as usize {
            s.read_page(pgno, &mut out[done..done + n])
        } else {
            page.resize(ps as usize, 0);
            let r = s.read_page(pgno, &mut page);
            out[done..done + n].copy_from_slice(&page[within..within + n]);
            r
        };
        match found {
            Ok(true) => {}
            Ok(false) => {
                out[done..].fill(0);
                return ffi::SQLITE_IOERR_SHORT_READ;
            }
            Err(_) => return ffi::SQLITE_IOERR_READ,
        }
        done += n;
    }
    ffi::SQLITE_OK
}

unsafe extern "C" fn z_write(
    file: *mut ffi::sqlite3_file,
    buf: *const c_void,
    amt: c_int,
    off: i64,
) -> c_int {
    let s = store(file);
    let data = std::slice::from_raw_parts(buf as *const u8, amt as usize);
    let mut ps = s.page_size() as u64;
    if ps == 0 {
        // The first write is page 1, a whole page: it fixes the page size.
        if off != 0 || !(512..=65536).contains(&amt) || (amt & (amt - 1)) != 0 {
            return ffi::SQLITE_IOERR_WRITE;
        }
        ps = amt as u64;
    }
    let mut done = 0usize;
    let mut page = Vec::new();
    while done < data.len() {
        let abs = off as u64 + done as u64;
        let pgno = (abs / ps + 1) as u32;
        let within = (abs % ps) as usize;
        let n = (ps as usize - within).min(data.len() - done);
        let r = if within == 0 && n == ps as usize {
            s.write_page(pgno, &data[done..done + n])
        } else {
            // Partial page: read, modify, write.
            page.resize(ps as usize, 0);
            if s.page_size() != 0 && s.read_page(pgno, &mut page).is_err() {
                return ffi::SQLITE_IOERR_WRITE;
            }
            page[within..within + n].copy_from_slice(&data[done..done + n]);
            s.write_page(pgno, &page)
        };
        if r.is_err() {
            return ffi::SQLITE_IOERR_WRITE;
        }
        done += n;
    }
    ffi::SQLITE_OK
}

unsafe extern "C" fn z_truncate(file: *mut ffi::sqlite3_file, size: i64) -> c_int {
    match store(file).truncate(size as u64) {
        Ok(()) => ffi::SQLITE_OK,
        Err(e) => {
            tracing::error!("zvfs: truncate failed: {e}");
            ffi::SQLITE_IOERR_TRUNCATE
        }
    }
}

unsafe extern "C" fn z_sync(file: *mut ffi::sqlite3_file, _flags: c_int) -> c_int {
    match store(file).sync() {
        Ok(()) => ffi::SQLITE_OK,
        Err(_) => ffi::SQLITE_IOERR_FSYNC,
    }
}

unsafe extern "C" fn z_file_size(file: *mut ffi::sqlite3_file, size: *mut i64) -> c_int {
    *size = store(file).size() as i64;
    ffi::SQLITE_OK
}

unsafe extern "C" fn z_file_control(
    file: *mut ffi::sqlite3_file,
    op: c_int,
    arg: *mut c_void,
) -> c_int {
    match op {
        // Size hints and chunking apply to the stub; the store grows itself.
        ffi::SQLITE_FCNTL_SIZE_HINT | ffi::SQLITE_FCNTL_CHUNK_SIZE => ffi::SQLITE_OK,
        FCNTL_COMPACT => match store(file).compact_if_garbage() {
            Ok(_) => ffi::SQLITE_OK,
            Err(_) => ffi::SQLITE_IOERR_WRITE,
        },
        _ => p_file_control(file, op, arg),
    }
}

/// Starting a read transaction (a shared WAL read lock): pick up pages
/// another process checkpointed since we last looked.
unsafe extern "C" fn z_shm_lock(
    file: *mut ffi::sqlite3_file,
    offset: c_int,
    n: c_int,
    flags: c_int,
) -> c_int {
    let rc = p_shm_lock(file, offset, n, flags);
    if rc == ffi::SQLITE_OK
        && flags & ffi::SQLITE_SHM_LOCK != 0
        && flags & ffi::SQLITE_SHM_SHARED != 0
    {
        // A failed refresh means our in-memory page map is stale; serving
        // reads from it would produce zero / short pages and SQLite would
        // report "database disk image is malformed." Surface the I/O
        // error instead so the client sees an honest failure.
        if let Err(e) = store(file).refresh() {
            tracing::error!("zvfs: shm refresh failed: {e}");
            return ffi::SQLITE_IOERR;
        }
    }
    rc
}

// --- Pass-through (WAL, journals, temp files; locks and shm of the main db) ---

static PASS_METHODS: ffi::sqlite3_io_methods = ffi::sqlite3_io_methods {
    iVersion: 3,
    xClose: Some(p_close),
    xRead: Some(p_read),
    xWrite: Some(p_write),
    xTruncate: Some(p_truncate),
    xSync: Some(p_sync),
    xFileSize: Some(p_file_size),
    xLock: Some(p_lock),
    xUnlock: Some(p_unlock),
    xCheckReservedLock: Some(p_check_reserved),
    xFileControl: Some(p_file_control),
    xSectorSize: Some(p_sector_size),
    xDeviceCharacteristics: Some(p_device_characteristics),
    xShmMap: Some(p_shm_map),
    xShmLock: Some(p_shm_lock),
    xShmBarrier: Some(p_shm_barrier),
    xShmUnmap: Some(p_shm_unmap),
    xFetch: Some(p_fetch),
    xUnfetch: Some(p_unfetch),
};

unsafe extern "C" fn p_close(file: *mut ffi::sqlite3_file) -> c_int {
    let f = zf(file);
    let mut rc = ffi::SQLITE_OK;
    if !f.inner.is_null() {
        if !(*f.inner).pMethods.is_null() {
            rc = (inner_methods(f).xClose.unwrap())(f.inner);
        }
        ffi::sqlite3_free(f.inner as *mut c_void);
        f.inner = ptr::null_mut();
    }
    rc
}

macro_rules! forward {
    ($name:ident, $method:ident, ($($arg:ident: $ty:ty),*), $ret:ty, $default:expr) => {
        unsafe extern "C" fn $name(file: *mut ffi::sqlite3_file $(, $arg: $ty)*) -> $ret {
            let f = zf(file);
            match inner_methods(f).$method {
                Some(m) => m(f.inner $(, $arg)*),
                None => $default,
            }
        }
    };
}

forward!(p_read, xRead, (buf: *mut c_void, amt: c_int, off: i64), c_int, ffi::SQLITE_IOERR_READ);
forward!(p_write, xWrite, (buf: *const c_void, amt: c_int, off: i64), c_int, ffi::SQLITE_IOERR_WRITE);
forward!(p_truncate, xTruncate, (size: i64), c_int, ffi::SQLITE_IOERR_TRUNCATE);
forward!(p_sync, xSync, (flags: c_int), c_int, ffi::SQLITE_IOERR_FSYNC);
forward!(p_file_size, xFileSize, (size: *mut i64), c_int, ffi::SQLITE_IOERR_FSTAT);
forward!(p_lock, xLock, (level: c_int), c_int, ffi::SQLITE_IOERR_LOCK);
forward!(p_unlock, xUnlock, (level: c_int), c_int, ffi::SQLITE_IOERR_UNLOCK);
forward!(p_check_reserved, xCheckReservedLock, (out: *mut c_int), c_int, ffi::SQLITE_IOERR_CHECKRESERVEDLOCK);
forward!(p_file_control, xFileControl, (op: c_int, arg: *mut c_void), c_int, ffi::SQLITE_NOTFOUND);
forward!(p_sector_size, xSectorSize, (), c_int, 4096);
forward!(
    p_device_characteristics,
    xDeviceCharacteristics,
    (),
    c_int,
    0
);
forward!(p_shm_map, xShmMap, (region: c_int, size: c_int, extend: c_int, out: *mut *mut c_void), c_int, ffi::SQLITE_IOERR_SHMMAP);
forward!(p_shm_lock, xShmLock, (offset: c_int, n: c_int, flags: c_int), c_int, ffi::SQLITE_IOERR_SHMLOCK);
forward!(p_shm_unmap, xShmUnmap, (delete: c_int), c_int, ffi::SQLITE_OK);
forward!(p_fetch, xFetch, (off: i64, amt: c_int, out: *mut *mut c_void), c_int, {
    *out = ptr::null_mut();
    ffi::SQLITE_OK
});
forward!(p_unfetch, xUnfetch, (off: i64, p: *mut c_void), c_int, ffi::SQLITE_OK);

unsafe extern "C" fn p_shm_barrier(file: *mut ffi::sqlite3_file) {
    let f = zf(file);
    if let Some(m) = inner_methods(f).xShmBarrier {
        m(f.inner)
    }
}
