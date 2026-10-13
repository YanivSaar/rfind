#!/usr/bin/env bash
# rfind benchmark — compares GNU find, GNU locate and rfind on the live system.
#
# Metrics
#   1. Environment & index footprint  (entries, DB size, daemon RSS/CPU/fds/watches)
#   2. Index build time               (updatedb, timed now; rfind, from the daemon log)
#   3. Search latency                 (median of N runs; find run once per scope)
#   4. Accuracy vs find               (find is ground truth: matched / missing / extra)
#   5. Freshness                      (create / rename / delete → visible in each tool)
#
# Scopes
#   linux  — everything rfind indexes except periodic roots (/mnt/c). Fast for find.
#   full   — including periodic roots. find over a WSL2 Windows drive is slow,
#            so it runs under --find-timeout and is skipped after the first timeout.
#
# Usage: sudo scripts/bench.sh [--quick] [--runs N] [--find-timeout SEC] [--keep]
#   --quick          linux scope only (no Windows-drive find / updatedb)
#   --runs N         timed runs per query for rfind and locate (default 5)
#   --find-timeout   seconds before a full-scope find is abandoned (default 300)
#   --keep           keep the work dir (query outputs, locate DB)

set -uo pipefail

RUNS=5
FIND_TIMEOUT=300
QUICK=0
KEEP=0
CONFIG=/etc/rfindd/config.toml
RFIND=${RFIND:-rfind}

while [ $# -gt 0 ]; do
    case "$1" in
        --quick)        QUICK=1 ;;
        --runs)         RUNS=$2; shift ;;
        --find-timeout) FIND_TIMEOUT=$2; shift ;;
        --keep)         KEEP=1 ;;
        -h|--help)      sed -n '2,22p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "unknown option: $1" >&2; exit 2 ;;
    esac
    shift
done

# ── Presentation ───────────────────────────────────────────────────────────────
if [ -t 1 ]; then
    B=$'\e[1m'; D=$'\e[2m'; R=$'\e[0m'
    RED=$'\e[31m'; GRN=$'\e[32m'; YEL=$'\e[33m'; BLU=$'\e[34m'; MAG=$'\e[35m'; CYN=$'\e[36m'
    TTY=1
else
    B= D= R= RED= GRN= YEL= BLU= MAG= CYN=; TTY=0
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
    [ "$a" = "-" ] || [ "$b" = "-" ] && { printf -- '-'; return; }
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
[ "$(id -u)" -eq 0 ] || { echo "${RED}Run as root${R} (updatedb and the rfind index need it)"; exit 1; }
for t in find locate updatedb "$RFIND"; do
    command -v "$t" >/dev/null || { echo "${RED}missing: $t${R}"; exit 1; }
done
PID=$(pgrep -xo rfindd || true)
[ -n "$PID" ] || { echo "${RED}rfindd is not running${R} (systemctl start rfindd)"; exit 1; }

WORK=$(mktemp -d /tmp/rfind-bench.XXXXXX)
FRESH_DIR=/root/.rfind-bench-$$
cleanup() { rm -rf "$FRESH_DIR"; [ $KEEP -eq 1 ] || rm -rf "$WORK"; }
trap cleanup EXIT
trap 'echo; echo "${YEL}interrupted${R}"; exit 130' INT

# Same scope as rfind: its excludes, and its periodic roots for the linux scope.
EXCLUDES=$(sed -n '/^exclude/,/]/p' "$CONFIG" | grep -o '"[^"]*"' | tr -d '"' | xargs)
PERIODIC=$(grep -A1 '^\[\[scan.periodic_roots\]\]' "$CONFIG" | grep -o '"[^"]*"' | tr -d '"' | xargs)
PERIODIC_RE=$(echo "$PERIODIC" | tr ' ' '|')
[ -n "$PERIODIC" ] || QUICK=1

linux_only() { if [ -n "$PERIODIC_RE" ]; then grep -Ev "^($PERIODIC_RE)(/|\$)"; else cat; fi; }

