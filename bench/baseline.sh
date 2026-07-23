#!/usr/bin/env bash
# Internal A/B harness for the speedup work: measures ONE grix binary on a
# corpus (build / no-op refresh / incremental refresh / query suite).
# Unlike bench/run.sh this does not need ripgrep; parity vs rg is checked
# separately at the end of the work.
#
#   bench/baseline.sh <grix-exe> <corpus-dir> <data-dir-win> [label]

set -uo pipefail

GRIX=${1:?grix exe}
CORPUS=${2:?corpus dir}
DATA=${3:?GRIX_DATA_DIR (windows style)}
LABEL=${4:-run}
RUNS=${RUNS:-10}

export GRIX_DATA_DIR="$DATA"
GRIX_CMD=$(cygpath -m "$GRIX" 2>/dev/null || echo "$GRIX")

cd "$CORPUS"

ms() { echo $(( ($2 - $1) / 1000000 )); }

echo "### $LABEL — $(date +%H:%M:%S)"
echo "binary: $GRIX_CMD"
"$GRIX" -V

"$GRIX" forget . >/dev/null 2>&1 || true
s=$(date +%s%N); "$GRIX" index . 2>&1 | tail -1; e=$(date +%s%N)
echo "build_first_ms=$(ms "$s" "$e")"
"$GRIX" forget . >/dev/null 2>&1
s=$(date +%s%N); "$GRIX" index . 2>&1 | tail -1; e=$(date +%s%N)
echo "build_warm_ms=$(ms "$s" "$e")"

for i in 1 2 3; do
  s=$(date +%s%N); "$GRIX" index . >/dev/null 2>&1; e=$(date +%s%N)
  echo "noop_refresh_ms=$(ms "$s" "$e")"
done

# Incremental: touch one file (mtime change -> re-extract 1 file + rewrite).
T=drivers/net/ethernet/intel/e1000/e1000_main.c
touch "$T"
s=$(date +%s%N); "$GRIX" index . >/dev/null 2>&1; e=$(date +%s%N)
echo "incr_1file_ms=$(ms "$s" "$e")"

echo
echo "-- query suite (--no-auto-index, hyperfine runs=$RUNS) --"
declare -a P=(
  'PageTransHuge|'
  'EXPORT_SYMBOL|'
  'static\s+int\s+\w+_probe|'
  'spinlock|-i'
  'zzqqxx_does_not_exist|'
)
for spec in "${P[@]}"; do
  IFS='|' read -r pattern flags <<<"$spec"
  count=$("$GRIX" $flags "$pattern" . --no-heading --color never --no-auto-index 2>/dev/null | wc -l | tr -d ' ')
  echo "## $pattern ${flags:+($flags)} matched=$count"
  hyperfine --warmup 3 --runs "$RUNS" --shell=none --ignore-failure --style basic \
    -n grix "$GRIX_CMD $flags '$pattern' . --no-heading --color never --no-auto-index" 2>&1 | grep -E 'Time|range'
  "$GRIX" $flags "$pattern" . --no-heading --color never --no-auto-index --stats 2>&1 >/dev/null | grep -E 'query plan|index:|scanned|timing'
  echo
done
