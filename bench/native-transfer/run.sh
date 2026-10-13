#!/usr/bin/env bash
# Native-transfer benchmark: lean payment lane engine vs reth (latest release),
# N = 1 (one payment per transaction), consensus ignored. See README.md.
#
#   bench/native-transfer/run.sh                 # full default run
#   QUICK=1 bench/native-transfer/run.sh         # 100k accounts, 3 blocks
#   ACCOUNTS=1000000 TXS=20000 BLOCKS=5 THREADS=8 bench/native-transfer/run.sh
#   bench/native-transfer/run.sh --accounts 100000 --txs 10000 --blocks 3 --threads 4
#
# Env / flags: ACCOUNTS (--accounts), TXS (--txs), BLOCKS (--blocks),
# WARMUP (--warmup), THREADS (--threads), SEED (--seed), QUICK=1 (--quick),
# BENCH_TMPDIR (--tmpdir: where the reth datadir and the lean block log go;
# default bench/native-transfer/.tmp, i.e. on the repo's disk, not /tmp),
# SKIP_BUILD=1 (--skip-build), NO_VERIFY_ROOT=1 (skip reth's end-of-run full
# state-root recompute).
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
RETH_DIR="$HERE/reth-side"
RETH_TAG="v2.7.0"

die() { echo "error: $*" >&2; exit 1; }
note() { echo "==> $*" >&2; }

while [ $# -gt 0 ]; do
    case "$1" in
        --accounts) ACCOUNTS="$2"; shift 2 ;;
        --txs) TXS="$2"; shift 2 ;;
        --blocks) BLOCKS="$2"; shift 2 ;;
        --warmup) WARMUP="$2"; shift 2 ;;
        --threads) THREADS="$2"; shift 2 ;;
        --seed) SEED="$2"; shift 2 ;;
        --tmpdir) BENCH_TMPDIR="$2"; shift 2 ;;
        --quick) QUICK=1; shift ;;
        --skip-build) SKIP_BUILD=1; shift ;;
        -h|--help) sed -n '2,17p' "$0"; exit 0 ;;
        *) die "unknown argument: $1 (see --help)" ;;
    esac
done

OS="$(uname -s)"

physical_cores() {
    if [ "$OS" = "Darwin" ]; then
        sysctl -n hw.physicalcpu
    elif command -v lscpu >/dev/null 2>&1; then
        lscpu -p=CORE,SOCKET | grep -v '^#' | sort -u | wc -l | tr -d ' '
    else
        nproc
    fi
}
logical_cores() {
    if [ "$OS" = "Darwin" ]; then sysctl -n hw.logicalcpu; else nproc --all; fi
}

if [ "${QUICK:-0}" = "1" ]; then
    ACCOUNTS="${ACCOUNTS:-100000}"
    BLOCKS="${BLOCKS:-3}"
fi
ACCOUNTS="${ACCOUNTS:-1000000}"
TXS="${TXS:-20000}"
BLOCKS="${BLOCKS:-5}"
WARMUP="${WARMUP:-1}"
SEED="${SEED:-1}"
THREADS="${THREADS:-$(physical_cores)}"
BENCH_TMPDIR="${BENCH_TMPDIR:-$HERE/.tmp}"
for v in ACCOUNTS TXS BLOCKS WARMUP SEED THREADS; do
    [[ "${!v}" =~ ^[0-9]+$ ]] || die "$v must be a non-negative integer (got '${!v}')"
done
[ "$ACCOUNTS" -ge "$TXS" ] || die "ACCOUNTS ($ACCOUNTS) must be >= TXS ($TXS): every sender is a pre-funded account"
[ "$BLOCKS" -ge 1 ] && [ "$TXS" -ge 1 ] && [ "$THREADS" -ge 1 ] || die "BLOCKS, TXS and THREADS must be >= 1"

# ---- prerequisites ----
command -v cargo >/dev/null 2>&1 || die "cargo not found. Install Rust with rustup: https://rustup.rs"
command -v rustup >/dev/null 2>&1 || die "rustup not found (each side pins its toolchain in rust-toolchain.toml). Install: https://rustup.rs"
if [ "$OS" = "Darwin" ]; then
    CLANG_HINT="xcode-select --install   (or: brew install llvm)"
else
    CLANG_HINT="sudo apt-get install -y clang libclang-dev build-essential   (Fedora: sudo dnf install clang clang-devel gcc-c++)"
