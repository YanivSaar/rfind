# AppArmor profile for rfindd

Ships `usr.local.bin.rfindd`: an enforcing profile that mirrors the
systemd unit's `CapabilityBoundingSet` and the SELinux module's
intent. Ubuntu 20.04+ and SUSE 15+ enable AppArmor by default; this
profile is the right confinement on those distros.

## Install

```sh
install -m 0644 usr.local.bin.rfindd /etc/apparmor.d/
apparmor_parser -r /etc/apparmor.d/usr.local.bin.rfindd
systemctl restart rfindd    # pick up the profile
aa-status | grep rfindd     # expect: "enforce"
```

`scripts/install.sh` does this automatically when `aa-enabled`
returns 0 and `apparmor_parser` is on PATH.

## Verify

```sh
dmesg | grep DENIED | grep rfindd    # should be empty after a scan
aa-logprof                             # review any denials interactively
```

If `dmesg` reports a `DENIED` line for a path rfindd should legitimately
read, add it to the profile and file a PR — the profile is intended
to allow a full-system scan with zero denials.

## Compatibility

- Ubuntu 20.04 (focal) — AppArmor 2.13, abi/3.0 may need to drop to
  2.12 syntax. Remove the `abi <abi/3.0>,` line if `apparmor_parser`
  rejects it.
- SUSE Tumbleweed — recent enough, no changes needed.
- Debian 12 — AppArmor is available but not enforcing by default;
  this profile only takes effect once AppArmor is loaded.
