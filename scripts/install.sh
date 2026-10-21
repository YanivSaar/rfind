#!/usr/bin/env bash
# rfind installer — install binaries, man pages, completions and the
# systemd service. Uses prebuilt static binaries from GitHub releases when
# available; otherwise (or from a clone) builds from source.
#
#   Install / update (from anywhere):
#     curl -fsSL https://raw.githubusercontent.com/YanivSaar/rfind/main/scripts/install.sh | sudo bash
#   From a clone (builds that checkout):
#     sudo ./scripts/install.sh
#   Uninstall (keeps the index and config; add --purge to remove them too):
#     sudo rfind uninstall [--purge]      or      sudo ./scripts/install.sh --uninstall [--purge]
#
# Re-running is safe: it replaces the binaries and restarts the daemon. An
# existing /etc/rfindd/config.toml is never overwritten. If the index
# format changed, rfindd rebuilds the index automatically.
#
# Environment:
#   RFIND_VERSION      release to install, e.g. v1.0.1 (default: latest)
#   RFIND_FROM_SOURCE  1 = always build from source
#   RFIND_REPO         git URL to clone for source builds
#   RFIND_REF          branch or tag to build from source (default: main)
#   PREFIX             install prefix (default: /usr/local)

set -euo pipefail

GITHUB=YanivSaar/rfind
RFIND_VERSION=${RFIND_VERSION:-latest}
RFIND_FROM_SOURCE=${RFIND_FROM_SOURCE:-0}
RFIND_REPO=${RFIND_REPO:-https://github.com/$GITHUB.git}
RFIND_REF=${RFIND_REF:-main}
PREFIX=${PREFIX:-/usr/local}
BIN_DIR=$PREFIX/bin
SHARE=$PREFIX/share
UNIT=/etc/systemd/system/rfindd.service
CONFIG=/etc/rfindd/config.toml

if [ -t 1 ]; then B=$'\e[1m'; G=$'\e[32m'; Y=$'\e[33m'; RD=$'\e[31m'; C=$'\e[36m'; R=$'\e[0m'
else B= G= Y= RD= C= R=; fi
step() { printf '%s==>%s %s%s%s\n' "$C" "$R" "$B" "$*" "$R"; }
ok()   { printf '    %s✔%s %s\n' "$G" "$R" "$*"; }
warn() { printf '    %s▲%s %s\n' "$Y" "$R" "$*"; }
die()  { printf '%s✘ %s%s\n' "$RD" "$*" "$R" >&2; exit 1; }

UNINSTALL=0; PURGE=0; FROM_TARBALL=""
while [ $# -gt 0 ]; do
    case "$1" in
        --uninstall)       UNINSTALL=1 ;;
        --purge)           PURGE=1 ;;
        --from-tarball)    FROM_TARBALL=${2:-}; shift
                           [ -n "$FROM_TARBALL" ] || die "--from-tarball needs a path" ;;
        --from-tarball=*)  FROM_TARBALL=${1#--from-tarball=} ;;
        -h|--help)         sed -n '2,23p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) die "unknown option: $1" ;;
    esac
    shift
done

[ "$(uname -s)" = Linux ] || die "rfind supports Linux only"
[ "$(id -u)" -eq 0 ] || die "run as root (sudo)"

# Init-system detection. First match wins; systemd always wins when
# present because that's what packaging/systemd/rfindd.service is
# written for and what every CI run exercises.
INIT=""
HAS_SYSTEMD=0
if [ -d /run/systemd/system ]; then
    INIT=systemd; HAS_SYSTEMD=1
elif command -v openrc >/dev/null 2>&1 || [ -x /sbin/openrc-run ]; then
    INIT=openrc
elif command -v sv >/dev/null 2>&1 && [ -d /etc/sv ]; then
    INIT=runit
elif [ -d /etc/s6-rc ] || [ -d /etc/s6/sv ]; then
    INIT=s6
elif [ -d /etc/dinit.d ] || command -v dinitctl >/dev/null 2>&1; then
    INIT=dinit
fi

