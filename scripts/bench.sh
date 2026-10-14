#!/usr/bin/env bash
# rfind benchmark — compares rfind with plocate, GNU locate and GNU find on
# the live system.
#
# Metrics
#   1. Environment
#   2. Index build time     (plocate / locate updatedb timed now; rfind from its log)
#   3. Footprint            (entries, index size, daemon RSS/CPU/fds/watches)
#   4. Search latency       (median of N runs; find once per scope) and
#      accuracy vs find     (find is ground truth: matched / missing / extra)
#   5. Freshness            (create / rename / delete → visible in each tool)
#   6. Disk usage           (rfind size / largest vs find + du)
#
# Scopes
#   linux  — everything rfind indexes except periodic roots (/mnt/c). Fast for find.
#   full   — including periodic roots. find over a WSL2 Windows drive is slow,
#            so it runs under --find-timeout and is skipped after the first timeout.
#
# Tools that aren't installed are skipped (plocate: apt install plocate).
#
# Usage: sudo scripts/bench.sh [--quick] [--runs N] [--find-timeout SEC] [--keep]
#   --quick          linux scope only (no Windows-drive find / updatedb)
#   --runs N         timed runs per query for rfind and the locates (default 5)
#   --find-timeout   seconds before a full-scope find is abandoned (default 300)
#   --keep           keep the work dir (query outputs, locate databases)

set -uo pipefail

RUNS=5
FIND_TIMEOUT=300
QUICK=0
KEEP=0
CONFIG=${RFIND_CONFIG:-/etc/rfindd/config.toml}
RFIND=${RFIND:-rfind}

while [ $# -gt 0 ]; do
    case "$1" in
        --quick)        QUICK=1 ;;
        --runs)         RUNS=$2; shift ;;
        --find-timeout) FIND_TIMEOUT=$2; shift ;;
        --keep)         KEEP=1 ;;
        -h|--help)      sed -n '2,26p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "unknown option: $1" >&2; exit 2 ;;
    esac
    shift
done

# ── Presentation ───────────────────────────────────────────────────────────────
if [ -t 1 ]; then
    B=$'\e[1m'; D=$'\e[2m'; R=$'\e[0m'
    RED=$'\e[31m'; GRN=$'\e[32m'; YEL=$'\e[33m'; MAG=$'\e[35m'; CYN=$'\e[36m'
    TTY=1
else
    B= D= R= RED= GRN= YEL= MAG= CYN=; TTY=0
fi
OK="${GRN}✔${R}"; BAD="${RED}✘${R}"; WARN="${YEL}▲${R}"

