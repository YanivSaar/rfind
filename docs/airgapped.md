# Air-gapped install

The default `install.sh` pulls release artifacts from GitHub and verifies
them with `cosign` against our OIDC-issued certificate. Air-gapped hosts
cannot do either — but can still install from a locally-staged tarball
and still verify the artifact's provenance.

## 1. On an internet-connected machine: stage the bundle

Download the artifact + its signatures for the release you want to
deploy. For `v1.7.0` on `x86_64`:

```sh
mkdir rfind-airgap && cd rfind-airgap
base=https://github.com/YanivSaar/rfind/releases/download/v1.7.0
curl -fsSLO $base/rfind-x86_64-linux.tar.gz
curl -fsSLO $base/rfind-x86_64-linux.tar.gz.sha256
curl -fsSLO $base/rfind-x86_64-linux.tar.gz.sig
curl -fsSLO $base/rfind-x86_64-linux.tar.gz.pem

# The signing identity's long-lived public key, pinned per release. Replace
# with the value published on https://github.com/YanivSaar/rfind/releases.
cat > rfind-x86_64-linux.tar.gz.cosign.pub <<'EOF'
-----BEGIN PUBLIC KEY-----
# ...the project's current cosign public key...
-----END PUBLIC KEY-----
EOF
```

The bundle manifest for a complete offline install is:

| File | Required? | Purpose |
|---|---|---|
| `rfind-<arch>-linux.tar.gz` | yes | binaries, man pages, packaging/ |
| `rfind-<arch>-linux.tar.gz.sha256` | yes | integrity check |
| `rfind-<arch>-linux.tar.gz.cosign.pub` | strongly recommended | offline cosign verification key |
| `rfind-<arch>-linux.tar.gz.sig` | strongly recommended | cosign signature |
| `scripts/install.sh` (from the same release tag) | yes | the installer |

The tarball already contains `packaging/systemd/rfindd.service`,
`packaging/selinux/{rfindd.te,rfindd.fc,rfindd.if}` and
`packaging/apparmor/usr.local.bin.rfindd`, so you do not need to stage
those separately.

Verify once online, before transfer, with the standard OIDC flow:

```sh
cosign verify-blob \
    --certificate rfind-x86_64-linux.tar.gz.pem \
    --signature   rfind-x86_64-linux.tar.gz.sig \
    --certificate-identity-regexp \
      '^https://github.com/YanivSaar/rfind/.github/workflows/release.yml@refs/tags/' \
    --certificate-oidc-issuer 'https://token.actions.githubusercontent.com' \
    rfind-x86_64-linux.tar.gz
```

Then burn the bundle to the media of your choice.

## 2. On the air-gapped machine: install

```sh
sudo ./install.sh --from-tarball ./rfind-x86_64-linux.tar.gz
```

`--from-tarball` runs an entirely offline path:

- `sha256sum -c` against the sibling `.sha256`.
- `cosign verify-blob --key <.cosign.pub> --signature <.sig>` if those
  files are present and `cosign` is on PATH. If any one of the three is
  missing, you get a warning (not a hard failure) — override with
  `RFIND_COSIGN_PUBKEY=/absolute/path/to/key.pub` to force it.
- No network calls. No `curl`. No `rustup`. No `apt install`.

The rest of the install is identical to the online path: SELinux
module load (if `selinuxenabled`), AppArmor profile load (if
`aa-enabled`), systemd unit install + start.

## 3. On SELinux without devel tools

The policy module (`packaging/selinux/rfindd.pp`) is built on the host
using `/usr/share/selinux/devel/Makefile`. On RHEL systems where you
cannot install `selinux-policy-devel`, build the `.pp` on a *matched*
RHEL + SELinux-policy version on your internet-connected machine and
stage it alongside the tarball:

```sh
# on the connected box:
cd packaging/selinux
make -f /usr/share/selinux/devel/Makefile rfindd.pp
# transfer rfindd.pp with the tarball

# on the air-gapped box:
sudo cp rfindd.pp /tmp/
sudo semodule -i /tmp/rfindd.pp
sudo restorecon -R /usr/local/bin/rfindd /etc/rfindd /var/lib/rfindd /run/rfindd
```

`install.sh` detects the pre-built `.pp` and skips the on-host build.

## 4. Later: upgrades

Repeat section 1 for the new version and run `install.sh
--from-tarball` again. The installer preserves `/etc/rfindd/config.toml`
and `/var/lib/rfindd/index.db` across upgrades. If the index schema
version changed (`SCHEMA_VERSION` in `crates/rfind-core/src/db/mod.rs`),
rfindd rebuilds the index automatically on first start.

## 5. Non-goals

- There is no "zero network, zero signature" path. We will not add
  `--no-verify-signatures`: air-gapped is a reason to verify offline,
  not a reason to skip verification.
- Downloading or stashing a `rustup` toolchain on the air-gapped host
  is out of scope. Build from a connected mirror and transfer the
  binary.
