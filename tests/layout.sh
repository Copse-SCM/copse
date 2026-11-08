#!/usr/bin/env bash
# Tags between copse repositories, shallow clones, sparse checkout and worktrees.
# Usage: tests/layout.sh [work_dir]     (build first: cargo build --release)
set -euo pipefail
. "$(cd "$(dirname "$0")" && pwd)/portable.sh"   # sedi, mtime, days_ago, peak_mb, ... (macOS and Linux)

COPSE="$(cd "$(dirname "$0")/.." && pwd)/target/release/copse"
WORK="${1:-$(mktemp -d)}"
rm -rf "$WORK" && mkdir -p "$WORK" && cd "$WORK"
WORK="$(pwd -P)"
cps() { "$COPSE" "$@"; }
export COPSE_AUTHOR="Test Person" COPSE_EMAIL="test@example.com" COPSE_FSYNC=0
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1  # your own git settings (signing, identity) stay out of the tests
export HOME="$WORK/home" XDG_CONFIG_HOME="$WORK/home/.config"; mkdir -p "$HOME"
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
titles() { cps log -n 100 --oneline "$@" | sed -E 's/^[0-9a-f]{10} (\([^)]*\) )?//' | tr '\n' ',' | sed 's/,$//'; }
tags() { cps tag | awk '{print $1}' | tr '\n' ',' | sed 's/,$//'; }
files() { find . -path ./.copse -prune -o -type f -print | sed 's|^\./||' | sort | tr '\n' ' ' | sed 's/ $//'; }

mkdir src && cd src && cps init >/dev/null
for i in $(seq 1 10); do echo "$i" > n.txt; echo "file $i" > "f$i.txt"; cps commit -m "c$i" >/dev/null; done
cps tag v0.5 HEAD~5 >/dev/null && cps tag -a v1.0 -m "Release 1.0" >/dev/null
cd ..

echo "== tags between copse repositories"
cps clone src full >/dev/null
same "$(cd full && tags)" "v0.5,v1.0" "clone brings tags, lightweight and annotated"
same "$(cd full && cps show v1.0 | head -1)" "tag v1.0" "an annotated tag arrives whole"
cps init --bare server >/dev/null
cd src && cps remote add up ../server >/dev/null && cps push up >/dev/null
out=$(cps push up --tags); [[ "$out" == *"Pushed 2 tag(s) to up"* ]] && ok "push --tags sends every tag" || fail "$out"
same "$(cps push up --tags | tail -1)" "Pushed 0 tag(s) to up (2 already there)." "pushing again sends nothing"
cps tag v1.1 >/dev/null && cps push up v1.1 | has "tag v1.1  new" && ok "push NAME sends one tag" || fail "line $LINENO: a step before this check failed"
cps tag -d v0.5 >/dev/null && cps tag v0.5 HEAD~1 >/dev/null
expect_fail "a tag that differs on the remote is refused" "already exist on up with a different commit" cps push up v0.5
cps push up v0.5 --force | has "replaced" && ok "unless --force" || fail "line $LINENO: a step before this check failed"
cps push up --delete v1.1 >/dev/null 2>&1 || true
cd .. && cps clone server fromserver >/dev/null
same "$(cd fromserver && tags)" "v0.5,v1.0,v1.1" "a clone of the server has them all"
cd full && cps tag mine HEAD~2 >/dev/null && old_v05=$(cps tag | awk '$1=="v0.5" {print $2}')
out=$(cps fetch); [[ "$out" == *"tag v1.1  new tag"* && "$out" == *"tag v0.5 on the server differs from yours; kept yours"* ]] && ok "fetch brings new tags, and says when one differs" || fail "$out"
same "$(tags) $(cps tag | awk '$1=="v0.5" {print $2}')" "mine,v0.5,v1.0,v1.1 $old_v05" "keeping your own tags as they are"
cd ..

