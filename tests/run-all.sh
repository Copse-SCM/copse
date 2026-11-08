#!/usr/bin/env bash
# Run every test suite, each in a fresh folder, and summarize.
# Usage: tests/run-all.sh [work_dir]     (build first: cargo build --release)
# Exits 1 if any suite fails, printing the end of its output.
set -uo pipefail
. "$(cd "$(dirname "$0")" && pwd)/portable.sh"

HERE="$(cd "$(dirname "$0")" && pwd)"
WORK="${1:-$(mktemp -d)}"
mkdir -p "$WORK"
failed=()
total=0
for suite in "$HERE"/*.sh; do
  name=$(basename "$suite" .sh)
  [[ "$name" == portable || "$name" == run-all ]] && continue
  start=$(date +%s)
  out=$(bash "$suite" "$WORK/$name" 2>&1)
  code=$?
  secs=$(( $(date +%s) - start ))
  if [[ $code -eq 0 ]]; then
    n=$(echo "$out" | sed -n 's/^All \([0-9]*\) checks passed\.$/\1/p')
    total=$(( total + ${n:-0} ))
    printf '  %-12s %4s checks  (%ss)\n' "$name" "${n:-?}" "$secs"
  else
    failed+=("$name")
    printf '  %-12s FAILED (exit %s, %ss)\n' "$name" "$code" "$secs"
    echo "$out" | tail -15 | sed 's/^/      /'
  fi
done
echo
if [[ ${#failed[@]} -eq 0 ]]; then
  echo "All suites passed: $total checks."
else
  echo "Failed: ${failed[*]}"
  exit 1
fi