WORK=$(mktemp -d /tmp/rfind-install.XXXXXX)
trap 'rm -rf "$WORK"' EXIT

# ── Uninstall ─────────────────────────────────────────────────────────────────
if [ $UNINSTALL -eq 1 ]; then
    if [ -x "$BIN_DIR/rfind" ]; then
        exec "$BIN_DIR/rfind" uninstall --yes $( [ $PURGE -eq 1 ] && echo --purge )
    fi
    # No rfind binary left: remove what we can by hand. The service-stop
    # ladder mirrors `rfind uninstall`'s so a wedged daemon can never make
    # this hang: `systemctl stop --no-block`, poll for exit, then SIGTERM,
    # then SIGKILL.
    step "Uninstalling rfind"
    # Grace: 5 s with --purge (we're nuking the index anyway, no reason to
    # wait for a clean flush), 30 s without (scanner needs several seconds
    # to cancel in-flight work on slow mounts like WSL2 /mnt/c).
    stop_rfindd() {
        local pid grace
        pid=$(cat /run/rfindd/rfindd.pid 2>/dev/null || true)
        grace=$( [ $PURGE -eq 1 ] && echo 50 || echo 300 )  # 100ms ticks
        if [ $HAS_SYSTEMD -eq 1 ] && [ -f "$UNIT" ]; then
            systemctl disable rfindd 2>/dev/null || true
            systemctl stop --no-block rfindd 2>/dev/null || true
        fi
        [ -z "$pid" ] && return 0
        for _ in $(seq 1 "$grace"); do
            kill -0 "$pid" 2>/dev/null || return 0
            sleep 0.1
        done
        echo "install.sh: rfindd (pid $pid) still running after systemctl stop; sending SIGKILL" >&2
        kill -KILL "$pid" 2>/dev/null || true
        for _ in $(seq 1 30); do
            kill -0 "$pid" 2>/dev/null || return 0
            sleep 0.1
        done
        echo "install.sh: WARNING: rfindd (pid $pid) survived SIGKILL; continuing anyway" >&2
    }
    stop_rfindd
    if [ $HAS_SYSTEMD -eq 1 ] && [ -f "$UNIT" ]; then
        rm -f "$UNIT"
        systemctl daemon-reload
        # Clear systemd's cached failed-unit state so `systemctl status
        # rfindd` doesn't keep showing `× rfindd.service … Active: failed`
        # after the unit file is gone.
        systemctl reset-failed rfindd 2>/dev/null || true
    fi
    rm -f "$BIN_DIR/rfindd" "$SHARE"/man/man1/rfind.1 "$SHARE"/man/man5/rfind.conf.5 "$SHARE"/man/man8/rfindd.8 \
          "$SHARE"/bash-completion/completions/rfind "$SHARE"/zsh/site-functions/_rfind \
          "$SHARE"/fish/vendor_completions.d/rfind.fish /etc/bash_completion.d/rfind
    [ $PURGE -eq 1 ] && rm -rf /var/lib/rfindd /etc/rfindd /run/rfindd
    ok "removed"
    exit 0
fi

# ── Get binaries ──────────────────────────────────────────────────────────────
# Sets PKG: a directory with bin/rfind, bin/rfindd, man/, packaging/.
PKG=""

HERE=$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" 2>/dev/null && pwd || true)
IN_CLONE=0
if [ -n "$HERE" ] && [ -f "$HERE/../Cargo.toml" ] && grep -q rfindd "$HERE/../Cargo.toml" 2>/dev/null; then
    IN_CLONE=1
fi