# find with rfind's exclusions pruned. Usage: find_scoped SCOPE START PRED...
find_scoped() {
    local scope=$1 start=$2; shift 2
    local prune=() p
    for p in $EXCLUDES; do prune+=(-path "$p" -o); done
    if [ "$scope" = linux ]; then for p in $PERIODIC; do prune+=(-path "$p" -o); done; fi
    unset 'prune[${#prune[@]}-1]'
    find "$start" \( "${prune[@]}" \) -prune -o \( "$@" \) -print 2>/dev/null
}

banner "rfind benchmark · find vs locate vs rfind"
printf '  %s%s%s\n' "$D" "$(date '+%Y-%m-%d %H:%M:%S')  ·  runs=$RUNS  ·  find-timeout=${FIND_TIMEOUT}s  ·  $([ $QUICK -eq 1 ] && echo 'linux scope only' || echo 'linux + full scope')" "$R"

# ── 1. Environment & footprint ─────────────────────────────────────────────────
section "1 · Environment"
kv "Kernel"   "$(uname -r)"
grep -qi microsoft /proc/version && kv "Platform" "WSL2" || kv "Platform" "$(systemd-detect-virt 2>/dev/null || echo unknown)"
kv "CPU"      "$(nproc) cores · $(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2 | xargs)"
kv "Memory"   "$(free -h | awk '/Mem:/ {print $2 " total, " $7 " available"}')"
kv "Excludes" "$EXCLUDES"
kv "Periodic roots" "${PERIODIC:-none}"
kv "Tools"    "$(find --version | head -1) · $(locate --version | head -1)"

section "2 · Index build"
LOCATEDB=$WORK/locatedb
PRUNE="$EXCLUDES"; [ $QUICK -eq 1 ] && PRUNE="$PRUNE $PERIODIC"
run_timed "updatedb (building locate DB with rfind's scope)" /dev/null \
    updatedb --localpaths=/ --prunepaths="$PRUNE" --prunefs='' --output="$LOCATEDB"
UPDATEDB_MS=$ELAPSED
LOCATE_N=$(locate -d "$LOCATEDB" -c '*' 2>/dev/null || echo 0)
kv "locate · updatedb" "$(fmt_ms $UPDATEDB_MS)  ${D}single-threaded find, $(fmt_num "$LOCATE_N") entries$([ $QUICK -eq 1 ] && echo ', linux scope')${R}"

JLOG=$(journalctl -u rfindd --no-pager -o cat 2>/dev/null | sed 's/\x1b\[[0-9;]*m//g')
LIVE_SCAN=$(echo "$JLOG" | grep -oE 'Ready \| [0-9]+ entries \| [0-9]+ dirs \| [0-9]+ entries/s \| [0-9.]+s' | tail -1)
BG_SCAN=$(echo "$JLOG" | grep -oE 'Background scan of .* done: [0-9]+ entries in [0-9.]+s' | tail -1)
if [ -n "$LIVE_SCAN" ]; then
    kv "rfind · live roots" "$(echo "$LIVE_SCAN" | awk -F' \\| ' '{print $NF}')  ${D}$(echo "$LIVE_SCAN" | awk -F' \\| ' '{print $2 ", " $4}') · parallel getdents64+statx${R}"
else
    kv "rfind · live roots" "${D}no full scan in the log yet (run: rfind update)${R}"
fi
[ -n "$BG_SCAN" ] && kv "rfind · periodic roots" "$(echo "$BG_SCAN" | grep -oE '[0-9.]+s$')  ${D}$(echo "$BG_SCAN" | grep -oE '[0-9]+ entries') · background${R}"
note "rfind builds once, then stays current from events; locate must re-run updatedb to see any change."

