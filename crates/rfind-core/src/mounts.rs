//! Mount table (/proc/self/mountinfo) parsing and classification.
//!
//! Used by the daemon to skip bind-mount aliases and warn about Windows
//! drives, and to choose which filesystems to mark for fanotify.

use std::path::{Path, PathBuf};

/// One line of /proc/self/mountinfo.
#[derive(Debug, Clone, PartialEq)]
pub struct MountEntry {
    /// "major:minor" of the filesystem.
    pub dev: String,
    /// Directory of the filesystem shown at this mount (bind mounts of a
    /// subdirectory have a non-"/" root).
    pub fsroot: PathBuf,
    pub point: PathBuf,
    pub fstype: String,
    /// Per-superblock options (e.g. "aname=drvfs;path=C:\" for WSL2 drives).
    pub super_options: String,
}

impl MountEntry {
    /// WSL2 Windows drive (drvfs over 9p).
    pub fn is_drvfs(&self) -> bool {
        self.fstype == "drvfs"
            || (self.fstype == "9p" && self.super_options.contains("aname=drvfs"))
    }

    /// Kernel / runtime pseudo filesystems: no user files worth monitoring.
    pub fn is_pseudo(&self) -> bool {
        matches!(
            self.fstype.as_str(),
            "proc"
                | "sysfs"
                | "devtmpfs"
                | "devpts"
                | "cgroup"
                | "cgroup2"
                | "debugfs"
                | "tracefs"
                | "securityfs"
                | "pstore"
                | "bpf"
                | "configfs"
                | "fusectl"
                | "mqueue"
                | "hugetlbfs"
                | "autofs"
                | "binfmt_misc"
                | "efivarfs"
                | "rpc_pipefs"
                | "nsfs"
                | "selinuxfs"
        )
    }

    /// Filesystems whose changes fanotify can't see or mark reliably:
    /// network / remote (only local changes would be reported), overlay
    /// (container layers) and read-only images.
    pub fn is_unmonitorable(&self) -> bool {
        matches!(
            self.fstype.as_str(),
            "9p" | "drvfs"
                | "virtiofs"
                | "nfs"
                | "nfs4"
                | "cifs"
                | "smb3"
                | "ceph"
                | "glusterfs"
                | "overlay"
                | "squashfs"
                | "erofs"
                | "iso9660"
                | "udf"
        ) || self.fstype.starts_with("fuse")
    }

    /// Filesystems where mmapped page eviction can SIGBUS the daemon
    /// (virtio/9p/drvfs/virtiofs under memory pressure). Scan paths must
    /// use one-shot statx, not io_uring batched statx, when under these.
    pub fn is_mmap_fragile(&self) -> bool {
        matches!(self.fstype.as_str(), "9p" | "drvfs" | "virtiofs")
    }

    /// How this mount should be treated by the indexing + monitoring
    /// pipeline. A single enum per mount is clearer than the mix of
    /// is_pseudo/is_unmonitorable/is_local_block booleans and gives
    /// each callsite one place to decide.
    pub fn index_strategy(&self) -> IndexStrategy {
        if self.is_pseudo() {
            return IndexStrategy::Skip;
        }
        if self.is_unmonitorable() {
            return IndexStrategy::PeriodicRescan;
        }
        // eCryptfs is stacked FUSE-ish: event delivery is unreliable
        // across the stack but a one-shot walk of the mounted view
        // works. Treat like a periodic root.
        if self.fstype == "ecryptfs" {
            return IndexStrategy::PeriodicRescan;
        }
        if self.is_local_block_fs() {
            return IndexStrategy::FanotifyLive;
        }
        IndexStrategy::InotifyLive
    }
}

/// Per-filesystem indexing + monitoring strategy. One enum variant
/// per real behavior; the daemon's main loop picks branches from this
/// instead of a mix of booleans.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexStrategy {
    /// Attach a fanotify FS mark and expect live events.
    FanotifyLive,
    /// Watch directories with inotify (per-dir watches).
    InotifyLive,
    /// Walk once at startup and again on scan.reindex_interval_hours.
    /// No live events; the operator opted into stale-for-a-few-hours.
    PeriodicRescan,
    /// Walk once at startup, never again. For read-only images (ISOs,
    /// squashfs mounted at a stable path).
    OnceAtStartup,
    /// Pseudo-fs, excluded, or otherwise ignored.
    Skip,
}

