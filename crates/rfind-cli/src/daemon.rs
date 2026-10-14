//! Finding and signalling the running rfindd.

/// PID of the running rfindd. Prefers the daemon's own pidfile at
/// /run/rfindd/rfindd.pid (written while holding an exclusive flock), and
/// falls back to a /proc scan when the pidfile isn't there (older daemon,
/// or /run/rfindd not available).
pub fn rfindd_pid() -> Option<libc::pid_t> {
    if let Ok(s) = std::fs::read_to_string("/run/rfindd/rfindd.pid") {
        if let Ok(pid) = s.trim().parse::<libc::pid_t>() {
            // Verify the process is actually alive.
            if unsafe { libc::kill(pid, 0) } == 0 {
                return Some(pid);
            }
        }
    }
    for entry in std::fs::read_dir("/proc").ok()?.flatten() {
        let name = entry.file_name();
        let Ok(pid) = name.to_string_lossy().parse::<libc::pid_t>() else {
            continue;
        };
        if std::fs::read_to_string(entry.path().join("comm")).is_ok_and(|c| c.trim() == "rfindd") {
            return Some(pid);
        }
    }
    None
}

/// Send `sig` to rfindd. Returns its pid.
pub fn signal(sig: libc::c_int) -> Result<libc::pid_t, String> {
    let pid =
        rfindd_pid().ok_or("rfindd is not running (start it: sudo systemctl start rfindd)")?;
    if unsafe { libc::kill(pid, sig) } != 0 {
        return Err(format!(
            "cannot signal rfindd (pid {pid}): {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(pid)
}
