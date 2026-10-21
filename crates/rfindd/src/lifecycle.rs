//! Pidfile acquisition, clean-shutdown marker, index-file permission
//! tightening, and the exec-replace used by SIGHUP reload. Kept
//! together because they all run on the boot path and share the same
//! "best-effort, log on failure" shape.

use rfind_core::log::sanitize_path;
use std::fs;
use std::path::{Path, PathBuf};
use tracing::{error, warn};

/// Path of the clean-shutdown marker alongside `index.db`. Written on
/// orderly exit (SIGTERM) and before `exec`-reload (SIGHUP), removed on
/// startup. If absent at boot, the prior run exited abnormally
/// (SIGKILL, OOM, kernel crash, power loss) and the index is opened with
/// an `integrity_check` before serving reads.
pub(crate) fn clean_marker_path(db_file: &Path) -> PathBuf {
    let mut p = db_file.to_path_buf();
    let name = db_file
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "index.db".to_string());
    p.set_file_name(format!(".{name}.clean"));
    p
}

/// Returns true iff the marker existed and was removed. The directory is
/// 0700 root-only, so the marker inherits those permissions.
pub(crate) fn take_clean_marker(path: &Path) -> bool {
    match fs::remove_file(path) {
        Ok(()) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => {
            warn!(
                "clean-shutdown marker {} present but unreadable: {e}",
                path.display()
            );
            // Treat as absent: forces integrity_check, which is the safe side.
            false
        }
    }
}

/// Best-effort: a failure to write the marker does not block shutdown,
/// it only forces an integrity_check on the next boot.
pub(crate) fn write_clean_marker(path: &Path) {
    use std::os::unix::fs::OpenOptionsExt;
    if let Err(e) = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
    {
        warn!("cannot write clean-shutdown marker {}: {e}", path.display());
    }
}

pub(crate) fn restrict_index_permissions(db_file: &Path) {
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::io::AsRawFd;
    let chmod_fd = |fd: i32, mode: u32| {
        if unsafe { libc::fchmod(fd, mode as libc::mode_t) } != 0 {
            let e = std::io::Error::last_os_error();
            warn!("fchmod {mode:04o}: {e}");
        }
    };
    if let Some(dir) = db_file.parent() {
        match fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY | libc::O_CLOEXEC)
            .open(dir)
        {
            Ok(f) => chmod_fd(f.as_raw_fd(), 0o700),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => warn!("open index dir {}: {e}", sanitize_path(dir)),
        }
    }
    for suffix in ["", "-wal", "-shm"] {
        let mut p = db_file.as_os_str().to_owned();
        p.push(suffix);
        let p = Path::new(&p);
        match fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(p)
        {
            Ok(f) => chmod_fd(f.as_raw_fd(), 0o600),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => warn!("open index file {}: {e}", sanitize_path(p)),
        }
    }
}

/// Replace this process with a fresh rfindd (same pid, so systemd keeps
/// tracking it). After an upgrade /proc/self/exe names the deleted old
/// binary; exec the path it was installed at instead.
pub(crate) fn reexec() -> ! {
    use std::os::unix::process::CommandExt;
    let exe = std::env::current_exe()
        .map(|p| {
            let s = p.to_string_lossy();
            PathBuf::from(s.strip_suffix(" (deleted)").unwrap_or(&s).to_string())
        })
        .unwrap_or_else(|_| PathBuf::from("/proc/self/exe"));
    let err = std::process::Command::new(&exe)
        .args(std::env::args_os().skip(1))
        .exec();
    error!("re-exec of {} failed: {err}", exe.display());
    std::process::exit(1);
}

/// Open and `flock(LOCK_EX|LOCK_NB)` the pidfile at `path`, creating the
/// parent dir if needed. On success the returned `File` must stay open
/// for the daemon's lifetime (dropping it releases the lock).
pub(crate) fn acquire_pidfile(path: &Path) -> Result<std::fs::File, String> {
    use std::io::Write;
    use std::os::unix::io::AsRawFd;
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(|e| format!("cannot open pidfile {}: {e}", path.display()))?;
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let e = std::io::Error::last_os_error();
        return Err(format!(
            "another rfindd is already running (pidfile {} is locked: {e})",
            path.display()
        ));
    }
    // Verify the dentry still names the inode we locked. If an earlier
    // instance's pidfile was unlinked (reinstall, admin `rm`, tmpfiles.d
    // reap) and then re-created at the same path, our flock is on a dead
    // inode and the next rfindd would flock the new one — two writers
    // against a shared SQLite WAL and zvfs snapshot. Refuse to start.
    use std::os::unix::fs::MetadataExt;
    let on_disk = std::fs::metadata(path)
        .map_err(|e| format!("cannot stat pidfile {} after flock: {e}", path.display()))?;
    let locked = f
        .metadata()
        .map_err(|e| format!("cannot fstat pidfile {} after flock: {e}", path.display()))?;
    if on_disk.ino() != locked.ino() || on_disk.dev() != locked.dev() {
        return Err(format!(
            "pidfile {} was unlinked/replaced after lock; refusing to start",
            path.display()
        ));
    }
    // Overwrite with our pid.
    if let Err(e) = f.set_len(0) {
        return Err(format!("cannot truncate pidfile {}: {e}", path.display()));
    }
    let _ = (&f).write_all(format!("{}\n", std::process::id()).as_bytes());
    Ok(f)
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn mode(p: &Path) -> u32 {
        fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn restrict_index_permissions_locks_down_dir_and_files() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("index.db");
        for suffix in ["", "-wal", "-shm"] {
            let p = PathBuf::from(format!("{}{}", db.display(), suffix));
            fs::write(&p, b"x").unwrap();
            fs::set_permissions(&p, fs::Permissions::from_mode(0o644)).unwrap();
        }
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).unwrap();

        restrict_index_permissions(&db);

        assert_eq!(mode(dir.path()), 0o700, "parent dir");
        for suffix in ["", "-wal", "-shm"] {
            let p = PathBuf::from(format!("{}{}", db.display(), suffix));
            assert_eq!(mode(&p), 0o600, "{}", p.display());
        }

        // Idempotent: a second call must not widen or error on already-tight modes.
        restrict_index_permissions(&db);
        assert_eq!(mode(dir.path()), 0o700);
        for suffix in ["", "-wal", "-shm"] {
            let p = PathBuf::from(format!("{}{}", db.display(), suffix));
            assert_eq!(mode(&p), 0o600);
        }
    }

    #[test]
    fn restrict_index_permissions_tolerates_missing_wal_shm() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("index.db");
        fs::write(&db, b"x").unwrap();
        fs::set_permissions(&db, fs::Permissions::from_mode(0o644)).unwrap();
        // No -wal, no -shm. Must still tighten what exists.
        restrict_index_permissions(&db);
        assert_eq!(mode(dir.path()), 0o700);
        assert_eq!(mode(&db), 0o600);
    }

    #[test]
    fn clean_marker_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("index.db");
        let m = clean_marker_path(&db);
        assert!(!take_clean_marker(&m));
        write_clean_marker(&m);
        assert!(m.exists());
        assert_eq!(mode(&m), 0o600);
        assert!(take_clean_marker(&m));
        assert!(!m.exists());
        assert!(!take_clean_marker(&m));
    }

    #[test]
    fn clean_marker_lives_next_to_the_db() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("index.db");
        let m = clean_marker_path(&db);
        assert_eq!(m.parent(), db.parent());
        assert_eq!(m.file_name().unwrap(), ".index.db.clean");
    }
}