download() {
    local arch asset base
    case "$(uname -m)" in
        x86_64|amd64)  arch=x86_64 ;;
        aarch64|arm64) arch=aarch64 ;;
        *) return 1 ;;
    esac
    command -v curl >/dev/null || return 1
    asset=rfind-$arch-linux.tar.gz
    if [ "$RFIND_VERSION" = latest ]; then
        base=https://github.com/$GITHUB/releases/latest/download
    else
        base=https://github.com/$GITHUB/releases/download/$RFIND_VERSION
    fi
    step "Downloading $RFIND_VERSION release for $arch"
    curl -fsSL "$base/$asset" -o "$WORK/$asset" 2>/dev/null || return 1
    curl -fsSL "$base/$asset.sha256" -o "$WORK/$asset.sha256" 2>/dev/null || return 1
    (cd "$WORK" && sha256sum -c --quiet "$asset.sha256") || die "checksum mismatch for $asset"

    # Sigstore (cosign) verification: tarballs tagged v* carry .sig + .pem
    # issued by the Release workflow via GitHub OIDC. The sha256 above only
    # proves the two files were fetched together (both come from the same
    # release bucket): a compromise of the release path replaces them
    # together and the sha check becomes a tautology. cosign proves the
    # tarball was built by *our* workflow on *our* repo, so we require it
    # by default and let the caller opt out explicitly.
    local verified_by="sha256"
    local require_cosign=${RFIND_REQUIRE_COSIGN:-1}
    if ! command -v cosign >/dev/null 2>&1; then
        if [ "$require_cosign" = "1" ]; then
            die "cosign is required to verify provenance. Install it (e.g. apt install cosign, \
or https://docs.sigstore.dev/system_config/installation), or re-run with RFIND_REQUIRE_COSIGN=0 \
to accept sha256-only verification (not recommended)."
        fi
        warn "cosign not installed and RFIND_REQUIRE_COSIGN=0; falling back to sha256 only"
    else
        if curl -fsSL "$base/$asset.sig" -o "$WORK/$asset.sig" 2>/dev/null \
           && curl -fsSL "$base/$asset.pem" -o "$WORK/$asset.pem" 2>/dev/null; then
            if cosign verify-blob \
                --certificate "$WORK/$asset.pem" \
                --signature "$WORK/$asset.sig" \
                --certificate-identity-regexp \
                    "^https://github.com/$GITHUB/.github/workflows/release.yml@refs/tags/" \
                --certificate-oidc-issuer "https://token.actions.githubusercontent.com" \
                "$WORK/$asset" >/dev/null 2>&1; then
                verified_by="cosign + sha256"
            else
                die "cosign signature check failed for $asset"
            fi
        else
            if [ "$require_cosign" = "1" ]; then
                die "no sigstore signature published for this release and \
RFIND_REQUIRE_COSIGN=1 (default). Re-run with RFIND_REQUIRE_COSIGN=0 to accept \
sha256-only verification, or wait for a signed release."
            fi
            warn "cosign installed but no signature published for this release"
        fi
    fi

    tar xzf "$WORK/$asset" -C "$WORK"
    PKG=$WORK/rfind-$arch-linux
    ok "verified $asset ($verified_by)"
}

from_tarball() {
    local tb=$1
    [ -r "$tb" ] || die "cannot read $tb"
    step "Installing from local tarball $tb (air-gapped mode)"

    local sha=$tb.sha256
    [ -r "$sha" ] || die "missing $sha next to the tarball"
    (cd "$(dirname "$tb")" && sha256sum -c --quiet "$(basename "$sha")") \
        || die "checksum mismatch for $tb"
    ok "sha256 verified"

    # Offline cosign: expects the signature + certificate as siblings and
    # a local public key at $tb.cosign.pub (or RFIND_COSIGN_PUBKEY).
    local pub=${RFIND_COSIGN_PUBKEY:-$tb.cosign.pub}
    local sig=$tb.sig
    if command -v cosign >/dev/null 2>&1 && [ -r "$sig" ] && [ -r "$pub" ]; then
        if cosign verify-blob --key "$pub" --signature "$sig" "$tb" >/dev/null 2>&1; then
            ok "cosign signature verified against $pub"
        else
            die "cosign signature check failed for $tb"
        fi
    else
        warn "cosign offline verification skipped (no cosign, or no $sig / $pub)"
        warn "re-run with RFIND_COSIGN_PUBKEY=/path/to/key.pub to require it"
    fi

    tar xzf "$tb" -C "$WORK"
    PKG=$(find "$WORK" -maxdepth 2 -type d -name 'rfind-*-linux' | head -n1)
    [ -n "$PKG" ] && [ -x "$PKG/bin/rfindd" ] && [ -x "$PKG/bin/rfind" ] \
        || die "tarball does not contain bin/rfindd + bin/rfind"
    ok "unpacked $(basename "$PKG")"
}

