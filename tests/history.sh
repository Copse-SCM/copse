#!/usr/bin/env bash
# cherry-pick, revert, clean and grep.
# Usage: tests/history.sh [work_dir]     (build first: cargo build --release)
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
titles() { cps log -n 100 --first-parent "$@" | grep -v UTC | sed 's/^ *//' | grep -v '^$' | tr '\n' ',' | sed 's/,$//'; }
lines() { tr '\n' ' ' < "$1" | sed 's/ $//'; }
clean_tree() { [[ "$(cps status | tail -1)" == "Nothing changed." ]]; }

mkdir repo && cd repo && cps init >/dev/null
printf '1\n2\n3\n4\n5\n' > n.txt && cps commit -m base >/dev/null
cps branch topic >/dev/null && cps checkout topic >/dev/null
sedi 's/^2$/TWO/' n.txt && cps commit -m "topic: two" >/dev/null
echo t > t.txt && cps commit -m "topic: add t" >/dev/null
sedi 's/^4$/FOUR/' n.txt && cps commit -m "topic: four" >/dev/null
cps checkout main >/dev/null && echo m > m.txt && cps commit -m "main work" >/dev/null

echo "== cherry-pick"
COPSE_AUTHOR="Someone Else" COPSE_EMAIL="else@example.com" sh -c "cd ../ && true"
out=$(cps cherry-pick topic~2); [[ "$out" == *"Cherry-picked 1 commit(s) onto main"* ]] && ok "cherry-pick one commit" || fail "$out"
same "$(lines n.txt) | $(titles -n 2)" "1 TWO 3 4 5 | topic: two,main work" "its change and message arrive on main"
same "$(cps log -n 1 | head -1 | awk '{print $(NF-1), $NF}')" "Test Person" "the original author is kept"
cps cherry-pick topic~2..topic | has "Cherry-picked 2 commit(s)" && same "$(titles -n 3)" "topic: four,topic: add t,topic: two" "a range picks its commits oldest first" || fail "line $LINENO: a step before this check failed"
same "$(lines n.txt) | $(cat t.txt)" "1 TWO 3 FOUR 5 | t" "with all their changes"
cps cherry-pick topic | has "already there" && ok "a change that's already there is skipped, not committed empty" || fail "line $LINENO: a step before this check failed"
clean_tree && ok "clean afterwards" || fail "line $LINENO: a step before this check failed"

echo "== cherry-pick conflicts"
cps branch other HEAD~3 >/dev/null && cps checkout other >/dev/null && sedi 's/^2$/deux/' n.txt && cps commit -m "other: deux" >/dev/null && echo o > o.txt && cps commit -m "other: add o" >/dev/null && cps checkout main >/dev/null
out=$(cps cherry-pick other~1 other); [[ "$out" == *"Conflict while applying"*"other: deux"*"copse cherry-pick --continue"* ]] && ok "a conflict stops with cherry-pick instructions" || fail "$out"
cps status | has "Cherry-picking onto main (0 of 2 commits done)" && ok "status shows the cherry-pick" || fail "line $LINENO: a step before this check failed"
expect_fail "other commands name the cherry-pick" "a cherry-pick is in progress" cps commit -m x
expect_fail "rebase --continue style mixups are caught" "is a cherry-pick, not a revert" cps revert --continue
printf '1\nTWO-deux\n3\nFOUR\n5\n' > n.txt
cps cherry-pick --continue | has "Cherry-picked 2 commit(s)" && same "$(lines n.txt) | $(cat o.txt)" "1 TWO-deux 3 FOUR 5 | o" "continue after fixing finishes the rest" || fail "line $LINENO: a step before this check failed"
cps reset --hard HEAD~2 >/dev/null
cps cherry-pick other~1 other >/dev/null || true
cps cherry-pick --skip | has "Cherry-picked 1 commit(s)" && [[ -f o.txt && "$(sed -n 2p n.txt)" == "TWO" ]] && ok "--skip drops the conflicting commit and picks the rest" || fail "line $LINENO: a step before this check failed"
cps reset --hard HEAD~1 >/dev/null
before=$(titles)
cps cherry-pick other~1 >/dev/null || true
cps cherry-pick --abort | has "Cherry-pick aborted" && same "$(titles)" "$before" "--abort puts everything back" || fail "line $LINENO: a step before this check failed"
clean_tree && ok "clean after abort" || fail "line $LINENO: a step before this check failed"

echo "== merges need a parent"
cps branch side HEAD~1 >/dev/null && cps checkout side >/dev/null && echo s > s.txt && cps commit -m "side: s" >/dev/null && cps checkout topic >/dev/null && cps merge side >/dev/null
merge_id=$(cps log -n 1 | awk "NR==1" | cut -c1-10)
cps checkout main >/dev/null
expect_fail "cherry-picking a merge asks which parent" "--mainline 1" cps cherry-pick "$merge_id"
cps cherry-pick --abort >/dev/null 2>&1 || true
cps cherry-pick -m 1 "$merge_id" >/dev/null && [[ -f s.txt ]] && ok "-m 1 picks what the merge brought in" || fail "line $LINENO: a step before this check failed"

