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
//! Implementation: userspace emulation of the Unix DAC check against
//! each path's ancestor directories.
//!
//! * **Why not `access(2)`?** access()'s result depends on the
//!   process's real UID/GID; we're running as root. Using
//!   `faccessat2(AT_EACCESS)` with uid changes requires `setfsuid`,
//!   which is process-wide shared state — unsafe across our thread
//!   pool.
//! * **Why not fork a helper?** For a few hundred results, the fork
//!   round-trip would dominate query time. Mode-bit math is pure
//!   arithmetic; one `stat()` per ancestor is already in page cache
//!   for anything the invoking user has touched recently.
//!
//! For each result path we stat each ancestor (/, /a, /a/b, ...) up to
//! but not including the leaf, and check `X` for the invoking user.
//! That matches `find`'s semantics: to list a file you only need
//! executable+search on its parents. ACLs are not honored (rare in
//! practice; adding them means parsing xattrs on every result).

use std::ffi::CString;
use std::mem::MaybeUninit;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

static WARNED_UNUSED: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Clone)]
struct Invoker {
    uid: u32,
    gid: u32,
    /// Supplementary groups. Looked up once via getgrouplist(SUDO_USER)
    /// so we honor group memberships the sudoer actually has, not just
    /// their primary gid.
    groups: Vec<u32>,
}

fn invoker() -> Option<Invoker> {
    let uid: u32 = std::env::var("SUDO_UID").ok()?.parse().ok()?;
    let gid: u32 = std::env::var("SUDO_GID").ok()?.parse().ok()?;
    // Fall through with just the primary gid if we can't resolve
    // supplementary groups — filter stays conservative, which here
    // means over-filtering (hiding paths the user could have reached
    // via a supplementary group). Preferable to the alternative
    // (showing paths they couldn't reach).
    let groups = sudo_user_groups(gid).unwrap_or_else(|| vec![gid]);
    Some(Invoker { uid, gid, groups })
}

/// Resolve SUDO_USER's supplementary groups via libc::getgrouplist.
/// Returns `None` on any error so the caller can fall back cleanly.
fn sudo_user_groups(primary: u32) -> Option<Vec<u32>> {
    let user = std::env::var("SUDO_USER").ok()?;
    let cuser = CString::new(user).ok()?;
    let mut ngroups: libc::c_int = 32;
    loop {
        let mut buf: Vec<libc::gid_t> = vec![0; ngroups as usize];
        let rc = unsafe {
            libc::getgrouplist(
                cuser.as_ptr(),
                primary as libc::gid_t,
                buf.as_mut_ptr(),
                &mut ngroups,
            )
        };
        if rc >= 0 {
            buf.truncate(ngroups as usize);
            return Some(buf.into_iter().map(|g| g as u32).collect());
        }
        // -1 means buf was too small; libc updated ngroups with the
        // needed size. Retry once. If the second call also fails,
        // give up — the system is behaving oddly.
        if ngroups <= buf.len() as libc::c_int {
            return None;
        }
    }
}

pub struct DacFilter {
    inv: Option<Invoker>,
}

impl Default for DacFilter {
    fn default() -> Self {
        Self { inv: None }
    }
}

impl DacFilter {
    /// Build a filter. When the config knob is off, or we are not
    /// running under `sudo`, the filter is a no-op and `keep` returns
    /// true for every path.
    pub fn new(enabled_by_config: bool) -> Self {
        if !enabled_by_config {
            return Self { inv: None };
        }
        let inv = invoker();
        if inv.is_none() && !WARNED_UNUSED.swap(true, Ordering::Relaxed) {
            eprintln!(
                "rfind: search.filter_by_dac is set but SUDO_UID is not — \
                 running directly, no filtering needed."
            );
        }
        Self { inv }
    }

    /// Should the caller emit this path?
    pub fn keep(&self, path: &Path) -> bool {
        let Some(inv) = &self.inv else {
            return true;
        };
        // root can always traverse; filtering as root is a no-op by
        // definition. Catches `sudo -u root sudo rfind …` edge cases.
        if inv.uid == 0 {
            return true;
        }
        user_can_reach(path, inv)
    }
}

/// True iff `inv` has X permission on every ancestor of `path`. Does
/// not check anything on `path` itself: `find` lists a file whose
/// parents are traversable, regardless of the file's own mode.
fn user_can_reach(path: &Path, inv: &Invoker) -> bool {
    // Walk ancestors from the root down. `Path::ancestors()` yields
    // from the leaf up to "/"; collect and reverse so a missing X
    // high in the tree short-circuits quickly.
    let mut ancestors: Vec<&Path> = path.ancestors().collect();
    ancestors.reverse();
    for anc in ancestors.iter().take(ancestors.len().saturating_sub(1)) {
        if anc.as_os_str().is_empty() {
            continue;
        }
        match stat_mode_owner(anc) {
            Some((mode, uid, gid)) => {
                if !has_perm(mode, uid, gid, inv, 0o1) {
                    return false;
                }
            }
            None => return false, // can't stat — treat as unreachable
        }
    }
    true
}

