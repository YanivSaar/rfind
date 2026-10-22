//! DAC filtering of search results for `sudo rfind` on shared boxes.
//!
//! On multi-user systems, `sudo rfind` returns every path on the box
//! — including paths inside other users' homes that the invoking
//! user's DAC permissions would normally forbid listing. That is
//! intentional: see `docs/threat-model.md § 6a`. The
//! `search.filter_by_dac` config knob lets operators restrict
//! `sudo rfind` to "what *the invoking user* could have found with
//! their own `find`", by re-stating each result as that user before
//! emitting it.
//!
//! ## Where we are
//!
//! This module ships the config knob, the threat-model doc, and the
//! API surface — [`DacFilter::new`], [`DacFilter::keep`]. The
//! kernel-side implementation is deferred to a follow-up CL (see
//! issue #TBD) because it needs careful handling: `setfsuid` is
//! process-wide state in Linux, and threading access checks through
//! a per-request fork introduces latency we want to measure before
//! committing to.
//!
//! Until that lands, `DacFilter::new` returns a filter that passes
//! everything through and logs a one-time stderr warning when the
//! operator enables the knob, so nobody is surprised to get fewer
//! results than they expected — because today, they get all of them.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

static WARNED: AtomicBool = AtomicBool::new(false);

/// Resolve the invoking user from the environment that `sudo` sets
/// (`SUDO_UID` / `SUDO_GID`). Returns `None` when `rfind` was not
/// launched via `sudo` — in which case there is nothing to filter
/// against: the caller IS the user.
fn invoking_user() -> Option<(u32, u32)> {
    let uid: u32 = std::env::var("SUDO_UID").ok()?.parse().ok()?;
    let gid: u32 = std::env::var("SUDO_GID").ok()?.parse().ok()?;
    // sudo-to-root with no user drop (sudo -u root) still sets SUDO_UID
    // to the caller's real uid, so we don't short-circuit on uid==0.
    Some((uid, gid))
}

pub struct DacFilter {
    enabled: bool,
}

#[allow(dead_code)] // Wired to search command in a follow-up CL; this
                   // ships the module + config knob + doc first.
impl DacFilter {
    /// Build a filter. When the config knob is off, or we are not
    /// running under `sudo`, the filter is a no-op and `keep` returns
    /// true for every path.
    pub fn new(enabled_by_config: bool) -> Self {
        let under_sudo = invoking_user().is_some();
        let enabled = enabled_by_config && under_sudo;
        if enabled && !WARNED.swap(true, Ordering::Relaxed) {
            eprintln!(
                "rfind: search.filter_by_dac is enabled, but the \
                 kernel-side filtering is not yet implemented. Every \
                 result is being passed through. See docs/threat-model.md \
                 § 6a for status."
            );
        }
        Self { enabled }
    }

    /// Should the caller emit this path?
    ///
    /// Today: always true. The config knob is honored (we warned on
    /// construction); the implementation hook is below.
    pub fn keep(&self, _path: &Path) -> bool {
        if !self.enabled {
            return true;
        }
        // TODO: setfsuid(invoking_user().0) in a dedicated worker
        // thread, faccessat(AT_FDCWD, path, F_OK, AT_EACCESS), restore.
        // Ancestor-x check: open each ancestor O_PATH|O_NOFOLLOW,
        // faccessat(X_OK) through the open fd. Tracked in a follow-up
        // because process-wide setfsuid interacts with any concurrent
        // index read; we may end up with a forked helper instead.
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_passes_everything() {
        let f = DacFilter::new(false);
        assert!(f.keep(Path::new("/etc/shadow")));
    }

    #[test]
    fn not_under_sudo_passes_everything() {
        // Clear SUDO_UID so the filter sees "not under sudo" even if
        // the test host actually is running under sudo.
        // SAFETY: single-threaded unit test.
        unsafe {
            std::env::remove_var("SUDO_UID");
            std::env::remove_var("SUDO_GID");
        }
        let f = DacFilter::new(true);
        assert!(f.keep(Path::new("/etc/shadow")));
    }
}