impl IndexStrategy {
    pub fn walks_at_startup(self) -> bool {
        matches!(
            self,
            IndexStrategy::FanotifyLive
                | IndexStrategy::InotifyLive
                | IndexStrategy::PeriodicRescan
                | IndexStrategy::OnceAtStartup
        )
    }

    pub fn uses_fanotify(self) -> bool {
        matches!(self, IndexStrategy::FanotifyLive)
    }
}

impl MountEntry {
    /// Allowlist of local, block-device-backed filesystems that are safe
    /// to auto-index when a new mount appears under a live root. We used
    /// to rely on a FUSE-family denylist, but unprivileged users with
    /// user-namespaces can mount tmpfs/ramfs/ext*-via-loop too — and a
    /// root-owned walk of attacker-controlled contents is the whole thing
    /// we are trying to avoid. This allowlist is intentionally narrow;
    /// everything else must be opted in explicitly via `scan.periodic_roots`.
    pub fn is_local_block_fs(&self) -> bool {
        matches!(
            self.fstype.as_str(),
            "ext2"
                | "ext3"
                | "ext4"
                | "xfs"
                | "btrfs"
                | "f2fs"
                | "zfs"
                | "bcachefs"
                | "jfs"
                | "reiserfs"
                | "exfat"
                | "vfat"
                | "ntfs"
                | "ntfs3"
        )
    }
}

pub fn read_mountinfo() -> Vec<MountEntry> {
    std::fs::read_to_string("/proc/self/mountinfo")
        .map(|t| parse_mountinfo(&t))
        .unwrap_or_default()
}

/// Parse mountinfo text. Fields: `id parent maj:min root point opts
/// [optional...] - fstype source super_opts`; octal escapes (`\040` =
/// space) are decoded in root and point.
pub fn parse_mountinfo(text: &str) -> Vec<MountEntry> {
    text.lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split(' ').collect();
            let sep = f.iter().position(|x| *x == "-")?;
            (sep >= 6 && f.len() > sep + 1).then(|| MountEntry {
                dev: f[2].to_string(),
                fsroot: PathBuf::from(unescape(f[3])),
                point: PathBuf::from(unescape(f[4])),
                fstype: f[sep + 1].to_string(),
                super_options: f.get(sep + 3).unwrap_or(&"").to_string(),
            })
        })
        .collect()
}