/// Stat `path` and return (mode, uid, gid), or None on error.
fn stat_mode_owner(path: &Path) -> Option<(u32, u32, u32)> {
    let c = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut st: MaybeUninit<libc::stat> = MaybeUninit::uninit();
    let rc = unsafe { libc::lstat(c.as_ptr(), st.as_mut_ptr()) };
    if rc != 0 {
        return None;
    }
    let st = unsafe { st.assume_init() };
    // Follow symlinks for directories (we're checking traversal), but
    // avoid chasing symlinks that could race. lstat gets the link
    // itself; a symlink in the middle of a path is traversed by the
    // kernel if we had use access(). For simplicity we approximate:
    // use stat (not lstat) so a symlinked dir resolves to its target.
    let mut st_follow: MaybeUninit<libc::stat> = MaybeUninit::uninit();
    let rc2 = unsafe { libc::stat(c.as_ptr(), st_follow.as_mut_ptr()) };
    if rc2 == 0 {
        let s = unsafe { st_follow.assume_init() };
        return Some((s.st_mode as u32, s.st_uid, s.st_gid));
    }
    Some((st.st_mode as u32, st.st_uid, st.st_gid))
}

/// Mode-bit check: does `inv` have `want_bits` (R=4, W=2, X=1) on
/// an object with the given (mode, uid, gid)?
fn has_perm(mode: u32, uid: u32, gid: u32, inv: &Invoker, want_bits: u32) -> bool {
    let bits = if inv.uid == uid {
        (mode >> 6) & 0o7
    } else if inv.gid == gid || inv.groups.contains(&gid) {
        (mode >> 3) & 0o7
    } else {
        mode & 0o7
    };
    bits & want_bits == want_bits
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    fn mk_inv(uid: u32, gid: u32) -> Invoker {
        Invoker { uid, gid, groups: vec![gid] }
    }

    #[test]
    fn disabled_passes_everything() {
        let f = DacFilter::new(false);
        assert!(f.keep(Path::new("/etc/shadow")));
    }

    #[test]
    fn not_under_sudo_passes_everything() {
        unsafe {
            std::env::remove_var("SUDO_UID");
            std::env::remove_var("SUDO_GID");
        }
        let f = DacFilter::new(true);
        assert!(f.keep(Path::new("/etc/shadow")));
    }

    #[test]
    fn has_perm_owner_group_other() {
        let inv = mk_inv(1000, 1000);
        // Owned by invoker, 0700
        assert!(has_perm(0o040700, 1000, 1000, &inv, 0o1));
        // Owned by someone else, group 1000, 0710 (X for group)
        assert!(has_perm(0o040710, 999, 1000, &inv, 0o1));
        // Owned by someone else, world 0711 (X for other)
        assert!(has_perm(0o040711, 999, 999, &inv, 0o1));
        // Owned by someone else, 0700 — no permission
        assert!(!has_perm(0o040700, 999, 999, &inv, 0o1));
    }

    #[test]
    fn user_can_reach_rejects_a_blocking_parent() {
        // Build a tree /tmp/<x>/private(0700,uid=999)/leaf
        let td = TempDir::new().unwrap();
        let private = td.path().join("private");
        std::fs::create_dir(&private).unwrap();
        let leaf = private.join("leaf");
        std::fs::write(&leaf, b"x").unwrap();
        std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o700)).unwrap();
        // Simulate "another user" by picking a non-current uid.
        // We're running as root in tests sometimes; use uid=0 for the
        // invoker since it's always root-reachable (early-return in
        // keep()). To actually exercise the deny branch, use a fake
        // invoker whose uid isn't the owner of `private`.
        let private_meta = std::fs::metadata(&private).unwrap();
        use std::os::unix::fs::MetadataExt;
        let owner_uid = private_meta.uid();
        let other_uid = if owner_uid == 1234 { 5678 } else { 1234 };
        let inv = mk_inv(other_uid, 9999);
        assert!(!user_can_reach(&leaf, &inv));
    }

    #[test]
    fn user_can_reach_allows_world_x_tree() {
        let td = TempDir::new().unwrap();
        let open = td.path().join("open");
        std::fs::create_dir(&open).unwrap();
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o755)).unwrap();
        let leaf = open.join("leaf");
        std::fs::write(&leaf, b"x").unwrap();
        let inv = mk_inv(9999, 9999);
        // May still fail if /tmp isn't 755 under this test env;
        // instead target our own tree whose ancestors we just made.
        // The temp dir default is 0700 on Linux — set it to 0755 for
        // the test.
        std::fs::set_permissions(td.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(user_can_reach(&leaf, &inv));
    }
}
