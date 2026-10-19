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

UNINSTALL=0; PURGE=0
for a in "$@"; do
    case "$a" in
        --uninstall) UNINSTALL=1 ;;
        --purge)     PURGE=1 ;;
        -h|--help)   sed -n '2,23p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) die "unknown option: $a" ;;
    esac
done

[ "$(uname -s)" = Linux ] || die "rfind supports Linux only"
[ "$(id -u)" -eq 0 ] || die "run as root (sudo)"
HAS_SYSTEMD=0
[ -d /run/systemd/system ] && HAS_SYSTEMD=1

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
    stop_rfindd() {
        local pid
        pid=$(cat /run/rfindd/rfindd.pid 2>/dev/null || true)
        if [ $HAS_SYSTEMD -eq 1 ] && [ -f "$UNIT" ]; then
            systemctl disable rfindd 2>/dev/null || true
            systemctl stop --no-block rfindd 2>/dev/null || true
        fi
        # 30 s grace for systemd's own SIGTERM to run its course. The daemon
        # may need several seconds to cancel an in-flight scan (notably on
        # WSL2 /mnt/c). Only after that do we SIGKILL. We do NOT send a
        # second SIGTERM: systemctl stop already did.
        [ -z "$pid" ] && return 0
        for _ in $(seq 1 300); do
            kill -0 "$pid" 2>/dev/null || return 0
            sleep 0.1
        done
        echo "install.sh: rfindd (pid $pid) still running 30s after systemctl stop; sending SIGKILL" >&2
        kill -KILL "$pid" 2>/dev/null || true
        for _ in $(seq 1 30); do
            kill -0 "$pid" 2>/dev/null || return 0
            sleep 0.1
        done
        echo "install.sh: WARNING: rfindd (pid $pid) survived SIGKILL; continuing anyway" >&2
    }
    stop_rfindd
    if [ $HAS_SYSTEMD -eq 1 ] && [ -f "$UNIT" ]; then
        rm -f "$UNIT"; systemctl daemon-reload
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
    # proves the two files were fetched together; cosign proves the tarball
    # was built by *our* workflow on *our* repo. Use it when available.
    local verified_by="sha256"
    if command -v cosign >/dev/null 2>&1; then
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
            warn "cosign installed but no signature published for this release"
        fi
    else
        warn "cosign not installed; falling back to sha256 only (install cosign for provenance)"
    fi

    tar xzf "$WORK/$asset" -C "$WORK"
    PKG=$WORK/rfind-$arch-linux
    ok "verified $asset ($verified_by)"
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

if [ $IN_CLONE -eq 1 ] || [ "$RFIND_FROM_SOURCE" = 1 ]; then
    build_from_source
elif ! download; then
    warn "no prebuilt binary for this system/version; building from source"
    build_from_source
fi

# ── Install ───────────────────────────────────────────────────────────────────
step "Installing"
WAS_RUNNING=0
if [ $HAS_SYSTEMD -eq 1 ] && systemctl is-active --quiet rfindd; then
    WAS_RUNNING=1; systemctl stop rfindd
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

if [ $HAS_SYSTEMD -eq 1 ]; then
    install -m 644 "$PKG/packaging/systemd/rfindd.service" "$UNIT"
    systemctl daemon-reload
    systemctl enable --now rfindd >/dev/null 2>&1
    sleep 1
    systemctl is-active --quiet rfindd || die "rfindd failed to start: journalctl -u rfindd -e"
    ok "service $( [ $WAS_RUNNING -eq 1 ] && echo restarted || echo enabled and started)"
else
    warn "systemd not running — start the daemon yourself:  sudo $BIN_DIR/rfindd &"
fi

printf '\n%s%s✔ rfind installed.%s The first scan runs in the background:\n' "$B" "$G" "$R"
printf '    sudo rfind status --watch      # scan progress\n'
printf '    sudo rfind name passwd         # search\n'
printf '    sudo rfind size /home          # disk usage\n'
printf '    man rfind                      # everything else\n'