fi
command -v clang >/dev/null 2>&1 || die "clang not found (reth's MDBX and RocksDB bindings need it). Install: $CLANG_HINT"
command -v c++ >/dev/null 2>&1 || command -v g++ >/dev/null 2>&1 || die "no C++ compiler (RocksDB needs one). Install: $CLANG_HINT"
if [ "$OS" = "Linux" ] && [ -z "${LIBCLANG_PATH:-}" ]; then
    if ! { ldconfig -p 2>/dev/null | grep -q 'libclang'; } &&
       ! ls /usr/lib/llvm-*/lib/libclang*.so* /usr/lib*/libclang*.so* /usr/lib/*/libclang*.so* >/dev/null 2>&1; then
        die "libclang not found (bindgen needs it). Install: $CLANG_HINT  (or set LIBCLANG_PATH)"
    fi
fi

toolchain_of() { sed -n 's/^channel *= *"\(.*\)"/\1/p' "$1/rust-toolchain.toml"; }
LEAN_TC="$(toolchain_of "$ROOT")"
RETH_TC="$(toolchain_of "$RETH_DIR")"

# ---- build ----
if [ "${SKIP_BUILD:-0}" != "1" ]; then
    for tc in "$LEAN_TC" "$RETH_TC"; do
        rustup toolchain list | grep -q "^$tc" || { note "installing Rust $tc"; rustup toolchain install "$tc" --profile minimal; }
    done
    note "building lean-bench (release, Rust $LEAN_TC)"
    (cd "$ROOT" && cargo build --release --locked -p lean-bench)
    note "building reth-native-bench against reth $RETH_TAG (release, Rust $RETH_TC; the first build takes a while)"
    (cd "$RETH_DIR" && cargo build --release --locked)
fi
LEAN_BIN="$ROOT/target/release/lean-bench"
RETH_BIN="$RETH_DIR/target/release/reth-native-bench"
[ -x "$LEAN_BIN" ] || die "missing $LEAN_BIN (run without SKIP_BUILD)"
[ -x "$RETH_BIN" ] || die "missing $RETH_BIN (run without SKIP_BUILD)"

# ---- work dir (cleaned on exit) ----
mkdir -p "$BENCH_TMPDIR"
WORK="$(mktemp -d "$BENCH_TMPDIR/run.XXXXXX")"
cleanup() { rm -rf "$WORK"; rmdir "$BENCH_TMPDIR" 2>/dev/null || true; }
trap cleanup EXIT
trap 'exit 130' INT TERM

# ---- hardware / software record ----
json_str() { printf '%s' "$1" | sed -e 's/\\/\\\\/g' -e 's/"/\\"/g' | tr -d '\n\r\t'; }
if [ "$OS" = "Darwin" ]; then
    CPU="$(sysctl -n machdep.cpu.brand_string)"
    RAM_GB="$(( $(sysctl -n hw.memsize) / 1073741824 ))"
    OS_NAME="macOS $(sw_vers -productVersion)"
    DISK="$(diskutil info "$(df "$WORK" | tail -1 | awk '{print $1}')" 2>/dev/null | awk -F: '/Solid State|Protocol|Media Name/{gsub(/^ +/,"",$2); printf "%s; ", $2}')"
else
    CPU="$(lscpu 2>/dev/null | sed -n 's/^Model name: *//p' | head -1)"
    RAM_GB="$(awk '/MemTotal/{printf "%d", $2/1048576}' /proc/meminfo)"
    OS_NAME="$( (. /etc/os-release 2>/dev/null && echo "$PRETTY_NAME") || echo Linux)"
    DISK="unknown"
    if command -v findmnt >/dev/null 2>&1; then
        SRC="$(findmnt -no SOURCE --target "$WORK" 2>/dev/null || true)"
        FST="$(findmnt -no FSTYPE --target "$WORK" 2>/dev/null || true)"
        DISK="fs=$FST"
        PK="$(lsblk -no PKNAME "$SRC" 2>/dev/null | head -1 || true)"
        DEV="${PK:-$(basename "$SRC")}"
        if [ -n "$DEV" ] && lsblk -dno ROTA "/dev/$DEV" >/dev/null 2>&1; then
            ROTA="$(lsblk -dno ROTA "/dev/$DEV" | tr -d ' ')"
            TRAN="$(lsblk -dno TRAN "/dev/$DEV" | tr -d ' ')"
            MODEL="$(lsblk -dno MODEL "/dev/$DEV" | sed 's/ *$//')"
            [ "$ROTA" = "0" ] && KIND="SSD" || KIND="HDD (rotational)"
            DISK="$KIND, ${TRAN:-?}, $MODEL, fs=$FST"
        fi
        [ "$FST" = "tmpfs" ] && note "WARNING: $WORK is on tmpfs; persist/fsync numbers will not reflect a disk. Set BENCH_TMPDIR."
    fi
fi
KERNEL="$(uname -r)"
HOST="$(hostname -s 2>/dev/null || hostname)"
PHYS="$(physical_cores)"
LOGI="$(logical_cores)"
LEAN_RUSTC="$(cd "$ROOT" && rustc --version)"
RETH_RUSTC="$(cd "$RETH_DIR" && rustc --version)"
COMMIT="$(git -C "$ROOT" rev-parse --short HEAD 2>/dev/null || echo unknown)"
DIRTY="$(git -C "$ROOT" status --porcelain 2>/dev/null | grep -qv '^??' && echo "-dirty" || true)"
DATE="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

