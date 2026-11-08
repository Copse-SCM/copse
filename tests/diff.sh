#!/usr/bin/env bash
# copse diff and copse show: every kind of change, revisions and ranges, path filters,
# --stat (checked against git's own --stat for the same commits), merges, tags.
# Usage: tests/diff.sh [work_dir]     (build first: cargo build --release; needs git)
set -euo pipefail

COPSE="$(cd "$(dirname "$0")/.." && pwd)/target/release/copse"
WORK="${1:-$(mktemp -d)}"
rm -rf "$WORK" && mkdir -p "$WORK" && cd "$WORK"
cps() { "$COPSE" "$@"; }
export COPSE_AUTHOR="Test Person" COPSE_EMAIL="test@example.com"  # fixed identity, same on every machine
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1
has() { grep -- "$1" >/dev/null; }
pass=0
ok() { echo "  ok: $1"; pass=$((pass + 1)); }
fail() { echo "  FAIL: $1"; exit 1; }
same() { [[ "$1" == "$2" ]] || fail "$3 (got '$1', want '$2')"; ok "$3"; }
expect_fail() {
  local what="$1" want="$2"; shift 2
  local out; if out=$("$@" 2>&1); then fail "$what (should have failed)"; fi
  [[ "$out" == *"$want"* ]] || fail "$what (wrong error: $out)"
  ok "$what"
}
# "path count" for each --stat line, plus the summary line: comparable between copse and git.
stat_shape() { grep -E '\|' | sed -E 's/^ *([^ ]+) +\| +(Bin|[0-9]+).*/\1 \2/'; }
summary() { grep -E 'files? changed' | sed 's/^ *//'; }

mkdir repo && cd repo && cps init >/dev/null
printf 'one\ntwo\nthree\nfour\nfive\n' > a.txt && echo "bee" > b.txt && printf '#!/bin/sh\n' > run.sh
python3 -c "import random; open('img.bin','wb').write(random.Random(1).randbytes(2000))"
cps commit -m "first" >/dev/null && cps tag c1 >/dev/null
sed -i '' 's/^three$/THREE/' a.txt && printf 'x\ny\nz\n' > c.txt && rm b.txt
cps commit -m "second" >/dev/null && cps tag c2 >/dev/null
chmod +x run.sh && python3 -c "import random; open('img.bin','wb').write(random.Random(2).randbytes(2000))"
cps commit -m "third" >/dev/null && cps tag c3 >/dev/null

echo "== copse diff"
same "$(cps diff)" "" "no changes: no output"
sed -i '' 's/^five$/FIVE/' a.txt
out=$(cps diff); [[ "$out" == *"-five"* && "$out" == *"+FIVE"* && "$out" == *"--- a/a.txt"* ]] && ok "your edits against the last commit" || fail "$out"
same "$(cps diff --stat | summary)" "1 file changed, 1 insertion(+), 1 deletion(-)" "--stat summary for an uncommitted edit"
out=$(cps diff c2); [[ "$out" == *"+FIVE"* && "$out" == *"mode changed from 100644 to 100755"* && "$out" == *"Binary file differs"* ]] && ok "your files against an older revision" || fail "$out"
cps restore a.txt >/dev/null; cps stash drop >/dev/null 2>&1 || true

out=$(cps diff c1 c2)
[[ "$out" == *"diff b.txt"*"deleted file, mode 100644"* ]] && ok "a deleted file" || fail "$out"
[[ "$out" == *"diff c.txt"*"new file, mode 100644"*"+x"* ]] && ok "a new file" || fail "$out"
[[ "$out" == *"-three"*"+THREE"* ]] && ok "an edited file" || fail "$out"
same "$(cps diff c1..c2)" "$out" "A..B is the same as A B"
same "$(cps diff c2 c1 --stat | summary)" "3 files changed, 2 insertions(+), 4 deletions(-)" "reversed order reverses the changes"
out=$(cps diff c1 c3 -- a.txt); [[ "$out" == *"diff a.txt"* && "$out" != *"c.txt"* && "$out" != *"img.bin"* ]] && ok "-- PATHS limits it to some files" || fail "$out"
mkdir -p sub && echo s > sub/s.txt && cps commit -m "sub" >/dev/null
out=$(cd sub && cps diff c3 HEAD -- .); [[ "$out" == *"diff sub/s.txt"* && "$(echo "$out" | grep -c '^diff ')" == "1" ]] && ok "paths are relative to where you are" || fail "$out"
expect_fail "three revisions is too many" "at most two" cps diff c1 c2 c3
expect_fail "unknown revisions are reported" "unknown revision" cps diff nope

echo "== copse show"
out=$(cps show c2)
[[ "$out" == "commit "*"(tag: c2)"* ]] && ok "starts with the commit id and its labels" || fail "$out"
[[ "$out" == *"Author: Test Person <test@example.com>"* ]] && ok "shows the author with email" || fail "$out"
[[ "$out" == *"Date:   "*" $(date +%z)"* ]] && ok "shows the date in the author's own timezone" || fail "$out"
[[ "$out" == *"    second"* && "$out" == *"+THREE"* && "$out" != *"run.sh"* ]] && ok "shows the message and only that commit's changes" || fail "$out"
same "$(cps show | head -1)" "$(cps show HEAD | head -1)" "defaults to HEAD"
out=$(cps show c1); [[ "$out" == *"new file"* && "$(echo "$out" | grep -c '^diff ')" == "4" ]] && ok "the first commit shows everything as new" || fail "$out"
out=$(cps show c3); [[ "$out" == *"mode changed from 100644 to 100755"* && "$out" == *"Binary file differs"* ]] && ok "shows permission and binary changes" || fail "$out"
cps tag -a v1 -m "Release one" c2 >/dev/null
out=$(cps show v1); [[ "$out" == "tag v1"*"Tagger: Test Person <test@example.com>"*"    Release one"*"commit "* ]] && ok "an annotated tag shows its own message first" || fail "$out"
cps branch side c3 >/dev/null && cps checkout side >/dev/null && echo side > side.txt && cps commit -m "side" >/dev/null && cps checkout main >/dev/null && cps merge side >/dev/null
out=$(cps show); [[ "$out" == *"Merge:  "* && "$out" == *"compared with the first parent"* && "$out" == *"diff side.txt"* ]] && ok "a merge lists its parents and its changes" || fail "$out"

echo "== --stat matches git's"
cd "$WORK" && git init -q --bare -b main server.git && cd repo
cps remote add gh ../server.git >/dev/null && cps push gh >/dev/null && cps push gh --tags >/dev/null
for t in c1 c2 c3; do
  same "$(cps show --stat "$t" | stat_shape)" "$(git -C ../server.git show --stat --format= "$t" | stat_shape)" "show --stat $t: same files and counts as git"
  same "$(cps show --stat "$t" | summary)" "$(git -C ../server.git show --stat --format= "$t" | summary)" "show --stat $t: same summary as git"
done
same "$(cps diff --stat c1 c3 | stat_shape)" "$(git -C ../server.git diff --stat c1 c3 | stat_shape)" "diff --stat between revisions matches git"

echo
echo "All $pass checks passed."