build_from_source() {
    local src
    if [ $IN_CLONE -eq 1 ]; then
        src=$(cd "$HERE/.." && pwd)
        step "Building from the clone at $src"
    else
        command -v git >/dev/null || die "git is required to build from source"
        src=$WORK/src
        step "Building $RFIND_REPO@$RFIND_REF from source"
        git clone --depth 1 --branch "$RFIND_REF" "$RFIND_REPO" "$src" >/dev/null 2>&1 \
            || die "could not clone $RFIND_REPO ($RFIND_REF)"
    fi
    if ! command -v cc >/dev/null; then
        # rusqlite bundles SQLite, which needs a C compiler.
        if   command -v apt-get >/dev/null; then apt-get update -qq && apt-get install -y -qq build-essential >/dev/null
        elif command -v dnf     >/dev/null; then dnf install -y -q gcc make
        elif command -v yum     >/dev/null; then yum install -y -q gcc make
        elif command -v pacman  >/dev/null; then pacman -Sy --noconfirm --needed base-devel >/dev/null
        elif command -v apk     >/dev/null; then apk add --quiet build-base
        elif command -v zypper  >/dev/null; then zypper -q install -y gcc make
        else die "no C compiler found; install gcc"; fi
    fi
    export PATH="$HOME/.cargo/bin:$PATH"
    if ! command -v cargo >/dev/null; then
        command -v curl >/dev/null || die "curl is required to install Rust"
        curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal >/dev/null
        ok "installed Rust via rustup"
    fi
    (cd "$src" && cargo build --release --locked -p rfindd -p rfind-cli 2>&1 | tail -1)
    [ -x "$src/target/release/rfindd" ] && [ -x "$src/target/release/rfind" ] || die "build failed"
    PKG=$WORK/pkg
    mkdir -p "$PKG/bin"
    cp "$src/target/release/rfind" "$src/target/release/rfindd" "$PKG/bin/"
    cp -r "$src/man" "$src/packaging" "$PKG/"
    ok "built rfindd and rfind"
}

# ── Preflight: existing installation ──────────────────────────────────────────
# Running the installer twice should not hang. The old behaviour used
# `systemctl stop rfindd`, which blocks up to TimeoutStopSec (90 s default)
# waiting for the daemon to exit — on WSL2 /mnt/c scans we have seen that
# take 30 s. Detect the previous install up front, show its state, and let
# the user pick: upgrade in place (keep index and config) or purge and
# reinstall fresh. Non-interactive runs (curl | sudo bash) default to
# upgrade.
MODE=upgrade  # upgrade | fresh
if [ -x "$BIN_DIR/rfind" ] || [ -x "$BIN_DIR/rfindd" ] || [ -f "$UNIT" ]; then
    step "Existing rfind installation detected"
    if [ -x "$BIN_DIR/rfind" ]; then
        INSTALLED_VER=$("$BIN_DIR/rfind" --version 2>/dev/null || echo 'rfind ?')
        ok "binary:  $BIN_DIR/rfind ($INSTALLED_VER)"
    fi
    [ -x "$BIN_DIR/rfindd" ] && ok "daemon:  $BIN_DIR/rfindd"
    [ -f "$UNIT" ]           && ok "unit:    $UNIT"
    [ -f "$CONFIG" ]         && ok "config:  $CONFIG"
    if [ -d /var/lib/rfindd ]; then
        SIZE=$(du -sh /var/lib/rfindd 2>/dev/null | awk '{print $1}')
        ok "index:   /var/lib/rfindd${SIZE:+ ($SIZE)}"
    fi
    if [ $HAS_SYSTEMD -eq 1 ] && systemctl is-active --quiet rfindd 2>/dev/null; then
        PID=$(cat /run/rfindd/rfindd.pid 2>/dev/null || echo '?')
        ok "state:   running (pid $PID)"
    elif [ $HAS_SYSTEMD -eq 1 ] && [ -f "$UNIT" ]; then
        ok "state:   stopped"
    fi

    if [ -t 0 ]; then
        printf '\n%sWhat would you like to do?%s\n' "$B" "$R"
        printf '  %s[U]%s Upgrade in place  — keep index and config (default)\n' "$G" "$R"
        printf '  %s[P]%s Purge and reinstall — removes index AND config\n' "$Y" "$R"
        printf '  %s[C]%s Cancel\n' "$RD" "$R"
        printf 'Choice [U/p/c]: '
        read -r CHOICE
        case "${CHOICE:-U}" in
            u|U|'') MODE=upgrade ;;
            p|P)    MODE=fresh ;;
            c|C)    die "cancelled" ;;
            *)      die "unknown choice: $CHOICE" ;;
        esac
    else
        warn "non-interactive — upgrading in place (keep index and config)"
        warn "to force a fresh install, run: sudo rfind uninstall --purge first"
    fi

    if [ "$MODE" = fresh ]; then
        step "Purging existing installation first"
        if [ -x "$BIN_DIR/rfind" ]; then
            "$BIN_DIR/rfind" uninstall --yes --purge
        else
            warn "no rfind binary found; cleaning up by hand"
            if [ $HAS_SYSTEMD -eq 1 ] && [ -f "$UNIT" ]; then
                systemctl disable rfindd 2>/dev/null || true
                systemctl stop --no-block rfindd 2>/dev/null || true
                rm -f "$UNIT"
                systemctl daemon-reload
                systemctl reset-failed rfindd 2>/dev/null || true
            fi
            rm -rf /var/lib/rfindd /etc/rfindd /run/rfindd
        fi
        ok "purged"
    fi
