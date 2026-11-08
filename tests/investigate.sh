#!/usr/bin/env bash
# blame (every line checked against git blame) and bisect.
# Usage: tests/investigate.sh [work_dir]     (build first: cargo build --release; needs git)
set -euo pipefail
. "$(cd "$(dirname "$0")" && pwd)/portable.sh"   # sedi, mtime, days_ago, peak_mb, ... (macOS and Linux)

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
tick() { perl -e 'select(undef,undef,undef,1.05)'; }
plain() { sed 's/\x1b\[[0-9;]*m//g'; }
# Per line: the title of the commit copse blames it on.
copse_titles() {
  cps blame "$@" | plain | while read -r id _; do
    if [[ "$id" == 0000000000 ]]; then echo "(uncommitted)"; else cps log --oneline -n 1 "$id" | sed -E 's/^[0-9a-f]{10} (\([^)]*\) )?//'; fi
  done
}
# The same from git blame, in a clone made by pushing.
git_titles() {
  git -C ../g blame --porcelain "$@" | awk '/^[0-9a-f]{40} [0-9]+ [0-9]+/ {print $1}' | while read -r sha; do git -C ../g log -1 --format=%s "$sha"; done
}

echo "== blame"
mkdir repo && cd repo && cps init >/dev/null
printf 'one\ntwo\nthree\nfour\nfive\nsix\n' > f.txt && echo keep > other.txt && cps commit -m "c1 base" >/dev/null && tick
sedi 's/^two$/TWO/' f.txt && cps commit -m "c2 shout two" >/dev/null && tick
COPSE_AUTHOR="Other Dev" COPSE_EMAIL=o@example.com sh -c 'printf "zero\n" | cat - f.txt > t && mv t f.txt' && COPSE_AUTHOR="Other Dev" COPSE_EMAIL=o@example.com cps commit -m "c3 add zero (other dev)" >/dev/null && tick
cps branch side >/dev/null && cps checkout side >/dev/null
sedi 's/^six$/SIX/' f.txt && echo seven >> f.txt && cps commit -m "c4 side: six, seven" >/dev/null && tick
cps checkout main >/dev/null
sedi '/^four$/d' f.txt && sedi 's/^one$/one!/' f.txt && cps commit -m "c5 main: drop four, one!" >/dev/null && tick
cps merge side >/dev/null && tick
sedi 's/^three$/three (after merge)/' f.txt && cps commit -m "c6 after merge" >/dev/null && tick
echo more >> other.txt && cps commit -m "c7 unrelated" >/dev/null

out=$(cps blame f.txt | plain)
same "$(echo "$out" | wc -l | tr -d ' ')" "$(wc -l < f.txt | tr -d ' ')" "one line of output per line of the file"
[[ "$(echo "$out" | sed -n 1p)" == *"(Other Dev"*" 1) zero" ]] && ok "each line shows the author, line number and text" || fail "$(echo "$out" | sed -n 1p)"
same "$(copse_titles f.txt | sed -n 1,3p | tr '\n' ',')" "c3 add zero (other dev),c5 main: drop four, one!,c2 shout two," "lines are blamed on the commit that last changed them"
same "$(copse_titles f.txt | sed -n 6p),$(copse_titles f.txt | tail -1)" "c4 side: six, seven,c4 side: six, seven" "lines from a merged branch are blamed on the branch's commit"

cd .. && git init -q --bare -b main g.git && cd repo && cps remote add gh ../g.git >/dev/null && cps push gh >/dev/null && cps push gh side >/dev/null && git clone -q ../g.git ../g
same "$(copse_titles f.txt HEAD)" "$(git_titles f.txt)" "every line matches git blame"
same "$(copse_titles f.txt HEAD~2)" "$(git_titles HEAD~2 -- f.txt)" "and in an older revision"
same "$(copse_titles f.txt side)" "$(git_titles origin/side -- f.txt)" "and on another branch"
same "$(copse_titles -L 2,4 f.txt HEAD)" "$(git_titles -L 2,4 f.txt)" "-L START,END limits the lines"
same "$(cps blame -L 6, f.txt | wc -l | tr -d ' ')" "2" "-L START, goes to the end"

