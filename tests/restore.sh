#!/usr/bin/env bash
# Restore and reset: putting files back, moving branches with and without touching
# files, the automatic stash backup whenever uncommitted work would be overwritten,
# and undo working after a reset.
# Usage: tests/restore.sh [work_dir]     (build first: cargo build --release)
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
clean() { [[ "$(cps status | tail -1)" == "Nothing changed." ]]; }
stashes() { cps stash list | grep -vc '^No stashes' || true; }

mkdir repo && cd repo && cps init >/dev/null
mkdir -p src/deep && echo v1 > a.txt && echo s1 > src/one.txt && echo d1 > src/deep/two.txt
printf '#!/bin/sh\n' > run.sh && chmod +x run.sh && ln -s a.txt link
cps commit -m one >/dev/null
echo v2 > a.txt && echo s2 > src/one.txt && cps commit -m two >/dev/null

echo "== restore"
echo scribble > a.txt
cps restore a.txt | has "Restored 1 file(s) from the last commit" && same "$(cat a.txt)" "v2" "restore an edited file" || fail "line $LINENO: a step before this check failed"
same "$(stashes)" "1" "the overwritten edit was saved as a stash first"
cps stash apply 0 >/dev/null 2>&1; same "$(cat a.txt)" "scribble" "and stash apply brings it back"
cps restore a.txt >/dev/null && cps stash drop >/dev/null && cps stash drop >/dev/null && clean && ok "back to a clean state (that restore saved a second backup; both dropped)" || fail "line $LINENO: a step before this check failed"
rm src/deep/two.txt
cps restore src/deep/two.txt >/dev/null && same "$(cat src/deep/two.txt)" "d1" "restore a deleted file" || fail "line $LINENO: a step before this check failed"
same "$(stashes)" "0" "nothing to lose there, so no backup"
echo x > src/one.txt && rm src/deep/two.txt && echo new > src/new.txt
cps restore src | has "Restored 2 file(s)" && [[ "$(cat src/one.txt)" == "s2" && -f src/deep/two.txt ]] && ok "restore a whole folder" || fail "line $LINENO: a step before this check failed"
[[ -f src/new.txt ]] && ok "new files in it are left alone" && rm src/new.txt && cps stash drop >/dev/null || fail "line $LINENO: a step before this check failed"
cps restore --from HEAD~1 a.txt | has "from HEAD~1" && same "$(cat a.txt)" "v1" "restore an older version with --from" || fail "line $LINENO: a step before this check failed"
cps status | has "M a.txt" && ok "which then shows as an uncommitted change" && cps restore a.txt >/dev/null && cps stash drop >/dev/null || fail "line $LINENO: a step before this check failed"
cd src && echo y > one.txt && cps restore one.txt >/dev/null && same "$(cat one.txt)" "s2" "paths are relative to where you are" && cd .. || fail "line $LINENO: a step before this check failed"
cps stash drop >/dev/null
chmod -x run.sh && rm link && cps restore run.sh link >/dev/null && [[ -x run.sh && -L link ]] && ok "permissions and symlinks come back" || fail "line $LINENO: a step before this check failed"
same "$(stashes)" "1" "a permission change counts as an edit, so it was backed up too"
cps stash drop >/dev/null
echo brand-new > fresh.txt
expect_fail "a new file isn't in the last commit" "it's new since then" cps restore fresh.txt
rm fresh.txt
expect_fail "a missing path is reported" "doesn't exist in" cps restore nope.txt
same "$(cps restore a.txt)" "Nothing to restore: already as in HEAD." "nothing to do when it already matches"
clean && ok "status is clean after all that" || fail "line $LINENO: a step before this check failed"

echo "== reset (keep files)"
echo v3 > a.txt && cps commit -m three >/dev/null
out=$(cps reset HEAD~1); [[ "$out" == *"main moved from"*"1 difference(s) now show as uncommitted changes"* ]] && ok "reset HEAD~1 uncommits, keeping your files" || fail "$out"
same "$(history) | $(cat a.txt)" "two,one | v3" "branch moved back, file untouched"
cps undo | has "Undid" && same "$(history)" "three,two,one" "undo puts it back (your files already matched)" || fail "line $LINENO: a step before this check failed"
clean && ok "and the status is clean again" || fail "line $LINENO: a step before this check failed"
cps reset HEAD~2 >/dev/null && cps commit -m "two and three, as one commit" >/dev/null && same "$(history)" "two and three, as one commit,one" "reset + commit squashes by hand" || fail "line $LINENO: a step before this check failed"
cps reset --hard 'main@{2}' >/dev/null && same "$(history)" "three,two,one" "the reflog gets the original history back (main@{2}: before the commit and the reset)" || fail "line $LINENO: a step before this check failed"

echo "== reset --hard"
cps reset --hard HEAD~1 | has "your files match it" && same "$(history) | $(cat a.txt)" "two,one | v2" "reset --hard moves the branch and the files" || fail "line $LINENO: a step before this check failed"
same "$(stashes)" "0" "no uncommitted work, so no backup"
cps undo >/dev/null && same "$(history) | $(cat a.txt)" "three,two,one | v3" "undo reverses a hard reset" || fail "line $LINENO: a step before this check failed"
echo unsaved > a.txt
cps reset --hard | has "Saved your uncommitted work as stash 0" && same "$(cat a.txt)" "v3" "reset --hard discards uncommitted changes" || fail "line $LINENO: a step before this check failed"
cps stash pop >/dev/null && same "$(cat a.txt)" "unsaved" "and they're recoverable from the stash" || fail "line $LINENO: a step before this check failed"
cps reset --hard >/dev/null && cps stash drop >/dev/null
cps tag v-two HEAD~1 >/dev/null && cps reset --hard v-two >/dev/null && same "$(cat a.txt)" "v2" "reset to a tag" && cps undo >/dev/null || fail "line $LINENO: a step before this check failed"
[[ "$(cps reflog | head -1)" == *"undo: reset --hard v-two"* ]] && ok "resets are in the reflog" || fail "line $LINENO: a step before this check failed"
same "$(cps reset)" "Nothing to do: main is already at $(cps log -n 1 | head -1 | cut -c1-10) and your files are as they were." "plain reset with nothing to do"

echo "== guards and detached HEAD"
cps branch side HEAD~1 >/dev/null && cps checkout side >/dev/null && echo side > a.txt && cps commit -m side >/dev/null
cps rebase main >/dev/null || true
expect_fail "reset is refused during a rebase" "rebase is in progress" cps reset --hard HEAD~1
cps rebase --abort >/dev/null && cps checkout -f main >/dev/null
cps checkout HEAD~1 >/dev/null && cps reset --hard HEAD~1 >/dev/null && same "$(cat a.txt)" "v1" "reset works on a detached HEAD" || fail "line $LINENO: a step before this check failed"
cps checkout -f main >/dev/null

echo
echo "All $pass checks passed."