fi

if [ -n "$FROM_TARBALL" ]; then
    from_tarball "$FROM_TARBALL"
elif [ $IN_CLONE -eq 1 ] || [ "$RFIND_FROM_SOURCE" = 1 ]; then
    build_from_source
elif ! download; then
    warn "no prebuilt binary for this system/version; building from source"
    build_from_source
fi

# ── Install ───────────────────────────────────────────────────────────────────
step "Installing"
WAS_RUNNING=0
# Stop the running daemon before overwriting its binary, using the same
# non-blocking escalation ladder as `rfind uninstall` so a slow shutdown
# cannot hang the installer. `systemctl stop` on its own blocks up to
# TimeoutStopSec (90s) — hence --no-block + poll + SIGKILL fallback.
if [ $HAS_SYSTEMD -eq 1 ] && systemctl is-active --quiet rfindd 2>/dev/null; then
    WAS_RUNNING=1
    PID=$(cat /run/rfindd/rfindd.pid 2>/dev/null || true)
    ok "stopping running rfindd${PID:+ (pid $PID)} before overwrite"
    systemctl stop --no-block rfindd 2>/dev/null || true
    if [ -n "$PID" ]; then
        STOPPED=0
        for _ in $(seq 1 300); do  # 30s @ 100ms
            kill -0 "$PID" 2>/dev/null || { STOPPED=1; break; }
            sleep 0.1
        done
        if [ $STOPPED -eq 0 ]; then
            warn "rfindd (pid $PID) still running 30s after systemctl stop; sending SIGKILL"
            kill -KILL "$PID" 2>/dev/null || true
            for _ in $(seq 1 30); do
                kill -0 "$PID" 2>/dev/null || { STOPPED=1; break; }
                sleep 0.1
            done
        fi
        [ $STOPPED -eq 1 ] && ok "rfindd stopped" || warn "rfindd survived SIGKILL; continuing anyway"
    fi
    # Clear any failed-unit state from a previous crash so `systemctl
    # start` below doesn't inherit it.
    systemctl reset-failed rfindd 2>/dev/null || true
fi
install -d "$BIN_DIR"
install -m 755 "$PKG/bin/rfindd" "$PKG/bin/rfind" "$BIN_DIR/"
ok "binaries → $BIN_DIR ($("$BIN_DIR/rfind" --version))"

