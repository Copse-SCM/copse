#!/usr/bin/env bash
# Stash tests: save/restore round trips, several stashes, restoring onto moved
# history, conflicts, ignored files, guards, and stashes surviving gc.
# Usage: tests/stash.sh [work_dir]     (build first: cargo build --release)
set -euo pipefail

COPSE="$(cd "$(dirname "$0")/.." && pwd)/target/release/copse"
WORK="${1:-$(mktemp -d)}"
rm -rf "$WORK" && mkdir -p "$WORK" && cd "$WORK"
cps() { "$COPSE" "$@"; }
export COPSE_AUTHOR="Test Person" COPSE_EMAIL="test@example.com"  # fixed identity, same on every machine
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1  # your own git settings (signing, identity) stay out of the tests
has() { grep -- "$1" >/dev/null; }  # like grep -q, but reads all input (no early pipe close)
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
fingerprint() {
  find . -path ./.copse -prune -o \( -type f -o -type l \) -print | LC_ALL=C sort | while read -r f; do
    if [[ -L "$f" ]]; then echo "$f link $(readlink "$f")"; else echo "$f $(stat -f %p "$f") $(shasum < "$f")"; fi
  done | shasum | cut -c1-16
}

echo "== guards before any commits"
mkdir empty && cd empty && cps init >/dev/null && echo x > x.txt
expect_fail "stash with no commits is refused" "commit something first" cps stash
cd ..

mkdir repo && cd repo && cps init >/dev/null
printf '1\n2\n3\n4\n5\n' > notes.txt && echo keep > keep.txt && echo gone > gone.txt && printf '#!/bin/sh\n' > run.sh
echo "*.log" > .gitignore
cps commit -m base >/dev/null
same "$(cps stash)" "No changes to stash." "nothing to stash on a clean tree"

echo "== save and restore every kind of change"
sed -i '' 's/^3$/THREE/' notes.txt     # modified
echo brand-new > new.txt                # added
rm gone.txt                             # deleted
chmod +x run.sh                         # mode change
ln -s notes.txt link                    # new symlink
echo "ignored" > debug.log              # ignored: must not be stashed or touched
before=$(fingerprint)
out=$(cps stash -m "work in progress"); [[ "$out" == "Stashed 5 change(s) as stash 0: work in progress"* ]] && ok "stash reports 5 changes" || fail "$out"
same "$(cps status | tail -1)" "Nothing changed." "working directory is clean after stashing"
[[ ! -e new.txt && -e gone.txt && ! -L link && ! -x run.sh && "$(sed -n 3p notes.txt)" == "3" ]] && ok "files are back to the last commit" || fail "not reset"
[[ "$(cat debug.log)" == "ignored" ]] && ok "ignored file left alone" || fail "line $LINENO: a step before this check failed"
cps status | has "1 stash(es) saved" && ok "status mentions the stash" || fail "line $LINENO: a step before this check failed"
show=$(cps stash show)
[[ "$show" == *"A link"* && "$show" == *"A new.txt"* && "$show" == *"D gone.txt"* && "$show" == *"M notes.txt"* && "$show" == *"M run.sh"* ]] && ok "stash show lists the changes" || fail "show: $show"
out=$(cps stash pop); [[ "$out" == "Restored stash 0: work in progress"* ]] && ok "pop reports success" || fail "$out"
same "$(fingerprint)" "$before" "every change restored byte-for-byte (content, modes, symlink)"
same "$(cps status | tail -1)" "2 added, 2 modified, 1 deleted" "restored work shows as uncommitted changes"
same "$(cps stash list)" "No stashes." "pop removed the stash"
cps commit -m "the work" >/dev/null && ok "restored work commits normally" || fail "line $LINENO: a step before this check failed"

