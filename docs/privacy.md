# rfind and privacy

rfindd runs as root and indexes every file name on filesystems you let
it see. The *contents* of files are never read or stored — just path,
size, mtime, and whether an entry is a directory — but file **names**
are often sensitive on their own: `.ssh/id_ed25519_prod`,
`Firefox/Profiles/x9k2.default-release/logins.json`,
`Mail/INBOX/cur/1700293318.M…`.

Only root can read the index (`/var/lib/rfindd/index.db` is `0600` in a
`0700` directory), so a non-root attacker on the same host sees nothing
new. But the index does mean a **compromised root process** anywhere
on the box can enumerate every file name on the system in milliseconds,
without walking the filesystem. That's the trade: fast search in
exchange for a concentrated, easy-to-read list.

This page lists what rfind indexes by default, what you may want to
exclude, and how.

## What is indexed

By default (`/etc/rfindd/config.toml`):

- **Roots:** `/`
- **Excluded:** `/proc`, `/sys`, `/dev`, `/run`, `/media`

So **every other path on your root filesystem** gets its name in the
index: `/home/*`, `/root`, `/etc`, `/var`, `/tmp`, mounted USB drives
under `/mnt`, container storage, bind-mounts (unless in the fanotify
mount allowlist).

## Trees you may want to exclude

Not because rfind would misuse them — it won't — but because having
their names in a single plaintext index raises the blast radius of a
future root compromise.

| Tree | Why consider excluding |
|---|---|
| `/home/*/.ssh/` | Key filenames hint at account purpose (`id_rsa_prod`, `aws-vault-*`) |
| `/home/*/.gnupg/`, `/root/.gnupg/` | Keyring filenames + revocation certs |
| `/home/*/.password-store/`, `/home/*/.config/*/pass/` | `pass` entries have semantic filenames (`banking/chase.gpg`) |
| `/home/*/.aws/`, `/home/*/.config/gcloud/`, `/root/.kube/` | Cloud credential paths and context names |
| `/home/*/.mozilla/`, `/home/*/.config/google-chrome/`, `.config/BraveSoftware/` | Browser profiles leak sites, extensions, saved-login filenames |
| `/home/*/Mail/`, `/home/*/.thunderbird/`, `/home/*/.local/share/evolution/` | Maildir filenames encode message IDs + flags |
| `/var/lib/docker/`, `/var/lib/containerd/`, `/var/lib/containers/` | Image layer paths and overlay diffs — huge churn, limited search value |
| `/var/lib/snapd/`, `/var/lib/flatpak/` | Same reasoning: churn-heavy, low search value |
| Encrypted mounts (`/mnt/vault`, `~/Private`) | Unlocked contents become indexed in the clear |

## How to exclude

Edit `/etc/rfindd/config.toml` and append to `scan.exclude`. The
templates under `packaging/exclusions.d/` in this repo are drop-in
fragments you can paste into that file:

```toml
# /etc/rfindd/config.toml
[scan]
exclude = [
  "/proc", "/sys", "/dev", "/run", "/media",
  # From packaging/exclusions.d/credentials.toml:
  "/root/.ssh",
  "/root/.gnupg",
  "/root/.aws",
  "/root/.kube",
  # From packaging/exclusions.d/browsers.toml:
  # … (per-user; see template for the pattern)
]
```

Then:

```bash
sudo rfind config edit                    # or just edit the file
sudo systemctl reload rfindd              # SIGHUP: re-exec with new config
```

The daemon runs `delete_outside_roots` on reload to drop entries no
longer in scope, so excluded trees disappear from search results on
the next reload — no manual rebuild needed.

### Per-user `/home/*/…` exclusions

`scan.exclude` takes **literal paths**, not globs — so excluding
`/home/alice/.ssh` and `/home/bob/.ssh` is two entries, not one. If
your host has many users and the list would be unmanageable, exclude
`/home` entirely and add back only the trees you want searchable
(e.g. `scan.roots = ["/", "/home/*/Documents"]` — the daemon expands
root globs at load time; see
`rfind-config/src/lib.rs::ScanConfig::validate`).

## Checking what's indexed

```bash
sudo rfind under /home/alice/.ssh         # names only, root-only read
sudo rfind stats                          # top extensions, biggest dirs
```

If something turned up that shouldn't have, add it to `scan.exclude`
and reload.

## What rfind does *not* defend against

- An attacker who already has root. The index is a convenience for
  them, not a new capability — they could have walked the fs anyway.
- Side channels (disk-usage of `/var/lib/rfindd`, scan timings) that
  reveal rough index size. Accepted.
- Hostile filesystems designed to defeat `statx`/`getdents`. The
  mount allowlist is the mitigation; see `docs/threat-model.md`.

See also: [`docs/threat-model.md`](threat-model.md).
