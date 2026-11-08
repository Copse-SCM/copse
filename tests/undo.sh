#!/usr/bin/env bash
# Reflog and undo: every branch/HEAD move is recorded, `copse undo` reverses the last
# one (and redoes when run again), deleted branches stay recoverable, and the
# reflog can be used as revisions (HEAD@{n}, branch@{n}).
# Usage: tests/undo.sh [work_dir]     (build first: cargo build --release)
set -euo pipefail
. "$(cd "$(dirname "$0")" && pwd)/portable.sh"   # sedi, mtime, days_ago, peak_mb, ... (macOS and Linux)

COPSE="$(cd "$(dirname "$0")/.." && pwd)/target/release/copse"
WORK="${1:-$(mktemp -d)}"
rm -rf "$WORK" && mkdir -p "$WORK" && cd "$WORK"
cps() { "$COPSE" "$@"; }
export COPSE_AUTHOR="Test Person" COPSE_EMAIL="test@example.com"  # fixed identity, same on every machine
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1  # your own git settings (signing, identity) stay out of the tests
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
history() { cps log -n 50 | grep -v UTC | sed 's/^ *//' | grep -v '^$' | tr '\n' ',' | sed 's/,$//'; }
tip() { cut -c1-10 < .copse/refs/"${1:-main}"; }

mkdir repo && cd repo && cps init >/dev/null
echo 1 > f.txt && cps commit -m one >/dev/null
expect_fail "nothing to undo on a brand-new branch" "was created by" cps undo

echo "== undo and redo a commit"
echo 2 > f.txt && cps commit -m two >/dev/null
out=$(cps undo); [[ "$out" == *"Undid \`commit: two\`"* ]] && ok "undo names what it undid" || fail "$out"
same "$(history) | $(cat f.txt)" "one | 1" "branch and files are back before the commit"
cps undo >/dev/null && same "$(history) | $(cat f.txt)" "two,one | 2" "undo again redoes it" || fail "line $LINENO: a step before this check failed"

echo "== the reflog"
cps reflog | head -1 | has 'HEAD@{0}' && ok "reflog lists HEAD's moves, newest first" || fail "line $LINENO: a step before this check failed"
[[ "$(cps reflog)" == *"commit: two"* && "$(cps reflog)" == *"undo: commit: two"* ]] && ok "with what caused each one" || fail "line $LINENO: a step before this check failed"
cps reflog main | head -1 | has 'main@{0}' && ok "branches have their own reflog" || fail "line $LINENO: a step before this check failed"
cps checkout -f 'HEAD@{1}' >/dev/null && same "$(cat f.txt)" "1" "HEAD@{1} is where HEAD was one move ago" && cps checkout -f main >/dev/null || fail "line $LINENO: a step before this check failed"

echo "== undo a squash"
for i in 3 4 5; do echo "$i" > "n$i.txt" && cps commit -m "n$i" >/dev/null; done
before=$(history)
printf '%s\n' '#!/bin/sh' 'sed -i "" -e "2s/^pick/fixup/" -e "3s/^pick/fixup/" "$1"' > ../squash-editor && chmod +x ../squash-editor
COPSE_EDITOR=../squash-editor cps rebase -i HEAD~3 >/dev/null
same "$(history)" "n3,two,one" "squashed three commits into one"
cps undo | has "Undid \`rebase -i HEAD~3\`" && same "$(history)" "$before" "undo brings all three back" || fail "line $LINENO: a step before this check failed"

echo "== undo a merge and a pull"
cps branch side HEAD~1 >/dev/null && cps checkout side >/dev/null && echo s > s.txt && cps commit -m side >/dev/null && cps checkout main >/dev/null
cps merge side >/dev/null && [[ -f s.txt ]] && ok "merged" || fail "line $LINENO: a step before this check failed"
cps undo >/dev/null && [[ ! -e s.txt ]] && same "$(history | cut -d, -f1)" "n5" "undo the merge" || fail "line $LINENO: a step before this check failed"
cd .. && cps init --bare hub.copse >/dev/null && cd repo && cps remote add origin ../hub.copse >/dev/null && cps push >/dev/null
cd .. && cps clone hub.copse other >/dev/null && cd other && echo o > o.txt && cps commit -m "from other" >/dev/null && cps push >/dev/null && cd ../repo
cps pull | has "Fast-forward" && [[ -f o.txt ]] && ok "pulled" || fail "line $LINENO: a step before this check failed"
cps undo >/dev/null && [[ ! -e o.txt ]] && same "$(history | cut -d, -f1)" "n5" "undo the pull" || fail "line $LINENO: a step before this check failed"

echo "== guards"
echo dirty >> f.txt
expect_fail "undo refuses to run over uncommitted changes" "uncommitted change" cps undo
cps checkout -f main >/dev/null
cps checkout side >/dev/null && echo x > n3.txt && cps commit -m "side edit n3" >/dev/null && cps checkout main >/dev/null && echo y > n3.txt && cps commit -m "main edit n3" >/dev/null
cps checkout side >/dev/null && cps rebase main >/dev/null
expect_fail "undo points at --abort during a rebase" "rebase --abort" cps undo
cps rebase --abort >/dev/null && cps checkout -f main >/dev/null

echo "== deleted and renamed branches"
gone=$(tip side)
cps branch -D side >/dev/null
cps reflog side | head -1 | has "(deleted)" && ok "a deleted branch keeps its reflog" || fail "line $LINENO: a step before this check failed"
cps branch side 'side@{1}' >/dev/null && same "$(tip side)" "$gone" "and comes back with branch NAME NAME@{1}" || fail "line $LINENO: a step before this check failed"
cps branch -m side renamed >/dev/null
[[ "$(cps reflog renamed)" == *"commit: side edit n3"* ]] && ok "renaming keeps the branch's history" || fail "line $LINENO: a step before this check failed"
cps checkout renamed >/dev/null
expect_fail "undo skips the rename and finds the real last change (the branch being re-created)" 'was created by `branch side side@{1}`' cps undo
cps checkout -f main >/dev/null

echo "== detached HEAD"
cps checkout -f 'main~1' >/dev/null && echo d > d.txt && cps commit -m detached >/dev/null
cps undo >/dev/null && [[ ! -e d.txt ]] && ok "undo works on a detached HEAD too" || fail "line $LINENO: a step before this check failed"
cps checkout -f main >/dev/null

echo "== a server keeps a record of what pushes replaced"
cd ../other && cps pull >/dev/null && echo forced > forced.txt && cps commit -m forced >/dev/null
overwritten=$(cut -c1-10 < ../hub.copse/refs/main)
cd ../repo && echo mine > mine.txt && cps commit -m mine >/dev/null && cps push --force >/dev/null
cd ../hub.copse && [[ "$(cps reflog main)" == *"push received"* ]] && ok "the server's reflog records received pushes" || fail "line $LINENO: a step before this check failed"
[[ "$(cps reflog main | sed -n 2p)" == *"$overwritten"* ]] && ok "including the commit a force-push replaced, so it can be recovered" || fail "line $LINENO: a step before this check failed"

echo
echo "All $pass checks passed."