echo "== shallow clones"
(cd src && cps tag ancient HEAD~7 >/dev/null)
out=$(cps clone --depth 3 src shallow); [[ "$out" == *"Checked out main (11 files)"* ]] && ok "clone --depth 3 checks out every file" || fail "$out"
cd shallow
same "$(titles)" "c10,c9,c8" "with only the newest 3 commits of history"
same "$(tags)" "v0.5,v1.0,v1.1" "and only the tags that point into it (not 'ancient')"
cps fsck | has "everything is intact" && ok "fsck accepts the shallow boundary" || fail "line $LINENO: a step before this check failed"
[[ -f .copse/shallow ]] && ok "the boundary is recorded in .copse/shallow" || fail "line $LINENO: a step before this check failed"
size_shallow=$(bytes_in .copse/objects)
cps gc --prune now >/dev/null && cps fsck >/dev/null && ok "gc works on it" || fail "line $LINENO: a step before this check failed"
cd ../src && echo 11 > n.txt && cps commit -m c11 >/dev/null && cd ../shallow
cps pull >/dev/null && same "$(titles)" "c11,c10,c9,c8" "pull brings new commits, staying shallow" || fail "line $LINENO: a step before this check failed"
echo mine > mine.txt && cps commit -m "local work" >/dev/null && same "$(cps log --oneline -n 1 | cut -c12-)" "(main) local work" "you can commit as usual" || fail "line $LINENO: a step before this check failed"
cps init --bare ../server2 >/dev/null && cps remote add s2 ../server2 >/dev/null
cps push s2 >/dev/null && cd .. && cps clone server2 fromshallow >/dev/null && same "$(cd fromshallow && titles)" "local work,c11,c10,c9,c8" "a shallow clone can be pushed from, and cloning that gives the same shallow history" || fail "line $LINENO: a step before this check failed"
[[ -f fromshallow/.copse/shallow ]] && ok "which is shallow too" || fail "line $LINENO: a step before this check failed"
cd shallow
out=$(cps fetch --unshallow); [[ "$out" == *"no longer a shallow clone"* ]] && ok "fetch --unshallow fetches the rest" || fail "$out"
same "$(cps log --oneline -n 100 | wc -l | tr -d ' ')" "12" "giving all of history"
[[ ! -f .copse/shallow ]] && cps fsck >/dev/null && ok "and it's an ordinary repository again" || fail "line $LINENO: a step before this check failed"
same "$(tags)" "ancient,v0.5,v1.0,v1.1" "tags from the older history arrive too"
expect_fail "--unshallow on a full repository is explained" "isn't shallow" cps fetch --unshallow
mkdir ../gitsrc && cd ../gitsrc && git init -q -b main && echo x > x && git add x && git -c user.name=a -c user.email=b commit -qm x && cd ..
expect_fail "--depth with a git server says it isn't supported yet" "copse remotes" cps clone --depth 1 "$WORK/gitsrc/.git" gitshallow
cd "$WORK"

echo "== sparse checkout"
cd "$WORK" && mkdir sp && cd sp && cps init >/dev/null
mkdir -p app/ui lib docs && echo a > app/a.txt && echo u > app/ui/u.txt && echo l > lib/l.txt && echo d > docs/d.txt && echo top > README
cps commit -m base >/dev/null
out=$(cps sparse set app); [[ "$out" == *"2 removed"* ]] && ok "sparse set keeps only those folders on disk" || fail "$out"
same "$(files)" "README app/a.txt app/ui/u.txt" "plus top-level files"
same "$(cps status | tail -1)" "Nothing changed." "the missing files don't count as deleted"
echo a2 > app/a.txt && cps commit -m "app change" >/dev/null
same "$(cps show --stat | grep '|' | sed 's/ *|.*//; s/^ *//')" "app/a.txt" "commits only record what you changed"
same "$(cps grep -c . HEAD -- lib docs | tr '\n' ' ')" "HEAD:docs/d.txt:1 HEAD:lib/l.txt:1 " "and still contain every file"
cps diff HEAD~1 --stat | has "1 file changed" && ok "diff doesn't show the missing files" || fail "line $LINENO: a step before this check failed"
cps branch other HEAD~1 >/dev/null
cps checkout other >/dev/null && same "$(files)" "README app/a.txt app/ui/u.txt" "switching branches leaves the other folders off disk" || fail "line $LINENO: a step before this check failed"
same "$(cat app/a.txt)" "a" "while updating the checked-out ones"
cps checkout main >/dev/null
echo stashme > app/s.txt && cps stash >/dev/null && cps stash pop >/dev/null && same "$(files)" "README app/a.txt app/s.txt app/ui/u.txt" "stash and pop work, without bringing the other folders back" || fail "line $LINENO: a step before this check failed"
rm app/s.txt
cps sparse add lib >/dev/null && same "$(files)" "README app/a.txt app/ui/u.txt lib/l.txt" "sparse add checks out another folder" || fail "line $LINENO: a step before this check failed"
same "$(cps sparse | tail -2 | tr -d ' ' | tr '\n' ' ')" "app/ lib/ " "sparse lists the folders"
echo dirty > lib/l.txt
expect_fail "leaving out a folder with uncommitted changes is refused" "commit or stash your changes" cps sparse set app
cps restore lib/l.txt >/dev/null
cps sparse disable >/dev/null && same "$(files)" "README app/a.txt app/ui/u.txt docs/d.txt lib/l.txt" "sparse disable brings everything back" || fail "line $LINENO: a step before this check failed"
cps sparse set app >/dev/null
cps branch conflict HEAD >/dev/null && cps sparse disable >/dev/null && cps checkout conflict >/dev/null && echo "theirs" > docs/d.txt && cps commit -m "docs on conflict" >/dev/null && cps checkout main >/dev/null && echo "ours" > docs/d.txt && cps commit -m "docs on main" >/dev/null && cps sparse set app >/dev/null
[[ ! -e docs/d.txt ]] && ok "(docs is off disk again)" || fail "line $LINENO: a step before this check failed"
cps merge conflict >/dev/null || true
[[ -f docs/d.txt ]] && grep -q "<<<<<<<" docs/d.txt && ok "a conflict outside the checked-out folders is written out so you can fix it" || fail "line $LINENO: a step before this check failed"
echo "both" > docs/d.txt && cps commit -m "merged" >/dev/null || fail "line $LINENO: committing the merge failed"
same "$(cps grep -c both HEAD -- docs)" "HEAD:docs/d.txt:1" "and committing the fix records it in the merge"
cd ..

