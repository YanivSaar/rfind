//! Finding and signalling the running rfindd.

/// PID of the running rfindd. Prefers the daemon's own pidfile at
/// /run/rfindd/rfindd.pid (written while holding an exclusive flock), and
/// falls back to a /proc scan when the pidfile isn't there (older daemon,
/// or /run/rfindd not available).
pub fn rfindd_pid() -> Option<libc::pid_t> {
    if let Ok(s) = std::fs::read_to_string("/run/rfindd/rfindd.pid") {
        if let Ok(pid) = s.trim().parse::<libc::pid_t>() {
            // kill(pid, 0) only checks that the pid exists — if the
            // daemon crashed without clearing the pidfile and the pid
            // was reused (kernel OOM killer, external SIGKILL), we'd
            // signal an unrelated process. In uninstall --purge that
            // signal is SIGKILL as root, which could kill a critical
            // daemon. Verify /proc/<pid>/comm == "rfindd" too.
            if unsafe { libc::kill(pid, 0) } == 0 && is_rfindd(pid) {
                return Some(pid);
            }
        }
    }
    for entry in std::fs::read_dir("/proc").ok()?.flatten() {
        let name = entry.file_name();
        let Ok(pid) = name.to_string_lossy().parse::<libc::pid_t>() else {
            continue;
        };
        if is_rfindd(pid) {
            return Some(pid);
        }
    }
    None
}

fn is_rfindd(pid: libc::pid_t) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .is_ok_and(|c| c.trim() == "rfindd")
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