for page in rfind.1 rfind.conf.5 rfindd.8; do
    sec=${page##*.}
    install -D -m 644 "$PKG/man/$page" "$SHARE/man/man$sec/$page"
done
command -v mandb >/dev/null && mandb -q 2>/dev/null || true
ok "man pages: rfind(1), rfindd(8), rfind.conf(5)"

install -d "$SHARE/bash-completion/completions" "$SHARE/zsh/site-functions" "$SHARE/fish/vendor_completions.d"
"$BIN_DIR/rfind" completions bash > "$SHARE/bash-completion/completions/rfind"
"$BIN_DIR/rfind" completions zsh  > "$SHARE/zsh/site-functions/_rfind"
"$BIN_DIR/rfind" completions fish > "$SHARE/fish/vendor_completions.d/rfind.fish"
# bash-completion's dynamic loader looks under XDG_DATA_DIRS, which on
# most distros includes $PREFIX/share. On minimal setups (containers,
# plain Debian without the bash-completion package initialised in the
# shell rc) it may not — so drop a second copy at /etc/bash_completion.d,
# which plain `. /etc/bash_completion` setups read directly.
if [ -d /etc/bash_completion.d ]; then
    install -m 644 "$SHARE/bash-completion/completions/rfind" /etc/bash_completion.d/rfind
fi
ok "shell completions: bash, zsh, fish"
if ! [ -r /usr/share/bash-completion/bash_completion ] \
   && ! [ -r /etc/bash_completion ]; then
    warn "bash-completion package not detected; for tab-complete, install it:"
    warn "    apt install bash-completion    # or dnf/zypper/pacman"
    warn "then start a new shell."
fi

# First install on WSL2: Windows drives get periodic rescans, not live
# monitoring (the kernel sees no events for changes made from Windows).
if [ ! -f "$CONFIG" ] && grep -qi microsoft /proc/version 2>/dev/null; then
    DRIVES=$(awk '$3 == "9p" && $4 ~ /aname=drvfs/ {print $2} $3 == "drvfs" {print $2}' /proc/mounts | xargs)
    if [ -n "$DRIVES" ]; then
        # shellcheck disable=SC2086
        RFIND_CONFIG=$CONFIG "$BIN_DIR/rfind" config periodic add $DRIVES --no-reload >/dev/null
        ok "WSL2: Windows drives rescanned every 12h: $DRIVES"
    fi
fi
[ -f "$CONFIG" ] && ok "config: $CONFIG" || ok "config: defaults written on first start"

# ── MAC: SELinux ──────────────────────────────────────────────────────────────
# On RHEL/Fedora/Rocky/Alma with SELinux enforcing, running rfindd
# unconfined is unacceptable. Build + load our policy module if the
# toolchain is present; otherwise warn so the operator knows the
# daemon is running under a less-restrictive domain than intended.
if command -v selinuxenabled >/dev/null 2>&1 && selinuxenabled; then
    POLDIR=$PKG/packaging/selinux
    if [ -d "$POLDIR" ] && command -v semodule >/dev/null 2>&1; then
        step "Loading SELinux policy for rfindd"
        if [ -f /usr/share/selinux/devel/Makefile ] \
           && command -v checkmodule >/dev/null 2>&1 \
           && command -v semodule_package >/dev/null 2>&1; then
            (cd "$POLDIR" && make -f /usr/share/selinux/devel/Makefile rfindd.pp >/dev/null)
            semodule -i "$POLDIR/rfindd.pp"
            restorecon -R "$BIN_DIR/rfindd" /etc/rfindd /var/lib/rfindd /run/rfindd 2>/dev/null || true
            ok "SELinux module rfindd installed (domain rfindd_t)"
        else
            warn "SELinux is enforcing but policy devel tools missing;"
            warn "install selinux-policy-devel (RHEL/Fedora) or selinux-policy-dev (SUSE)"
            warn "then: cd $POLDIR && make -f /usr/share/selinux/devel/Makefile && semodule -i rfindd.pp"
        fi
    fi
fi

# ── MAC: AppArmor ─────────────────────────────────────────────────────────────
# Ubuntu 20.04+ and SUSE 15+ ship AppArmor enabled. Load our profile if
# apparmor_parser is on PATH.
if command -v aa-enabled >/dev/null 2>&1 && aa-enabled --quiet 2>/dev/null \
   && command -v apparmor_parser >/dev/null 2>&1 \
   && [ -r "$PKG/packaging/apparmor/usr.local.bin.rfindd" ]; then
    step "Loading AppArmor profile for rfindd"
    install -m 0644 "$PKG/packaging/apparmor/usr.local.bin.rfindd" /etc/apparmor.d/
    if apparmor_parser -r /etc/apparmor.d/usr.local.bin.rfindd 2>/dev/null; then
        ok "AppArmor profile loaded (enforce)"
    else
        warn "apparmor_parser rejected the profile; the daemon will run unconfined."
        warn "Debian-stable AppArmor may need the abi line removed — see packaging/apparmor/README.md"
    fi
fi

install -d -m 0700 /run/rfindd /var/lib/rfindd

case "$INIT" in
    systemd)
        install -m 644 "$PKG/packaging/systemd/rfindd.service" "$UNIT"
        ok "unit → $UNIT"
        systemctl daemon-reload
        systemctl enable --now rfindd >/dev/null 2>&1
        sleep 1
        systemctl is-active --quiet rfindd || die "rfindd failed to start: journalctl -u rfindd -e"
        NEWPID=$(cat /run/rfindd/rfindd.pid 2>/dev/null || echo '?')
        ok "service $( [ $WAS_RUNNING -eq 1 ] && echo restarted || echo "enabled and started") (pid $NEWPID)"
        ;;
    openrc)
        install -m 755 "$PKG/packaging/openrc/rfindd" /etc/init.d/rfindd
        rc-update add rfindd default >/dev/null 2>&1 || true
        rc-service rfindd restart || die "rfindd failed to start: tail /var/log/messages"
        ok "OpenRC service installed and started"
        ;;
    runit)
        install -d -m 0755 /etc/sv/rfindd /etc/sv/rfindd/log /var/log/rfindd
        install -m 755 "$PKG/packaging/runit/rfindd/run"     /etc/sv/rfindd/run
        install -m 755 "$PKG/packaging/runit/rfindd/log/run" /etc/sv/rfindd/log/run
        [ -L /var/service/rfindd ] || ln -s /etc/sv/rfindd /var/service/
        sv up rfindd 2>/dev/null || true
        ok "runit service installed; sv status rfindd"
        ;;
    s6)
        install -d -m 0755 /etc/s6-rc/sv/rfindd
        install -m 755 "$PKG/packaging/s6/rfindd/run"          /etc/s6-rc/sv/rfindd/run
        install -m 644 "$PKG/packaging/s6/rfindd/type"         /etc/s6-rc/sv/rfindd/type
        install -m 644 "$PKG/packaging/s6/rfindd/dependencies" /etc/s6-rc/sv/rfindd/dependencies
        warn "s6: run \`s6-rc-compile\` + \`s6-rc -u change rfindd\` to activate"
        ok "s6 service template installed under /etc/s6-rc/sv/rfindd"
        ;;
    dinit)
        install -m 644 "$PKG/packaging/dinit/rfindd" /etc/dinit.d/rfindd
        dinitctl enable rfindd 2>/dev/null || true
        dinitctl start rfindd  2>/dev/null || die "rfindd failed to start: dinitctl status rfindd"
        ok "dinit service installed and started"
        ;;
    *)
        warn "no supported init system detected — start the daemon yourself:"
        warn "    sudo $BIN_DIR/rfindd --pidfile=/run/rfindd/rfindd.pid &"
        ;;
esac

printf '\n%s%s✔ rfind installed.%s The first scan runs in the background:\n' "$B" "$G" "$R"
printf '    sudo rfind status --watch      # scan progress\n'
printf '    sudo rfind name passwd         # search\n'
printf '    sudo rfind size /home          # disk usage\n'
printf '    man rfind                      # everything else\n'
