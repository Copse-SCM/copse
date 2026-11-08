#!/usr/bin/env bash
# Run every test suite, each in a fresh folder, and summarize.
# Usage: tests/run-all.sh [--update-docs | --check-docs] [work_dir]
#        (build first: cargo build --release)
# Exits 1 if any suite fails, printing the end of its output.
#   --update-docs  write each suite's count into docs/guide.md's table, and the
#                  total into README.md, so the numbers there are never typed by hand
#   --check-docs   fail if those numbers are out of date (CI runs this)
set -uo pipefail
. "$(cd "$(dirname "$0")" && pwd)/portable.sh"

HERE="$(cd "$(dirname "$0")" && pwd)"
docs=""
case "${1:-}" in
  --update-docs|--check-docs) docs="$1"; shift ;;
esac
WORK="${1:-$(mktemp -d)}"
counts=()
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
    counts+=("$name=${n:-0}")
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

# The counts in the docs: docs/guide.md has a row "| `NAME.sh` | COUNT | ..." per
# suite, and README.md says "TOTAL end-to-end checks" (with thousands commas).
if [[ -n "$docs" ]]; then
  ROOT="$(cd "$HERE/.." && pwd)"
  python3 - "$docs" "$total" "$ROOT" "${counts[@]}" <<'PY'
import re, sys
mode, total, root, *pairs = sys.argv[1:]
counts = dict(p.split("=") for p in pairs)
guide_path, readme_path = f"{root}/docs/guide.md", f"{root}/README.md"
guide, readme = open(guide_path).read(), open(readme_path).read()
problems = []
def row(m):
    name, old = m.group(1), m.group(2)
    if name not in counts:
        problems.append(f"docs/guide.md lists {name}.sh, which isn't a test suite")
        return m.group(0)
    if old != counts[name]:
        problems.append(f"docs/guide.md: {name}.sh has {counts[name]} checks, not {old}")
    return f"| `{name}.sh` | {counts[name]} |"
new_guide = re.sub(r"\| `([\w-]+)\.sh` \| (\d+) \|", row, guide)
for name in counts:
    if f"| `{name}.sh` |" not in guide:
        problems.append(f"docs/guide.md has no row for {name}.sh (add one: | `{name}.sh` | {counts[name]} | what it covers |)")
want = f"{int(total):,} end-to-end checks"
m = re.search(r"[\d,]+ end-to-end checks", readme)
if not m:
    problems.append("README.md doesn't say how many end-to-end checks there are")
elif m.group(0) != want:
    problems.append(f"README.md: {want}, not {m.group(0)}")
new_readme = re.sub(r"[\d,]+ end-to-end checks", want, readme, count=1)
if mode == "--check-docs":
    if problems:
        print("The docs' test counts are out of date (run tests/run-all.sh --update-docs):")
        for p in problems:
            print("  " + p)
        sys.exit(1)
    print("The docs' test counts are up to date.")
else:
    open(guide_path, "w").write(new_guide)
    open(readme_path, "w").write(new_readme)
    missing = [p for p in problems if "has no row" in p or "isn't a test suite" in p]
    for p in missing:
        print("  " + p)
    print("Updated the test counts in docs/guide.md and README.md." if not missing else "Updated what could be; add the rows above by hand.")
PY
fi
