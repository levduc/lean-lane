#!/usr/bin/env bash
# gen-lean-fund.sh [count] [outfile] — regenerate the lean lane's genesis fund file.
#
# The lean node funds these addresses at genesis (--fund-file). They must be the
# exact accounts the spammer signs with, or every generated transaction is
# rejected for insufficient funds: derivation is
#   m/44'/60'/1'/0/{i}   from the standard test mnemonic
# note the 1' (NOT the usual 0') — that mismatch cost a debugging session once.
#
#   ./gen-lean-fund.sh              # 800 addresses -> ~/lean-fund.txt
#   ./gen-lean-fund.sh 1600 /tmp/f  # explicit count and path
set -euo pipefail
COUNT=${1:-800}
OUT=${2:-$HOME/lean-fund.txt}
MNEMONIC=${MNEMONIC:-"test test test test test test test test test test test junk"}
export PATH="$PATH:$HOME/.foundry/bin"
command -v cast >/dev/null || { echo "cast (foundry) not found — install foundry first"; exit 1; }

echo "deriving $COUNT addresses at m/44'/60'/1'/0/i ..."
: > "$OUT.tmp"
for i in $(seq 0 $((COUNT-1))); do
  cast wallet address --mnemonic "$MNEMONIC" --mnemonic-derivation-path "m/44'/60'/1'/0/$i" >> "$OUT.tmp"
done
mv "$OUT.tmp" "$OUT"
echo "wrote $(wc -l < "$OUT") addresses to $OUT"
head -2 "$OUT"
