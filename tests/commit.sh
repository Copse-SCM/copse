#!/usr/bin/env bash
# Partial commits, commit --amend, and the full log (history order, --graph checked
# against git's own drawing, -p, --stat, --oneline, filters, --all, revisions).
# Usage: tests/commit.sh [work_dir]     (build first: cargo build --release; needs git)
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
titles() { cps log -n 100 "$@" | grep -v UTC | sed 's/^ *//' | grep -v '^$' | tr '\n' ',' | sed 's/,$//'; }
clean() { [[ "$(cps status | tail -1)" == "Nothing changed." ]]; }
tick() { perl -e 'select(undef,undef,undef,1.05)'; }   # distinct commit times keep ordering unambiguous

mkdir repo && cd repo && cps init >/dev/null
mkdir src && echo a > a.txt && echo b > b.txt && echo s > src/s.txt && echo old > src/old.txt
cps commit -m base >/dev/null

echo "== partial commits"
echo a2 > a.txt && echo b2 > b.txt
out=$(cps commit -m "only a" a.txt); [[ "$out" == *"0 added, 1 modified, 0 deleted"* && "$out" == *"1 other change(s) left uncommitted"* ]] && ok "commit one file, leaving another uncommitted" || fail "$out"
same "$(cps show --stat | grep '|' | sed 's/ *|.*//; s/^ *//')" "a.txt" "the commit contains only that file"
cps status | has "M b.txt" && ok "the other edit is still an uncommitted change" || fail "line $LINENO: a step before this check failed"
echo s2 > src/s.txt && rm src/old.txt && echo n > src/new.txt
out=$(cps commit -m "src only" src); [[ "$out" == *"1 added, 1 modified, 1 deleted"* ]] && ok "commit a folder: new, edited and deleted files under it" || fail "$out"
same "$(cps status | grep -E '^  [AMD] ' | sed 's/^ *//')" "M b.txt" "only b.txt is left"
same "$(cps commit -m nothing src)" "Nothing to commit under those paths." "nothing to commit under a path"
expect_fail "a path that matches nothing is reported" "doesn't match any file" cps commit -m x nope.txt
cd src && echo s3 > s.txt && cps commit -m "from inside src" s.txt >/dev/null && cd .. && cps status | has "M b.txt" && ok "paths are relative to where you are" || fail "line $LINENO: a step before this check failed"
cps commit -m "the rest" >/dev/null && clean && ok "a plain commit takes everything that's left" || fail "line $LINENO: a step before this check failed"

echo "== commit --amend"
before_date=$(cps show | grep '^Date:')
cps commit --amend -m "the rest, reworded" | has "(amended)" && same "$(titles -n 2)" "the rest, reworded,from inside src" "amend just the message" || fail "line $LINENO: a step before this check failed"
same "$(cps show | grep '^Date:')" "$before_date" "the original date is kept"
echo forgot > forgot.txt
cps commit --amend >/dev/null && same "$(titles -n 1)" "the rest, reworded" "amend adds forgotten changes and keeps the message" || fail "line $LINENO: a step before this check failed"
cps show --stat | has "forgot.txt" && clean && ok "the change is in the amended commit" || fail "line $LINENO: a step before this check failed"
echo x > x.txt && echo y > y.txt && cps commit --amend x.txt >/dev/null && cps status | has "A y.txt" && ok "amend with paths takes only those" || fail "line $LINENO: a step before this check failed"
cps undo >/dev/null 2>&1 || true
cps reset --hard >/dev/null 2>&1; cps stash drop >/dev/null 2>&1 || true
rm -f y.txt
[[ "$(cps reflog | head -3)" == *"commit --amend"* ]] && ok "amends are in the reflog (so undo can reverse them)" || fail "line $LINENO: a step before this check failed"
cd .. && mkdir empty && cd empty && cps init >/dev/null && echo z > z
expect_fail "nothing to amend before the first commit" "no commit to amend" cps commit --amend -m x
cd ../repo