fn unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        // Operate on &[u8], not &str: slicing &s[i+1..i+4] can panic
        // if the window straddles a UTF-8 continuation byte (seen on
        // FUSE and user-namespace mounts with unusual labels).
        if b[i] == b'\\' && i + 3 < b.len() {
            if let Ok(digits) = std::str::from_utf8(&b[i + 1..i + 4]) {
                if let Ok(v) = u8::from_str_radix(digits, 8) {
                    out.push(v);
                    i += 4;
                    continue;
                }
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Mount points whose content is already reachable through another mount
/// of the same filesystem (bind mounts, e.g. WSL2's /mnt/wslg/distro is a
/// second view of /), each paired with the canonical path of that content.
/// Indexing both would duplicate every entry, and inotify returns one
/// watch per inode, so events would be attributed to whichever path
/// registered last. find and updatedb skip these as filesystem loops.
///
/// For each filesystem the canonical mount is the shallowest mount point
/// whose fsroot contains the other's; ties go to the earlier mount.
pub fn alias_mounts(mounts: &[MountEntry]) -> Vec<(PathBuf, PathBuf)> {
    let depth = |p: &Path| p.components().count();
    let mut aliases = Vec::new();
    for (i, b) in mounts.iter().enumerate() {
        let canonical = mounts
            .iter()
            .enumerate()
            .filter(|(j, a)| {
                *j != i
                    && a.dev == b.dev
                    && b.fsroot.starts_with(&a.fsroot)
                    && a.point.join(b.fsroot.strip_prefix(&a.fsroot).unwrap()) != b.point
                    && !a.point.starts_with(&b.point)
                    && (depth(&a.point), *j) < (depth(&b.point), i)
            })
            .min_by_key(|(j, a)| (depth(&a.point), *j));
        if let Some((_, a)) = canonical {
            let rel = b.fsroot.strip_prefix(&a.fsroot).unwrap();
            let target = if rel.as_os_str().is_empty() {
                a.point.clone()
            } else {
                a.point.join(rel)
            };
            aliases.push((b.point.clone(), target));
        }
    }
    aliases
}

/// One mount point per filesystem to mark with FAN_MARK_FILESYSTEM so that
/// every live root is covered: the mount containing each root plus every
/// mount beneath it, minus excluded, pseudo and unmonitorable ones.
/// Returns (mark points, skipped unmonitorable mounts under live roots).
pub fn fanotify_mark_points(
    mounts: &[MountEntry],
    roots: &[PathBuf],
    excludes: &[PathBuf],
) -> (Vec<PathBuf>, Vec<MountEntry>) {
    let excluded = |p: &Path| excludes.iter().any(|ex| p.starts_with(ex));
    // The mount containing a root is the deepest mount point above it.
    let containing = |root: &Path| {
        mounts
            .iter()
            .filter(|m| root.starts_with(&m.point))
            .max_by_key(|m| m.point.components().count())
    };
    let mut wanted: Vec<&MountEntry> = Vec::new();
    for root in roots {
        if let Some(m) = containing(root) {
            wanted.push(m);
        }
        wanted.extend(
            mounts
                .iter()
                .filter(|m| m.point.starts_with(root) && m.point != *root),
        );
    }
    let mut points = Vec::new();
    let mut seen_devs: Vec<&str> = Vec::new();
    let mut skipped = Vec::new();
    for m in wanted {
        let is_root_mount = roots.iter().any(|r| r.starts_with(&m.point));
        if m.is_pseudo() || (!is_root_mount && excluded(&m.point)) {
            continue;
        }
        if m.is_unmonitorable() {
            if !skipped.contains(m) {
                skipped.push(m.clone());
            }
            continue;
        }
        if seen_devs.contains(&m.dev.as_str()) {
            continue;
        }
        seen_devs.push(&m.dev);
        points.push(m.point.clone());
    }
    (points, skipped)
}

#[cfg(test)]
mod tests {
    use super::*;

    const WSL: &str = "\
24 1 8:48 / / rw,relatime - ext4 /dev/sdd rw
30 24 0:40 / /mnt/wslg rw - tmpfs none rw
31 30 8:48 / /mnt/wslg/distro rw - ext4 /dev/sdd rw
40 24 0:70 / /mnt/c rw,noatime - 9p drvfs rw,aname=drvfs;path=C:\\;uid=1000
41 24 0:61 / /proc rw - proc proc rw
42 24 0:73 / /tmp rw - tmpfs tmpfs rw
43 24 0:57 / /usr/lib/wsl/lib ro - overlay none rw
50 24 0:88 / /run/user/1000 rw - tmpfs none rw
51 30 0:88 / /mnt/wslg/run/user/1000 rw - tmpfs none rw
";

    fn aliases(mi: &str) -> Vec<(String, String)> {
        alias_mounts(&parse_mountinfo(mi))
            .into_iter()
            .map(|(a, c)| (a.display().to_string(), c.display().to_string()))
            .collect()
    }

    #[test]
    fn unescape_does_not_panic_on_multibyte_boundary() {
        // Octal escape sequence "\0a" followed by a 3-byte UTF-8
        // character — the window s[i+1..i+4] would straddle the
        // multibyte boundary and panic with the old str-slicing code.
        // The incomplete escape should be left alone and the rest
        // passed through.
        let input = "\\0中";
        let out = unescape(input);
        assert!(out.contains('中'));
    }

    #[test]
    fn unescape_decodes_valid_octal() {
        // "\040" is a space (0o40 = 32 = ' ').
        assert_eq!(unescape(r"foo\040bar"), "foo bar");
    }

    #[test]
    fn parses_fstype_and_super_options() {
        let m = parse_mountinfo(WSL);
        let c = m.iter().find(|m| m.point == Path::new("/mnt/c")).unwrap();
        assert_eq!(c.fstype, "9p");
        assert!(c.is_drvfs());
        assert!(!m[0].is_drvfs());
    }

    #[test]
    fn optional_fields_before_separator_are_skipped() {
        let m = parse_mountinfo("1 0 8:1 / / rw shared:1 master:2 - ext4 /dev/sda1 rw\n");
        assert_eq!(
            (m[0].fstype.as_str(), m[0].super_options.as_str()),
            ("ext4", "rw")
        );
    }

    #[test]
    fn wsl_distro_bind_of_root_is_an_alias() {
        assert_eq!(
            aliases(WSL),
            vec![
                ("/mnt/wslg/distro".into(), "/".into()),
                ("/mnt/wslg/run/user/1000".into(), "/run/user/1000".into()),
            ]
        );
    }

    #[test]
    fn subdirectory_bind_mount_maps_to_canonical_path() {
        let mi = "\
1 0 8:1 / / rw - ext4 /dev/sda1 rw
2 1 8:1 /home/app/data /srv/data rw - ext4 /dev/sda1 rw
";
        assert_eq!(
            aliases(mi),
            vec![("/srv/data".into(), "/home/app/data".into())]
        );
    }

    #[test]
    fn separate_filesystems_and_self_nesting_are_not_aliases() {
        let mi = "\
1 0 8:1 / / rw - ext4 /dev/sda1 rw
2 1 8:2 / /home rw - ext4 /dev/sda2 rw
3 1 8:1 /var /var rw - ext4 /dev/sda1 rw
";
        assert!(aliases(mi).is_empty());
    }

    #[test]
    fn escaped_mount_points_are_decoded() {
        let mi = "\
1 0 8:1 / / rw - ext4 /dev/sda1 rw
2 1 8:1 / /mnt/my\\040disk rw - ext4 /dev/sda1 rw
";
        assert_eq!(aliases(mi), vec![("/mnt/my disk".into(), "/".into())]);
    }

    #[test]
    fn mark_points_cover_each_local_filesystem_once() {
        let m = parse_mountinfo(WSL);
        // As in the daemon: configured excludes plus every alias mount.
        let mut excludes: Vec<PathBuf> = ["/proc", "/run", "/mnt/c"]
            .iter()
            .map(PathBuf::from)
            .collect();
        excludes.extend(alias_mounts(&m).into_iter().map(|(alias, _)| alias));
        let (points, skipped) = fanotify_mark_points(&m, &[PathBuf::from("/")], &excludes);
        assert_eq!(
            points,
            ["/", "/mnt/wslg", "/tmp"]
                .iter()
                .map(PathBuf::from)
                .collect::<Vec<_>>()
        );
        // The overlay is reported as not monitorable; excluded /mnt/c is not.
        assert_eq!(
            skipped.iter().map(|m| m.point.clone()).collect::<Vec<_>>(),
            vec![PathBuf::from("/usr/lib/wsl/lib")]
        );
    }

    #[test]
    fn index_strategy_classifies_known_fstypes() {
        let mk = |fstype: &str| MountEntry {
            dev: "0:1".into(),
            fsroot: PathBuf::from("/"),
            point: PathBuf::from("/x"),
            fstype: fstype.into(),
            super_options: String::new(),
        };
        assert_eq!(mk("ext4").index_strategy(), IndexStrategy::FanotifyLive);
        assert_eq!(mk("btrfs").index_strategy(), IndexStrategy::FanotifyLive);
        assert_eq!(mk("tmpfs").index_strategy(), IndexStrategy::InotifyLive);
        assert_eq!(mk("virtiofs").index_strategy(), IndexStrategy::PeriodicRescan);
        assert_eq!(mk("nfs4").index_strategy(), IndexStrategy::PeriodicRescan);
        assert_eq!(mk("fuse.sshfs").index_strategy(), IndexStrategy::PeriodicRescan);
        assert_eq!(mk("ecryptfs").index_strategy(), IndexStrategy::PeriodicRescan);
        assert_eq!(mk("proc").index_strategy(), IndexStrategy::Skip);
        assert_eq!(mk("sysfs").index_strategy(), IndexStrategy::Skip);
        assert!(mk("ext4").index_strategy().uses_fanotify());
        assert!(!mk("tmpfs").index_strategy().uses_fanotify());
        assert!(!mk("proc").index_strategy().walks_at_startup());
    }

    #[test]
    fn mark_points_for_a_root_inside_a_mount() {
        let m = parse_mountinfo(
            "1 0 8:1 / / rw - ext4 /dev/sda1 rw\n2 1 8:2 / /home rw - xfs /dev/sda2 rw\n",
        );
        let (points, _) = fanotify_mark_points(&m, &[PathBuf::from("/home/app")], &[]);
        assert_eq!(points, vec![PathBuf::from("/home")]);
    }
}