echo "uncommitted" >> f.txt && sedi 's/^TWO$/TWO edited/' f.txt
same "$(copse_titles f.txt | sed -n '3p;$p' | tr '\n' ',')" "(uncommitted),(uncommitted)," "lines you haven't committed are marked as such"
cps blame f.txt | plain | has "Not committed yet" && ok "with 'Not committed yet' where the author would be" || fail "line $LINENO: a step before this check failed"
cps reset --hard >/dev/null && cps stash drop >/dev/null
mkdir sub && echo s > sub/s.txt && cps commit -m "c8 sub" >/dev/null && cd sub
same "$(copse_titles s.txt)" "c8 sub" "paths are relative to where you are"
cd ..
expect_fail "a file that isn't committed or there" "can't read" cps blame nope.txt
expect_fail "-L outside the file" "outside the file" cps blame -L 50,60 f.txt
python3 -c "open('bin.dat','wb').write(b'a\x00b')" && cps commit -m bin >/dev/null
expect_fail "binary files are refused" "isn't a text file" cps blame bin.dat

echo "== bisect"
cd .. && mkdir bis && cd bis && cps init >/dev/null
for i in $(seq 1 30); do echo "$i" > n.txt; [[ $i -ge 19 ]] && echo broken > bug.txt; cps commit -m "c$i" >/dev/null; done
cps branch keepme >/dev/null
same "$(cps bisect)" "No bisect in progress. Start one with \`copse bisect start BAD GOOD\`." "no bisect to begin with"
expect_fail "good must be older than bad" "check the order" cps bisect start HEAD~29 HEAD
out=$(cps bisect start HEAD HEAD~29); [[ "$out" == *"Bisecting: 28 commit(s) left to test"* ]] && ok "start checks out a commit in the middle" || fail "$out"
cps status | has "HEAD detached" && cps status | has "Bisecting: mark this commit" && ok "detached, so your branch isn't touched; status says you're bisecting" || fail "line $LINENO: a step before this check failed"
expect_fail "only one bisect at a time" "already in progress" cps bisect start HEAD HEAD~1
steps=0
while :; do
  if [[ -f bug.txt ]]; then out=$(cps bisect bad); else out=$(cps bisect good); fi
  steps=$((steps + 1))
  [[ "$out" == *"is the first bad commit"* ]] && break
  [[ $steps -lt 10 ]] || fail "bisect didn't finish in 10 steps"
done
[[ "$out" == *"    c19"* ]] && ok "marking good and bad by hand finds the first bad commit" || fail "$out"
[[ $steps -le 5 ]] && ok "in $steps steps for 29 commits (binary search)" || fail "took $steps steps"
echo dirt > dirt.txt
expect_fail "reset won't overwrite uncommitted work" "commit or stash" cps bisect reset
rm dirt.txt
same "$(cps bisect reset)" "Bisect ended; back on main." "reset goes back to the branch"
same "$(cps status | head -1),$(cat n.txt)" "On branch main,30" "with its files"

echo x > dirt.txt
expect_fail "bisect won't start with uncommitted changes" "commit or stash" cps bisect start HEAD HEAD~29
rm dirt.txt
cps bisect start HEAD HEAD~29 >/dev/null
out=$(cps bisect run sh -c 'test ! -f bug.txt'); [[ "$out" == *"    c19"* ]] && ok "run finds it automatically from a command's exit status" || fail "$out"
cps bisect reset >/dev/null
cps bisect start HEAD HEAD~29 >/dev/null
out=$(cps bisect run 'n=$(cat n.txt); if [ "$n" -ge 15 ] && [ "$n" -le 22 ]; then exit 125; fi; test ! -f bug.txt')
[[ "$out" == *"skip (exit 125)"* && "$out" == *"Only skipped commits are left"* ]] && ok "exit 125 skips a commit; with the answer among skipped ones, it says so" || fail "$out"
cps bisect reset >/dev/null
cps bisect start >/dev/null
same "$(cps bisect bad | tail -1)" "Waiting for a good commit (copse bisect good REV)." "start with nothing marked waits for marks"
cps bisect good HEAD~29 | has "left to test" && ok "then marking good begins the search" || fail "line $LINENO: a step before this check failed"
cps bisect skip | has "left to test" && ok "skip moves on to another commit" || fail "line $LINENO: a step before this check failed"
cps bisect reset >/dev/null
cps checkout keepme~3 >/dev/null 2>&1 && cps bisect start HEAD HEAD~5 >/dev/null && cps bisect reset | has "back on" && cps status | has "HEAD detached at" && ok "starting from a detached HEAD goes back there" || fail "line $LINENO: a step before this check failed"
cps checkout main >/dev/null
expect_fail "marks need a bisect" "no bisect is in progress" cps bisect good

echo
echo "All $pass checks passed."