echo "== commit -p: pick changes one at a time"
cd .. && mkdir pk && cd pk && cps init >/dev/null
seq 1 30 | sed 's/^/line /' > f.txt && echo gone > old.txt && printf 'bin\0ary' > b.bin && cps commit -m base >/dev/null
sedi 's/^line 3$/LINE THREE/; s/^line 25$/LINE 25/' f.txt && echo new > new.txt && rm old.txt
out=$(printf 'y\nn\ny\nn\n' | NO_COLOR=1 cps commit -p -m "picked"); [[ "$out" == *"(change 1 of 2)"* && "$out" == *"(picked changes)"* ]] && ok "commit -p shows each change and asks about it" || fail "$out"
cps show | has "+LINE THREE" && ! cps show | has "+LINE 25" && cps show | has "new.txt" && ! cps show | has "old.txt" && ok "only the picked changes are committed" || fail "line $LINENO: a step before this check failed"
grep -q "LINE 25" f.txt && [[ ! -e old.txt ]] && ok "your files are untouched" || fail "line $LINENO: a step before this check failed"
same "$(cps status | grep -E '^  [AMD] ' | sed 's/^ *//' | tr '\n' ',')" "M f.txt,D old.txt," "what wasn't picked is still an uncommitted change"
cps diff | has "+LINE 25" && ! cps diff | has "LINE THREE" && ok "and diff shows just that part" || fail "line $LINENO: a step before this check failed"
cps commit -m "the rest" >/dev/null && cps status | tail -1 | has "Nothing changed." && ok "a normal commit then takes the rest" || fail "line $LINENO: a step before this check failed"
for n in 2 12 22; do sedi "s/^line $n\$/LINE $n/" f.txt; done
printf 'n\na\n' | cps commit -p -m "a takes the rest" >/dev/null && ! cps show | has "+LINE 2$" && cps show | has "+LINE 12" && cps show | has "+LINE 22" && ok "a takes this change and the rest of the file" || fail "line $LINENO: a step before this check failed"
cps commit -m sync >/dev/null; for n in 4 14 24; do sedi "s/^line $n\$/LINE $n/" f.txt; done
printf 'y\nd\n' | cps commit -p -m "d skips the rest" >/dev/null && cps show | has "+LINE 4" && ! cps show | has "LINE 14" && ! cps show | has "LINE 24" && ok "d skips the rest of the file" || fail "line $LINENO: a step before this check failed"
cps commit -m sync2 >/dev/null; echo more > more.txt && echo extra > extra.txt
printf 'y\nq\n' | cps commit -p -m "q keeps what's picked" >/dev/null && cps show | has "extra.txt" && ! cps show | has "more.txt" && ok "q stops, committing what was picked so far" || fail "line $LINENO: a step before this check failed"
before=$(cps log --oneline -n 1)
printf 'n\n' | cps commit -p -m none | has "Nothing picked, so nothing was committed." && same "$(cps log --oneline -n 1)" "$before" "picking nothing commits nothing" || fail "line $LINENO: a step before this check failed"
printf 'y\nA message typed after picking\n' | cps commit -p >/dev/null && same "$(cps log -n 1 --oneline | sed -E 's/^[0-9a-f]{10} (\([^)]*\) )?//')" "A message typed after picking" "without -m, it asks for the message after picking"
printf 'bin\0ARY' > b.bin && printf 'y\n' | NO_COLOR=1 cps commit -p -m bin | has "(not text): commit the change?" && ok "files that aren't text are all-or-nothing" || fail "line $LINENO: a step before this check failed"
mkdir -p sub && echo s > sub/s.txt && echo t > top2.txt && printf 'y\n' | cps commit -p -m "just sub" sub >/dev/null && cps show | has "sub/s.txt" && ! cps show | has "top2.txt" && ok "-p with a path asks only about changes there" || fail "line $LINENO: a step before this check failed"
expect_fail "-p and --amend don't mix" "don't go together" cps commit -p --amend -m x
cps fsck | has "everything is intact" && ok "fsck agrees after all that" || fail "line $LINENO: a step before this check failed"
cd ../repo   # where the next section expects to start

echo "== log: whole history and options"
cd .. && rm -rf hist && mkdir hist && cd hist && cps init >/dev/null
echo 1 > a && cps commit -m base >/dev/null && tick
cps branch feature >/dev/null && cps checkout feature >/dev/null
echo f > f && cps commit -m "feature one" >/dev/null && tick && echo f2 >> f && cps commit -m "feature two" >/dev/null && tick
cps checkout main >/dev/null && echo m > m && cps commit -m "main work" >/dev/null && tick
cps merge feature >/dev/null && tick && echo after >> a && cps commit -m "after merge" >/dev/null
same "$(titles)" "after merge,Merge feature into main,main work,feature two,feature one,base" "log shows all of history, merged branches included"
same "$(titles --first-parent)" "after merge,Merge feature into main,main work,base" "--first-parent follows only the main line"
same "$(titles feature)" "feature two,feature one,base" "log REV starts from that revision"
same "$(cps log --oneline -n 1 | sed 's/^[0-9a-f]\{10\} //')" "(main) after merge" "--oneline: id, labels and title"
same "$(titles -n 2)" "after merge,Merge feature into main" "-n limits the count"
same "$(titles --grep FEATURE)" "Merge feature into main,feature two,feature one" "--grep matches messages, ignoring case"
same "$(titles --author 'test person' -n 1)" "after merge" "--author matches authors"
same "$(titles --author nobody)" "" "and filters out the rest"
same "$(titles -- f)" "feature two,feature one" "-- PATH shows only commits that changed it (not the merge that brought it in)"
cps log -p -n 1 | has "+after" && ok "-p shows each commit's changes" || fail "line $LINENO: a step before this check failed"
cps log --stat -n 1 | has "1 file changed, 1 insertion(+)" && ok "--stat summarises them" || fail "line $LINENO: a step before this check failed"
cps branch other HEAD~1 >/dev/null && cps checkout other >/dev/null && echo o > o && cps commit -m "other work" >/dev/null && cps checkout main >/dev/null
[[ "$(titles)" != *"other work"* && "$(titles --all)" == *"other work"* ]] && ok "--all includes every branch" || fail "line $LINENO: a step before this check failed"

echo "== --graph matches git's drawing"
cd .. && git init -q --bare -b main graph.git && cd hist && cps remote add gh ../graph.git >/dev/null && cps push gh >/dev/null
shape() { sed -E 's/[0-9a-f]{10} //; s/\([^)]*\) //; s/ +/ /g; s/ +$//'; }
same "$(cps log --oneline --graph main | shape)" "$(git -C ../graph.git log --graph --format=%s main | shape)" "same lines and order as git log --graph"
git -C ../graph.git log --format=%s --topo-order -- f > /dev/null
same "$(titles -- f | tr ',' '\n' | sort)" "$(git -C ../graph.git log --format=%s -- f | sort)" "-- PATH picks the same commits as git"

echo
echo "All $pass checks passed."