note "host $HOST: $CPU, $PHYS cores / $LOGI threads, ${RAM_GB} GB, $OS_NAME, kernel $KERNEL; disk: $DISK"
note "params: $ACCOUNTS accounts, $TXS txs/block, $WARMUP warm-up + $BLOCKS measured blocks, $THREADS threads, seed $SEED"

# ---- run ----
note "lean (lean-core engine, in-memory flat state)"
"$LEAN_BIN" native --accounts "$ACCOUNTS" --txs "$TXS" --blocks "$BLOCKS" --warmup "$WARMUP" \
    --threads "$THREADS" --seed "$SEED" --log-dir "$WORK/lean" --json > "$WORK/lean.json"

note "reth $RETH_TAG (direct path: executor + state root + persist on MDBX; then the engine path)"
RETH_FLAGS=()
[ "${NO_VERIFY_ROOT:-0}" = "1" ] && RETH_FLAGS+=(--no-verify-root)
"$RETH_BIN" --accounts "$ACCOUNTS" --txs "$TXS" --blocks "$BLOCKS" --warmup "$WARMUP" \
    --threads "$THREADS" --seed "$SEED" --tmpdir "$WORK/reth" --out "$WORK/reth.json" "${RETH_FLAGS[@]+"${RETH_FLAGS[@]}"}"

# ---- report ----
# phases: "engine<TAB>phase<TAB>ms<TAB>tx/s" from the pretty-printed JSON
phases() {
    awk -v eng="$1" '
        /"phase": / { sub(/.*"phase": "/, ""); sub(/",?$/, ""); p = $0 }
        /"ms_per_block": / { sub(/.*: /, ""); sub(/,$/, ""); m = $0 }
        /"tx_per_s": / { sub(/.*: /, ""); sub(/,$/, ""); printf "%s\t%s\t%s\t%s\n", eng, p, m, $0 }
    ' "$2"
}
{ phases lean "$WORK/lean.json"; phases reth "$WORK/reth.json"; } > "$WORK/phases.tsv"
passed() { grep -q '"all_passed": true' "$1" && echo PASS || echo FAIL; }
LEAN_OK="$(passed "$WORK/lean.json")"
RETH_OK="$(passed "$WORK/reth.json")"

echo
echo "native transfers, N=1 | $ACCOUNTS accounts | $TXS txs/block | $WARMUP warm-up + $BLOCKS measured blocks (median) | $THREADS threads"
echo "$CPU | $PHYS C / $LOGI T | ${RAM_GB} GB | $OS_NAME | disk: $DISK"
awk -F'\t' '{ printf "%-5s %-58s %10.2f ms %12.0f tx/s\n", $1, $2, $3, $4 }' "$WORK/phases.tsv"
awk -F'\t' '
    $1=="lean" && $2 ~ /^decode \+ recover/ { lean = $4 }
    $1=="reth" && $2=="recover + execute" { re = $4 }
    $1=="reth" && $2 ~ /^full import/ { full = $4 }
    $1=="reth" && $2 ~ /^engine newPayload \(/ { np = $4 }
    END {
        if (lean && re)   printf "lean / reth direct recover+execute         : %.2fx\n", lean / re
        if (lean && np)   printf "lean / reth engine newPayload (no persist) : %.2fx\n", lean / np
        if (lean && full) printf "lean / reth direct full import             : %.2fx\n", lean / full
    }' "$WORK/phases.tsv"
echo "correctness: lean $LEAN_OK, reth $RETH_OK"

mkdir -p "$HERE/results"
OUT="$HERE/results/${HOST}-$(date +%Y%m%d-%H%M%S).json"
cat > "$OUT" <<EOF
{
  "bench": "native-transfer",
  "version": 1,
  "date_utc": "$DATE",
  "host": "$(json_str "$HOST")",
  "params": { "accounts": $ACCOUNTS, "txs_per_block": $TXS, "warmup_blocks": $WARMUP, "measured_blocks": $BLOCKS, "threads": $THREADS, "seed": $SEED, "payments_per_tx": 1 },
  "hardware": {
    "cpu": "$(json_str "$CPU")",
    "physical_cores": $PHYS,
    "logical_cores": $LOGI,
    "ram_gb": $RAM_GB,
    "os": "$(json_str "$OS_NAME")",
    "kernel": "$(json_str "$KERNEL")",
    "tmpdir_disk": "$(json_str "$DISK")"
  },
  "software": {
    "git_commit": "$COMMIT$DIRTY",
    "reth_tag": "$RETH_TAG",
    "rustc_lean": "$(json_str "$LEAN_RUSTC")",
    "rustc_reth": "$(json_str "$RETH_RUSTC")"
  },
  "correctness": { "lean": "$LEAN_OK", "reth": "$RETH_OK" },
  "lean": $(cat "$WORK/lean.json"),
  "reth": $(cat "$WORK/reth.json")
}
EOF
echo "result: $OUT"
[ "$LEAN_OK" = PASS ] && [ "$RETH_OK" = PASS ] || die "correctness checks failed (see $OUT)"
