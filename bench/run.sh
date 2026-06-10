#!/usr/bin/env bash
# Reproducible grix vs ripgrep benchmark.
#
#   bench/run.sh <corpus-dir>
#
# Requires: hyperfine, ripgrep, a release grix build.
# Override binaries with GRIX=/path/to/grix RG=/path/to/rg.
#
# Every pattern is parity-checked (identical matched-line counts) before it
# is timed; a benchmark against wrong results would be meaningless.

set -euo pipefail

CORPUS=${1:?usage: bench/run.sh <corpus-dir>}
GRIX=${GRIX:-grix}
RG=${RG:-rg}
RUNS=${RUNS:-10}

command -v hyperfine >/dev/null || { echo "error: hyperfine not found" >&2; exit 1; }
command -v "$RG" >/dev/null || { echo "error: ripgrep not found" >&2; exit 1; }

cd "$CORPUS"

echo "## corpus"
files=$("$RG" --files | wc -l | tr -d ' ')
echo "- files (as rg sees them): $files"
echo

echo "## index build"
"$GRIX" forget . >/dev/null 2>&1 || true
start=$(date +%s%N)
"$GRIX" index .
end=$(date +%s%N)
echo "- cold build: $(( (end - start) / 1000000 )) ms"
start=$(date +%s%N)
"$GRIX" index .
end=$(date +%s%N)
echo "- refresh (no changes): $(( (end - start) / 1000000 )) ms"
echo

# pattern | flags (applied to both tools) | description
benchmarks=(
  'PageTransHuge||rare literal'
  'EXPORT_SYMBOL||common literal (tens of thousands of hits)'
  'static\s+int\s+\w+_probe||regex with literal core'
  'spinlock|-i|case-insensitive literal'
  'zzqqxx_does_not_exist||no match'
)

for spec in "${benchmarks[@]}"; do
  IFS='|' read -r pattern flags desc <<<"$spec"
  echo "## $desc: $pattern ${flags:+($flags)}"

  # Parity gate.
  rg_count=$("$RG" $flags "$pattern" --no-heading 2>/dev/null | wc -l | tr -d ' ' || true)
  grix_count=$("$GRIX" $flags "$pattern" . --no-heading --color never 2>/dev/null | wc -l | tr -d ' ' || true)
  if [ "$rg_count" != "$grix_count" ]; then
    echo "PARITY FAILURE: rg=$rg_count grix=$grix_count -- skipping timing" >&2
    exit 1
  fi
  echo "- matched lines (both tools): $rg_count"

  hyperfine --warmup 3 --runs "$RUNS" --ignore-failure --style basic \
    -n "rg" "$RG $flags '$pattern' --no-heading" \
    -n "grix" "$GRIX $flags '$pattern' . --no-heading --color never"
  echo
done
