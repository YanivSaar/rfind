//! Runtime environment detection.
//!
//! The daemon is written for the host of a Linux box but is reached
//! for in enough other places — containers, WSL2, LXC, Kubernetes —
//! that silently retrying a fanotify filesystem mark that will never
//! work is a bad default. This module answers two questions once at
//! startup and lets the rest of the daemon branch on the answer:
//!
//! 1. What runtime are we in?  (host, Docker, Podman, LXC, …)
//! 2. Do we have the capabilities fanotify FS marks need?  (CAP_SYS_ADMIN
//!    AND CAP_DAC_READ_SEARCH, in the init user namespace.)
//!
//! Negative answers to #2 mean we should go straight to inotify and
//! stop logging "fanotify_mark EPERM" per filesystem.
//!
//! Nothing here reaches outside /proc or two files that containers
//! create. Reads are best-effort; "unknown" is a valid result and the
//! caller treats it as "assume host".

use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // `Unknown` is reserved; detect_runtime defaults to Host.
pub enum Runtime {
    Host,
    Docker,
    Podman,
    Lxc,
    Kubernetes,
    SystemdNspawn,
    Wsl1,
    Wsl2,
    Unknown,
}

impl Runtime {
    pub fn name(self) -> &'static str {
        match self {
            Runtime::Host => "host",
            Runtime::Docker => "docker",
            Runtime::Podman => "podman",
            Runtime::Lxc => "lxc",
            Runtime::Kubernetes => "kubernetes",
            Runtime::SystemdNspawn => "systemd-nspawn",
            Runtime::Wsl1 => "wsl1",
            Runtime::Wsl2 => "wsl2",
            Runtime::Unknown => "unknown",
        }
    }

    /// Fanotify filesystem marks require CAP_SYS_ADMIN in the *init*
    /// user namespace. Any userns-based container (rootless Podman,
    /// Kubernetes without privileged, LXC unprivileged) cannot do this
    /// even if the process is uid 0 inside its namespace.
    pub fn fanotify_fs_marks_usually_work(self) -> bool {
        matches!(self, Runtime::Host | Runtime::Wsl2 | Runtime::SystemdNspawn)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Capabilities {
    pub cap_sys_admin: bool,
    pub cap_dac_read_search: bool,
    /// True when we are in a user namespace that is NOT the init one.
    /// Any CAP_* we hold in it does not count for fanotify FS marks.
    pub in_userns: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct RuntimeEnv {
    pub runtime: Runtime,
    pub caps: Capabilities,
}

impl RuntimeEnv {
    pub fn detect() -> Self {
        Self {
            runtime: detect_runtime(),
            caps: detect_capabilities(),
        }
    }

    /// True when we should skip fanotify FS marks and go straight to
    /// inotify — either the runtime cannot grant them, or we lack the
    /// caps, or we're in a non-init userns.
    pub fn should_skip_fanotify(&self) -> bool {
        !self.runtime.fanotify_fs_marks_usually_work()
            || !self.caps.cap_sys_admin
            || !self.caps.cap_dac_read_search
            || self.caps.in_userns
    }

    pub fn log_summary(&self) {
        tracing::info!(
            "Runtime: {} | CAP_SYS_ADMIN={} CAP_DAC_READ_SEARCH={} userns={} \
             | fanotify FS marks: {}",
            self.runtime.name(),
            self.caps.cap_sys_admin,
            self.caps.cap_dac_read_search,
            self.caps.in_userns,
            if self.should_skip_fanotify() {
                "skip (will use inotify)"
            } else {
                "attempt"
            },
        );
    }
}

fn detect_runtime() -> Runtime {
    // WSL announces itself in /proc/version. Order matters: WSL2 strings
    // match "microsoft" too, so check WSL2 first.
    if let Ok(v) = fs::read_to_string("/proc/version") {
        let v = v.to_ascii_lowercase();
        if v.contains("microsoft") {
            // WSL2 ships a real Linux kernel (6.x+), WSL1 translates
            // syscalls to NT and reports an older kernel string.
            if v.contains("wsl2") || v.contains("microsoft-standard") {
                return Runtime::Wsl2;
            }
            return Runtime::Wsl1;
        }
    }

    // k8s is just a user of some other runtime (containerd/CRI-O), but
    // its env is distinctive enough that we want to call it out in
    // telemetry rather than hiding it under "docker".
    if std::env::var_os("KUBERNETES_SERVICE_HOST").is_some() {
        return Runtime::Kubernetes;
    }

    // Container runtimes drop their own marker file.
    if Path::new("/.dockerenv").exists() {
        return Runtime::Docker;
    }
    if Path::new("/run/.containerenv").exists() {
        return Runtime::Podman;
    }

    // Fall back to /proc/1/cgroup inspection. Docker/Podman/LXC all
    // leave distinctive prefixes; systemd-nspawn sets the "machine"
    // slice path.
    if let Ok(cg) = fs::read_to_string("/proc/1/cgroup") {
        if cg.contains("docker") || cg.contains("/moby/") {
            return Runtime::Docker;
        }
        if cg.contains("libpod") || cg.contains("podman") {
            return Runtime::Podman;
        }
        if cg.contains("lxc") {
            return Runtime::Lxc;
        }
        if cg.contains("machine.slice") || cg.contains("systemd-nspawn") {
            return Runtime::SystemdNspawn;
        }
    }

    // Last resort: /proc/1/sched's first line is "<comm> (<pid>, …)".
    // On a container, pid 1 is usually the app, not systemd/init, which
    // is a soft hint but we won't invent a Runtime variant for it.
    Runtime::Host
}

fn detect_capabilities() -> Capabilities {
    // Parse /proc/self/status for CapEff and the UID map; both are
    // ASCII and small enough to read whole.
    let status = fs::read_to_string("/proc/self/status").unwrap_or_default();
    let mut cap_eff: u64 = 0;
    for line in status.lines() {
        if let Some(hex) = line.strip_prefix("CapEff:") {
            cap_eff = u64::from_str_radix(hex.trim(), 16).unwrap_or(0);
            break;
        }
    }
    // Capability bit numbers (include/uapi/linux/capability.h):
    //   CAP_DAC_READ_SEARCH = 2
    //   CAP_SYS_ADMIN       = 21
    let cap_dac_read_search = (cap_eff & (1 << 2)) != 0;
    let cap_sys_admin = (cap_eff & (1 << 21)) != 0;

    // Detect "we are inside a user namespace that is not init". The
    // init userns always maps uid 0..4294967295 identity: a non-init
    // userns remaps some range. The cheapest reliable signal is
    // comparing /proc/self/uid_map against the known init form.
    let in_userns = match fs::read_to_string("/proc/self/uid_map") {
        Ok(s) => {
            // Init userns map on a modern kernel is "         0          0 4294967295".
            // Anything else (empty, truncated, non-identity) is a userns.
            s.split_whitespace().collect::<Vec<_>>() != ["0", "0", "4294967295"]
        }
        // Pre-userns kernel or unreadable: assume init.
        Err(_) => false,
    };

    Capabilities {
        cap_sys_admin,
        cap_dac_read_search,
        in_userns,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_names_are_unique() {
        let all = [
            Runtime::Host,
            Runtime::Docker,
            Runtime::Podman,
            Runtime::Lxc,
            Runtime::Kubernetes,
            Runtime::SystemdNspawn,
            Runtime::Wsl1,
            Runtime::Wsl2,
            Runtime::Unknown,
        ];
        let names: Vec<&str> = all.iter().map(|r| r.name()).collect();
        let mut sorted = names.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(names.len(), sorted.len(), "duplicate runtime name");
    }

    #[test]
    fn host_runtime_allows_fanotify() {
        assert!(Runtime::Host.fanotify_fs_marks_usually_work());
        assert!(Runtime::Wsl2.fanotify_fs_marks_usually_work());
    }

    #[test]
    fn container_runtimes_block_fanotify() {
        assert!(!Runtime::Docker.fanotify_fs_marks_usually_work());
        assert!(!Runtime::Podman.fanotify_fs_marks_usually_work());
        assert!(!Runtime::Lxc.fanotify_fs_marks_usually_work());
        assert!(!Runtime::Kubernetes.fanotify_fs_marks_usually_work());
    }

    #[test]
    fn should_skip_fanotify_when_capabilities_missing() {
        let env = RuntimeEnv {
            runtime: Runtime::Host,
            caps: Capabilities {
                cap_sys_admin: false,
                cap_dac_read_search: true,
                in_userns: false,
            },
        };
        assert!(env.should_skip_fanotify());
    }

    #[test]
    fn should_skip_fanotify_in_userns() {
        let env = RuntimeEnv {
            runtime: Runtime::Host,
            caps: Capabilities {
                cap_sys_admin: true,
                cap_dac_read_search: true,
                in_userns: true,
            },
        };
        assert!(env.should_skip_fanotify());
    }

    #[test]
    fn host_with_full_caps_attempts_fanotify() {
        let env = RuntimeEnv {
            runtime: Runtime::Host,
            caps: Capabilities {
                cap_sys_admin: true,
                cap_dac_read_search: true,
                in_userns: false,
            },
        };
        assert!(!env.should_skip_fanotify());
    }
}