echo "== several stashes"
echo one > s.txt && cps stash -m first >/dev/null
echo two > s.txt && cps stash -m second >/dev/null
echo three > s.txt && cps stash >/dev/null
[[ "$(cps stash list)" == "0 "*"on main: "* ]] && ok "default message names the branch and commit" || fail "default message"
same "$(cps stash list | sed -n 2p | awk '{print $NF}') $(cps stash list | sed -n 3p | awk '{print $NF}')" "second first" "newest first"
cps stash apply 2 >/dev/null && same "$(cat s.txt)" "one" "apply picks a specific stash" || fail "line $LINENO: a step before this check failed"
same "$(cps stash list | wc -l | tr -d ' ')" "3" "apply keeps the stash"
expect_fail "applying over uncommitted changes to the same files is refused" "you've also changed" cps stash apply 1
rm s.txt
cps stash drop 1 | has "Dropped stash 1: second" && ok "drop removes a specific stash" || fail "line $LINENO: a step before this check failed"
expect_fail "missing stash number is reported" "there is no stash 7" cps stash pop 7
cps stash drop >/dev/null && cps stash drop >/dev/null
same "$(cps stash list)" "No stashes." "all dropped"
expect_fail "pop with no stashes is reported" "there are no stashes" cps stash pop

echo "== restore after history moved on"
sed -i '' 's/^1$/ONE-stashed/' notes.txt && cps stash >/dev/null
sed -i '' 's/^5$/FIVE-committed/' notes.txt && cps commit -m "later work" >/dev/null
cps stash pop >/dev/null
same "$(tr '\n' ' ' < notes.txt)" "ONE-stashed 2 THREE 4 FIVE-committed " "stash merges cleanly onto newer commits"
cps commit -m "stashed edit" >/dev/null

echo "== restore on a different branch"
echo feature-idea > idea.txt && cps stash >/dev/null
cps branch other >/dev/null && cps checkout other >/dev/null
cps stash pop >/dev/null && [[ "$(cat idea.txt)" == "feature-idea" ]] && ok "stash moves work between branches" || fail "line $LINENO: a step before this check failed"
cps commit -m idea >/dev/null && cps checkout main >/dev/null

echo "== conflict keeps the stash"
sed -i '' 's/^2$/TWO-stashed/' notes.txt && cps stash >/dev/null
sed -i '' 's/^2$/TWO-committed/' notes.txt && cps commit -m conflicting >/dev/null
out=$(cps stash pop)
[[ "$out" == *"1 conflict(s)"* && "$out" == *"stash was kept"* ]] && ok "pop reports the conflict" || fail "$out"
grep -q '^<<<<<<< current$' notes.txt && grep -q 'TWO-stashed' notes.txt && ok "conflict markers written" || fail "line $LINENO: a step before this check failed"
same "$(cps stash list | wc -l | tr -d ' ')" "1" "conflicted stash not dropped"
printf '1\nTWO-both\n3\n4\n5\n' > notes.txt && cps commit -m resolved >/dev/null && cps stash drop >/dev/null && ok "resolve, commit, then drop" || fail "line $LINENO: a step before this check failed"

echo "== guards during merge and rebase"
cps checkout other >/dev/null && sed -i '' 's/^4$/FOUR-other/' notes.txt && cps commit -m o >/dev/null
cps checkout main >/dev/null && sed -i '' 's/^4$/FOUR-main/' notes.txt && cps commit -m m >/dev/null
cps merge other >/dev/null
expect_fail "stash refused during a merge" "merge is in progress" cps stash
cps merge --abort >/dev/null
cps checkout other >/dev/null && cps rebase main >/dev/null
expect_fail "stash refused during a rebase" "rebase is in progress" cps stash
cps rebase --abort >/dev/null && cps checkout main >/dev/null

echo "== stashes survive gc"
echo precious > precious.txt && cps stash -m precious >/dev/null
cps gc --prune now >/dev/null && [[ -z "$(find .copse/objects -type f -not -path '*/pack/*')" ]] && ok "everything packed, and unused objects removed" || fail "line $LINENO: a step before this check failed"
cps stash pop >/dev/null && same "$(cat precious.txt)" "precious" "stash still restores after gc" || fail "line $LINENO: a step before this check failed"

echo
echo "All $pass checks passed."