banner() {
    local t=" $1 " w=78
    local pad=$(( (w - ${#t}) / 2 ))
    printf '\n%s%s' "$B$CYN" "╔"; printf '═%.0s' $(seq 1 $w); printf '╗\n'
    printf '║%*s%s%*s║\n' $pad '' "$t" $(( w - pad - ${#t} )) ''
    printf '╚'; printf '═%.0s' $(seq 1 $w); printf '╝%s\n' "$R"
}
section() { printf '\n%s▌ %s%s\n%s' "$B$MAG" "$1" "$R" "$D"; printf '─%.0s' $(seq 1 80); printf '%s\n' "$R"; }
kv()      { printf '  %-28s %s\n' "$1" "$2"; }
note()    { printf '  %s%s%s\n' "$D" "$1" "$R"; }

fmt_ms() {  # 1234 → 1.23s, 87 → 87ms
    local ms=$1
    if [ "$ms" = "-" ]; then printf -- '-'; return; fi
    if [ "$ms" -ge 60000 ]; then printf '%dm%02ds' $((ms/60000)) $(((ms%60000)/1000))
    elif [ "$ms" -ge 1000 ]; then printf '%d.%02ds' $((ms/1000)) $(((ms%1000)/10))
    else printf '%dms' "$ms"; fi
}
fmt_bytes() { numfmt --to=iec --suffix=B "$1" 2>/dev/null || echo "${1}B"; }
fmt_num()   { printf "%'d" "$1" 2>/dev/null || echo "$1"; }
speedup() { # base_ms fast_ms
    local a=$1 b=$2
    if [ "$a" = "-" ] || [ "$b" = "-" ]; then printf -- '-'; return; fi
    [ "$b" -lt 1 ] && b=1
    awk -v a="$a" -v b="$b" 'BEGIN { r=a/b; if (r>=10) printf "×%d", r; else printf "×%.1f", r }'
}

now_ms() { echo $(( $(date +%s%N) / 1000000 )); }

# run_timed LABEL OUTFILE CMD... → sets ELAPSED (ms) and RC; shows a live spinner.
run_timed() {
    local label=$1 out=$2; shift 2
    local start; start=$(now_ms)
    "$@" >"$out" 2>/dev/null &
    local pid=$! i=0 frames='⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏'
    if [ $TTY -eq 1 ]; then
        while kill -0 $pid 2>/dev/null; do
            printf '\r  %s%s%s %s %s(%s)%s\e[K' "$CYN" "${frames:i++%10:1}" "$R" "$label" \
                "$D" "$(fmt_ms $(( $(now_ms) - start )))" "$R"
            sleep 0.1
        done
        printf '\r\e[K'
    fi
    wait $pid; RC=$?
    ELAPSED=$(( $(now_ms) - start ))
}

median() { sort -n | awk '{a[NR]=$1} END { print (NR%2) ? a[(NR+1)/2] : int((a[NR/2]+a[NR/2+1])/2) }'; }

# ── Preconditions ──────────────────────────────────────────────────────────────
[ "$(id -u)" -eq 0 ] || { echo "${RED}Run as root${R} (the rfind index and updatedb need it)"; exit 1; }
command -v "$RFIND" >/dev/null || { echo "${RED}missing: $RFIND${R}"; exit 1; }
PID=$(pgrep -xo rfindd || true)
[ -n "$PID" ] || { echo "${RED}rfindd is not running${R} (systemctl start rfindd)"; exit 1; }

# GNU locate by its real name: installing plocate re-points /usr/bin/locate.
GLOCATE=""; GUPDATEDB=""
if command -v locate.findutils >/dev/null; then GLOCATE=locate.findutils; GUPDATEDB=updatedb.findutils
elif locate --version 2>/dev/null | grep -q GNU; then GLOCATE=locate; GUPDATEDB=updatedb; fi
PLOCATE=""; PUPDATEDB=""
if command -v plocate >/dev/null; then
    PLOCATE=plocate
    PUPDATEDB=$(command -v updatedb.plocate || echo /usr/sbin/updatedb.plocate)
fi

WORK=$(mktemp -d /tmp/rfind-bench.XXXXXX)
FRESH_DIR=/root/.rfind-bench-$$
cleanup() { rm -rf "$FRESH_DIR"; [ $KEEP -eq 1 ] || rm -rf "$WORK"; }
trap cleanup EXIT
trap 'echo; echo "${YEL}interrupted${R}"; exit 130' INT

# Same scope as rfind: its excludes, and its periodic roots for the linux scope.
EXCLUDES=$("$RFIND" config get scan.exclude 2>/dev/null | xargs)
PERIODIC=$("$RFIND" --json config periodic list 2>/dev/null | grep -o '"path":"[^"]*"' | cut -d'"' -f4 | xargs)
PERIODIC_RE=$(echo "$PERIODIC" | tr ' ' '|')
[ -n "$PERIODIC" ] || QUICK=1

linux_only() { if [ -n "$PERIODIC_RE" ]; then grep -Ev "^($PERIODIC_RE)(/|\$)"; else cat; fi; }

# find with rfind's exclusions pruned. Usage: find_scoped SCOPE START PRED...
find_scoped() {
    local scope=$1 start=$2; shift 2
    local prune=() p
    for p in $EXCLUDES; do prune+=(-path "$p" -o); done
    if [ "$scope" = linux ]; then for p in $PERIODIC; do prune+=(-path "$p" -o); done; fi
    if [ ${#prune[@]} -gt 0 ]; then
        unset 'prune[${#prune[@]}-1]'
        find "$start" \( "${prune[@]}" \) -prune -o \( "$@" \) -print 2>/dev/null
    else
        find "$start" \( "$@" \) -print 2>/dev/null
    fi
}

TOOLS="rfind${PLOCATE:+, plocate}${GLOCATE:+, GNU locate}, find"
banner "rfind benchmark · $TOOLS"
printf '  %s%s%s\n' "$D" "$(date '+%Y-%m-%d %H:%M:%S')  ·  runs=$RUNS  ·  find-timeout=${FIND_TIMEOUT}s  ·  $([ $QUICK -eq 1 ] && echo 'linux scope only' || echo 'linux + full scope')" "$R"

# ── 1. Environment ─────────────────────────────────────────────────────────────
section "1 · Environment"
kv "Kernel"   "$(uname -r)"
grep -qi microsoft /proc/version && kv "Platform" "WSL2" || kv "Platform" "$(systemd-detect-virt 2>/dev/null || echo unknown)"
kv "CPU"      "$(nproc) cores · $(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2 | xargs)"
kv "Memory"   "$(free -h | awk '/Mem:/ {print $2 " total, " $7 " available"}')"
kv "Excludes" "$EXCLUDES"
kv "Periodic roots" "${PERIODIC:-none}"
kv "rfind"    "$("$RFIND" --version) · monitoring: $("$RFIND" --json health 2>/dev/null | grep -o '"monitoring":"[^"]*"' | cut -d'"' -f4)"
[ -n "$PLOCATE" ] && kv "plocate" "$(plocate --version | head -1)" || kv "plocate" "${D}not installed (apt install plocate)${R}"
[ -n "$GLOCATE" ] && kv "GNU locate" "$($GLOCATE --version | head -1)" || kv "GNU locate" "${D}not installed${R}"
kv "find"     "$(find --version | head -1)"

# ── 2. Index build ─────────────────────────────────────────────────────────────
section "2 · Index build"
PRUNE="$EXCLUDES"; [ $QUICK -eq 1 ] && PRUNE="$PRUNE $PERIODIC"
SCOPE_NOTE=$([ $QUICK -eq 1 ] && echo ', linux scope' || echo '')
if [ -n "$PLOCATE" ]; then
    PDB=$WORK/plocate.db
    run_timed "updatedb.plocate (rfind's scope)" /dev/null \
        "$PUPDATEDB" -U / -o "$PDB" --prunepaths "$PRUNE" --prunefs "" --prune-bind-mounts yes
    P_BUILD=$ELAPSED
    P_N=$(plocate -d "$PDB" -c / 2>/dev/null || echo 0)
    kv "plocate · updatedb" "$(fmt_ms $P_BUILD)  ${D}$(fmt_num "$P_N") entries$SCOPE_NOTE${R}"
fi
if [ -n "$GLOCATE" ]; then
    LDB=$WORK/locatedb
    run_timed "updatedb (GNU, rfind's scope)" /dev/null \
        "$GUPDATEDB" --localpaths=/ --prunepaths="$PRUNE" --prunefs='' --output="$LDB"
    L_BUILD=$ELAPSED
    L_N=$($GLOCATE -d "$LDB" -c '*' 2>/dev/null || echo 0)
    kv "GNU locate · updatedb" "$(fmt_ms $L_BUILD)  ${D}single-threaded find, $(fmt_num "$L_N") entries$SCOPE_NOTE${R}"
fi
JLOG=$(journalctl -u rfindd --no-pager -o cat 2>/dev/null | sed 's/\x1b\[[0-9;]*m//g')
LIVE_SCAN=$(echo "$JLOG" | grep -oE 'Scanned / : [0-9]+ entries, [0-9]+ dirs in [0-9.]+s.*|Scanned /: [0-9]+ entries, [0-9]+ dirs in [0-9.]+s.*' | tail -1)
BG_SCAN=$(echo "$JLOG" | grep -oE "Scanned (${PERIODIC_RE:-^$}): [0-9]+ entries, [0-9]+ dirs in [0-9.]+s.*" | tail -1)
if [ -n "$LIVE_SCAN" ]; then
    kv "rfind · live roots" "$(echo "$LIVE_SCAN" | grep -oE 'in [0-9.]+s' | cut -c4-)  ${D}$(echo "$LIVE_SCAN" | grep -oE '[0-9]+ entries') · parallel getdents64 + statx$(echo "$LIVE_SCAN" | grep -q io_uring && echo ' (io_uring)')${R}"
else
    kv "rfind · live roots" "${D}no full scan in the log yet (run: rfind update)${R}"
fi
[ -n "$BG_SCAN" ] && kv "rfind · periodic roots" "$(echo "$BG_SCAN" | grep -oE 'in [0-9.]+s' | cut -c4-)  ${D}$(echo "$BG_SCAN" | grep -oE '[0-9]+ entries') · background${R}"
note "rfind builds once, then stays current from kernel events; the locates must re-run updatedb."

# ── 3. Footprint ───────────────────────────────────────────────────────────────
section "3 · Footprint"
# From rfind itself: counts the compressed store (index.compress) and the WAL.
DB_BYTES=$("$RFIND" --json health 2>/dev/null | grep -o '"db_size_bytes":[0-9]*' | cut -d: -f2)
DB_BYTES=${DB_BYTES:-0}
RFIND_N=$("$RFIND" --json health 2>/dev/null | grep -o '"entries":[0-9]*' | cut -d: -f2)
RSS_KB=$(awk '/VmRSS/ {print $2}' /proc/$PID/status)
CPU_S=$(awk -v hz="$(getconf CLK_TCK)" '{printf "%.1f", ($14+$15)/hz}' /proc/$PID/stat)
WATCHES=$(cat /proc/$PID/fdinfo/* 2>/dev/null | grep -c '^inotify wd' || true)
MARKS=$(cat /proc/$PID/fdinfo/* 2>/dev/null | grep -c '^fanotify ' || true)
per() { [ "${1:-0}" -gt 0 ] && echo $(( $2 / $1 )) || echo -; }
printf '  %s%-22s %12s %12s %12s%s\n' "$B" "" "plocate" "GNU locate" "rfind" "$R"
printf '  %-22s %12s %12s %12s\n' "Entries" "$(fmt_num "${P_N:-0}")" "$(fmt_num "${L_N:-0}")" "$(fmt_num "${RFIND_N:-0}")"
printf '  %-22s %12s %12s %12s\n' "Index size" \
    "$([ -n "$PLOCATE" ] && fmt_bytes "$(stat -c %s "$PDB")" || echo -)" \
    "$([ -n "$GLOCATE" ] && fmt_bytes "$(stat -c %s "$LDB")" || echo -)" "$(fmt_bytes $DB_BYTES)"
printf '  %-22s %12s %12s %12s\n' "Bytes / entry" \
    "$([ -n "$PLOCATE" ] && per "${P_N:-0}" "$(stat -c %s "$PDB")" || echo -)" \
    "$([ -n "$GLOCATE" ] && per "${L_N:-0}" "$(stat -c %s "$LDB")" || echo -)" "$(per "${RFIND_N:-0}" $DB_BYTES)"
printf '  %-22s %12s %12s %12s\n' "Stores" "paths" "paths" "paths+size+mtime"
printf '  %-22s %12s %12s %12s\n' "Compression" "custom" "front-coded" "$("$RFIND" config get index.compress 2>/dev/null | grep -q true && echo zstd pages || echo none)"
printf '  %-22s %12s %12s %12s\n' "Daemon RSS" "-" "-" "$(fmt_bytes $((RSS_KB*1024)))"
printf '  %-22s %12s %12s %12s\n' "Daemon CPU (total)" "-" "-" "${CPU_S}s"
printf '  %-22s %12s %12s %12s\n' "Kernel watches" "-" "-" "$([ "$MARKS" -gt 0 ] && echo "fanotify" || echo "$(fmt_num "$WATCHES") inotify")"
note "The locates store compressed paths only; rfind also stores sizes and mtimes and indexes names for instant lookups."

# ── 4. Queries ─────────────────────────────────────────────────────────────────
QUERIES=(
    "exact|Exact name  passwd"
    "exact_win|Exact name  notepad.exe"
    "glob|Glob  *.conf"
    "glob_win|Glob  *.dll"
    "regex|Regex  ^lib.*\.so\.[0-9]+\$"
    "under|Under /etc  *.conf"
)
q_rfind() { case $1 in
    exact)     $RFIND name passwd ;;
    exact_win) $RFIND name notepad.exe ;;
    glob)      $RFIND name '*.conf' ;;
    glob_win)  $RFIND name '*.dll' ;;
    regex)     $RFIND name --regex '^lib.*\.so\.[0-9]+$' ;;
    under)     $RFIND name '*.conf' --under /etc ;;
esac; }
q_plocate() { case $1 in
    exact)     plocate -d "$PDB" -b --regex '^passwd$' ;;
    exact_win) plocate -d "$PDB" -b --regex '^notepad\.exe$' ;;
    glob)      plocate -d "$PDB" -b '*.conf' ;;
    glob_win)  plocate -d "$PDB" -b '*.dll' ;;
    regex)     plocate -d "$PDB" -b --regex '^lib.*\.so\.[0-9]+$' ;;
    under)     plocate -d "$PDB" '/etc/*.conf' ;;
esac; }
q_locate() { case $1 in
    exact)     $GLOCATE -d "$LDB" -b '\passwd' ;;
    exact_win) $GLOCATE -d "$LDB" -b '\notepad.exe' ;;
    glob)      $GLOCATE -d "$LDB" -b '*.conf' ;;
    glob_win)  $GLOCATE -d "$LDB" -b '*.dll' ;;
    regex)     $GLOCATE -d "$LDB" -b --regextype posix-extended -r '^lib.*\.so\.[0-9]+$' ;;
    under)     $GLOCATE -d "$LDB" '/etc/*.conf' ;;
esac; }
q_find() { local scope=$1; case $2 in
    exact)     find_scoped "$scope" / -name passwd ;;
    exact_win) find_scoped "$scope" / -name notepad.exe ;;
    glob)      find_scoped "$scope" / -name '*.conf' ;;
    glob_win)  find_scoped "$scope" / -name '*.dll' ;;
    regex)     find_scoped "$scope" / -regextype posix-extended -regex '.*/lib[^/]*\.so\.[0-9]+' ;;
    under)     find_scoped "$scope" /etc -name '*.conf' ;;
esac; }
export -f q_rfind q_plocate q_locate q_find find_scoped
export RFIND PDB LDB GLOCATE EXCLUDES PERIODIC

# accuracy TRUTH OTHER → "✔ 100%" or "✘ 99.1% (-missing +extra)"
accuracy() {
    local truth=$1 other=$2
    sort -u "$truth" -o "$truth.s"; sort -u "$other" -o "$other.s"
    local total missing extra
    total=$(wc -l <"$truth.s")
    missing=$(comm -23 "$truth.s" "$other.s" | wc -l)
    extra=$(comm -13 "$truth.s" "$other.s" | wc -l)
    if [ "$missing" -eq 0 ] && [ "$extra" -eq 0 ]; then
        printf '%s %s' "$OK" "100%"
    else
        local pct; pct=$(awk -v t="$total" -v m="$missing" 'BEGIN { printf "%.1f%%", t ? 100*(t-m)/t : 0 }')
        printf '%s %s %s(-%s +%s)%s' "$([ "$missing" -eq 0 ] && echo "$WARN" || echo "$BAD")" "$pct" "$D" "$missing" "$extra" "$R"
    fi
}

# timed TOOL ID → median ms over RUNS into T, output in $WORK/ID.TOOL
timed() {
    local tool=$1 id=$2 times=() i
    for i in $(seq 1 "$RUNS"); do
        run_timed "$tool  run $i/$RUNS" "$WORK/$id.$tool" bash -c "q_$tool $id"
        times+=("$ELAPSED")
    done
    T=$(printf '%s\n' "${times[@]}" | median)
}

declare -A T_RFIND T_PLOCATE T_LOCATE T_FIND_L T_FIND_F

section "4 · Search latency & accuracy  (median of $RUNS · find: 1 run, warm cache)"
FULL_FIND_DEAD=0
row() { printf '    %-8s %10s   %-26s %s\n' "$1" "$2" "$3" "$4"; }
for q in "${QUERIES[@]}"; do
    id=${q%%|*}; label=${q#*|}
    printf '  %s%s%s\n' "$B" "$label" "$R"

    run_timed "find   (linux scope)" "$WORK/$id.find.linux" bash -c "q_find linux $id"
    T_FIND_L[$id]=$ELAPSED
    N_L=$(wc -l <"$WORK/$id.find.linux")

    timed rfind "$id"; T_RFIND[$id]=$T
    linux_only <"$WORK/$id.rfind" >"$WORK/$id.rfind.linux"
    row rfind "$(fmt_ms "$T")" "$(accuracy "$WORK/$id.find.linux" "$WORK/$id.rfind.linux")" \
        "${D}$(fmt_num "$(wc -l <"$WORK/$id.rfind.linux")") results · linux scope${R}"
    T_PLOCATE[$id]="-"
    if [ -n "$PLOCATE" ]; then
        timed plocate "$id"; T_PLOCATE[$id]=$T
        linux_only <"$WORK/$id.plocate" >"$WORK/$id.plocate.linux"
        row plocate "$(fmt_ms "$T")" "$(accuracy "$WORK/$id.find.linux" "$WORK/$id.plocate.linux")" \
            "${D}$(fmt_num "$(wc -l <"$WORK/$id.plocate.linux")") results${R}"
    fi
    T_LOCATE[$id]="-"
    if [ -n "$GLOCATE" ]; then
        timed locate "$id"; T_LOCATE[$id]=$T
        linux_only <"$WORK/$id.locate" >"$WORK/$id.locate.linux"
        row locate "$(fmt_ms "$T")" "$(accuracy "$WORK/$id.find.linux" "$WORK/$id.locate.linux")" \
            "${D}$(fmt_num "$(wc -l <"$WORK/$id.locate.linux")") results${R}"
    fi
    row find "$(fmt_ms "${T_FIND_L[$id]}")" "${D}ground truth${R}" "${D}$(fmt_num "$N_L") results${R}"

    T_FIND_F[$id]="-"
    if [ $QUICK -eq 0 ]; then
        if [ $FULL_FIND_DEAD -eq 1 ]; then
            row find skipped "${D}full scope — an earlier run exceeded ${FIND_TIMEOUT}s${R}" ""
        else
            run_timed "find   (full scope, incl. $PERIODIC — slow over 9p)" "$WORK/$id.find.full" \
                timeout "$FIND_TIMEOUT" bash -c "q_find full $id"
            if [ $RC -eq 124 ]; then
                FULL_FIND_DEAD=1
                row find ">$(fmt_ms $((FIND_TIMEOUT*1000)))" "${RED}timed out${R}" "${D}full scope — remaining full-scope finds skipped${R}"
            else
                T_FIND_F[$id]=$ELAPSED
                row find "$(fmt_ms "$ELAPSED")" "${D}full scope · ground truth${R}" \
                    "${D}rfind ${R}$(accuracy "$WORK/$id.find.full" "$WORK/$id.rfind")"
            fi
        fi
    fi
done
[ $QUICK -eq 1 ] && [ -n "$PERIODIC" ] && note "Quick mode: the locate databases cover only the linux scope; rfind still searches $PERIODIC too."
note "Accuracy is each tool's result set vs find's. -missing: find saw it, the tool didn't · +extra: the reverse."
note "Small diffs are normal on a live system (files change between runs)."

# ── 5. Freshness ───────────────────────────────────────────────────────────────
section "5 · Freshness  (how long until a change is visible)"
TAG="rfindbench$$x$RANDOM"
mkdir -p "$FRESH_DIR"

# poll_rfind PATTERN EXPECT(present|absent) → ms until rfind agrees, or "-" after 20s
poll_rfind() {
    local pat=$1 want=$2 start; start=$(now_ms)
    while [ $(( $(now_ms) - start )) -lt 20000 ]; do
        local hit; hit=$($RFIND name "$pat" 2>/dev/null | grep -c "^$FRESH_DIR" || true)
        if { [ "$want" = present ] && [ "$hit" -gt 0 ]; } || { [ "$want" = absent ] && [ "$hit" -eq 0 ]; }; then
            echo $(( $(now_ms) - start )); return
        fi
        sleep 0.1
    done
    echo "-"
}
STALE="${BAD} until updatedb"
fresh_row() { # label rfind_ms
    local rs
    if [ "$2" = "-" ]; then rs="${BAD} not within 20s"; else rs="${OK} $(fmt_ms "$2")"; fi
    printf '  %-30s %-24s %-28s %s\n' "$1" "$rs" "$STALE" "${OK} immediate (walks the disk)"
}
printf '  %s%-30s %-15s %-19s %s%s\n' "$B" "Change" "rfind" "plocate / locate" "find" "$R"

mkdir -p "$FRESH_DIR/new/a/b"; head -c 3000 /dev/zero > "$FRESH_DIR/new/a/b/$TAG.txt"
[ $TTY -eq 1 ] && printf '\r  %s…%s waiting for rfind to see the new file\e[K' "$CYN" "$R"
T=$(poll_rfind "$TAG.txt" present)
CREATE_SEEN=$T
[ $TTY -eq 1 ] && printf '\r\e[K'
fresh_row "Create (in new nested dirs)" "$T"

mv "$FRESH_DIR/new/a" "$FRESH_DIR/new/renamed"
fresh_row "Rename directory" "$(poll_rfind renamed present)"

touch "$FRESH_DIR/new/renamed/b/after-$TAG.txt"
fresh_row "Create inside renamed dir" "$(poll_rfind "after-$TAG.txt" present)"

head -c 7000 /dev/zero >> "$FRESH_DIR/new/renamed/b/$TAG.txt"
start=$(now_ms); SZ=-
while [ $(( $(now_ms) - start )) -lt 20000 ]; do
    if $RFIND --json name "$TAG.txt" 2>/dev/null | grep -q '"size":10000'; then SZ=$(( $(now_ms) - start )); break; fi
    sleep 0.1
done
fresh_row "Append (size 3K → 10K)" "$SZ"

rm -rf "$FRESH_DIR/new"
if [ "$CREATE_SEEN" = "-" ]; then
    printf '  %-30s %s\n' "Delete tree" "${D}n/a — rfind never saw the file, so 'gone' proves nothing${R}"
else
    fresh_row "Delete tree" "$(poll_rfind "$TAG.txt" absent)"
fi
note "rfind applies kernel events every 5s, so expect 0–5s. Periodic roots ($PERIODIC) refresh only on rescan."

# ── 6. Disk usage ──────────────────────────────────────────────────────────────
section "6 · Disk usage  (/usr: total size and 10 largest files)"
run_timed "rfind size /usr" "$WORK/size.rfind" "$RFIND" --json size /usr --top 0
T_SZ_R=$ELAPSED
R_BYTES=$(grep -o '"total":{[^}]*' "$WORK/size.rfind" | grep -o '"bytes":[0-9]*' | cut -d: -f2)
run_timed "find /usr (sum of file sizes)" "$WORK/size.find" bash -c "find /usr -type f -printf '%s\n' 2>/dev/null | awk '{s+=\$1} END {print s+0}'"
T_SZ_F=$ELAPSED; F_BYTES=$(cat "$WORK/size.find")
run_timed "du -s --apparent-size /usr" "$WORK/size.du" du -s --apparent-size -B1 /usr
T_SZ_D=$ELAPSED
MATCH=$([ "$R_BYTES" = "$F_BYTES" ] && echo "$OK exact" || echo "$WARN $(awk -v a="$R_BYTES" -v b="$F_BYTES" 'BEGIN{printf "%.3f%% off", b?100*(a-b)/b:0}')")
printf '  %-24s %10s   %-14s %s\n' "rfind size" "$(fmt_ms $T_SZ_R)" "$(fmt_bytes "${R_BYTES:-0}")" "$MATCH vs find"
printf '  %-24s %10s   %-14s %s\n' "find -printf %s | sum" "$(fmt_ms $T_SZ_F)" "$(fmt_bytes "${F_BYTES:-0}")" "${D}ground truth (regular files)${R}"
printf '  %-24s %10s   %-14s %s\n' "du --apparent-size" "$(fmt_ms $T_SZ_D)" "$(fmt_bytes "$(cut -f1 "$WORK/size.du")")" "${D}counts hard-linked files once, adds directories${R}"

run_timed "rfind largest" "$WORK/largest.rfind" bash -c "$RFIND --json largest --under /usr --limit 10 | grep -o '\"path\":\"[^\"]*\"' | cut -d'\"' -f4"
T_LG_R=$ELAPSED
run_timed "find | sort (largest)" "$WORK/largest.find" bash -c "find /usr -type f -printf '%s %p\n' 2>/dev/null | sort -rn | head -10 | cut -d' ' -f2-"
T_LG_F=$ELAPSED
printf '  %-24s %10s   %s\n' "rfind largest" "$(fmt_ms $T_LG_R)" "$(accuracy "$WORK/largest.find" "$WORK/largest.rfind") same 10 files as find"
printf '  %-24s %10s   %s\n' "find | sort -rn | head" "$(fmt_ms $T_LG_F)" "${D}ground truth${R}"
note "Directory sizes come from the indexed file sizes; nothing extra is stored per directory."

# ── Summary ────────────────────────────────────────────────────────────────────
section "Summary"
printf '  %s%-24s %8s %8s %8s %8s %9s %9s %10s%s\n' "$B" "Query" "rfind" "plocate" "locate" "find" "find-full" "vs find" "vs plocate" "$R"
for q in "${QUERIES[@]}"; do
    id=${q%%|*}; label=${q#*|}
    base=$([ "${T_FIND_F[$id]}" != "-" ] && echo "${T_FIND_F[$id]}" || echo "${T_FIND_L[$id]}")
    printf '  %-24s %8s %8s %8s %8s %9s %s%9s%s %10s\n' "$label" \
        "$(fmt_ms "${T_RFIND[$id]}")" "$(fmt_ms "${T_PLOCATE[$id]}")" "$(fmt_ms "${T_LOCATE[$id]}")" \
        "$(fmt_ms "${T_FIND_L[$id]}")" "$(fmt_ms "${T_FIND_F[$id]}")" \
        "$GRN$B" "$(speedup "$base" "${T_RFIND[$id]}")" "$R" "$(speedup "${T_PLOCATE[$id]}" "${T_RFIND[$id]}")"
done
printf '  %-24s %8s %8s %8s %8s %9s %s%9s%s %10s\n' "Size of /usr" "$(fmt_ms $T_SZ_R)" "-" "-" "$(fmt_ms $T_SZ_F)" "-" "$GRN$B" "$(speedup $T_SZ_F $T_SZ_R)" "$R" "-"
note "'vs find' uses the full-scope find time when it completed, otherwise linux scope (an under-estimate)."
[ $KEEP -eq 1 ] && note "Outputs kept in $WORK"
echo
