# SELinux policy for rfindd

Ships a dedicated `rfindd_t` domain so rfindd can run under
`targeted` policy enforcing (RHEL, Rocky, Alma, Fedora, CentOS Stream)
without being left as `unconfined_service_t`. See
[`../../docs/threat-model.md`](../../docs/threat-model.md) for why
the daemon needs system-wide read + fanotify FS marks + handle
resolution and nothing more.

## Build

Needs `selinux-policy-devel` (RHEL/Fedora) or `selinux-policy-dev`
(SUSE):

```sh
make -f /usr/share/selinux/devel/Makefile rfindd.pp
```

That produces `rfindd.pp` from `rfindd.te` + `rfindd.fc` +
`rfindd.if`. The three source files already live in this directory
and are what `install.sh` ships.

## Install

```sh
semodule -i rfindd.pp
restorecon -R /usr/local/bin/rfindd /etc/rfindd /var/lib/rfindd /run/rfindd
```

`scripts/install.sh` runs both commands automatically when
`selinuxenabled` returns 0 and `semodule` is on PATH.

## Verify

```sh
ls -Z /usr/local/bin/rfindd      # system_u:object_r:rfindd_exec_t:s0
ps -eZ | grep rfindd             # system_u:system_r:rfindd_t:s0
ausearch -m avc -ts recent -c rfindd    # should be empty
```

If `ausearch` is non-empty, run `audit2allow -a` and file the suggested
rules against this directory as a PR — missing allows mean the policy
is tighter than reality and we want to know.

## Not supported

- MLS / MCS. The daemon reads every file; it cannot be meaningfully
  constrained to a sensitivity level.
- A per-user "my rfindd" domain. User-mode rfindd (`rfindd --user`,
  shipped in v1.9) runs under the invoking user's own domain and
  does not need its own policy; it uses inotify only, no fanotify,
  no handle resolution.
