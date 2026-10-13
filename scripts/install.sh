#!/usr/bin/env bash
# rfind installer — build from source, install binaries + systemd service.
#
#   Install / update (from anywhere):
#     curl -fsSL https://raw.githubusercontent.com/YanivSaar/rfind/main/scripts/install.sh | sudo bash
#   From a clone:
#     sudo ./scripts/install.sh
#   Uninstall (keeps the index and config; add --purge to remove them too):
#     sudo ./scripts/install.sh --uninstall [--purge]
#
# Re-running is safe: it rebuilds, replaces the binaries and restarts the
# daemon. An existing /etc/rfindd/config.toml is never overwritten. If the
# index schema changed, rfindd rebuilds the index automatically.
#
# Environment:
#   RFIND_REPO    git URL to clone when not run from a clone
#   RFIND_REF     branch or tag to build (default: main)
#   PREFIX        install prefix (default: /usr/local)

set -euo pipefail

RFIND_REPO=${RFIND_REPO:-https://github.com/YanivSaar/rfind.git}
RFIND_REF=${RFIND_REF:-main}
PREFIX=${PREFIX:-/usr/local}
BIN_DIR=$PREFIX/bin
UNIT=/etc/systemd/system/rfindd.service
CONFIG=/etc/rfindd/config.toml
DATA_DIR=/var/lib/rfindd

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
        -h|--help)   sed -n '2,19p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) die "unknown option: $a" ;;
    esac
done

[ "$(uname -s)" = Linux ] || die "rfind supports Linux only"
[ "$(id -u)" -eq 0 ] || die "run as root (sudo)"
HAS_SYSTEMD=0
[ -d /run/systemd/system ] && HAS_SYSTEMD=1

# ── Uninstall ─────────────────────────────────────────────────────────────────
if [ $UNINSTALL -eq 1 ]; then
    step "Uninstalling rfind"
    if [ $HAS_SYSTEMD -eq 1 ] && systemctl list-unit-files rfindd.service >/dev/null 2>&1; then
        systemctl disable --now rfindd 2>/dev/null || true
        rm -f "$UNIT"; systemctl daemon-reload
        ok "service removed"
    fi
    rm -f "$BIN_DIR/rfind" "$BIN_DIR/rfindd"; ok "binaries removed from $BIN_DIR"
    if [ $PURGE -eq 1 ]; then
        rm -rf "$DATA_DIR" /etc/rfindd /run/rfindd; ok "index and config removed"
    else
        warn "kept $DATA_DIR and /etc/rfindd (use --purge to remove)"
    fi
    exit 0
fi

# ── Source ────────────────────────────────────────────────────────────────────
step "Locating source"
SRC=""
HERE=$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" 2>/dev/null && pwd || true)
if [ -n "$HERE" ] && [ -f "$HERE/../Cargo.toml" ] && grep -q rfindd "$HERE/../Cargo.toml" 2>/dev/null; then
    SRC=$(cd "$HERE/.." && pwd)
    ok "using clone at $SRC"
else
    command -v git >/dev/null || die "git is required (apt install git / dnf install git)"
    SRC=$(mktemp -d /tmp/rfind-src.XXXXXX)
    trap 'rm -rf "$SRC"' EXIT
    git clone --depth 1 --branch "$RFIND_REF" "$RFIND_REPO" "$SRC" >/dev/null 2>&1 \
        || die "could not clone $RFIND_REPO ($RFIND_REF)"
    ok "cloned $RFIND_REPO@$RFIND_REF"
fi

# ── Toolchain ─────────────────────────────────────────────────────────────────
step "Checking build tools"
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
ok "C compiler: $(cc --version | head -1)"

export PATH="$HOME/.cargo/bin:$PATH"
if ! command -v cargo >/dev/null; then
    command -v curl >/dev/null || die "curl is required to install Rust"
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal >/dev/null
    ok "installed Rust via rustup"
fi
ok "$(cargo --version)"

# ── Build ─────────────────────────────────────────────────────────────────────
step "Building (release)"
(cd "$SRC" && cargo build --release --locked -p rfindd -p rfind-cli 2>&1 | tail -1)
[ -x "$SRC/target/release/rfindd" ] && [ -x "$SRC/target/release/rfind" ] || die "build failed"
ok "built rfindd and rfind"

# ── Install ───────────────────────────────────────────────────────────────────
step "Installing"
WAS_RUNNING=0
if [ $HAS_SYSTEMD -eq 1 ] && systemctl is-active --quiet rfindd; then
    WAS_RUNNING=1; systemctl stop rfindd
fi
install -d "$BIN_DIR"
install -m 755 "$SRC/target/release/rfindd" "$SRC/target/release/rfind" "$BIN_DIR/"
ok "binaries → $BIN_DIR"

# First install on WSL2: Windows drives get periodic rescans, not live
# monitoring (inotify can't see changes made from Windows).
if [ ! -f "$CONFIG" ] && grep -qi microsoft /proc/version 2>/dev/null; then
    DRIVES=$(awk '$3 == "9p" && $4 ~ /aname=drvfs/ {print $2} $3 == "drvfs" {print $2}' /proc/mounts)
    if [ -n "$DRIVES" ]; then
        install -d /etc/rfindd
        {
            printf '[scan]\nroots = ["/"]\nexclude = ["/proc", "/sys", "/dev", "/run", "/media"]\n'
            printf 'workers = 0\nreindex_interval_hours = 6\ncollect_mtime = true\n\n'
            printf '[index]\ndb_path = "%s/index.db"\nbatch_size = 10000\n\n' "$DATA_DIR"
            printf '[monitor]\nprefer_fanotify = true\ninotify_fallback = true\n'
            for d in $DRIVES; do
                printf '\n[[scan.periodic_roots]]\npath = "%s"\ninterval_hours = 12\n' "$d"
            done
        } >"$CONFIG"
        ok "WSL2: Windows drives as periodic roots: $(echo $DRIVES | tr '\n' ' ')"
    fi
fi
[ -f "$CONFIG" ] && ok "config: $CONFIG (kept)" || ok "config: defaults written on first start"

if [ $HAS_SYSTEMD -eq 1 ]; then
    install -m 644 "$SRC/packaging/systemd/rfindd.service" "$UNIT"
    systemctl daemon-reload
    systemctl enable --now rfindd >/dev/null 2>&1
    sleep 1
    systemctl is-active --quiet rfindd || die "rfindd failed to start: journalctl -u rfindd -e"
    ok "service $( [ $WAS_RUNNING -eq 1 ] && echo restarted || echo enabled and started)"
else
    warn "systemd not running — start the daemon yourself:  sudo $BIN_DIR/rfindd &"
fi

printf '\n%s%s✔ rfind installed.%s The first scan runs in the background:\n' "$B" "$G" "$R"
printf '    rfind status --watch      # scan progress\n'
printf '    rfind name passwd         # search\n'
printf '    rfind health              # index state\n'