section "3 · Footprint"
DB=$(grep -oP 'db_path\s*=\s*"\K[^"]+' "$CONFIG")
DB_BYTES=$(( $(stat -c %s "$DB" 2>/dev/null || echo 0) + $(stat -c %s "$DB-wal" 2>/dev/null || echo 0) ))
RFIND_N=$($RFIND health 2>/dev/null | awk -F: '/^Entries/ {gsub(/ /,"",$2); print $2}')
RSS_KB=$(awk '/VmRSS/ {print $2}' /proc/$PID/status)
CPU_S=$(awk -v hz="$(getconf CLK_TCK)" '{printf "%.1f", ($14+$15)/hz}' /proc/$PID/stat)
WATCHES=$(cat /proc/$PID/fdinfo/* 2>/dev/null | grep -c '^inotify wd' || true)
printf '  %s%-22s %14s %14s%s\n' "$B" "" "locate" "rfind" "$R"
printf '  %-22s %14s %14s\n' "Entries"        "$(fmt_num "$LOCATE_N")" "$(fmt_num "${RFIND_N:-0}")"
printf '  %-22s %14s %14s\n' "Index size"     "$(fmt_bytes "$(stat -c %s "$LOCATEDB")")" "$(fmt_bytes $DB_BYTES)"
printf '  %-22s %14s %14s\n' "Bytes / entry"  "$(( $(stat -c %s "$LOCATEDB") / (LOCATE_N>0?LOCATE_N:1) ))" "$(( DB_BYTES / (${RFIND_N:-1}>0?${RFIND_N:-1}:1) ))"
printf '  %-22s %14s %14s\n' "Daemon RSS"     "-" "$(fmt_bytes $((RSS_KB*1024)))"
printf '  %-22s %14s %14s\n' "Daemon CPU (total)" "-" "${CPU_S}s"
printf '  %-22s %14s %14s\n' "inotify watches" "-" "$(fmt_num "$WATCHES")"
printf '  %-22s %14s %14s\n' "Open fds"       "-" "$(ls /proc/$PID/fd | wc -l)"
note "locate's DB is front-compressed paths only; rfind stores mtime + indexes for name/path/time queries."

# ── Queries ────────────────────────────────────────────────────────────────────
# id | label | rfind args | locate args | find start | find predicate
QUERIES=(
    "exact|Exact name  passwd"
    "exact_win|Exact name  notepad.exe"
    "glob|Glob  *.conf"
    "glob_win|Glob  *.dll"
    "regex|Regex  ^lib.*\.so\.[0-9]+\$"
    "under|Under /etc  *.conf"
)
q_rfind() { case $1 in
    exact)     $RFIND --name passwd ;;
    exact_win) $RFIND --name notepad.exe ;;
    glob)      $RFIND --name '*.conf' ;;
    glob_win)  $RFIND --name '*.dll' ;;
    regex)     $RFIND --name --regex '^lib.*\.so\.[0-9]+$' ;;
    under)     $RFIND --name '*.conf' --under /etc ;;
esac; }
q_locate() { case $1 in
    exact)     locate -d "$LOCATEDB" -b '\passwd' ;;
    exact_win) locate -d "$LOCATEDB" -b '\notepad.exe' ;;
    glob)      locate -d "$LOCATEDB" -b '*.conf' ;;
    glob_win)  locate -d "$LOCATEDB" -b '*.dll' ;;
    regex)     locate -d "$LOCATEDB" -b --regextype posix-extended -r '^lib.*\.so\.[0-9]+$' ;;
    under)     locate -d "$LOCATEDB" '/etc/*.conf' ;;
esac; }
q_find() { local scope=$1; case $2 in
    exact)     find_scoped "$scope" / -name passwd ;;
    exact_win) find_scoped "$scope" / -name notepad.exe ;;
    glob)      find_scoped "$scope" / -name '*.conf' ;;
    glob_win)  find_scoped "$scope" / -name '*.dll' ;;
    regex)     find_scoped "$scope" / -regextype posix-extended -regex '.*/lib[^/]*\.so\.[0-9]+' ;;
    under)     find_scoped "$scope" /etc -name '*.conf' ;;
esac; }
export -f q_rfind q_locate q_find find_scoped
export RFIND LOCATEDB EXCLUDES PERIODIC

# accuracy TRUTH OTHER → "matched/total  -missing +extra" with a mark
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

declare -A T_RFIND T_LOCATE T_FIND_L T_FIND_F N_L N_F ACC_R_L ACC_L_L ACC_R_F ACC_L_F

