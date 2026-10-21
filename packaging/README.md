# Packaging artifacts

Service units, MAC policies, container images, and drop-ins — one
per init system or distro integration we support.

| Dir | What | When `install.sh` picks it |
|---|---|---|
| `systemd/` | `rfindd.service` | `/run/systemd/system` exists |
| `openrc/` | `/etc/init.d/rfindd` OpenRC script | `/sbin/openrc` present |
| `runit/` | `/etc/sv/rfindd/{run,log/run}` | `/usr/bin/sv` present |
| `s6/` | `/etc/s6-rc/sv/rfindd/{run,type,dependencies}` | `/etc/s6-rc` exists |
| `dinit/` | `/etc/dinit.d/rfindd` | `/etc/dinit.d` exists |
| `selinux/` | `rfindd.te`, `rfindd.fc`, `rfindd.if` | `selinuxenabled` returns 0 |
| `apparmor/` | `usr.local.bin.rfindd` | `aa-enabled` returns 0 |
| `container/` | `Containerfile`, run-mode docs | manual: `podman build` or pull from `ghcr.io/yanivsaar/rfind` |
| `exclusions.d/` | optional per-topic exclusion fragments | manually copied into `/etc/rfindd/exclusions.d/` |

## Running on each init system

All units pass `--pidfile=/run/rfindd/rfindd.pid` to the daemon so
the pidfile location is consistent across init systems. On systemd,
the unit uses `RuntimeDirectory=rfindd` and the daemon's default
path lands in the same place with no flag needed.

```sh
# systemd
systemctl enable --now rfindd
systemctl status rfindd

# OpenRC
rc-update add rfindd default
rc-service rfindd start
rc-service rfindd status

# runit
ln -s /etc/sv/rfindd /var/service/
sv status rfindd

# s6 / s6-rc
s6-rc-bundle add default rfindd
s6-rc-compile /etc/s6-rc/compiled /etc/s6-rc/sv
s6-rc -u change rfindd

# dinit
dinitctl enable rfindd
dinitctl start rfindd
dinitctl status rfindd
```

Across all five, `rfind status` queries the running daemon exactly
the same way: it reads `/run/rfindd/progress.json` and the index at
`/var/lib/rfindd/index.db`. Init-system choice does not change CLI
behavior.
