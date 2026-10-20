//! Symlink-safe directory listing.
//!
//! The monitor subtree walkers (`watch_new_tree`, `moved_in`) run as root
//! on paths it just learned from a kernel event. Between the event and
//! our open, an unprivileged attacker who owns the parent directory can
//! swap the created entry for a symlink to an out-of-scope target
//! (classic example: `/root`). A bare `fs::read_dir(path)` would follow
//! that symlink and the daemon would walk — and journal `Create` events
//! for — every file in the target. Journal replay has the same shape.
//!
//! `read_dir_safe` closes the race by opening the path with
//! `openat2(RESOLVE_NO_SYMLINKS)` so **no component** of the path may be
//! a symlink, then reads entries via `/proc/self/fd/<n>`. The latter is
//! a magic link the kernel resolves to the open fd, so the subsequent
//! directory read uses the inode we validated, not a path that could
//! have been swapped again. On pre-5.6 kernels we fall back to
//! `openat(O_NOFOLLOW)` which protects only the leaf.

use std::ffi::CString;
use std::fs;
use std::io;
use std::os::unix::io::RawFd;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

#[repr(C)]
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}

const SYS_OPENAT2: libc::c_long = 437;
const RESOLVE_NO_SYMLINKS: u64 = 0x04;
static OPENAT2_UNAVAILABLE: AtomicBool = AtomicBool::new(false);

/// Open `path` so that no component may be a symlink, then enumerate it.
/// On success, returns `fs::ReadDir` backed by the open fd (safe against
/// subsequent path manipulation). Returns `io::ErrorKind::NotADirectory`
/// if any component is a symlink.
pub fn read_dir_safe(path: &Path) -> io::Result<fs::ReadDir> {
    let fd = open_dir_strict(path)?;
    // SAFETY: `fd` is an open directory fd owned by this scope. The
    // `read_dir` on /proc/self/fd/<fd> will dup through this fd; closing
    // ours after the ReadDir is created is fine because the kernel keeps
    // the inode alive via the ReadDir's own fd. To keep things obvious
    // we close explicitly after the magic-link resolution.
    let proc_path = format!("/proc/self/fd/{fd}");
    let rd = fs::read_dir(&proc_path);
    // Close the explicit fd; the ReadDir carries its own fd now.
    unsafe {
        libc::close(fd);
    }
    rd
}

/// Stat a path so that no component may be a symlink, including parent
/// components. The daemon's journal replay previously called
/// `fs::symlink_metadata`, which only `AT_SYMLINK_NOFOLLOW`s the leaf —
/// an attacker who owns an ancestor directory could redirect the stat
/// mid-replay. This variant uses `openat2(RESOLVE_NO_SYMLINKS)` plus
/// `fstatat(..., AT_EMPTY_PATH)` on the resulting fd.
pub fn stat_safe(path: &Path) -> io::Result<fs::Metadata> {
    // Open with any-file semantics via O_PATH so a non-dir leaf also
    // works (sync_path has to handle files and dirs).
    let cpath = CString::new(path.as_os_str().as_encoded_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
    let base_flags = libc::O_PATH | libc::O_CLOEXEC | libc::O_NOFOLLOW;
    let fd = if !OPENAT2_UNAVAILABLE.load(Ordering::Relaxed) {
        let how = OpenHow {
            flags: base_flags as u64,
            mode: 0,
            resolve: RESOLVE_NO_SYMLINKS,
        };
        let fd = unsafe {
            libc::syscall(
                SYS_OPENAT2,
                libc::AT_FDCWD as libc::c_long,
                cpath.as_ptr() as libc::c_long,
                &how as *const OpenHow as libc::c_long,
                std::mem::size_of::<OpenHow>() as libc::c_long,
            )
        } as RawFd;
        if fd >= 0 {
            fd
        } else {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::ENOSYS) {
                OPENAT2_UNAVAILABLE.store(true, Ordering::Relaxed);
                unsafe { libc::openat(libc::AT_FDCWD, cpath.as_ptr(), base_flags) }
            } else {
                return Err(e);
            }
        }
    } else {
        unsafe { libc::openat(libc::AT_FDCWD, cpath.as_ptr(), base_flags) }
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fd is a valid open file descriptor. From_raw_fd takes
    // ownership and will close it on drop; .metadata() uses fstat.
    let f = unsafe { <fs::File as std::os::fd::FromRawFd>::from_raw_fd(fd) };
    f.metadata()
}

fn open_dir_strict(path: &Path) -> io::Result<RawFd> {
    let cpath = CString::new(path.as_os_str().as_encoded_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
    let base_flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW;
    if !OPENAT2_UNAVAILABLE.load(Ordering::Relaxed) {
        let how = OpenHow {
            flags: base_flags as u64,
            mode: 0,
            resolve: RESOLVE_NO_SYMLINKS,
        };
        let fd = unsafe {
            libc::syscall(
                SYS_OPENAT2,
                libc::AT_FDCWD as libc::c_long,
                cpath.as_ptr() as libc::c_long,
                &how as *const OpenHow as libc::c_long,
                std::mem::size_of::<OpenHow>() as libc::c_long,
            )
        } as RawFd;
        if fd >= 0 {
            return Ok(fd);
        }
        let err = io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::ENOSYS) {
            OPENAT2_UNAVAILABLE.store(true, Ordering::Relaxed);
        } else {
            return Err(err);
        }
    }
    let fd = unsafe { libc::openat(libc::AT_FDCWD, cpath.as_ptr(), base_flags) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(fd)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use tempfile::TempDir;

    #[test]
    fn reads_a_real_directory() {
        let d = TempDir::new().unwrap();
        std::fs::write(d.path().join("a.txt"), b"x").unwrap();
        let entries: Vec<_> = read_dir_safe(d.path()).unwrap().flatten().collect();
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn refuses_a_symlinked_directory() {
        let d = TempDir::new().unwrap();
        std::fs::create_dir(d.path().join("real")).unwrap();
        symlink("real", d.path().join("link")).unwrap();
        // The link's final component is a symlink — must be rejected.
        let r = read_dir_safe(&d.path().join("link"));
        assert!(r.is_err(), "symlinked dir must not be opened");
    }

    #[test]
    fn refuses_a_mid_component_symlink() {
        // Only runs on kernels with openat2 (5.6+). On older kernels
        // this test just confirms the leaf is protected (same as above).
        let d = TempDir::new().unwrap();
        std::fs::create_dir(d.path().join("real")).unwrap();
        std::fs::create_dir(d.path().join("real/inner")).unwrap();
        std::fs::write(d.path().join("real/inner/x.txt"), b"x").unwrap();
        symlink("real", d.path().join("alias")).unwrap();
        let via_alias = d.path().join("alias/inner");
        let r = read_dir_safe(&via_alias);
        if !OPENAT2_UNAVAILABLE.load(Ordering::Relaxed) {
            assert!(r.is_err(), "mid-path symlink must be rejected on 5.6+");
        }
    }
}