section "4 · Search latency  (rfind/locate: median of $RUNS · find: 1 run, warm cache)"
FULL_FIND_DEAD=0
for q in "${QUERIES[@]}"; do
    id=${q%%|*}; label=${q#*|}
    printf '  %s%s%s\n' "$B" "$label" "$R"

    times=()
    for i in $(seq 1 "$RUNS"); do run_timed "rfind  run $i/$RUNS" "$WORK/$id.rfind" bash -c "q_rfind $id"; times+=("$ELAPSED"); done
    T_RFIND[$id]=$(printf '%s\n' "${times[@]}" | median)
    times=()
    for i in $(seq 1 "$RUNS"); do run_timed "locate run $i/$RUNS" "$WORK/$id.locate" bash -c "q_locate $id"; times+=("$ELAPSED"); done
    T_LOCATE[$id]=$(printf '%s\n' "${times[@]}" | median)

    run_timed "find   (linux scope)" "$WORK/$id.find.linux" bash -c "q_find linux $id"
    T_FIND_L[$id]=$ELAPSED
    linux_only <"$WORK/$id.rfind"  >"$WORK/$id.rfind.linux"
    linux_only <"$WORK/$id.locate" >"$WORK/$id.locate.linux"
    N_L[$id]=$(wc -l <"$WORK/$id.find.linux")
    ACC_R_L[$id]=$(accuracy "$WORK/$id.find.linux" "$WORK/$id.rfind.linux")
    ACC_L_L[$id]=$(accuracy "$WORK/$id.find.linux" "$WORK/$id.locate.linux")

    printf '    %-8s %10s   %-24s %s\n' "rfind"  "$(fmt_ms "${T_RFIND[$id]}")"  "${ACC_R_L[$id]}" "${D}$(fmt_num "$(wc -l <"$WORK/$id.rfind.linux")") results · linux scope${R}"
    printf '    %-8s %10s   %-24s %s\n' "locate" "$(fmt_ms "${T_LOCATE[$id]}")" "${ACC_L_L[$id]}" "${D}$(fmt_num "$(wc -l <"$WORK/$id.locate.linux")") results${R}"
    printf '    %-8s %10s   %-24s %s\n' "find"   "$(fmt_ms "${T_FIND_L[$id]}")" "${D}ground truth${R}" "${D}$(fmt_num "${N_L[$id]}") results${R}"

    T_FIND_F[$id]="-"
    if [ $QUICK -eq 0 ]; then
        if [ $FULL_FIND_DEAD -eq 1 ]; then
            printf '    %-8s %10s   %s\n' "find" "skipped" "${D}full scope — an earlier run exceeded ${FIND_TIMEOUT}s${R}"
        else
            run_timed "find   (full scope, incl. $PERIODIC — slow over 9p)" "$WORK/$id.find.full" \
                timeout "$FIND_TIMEOUT" bash -c "q_find full $id"
            if [ $RC -eq 124 ]; then
                FULL_FIND_DEAD=1; T_FIND_F[$id]="-"
                printf '    %-8s %10s   %s\n' "find" ">$(fmt_ms $((FIND_TIMEOUT*1000)))" "${RED}timed out${R} ${D}full scope — remaining full-scope finds skipped${R}"
            else
                T_FIND_F[$id]=$ELAPSED
                N_F[$id]=$(wc -l <"$WORK/$id.find.full")
                ACC_R_F[$id]=$(accuracy "$WORK/$id.find.full" "$WORK/$id.rfind")
                ACC_L_F[$id]=$(accuracy "$WORK/$id.find.full" "$WORK/$id.locate")
                printf '    %-8s %10s   %s\n' "find" "$(fmt_ms "$ELAPSED")" "${D}full scope · $(fmt_num "${N_F[$id]}") results · rfind ${R}${ACC_R_F[$id]}${D} · locate ${R}${ACC_L_F[$id]}"
            fi
        fi
    fi
done
[ $QUICK -eq 1 ] && [ -n "$PERIODIC" ] && note "Quick mode: locate's DB covers only the linux scope, while rfind still searches $PERIODIC too — rfind does more work per query."
note "Accuracy compares each tool to find's result set. -missing: find saw it, tool didn't · +extra: tool has it, find doesn't."
note "Small diffs are normal on a live system (files changing between runs)."

# ── 5. Freshness ───────────────────────────────────────────────────────────────
section "5 · Freshness  (how long until a change is visible)"
TAG="rfindbench$$x$RANDOM"
mkdir -p "$FRESH_DIR"

# poll_rfind PATTERN EXPECT(present|absent) → ms until rfind agrees, or "-" after 20s
poll_rfind() {
    local pat=$1 want=$2 start; start=$(now_ms)
    while [ $(( $(now_ms) - start )) -lt 20000 ]; do
        local hit; hit=$($RFIND --name "$pat" 2>/dev/null | grep -c "^$FRESH_DIR" || true)
        if { [ "$want" = present ] && [ "$hit" -gt 0 ]; } || { [ "$want" = absent ] && [ "$hit" -eq 0 ]; }; then
            echo $(( $(now_ms) - start )); return
        fi
        sleep 0.1
    done
    echo "-"
}
fresh_row() { # label rfind_ms locate_state
    local r=$2 rs
    if [ "$r" = "-" ]; then rs="${BAD} not within 20s"; else rs="${OK} $(fmt_ms "$r")"; fi
    printf '  %-34s %-26s %-28s %s\n' "$1" "$rs" "$3" "${OK} immediate (walks the disk)"
}
printf '  %s%-34s %-17s %-19s %s%s\n' "$B" "Change" "rfind" "locate" "find" "$R"

mkdir -p "$FRESH_DIR/new/a/b"; touch "$FRESH_DIR/new/a/b/$TAG.txt"
[ $TTY -eq 1 ] && printf '\r  %s…%s waiting for rfind to see the new file\e[K' "$CYN" "$R"
T=$(poll_rfind "$TAG.txt" present)
CREATE_SEEN=$T
L=$(locate -d "$LOCATEDB" -b "\\$TAG.txt" | wc -l)
[ $TTY -eq 1 ] && printf '\r\e[K'
fresh_row "Create (in new nested dirs)" "$T" "$([ "$L" -gt 0 ] && echo "$OK" || echo "$BAD stale until updatedb")"

mv "$FRESH_DIR/new/a" "$FRESH_DIR/new/renamed"
T=$(poll_rfind "renamed" present)
fresh_row "Rename directory" "$T" "$BAD stale until updatedb"

touch "$FRESH_DIR/new/renamed/b/after-$TAG.txt"
T=$(poll_rfind "after-$TAG.txt" present)
fresh_row "Create inside renamed dir" "$T" "$BAD stale until updatedb"

rm -rf "$FRESH_DIR/new"
if [ "$CREATE_SEEN" = "-" ]; then
    printf '  %-34s %s\n' "Delete tree" "${D}n/a — rfind never saw the file, so 'gone' proves nothing${R}"
else
    T=$(poll_rfind "$TAG.txt" absent)
    fresh_row "Delete tree" "$T" "$BAD stale until updatedb"
fi
note "rfind applies journaled events every 5s, so expect 0–5s. Periodic roots ($PERIODIC) refresh only on rescan."

# ── Summary ────────────────────────────────────────────────────────────────────
section "Summary"
printf '  %s%-26s %9s %9s %9s %9s %11s %11s%s\n' "$B" "Query" "rfind" "locate" "find" "find-full" "vs find" "vs locate" "$R"
for q in "${QUERIES[@]}"; do
    id=${q%%|*}; label=${q#*|}
    printf '  %-26s %9s %9s %9s %9s %s%11s%s %11s\n' "$label" \
        "$(fmt_ms "${T_RFIND[$id]}")" "$(fmt_ms "${T_LOCATE[$id]}")" "$(fmt_ms "${T_FIND_L[$id]}")" "$(fmt_ms "${T_FIND_F[$id]}")" \
        "$GRN$B" "$(speedup "$( [ "${T_FIND_F[$id]}" != "-" ] && echo "${T_FIND_F[$id]}" || echo "${T_FIND_L[$id]}")" "${T_RFIND[$id]}")" "$R" \
        "$(speedup "${T_LOCATE[$id]}" "${T_RFIND[$id]}")"
done
note "'vs find' uses the full-scope find time when it completed, otherwise linux scope (an under-estimate)."
[ $KEEP -eq 1 ] && note "Outputs kept in $WORK"
echo