echo "== worktrees"
cd "$WORK/src"
out=$(cps worktree add ../feature -b feature); [[ "$out" == *"Made a worktree at $WORK/feature on branch feature"* ]] && ok "worktree add -b makes a new folder on a new branch" || fail "$out"
same "$(cd ../feature && files | wc -w | tr -d ' ')" "$(files | wc -w | tr -d ' ')" "with all the files checked out"
cd ../feature && echo feature > feature.txt && cps commit -m "feature work" >/dev/null
same "$(cd ../src && titles feature -n 1)" "feature work" "commits made there are seen from the main folder at once"
cd ../src && [[ ! -e feature.txt ]] && same "$(cps status | head -1)" "On branch main" "the main folder stays on its own branch, untouched" || fail "line $LINENO: a step before this check failed"
cd ../feature && cps stash list | has "No stashes" && cd ../src && echo s > s.txt && cps stash -m "from main" >/dev/null && cd ../feature && cps stash list | has "from main" && ok "stashes are shared" || fail "line $LINENO: a step before this check failed"
cd ../src && cps stash pop >/dev/null && rm s.txt
cps config alias.wl "worktree list" >/dev/null && cd ../feature && cps wl | has "feature" && ok "settings are shared" || fail "line $LINENO: a step before this check failed"
expect_fail "a branch can't be checked out in two worktrees" "checked out in the worktree at $WORK/src" cps checkout main
cd ../src
expect_fail "nor deleted while another worktree has it" "checked out in the worktree at $WORK/feature" cps branch -D feature
expect_fail "nor renamed from elsewhere" "checked out in the worktree" cps branch -m feature feat2
expect_fail "worktree add refuses a branch that's in use" "already checked out" cps worktree add ../dup main
cps worktree add ../old v0.5 >/dev/null && same "$(cd ../old && cps status | head -1 | cut -c1-17)" "HEAD detached at " "a worktree can be on a tag or commit, detached" || fail "line $LINENO: a step before this check failed"
same "$(cps worktree list | wc -l | tr -d ' ')" "3" "worktree list shows all three"
cd ../feature && cps worktree list | has "← you are here" && cps worktree list | grep "you are here" | has "feature" && ok "and marks the one you're in" || fail "line $LINENO: a step before this check failed"
cd ../src
cd ../old && cps checkout -f v1.0 >/dev/null 2>&1; cd ../src
cps gc --prune now >/dev/null && cps fsck | has "everything is intact" && ok "gc keeps every worktree's commits" || fail "line $LINENO: a step before this check failed"
cd ../feature && echo unsaved > unsaved.txt && cd ../src
expect_fail "removing a worktree with uncommitted changes is refused" "uncommitted change" cps worktree remove ../feature
cps worktree remove --force ../feature >/dev/null && [[ ! -e ../feature ]] && ok "unless --force" || fail "line $LINENO: a step before this check failed"
same "$(titles feature -n 1)" "feature work" "the branch and its commits remain"
cps checkout feature >/dev/null && cps checkout main >/dev/null && ok "and the branch is free to check out here now" || fail "line $LINENO: a step before this check failed"
rm -rf ../old && cps worktree list | has "missing" && cps worktree prune | has "Forgot" && same "$(cps worktree list | wc -l | tr -d ' ')" "1" "prune forgets worktrees whose folders were deleted" || fail "line $LINENO: a step before this check failed"

echo
echo "All $pass checks passed."