echo "== revert"
out=$(cps revert HEAD); [[ "$out" == *"Reverted 1 commit(s) on main"* && ! -e s.txt ]] && ok "revert undoes a commit" || fail "$out"
msg=$(cps log -n 1 | sed -n '2,4p' | sed 's/^ *//' | tr '\n' '|'); [[ "$msg" == "Revert \""*"\"||This reverts commit "* ]] && ok "with git's standard message" || fail "$msg"
cps revert HEAD~2 HEAD~3 >/dev/null && same "$(sed -n 4p n.txt) | $(test -e t.txt && echo t || echo no-t)" "4 | no-t" "revert several commits at once" || fail "line $LINENO: a step before this check failed"
cps reset --hard HEAD~2 >/dev/null
cps revert HEAD~4..HEAD~2 | has "Reverted 2 commit(s)" && [[ ! -e t.txt && "$(sed -n 4p n.txt)" == "4" ]] && ok "revert a range (newest first)" || fail "line $LINENO: a step before this check failed"
printf 'a\nb\nc\n' > r.txt && cps commit -m "r base" >/dev/null
sedi 's/^b$/B/' r.txt && cps commit -m "r: b to B" >/dev/null
sedi 's/^B$/BB/' r.txt && cps commit -m "r: B to BB" >/dev/null
out=$(cps revert HEAD~1); [[ "$out" == *"Conflict"*"copse revert --continue"* ]] && ok "a conflicting revert stops with revert instructions" || fail "$out"
printf 'a\nb\nc\n' > r.txt && cps revert --continue | has "Reverted 1 commit(s)" && same "$(lines r.txt)" "a b c" "and --continue finishes it" || fail "line $LINENO: a step before this check failed"
expect_fail "revert needs a commit" "say which commit" cps revert

echo "== clean"
echo junk > junk.txt && mkdir -p tmp/deep && echo x > tmp/deep/x.txt && echo build/ > .copseignore && cps commit -m "ignore build" .copseignore >/dev/null && mkdir -p build && echo o > build/out.o
out=$(cps clean); [[ "$out" == *"Would remove junk.txt"* && "$out" == *"Would remove tmp/deep/x.txt"* && "$out" == *"copse clean -f"* ]] && ok "clean on its own only lists new files" || fail "$out"
[[ -f junk.txt ]] && ok "and removes nothing" || fail "line $LINENO: a step before this check failed"
cps clean -f tmp | has "Removed 1 file(s)" && [[ ! -e tmp && -f junk.txt ]] && ok "clean -f PATH removes only there, and empty folders too" || fail "line $LINENO: a step before this check failed"
cps clean -f | has "Saved your uncommitted work as stash 0" && [[ ! -e junk.txt && -f build/out.o ]] && ok "clean -f removes new files, keeping ignored ones" || fail "line $LINENO: a step before this check failed"
cps stash pop >/dev/null && [[ -f junk.txt ]] && ok "and the removed files can be brought back from the stash" || fail "line $LINENO: a step before this check failed"
rm -rf junk.txt tmp
cps clean -f -x | has "build/out.o" && [[ ! -e build ]] && ok "-x also removes ignored files" || fail "line $LINENO: a step before this check failed"
same "$(cps clean)" "Nothing to clean." "nothing to clean"

echo "== grep"
printf 'alpha beta\nGamma delta\nalpha again\n' > words.txt && printf 'no match here\n' > other.txt && python3 -c "open('bin.dat','wb').write(b'alpha\x00binary')"
same "$(cps grep alpha | sed 's/\x1b\[[0-9;]*m//g')" $'words.txt:1:alpha beta\nwords.txt:3:alpha again' "find matching lines with line numbers (binary files skipped)"
same "$(cps grep -i gamma | cut -d: -f1-2)" "words.txt:2" "-i ignores case"
same "$(cps grep -l 'a.+a')" "words.txt" "-l lists files; patterns are regular expressions"
same "$(cps grep -c alpha)" "words.txt:2" "-c counts"
cps commit -m words words.txt other.txt bin.dat >/dev/null && sedi '1s/alpha/omega/' words.txt
same "$(cps grep -c alpha HEAD)" "HEAD:words.txt:2" "search a revision instead of your files"
same "$(cps grep -c alpha 2>/dev/null; echo "exit $?")" "words.txt:1
exit 0" "your files are searched as they are now"
cps grep nothing-matches >/dev/null 2>&1 && fail "no match should exit 1" || ok "no match exits with status 1, like grep"
same "$(cps grep -l match -- other.txt)" "other.txt" "-- PATHS limits the search"

echo
echo "All $pass checks passed."
