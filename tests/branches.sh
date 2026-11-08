#!/usr/bin/env bash
# Branch delete and rename: local safety checks, undo, renaming the current branch,
# deleting branches on copse remotes and real git servers, and fetch tidying away
# branches deleted on the server.
# Usage: tests/branches.sh [work_dir]     (build first: cargo build --release; needs git)
set -euo pipefail
. "$(cd "$(dirname "$0")" && pwd)/portable.sh"   # sedi, mtime, days_ago, peak_mb, ... (macOS and Linux)

COPSE="$(cd "$(dirname "$0")/.." && pwd)/target/release/copse"
WORK="${1:-$(mktemp -d)}"
rm -rf "$WORK" && mkdir -p "$WORK" && cd "$WORK"
cps() { "$COPSE" "$@"; }
export COPSE_AUTHOR="Test Person" COPSE_EMAIL="test@example.com"  # fixed identity, same on every machine
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1
export GIT_AUTHOR_NAME="Git User" GIT_AUTHOR_EMAIL="git@example.com" GIT_COMMITTER_NAME="Git User" GIT_COMMITTER_EMAIL="git@example.com"
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
branches() { cps branch | sed 's/^[* ] //' | cut -d' ' -f1 | tr '\n' ',' | sed 's/,$//'; }

mkdir repo && cd repo && cps init >/dev/null
echo base > base.txt && cps commit -m base >/dev/null

echo "== deleting"
cps branch merged >/dev/null
cps branch unmerged >/dev/null && cps checkout unmerged >/dev/null && echo u > u.txt && cps commit -m "unmerged work" >/dev/null && cps checkout main >/dev/null
tip=$(cat .copse/refs/unmerged | cut -c1-10)
cps branch -d merged | has "Deleted branch merged" && ok "delete a merged branch" || fail "line $LINENO: a step before this check failed"
expect_fail "unmerged branch is protected" "1 commit(s) that aren't merged" cps branch -d unmerged
out=$(cps branch -D unmerged); [[ "$out" == *"Deleted branch unmerged (was $tip)"* && "$out" == *"To undo: copse branch unmerged $tip"* ]] && ok "-D deletes it anyway and prints how to undo" || fail "$out"
same "$(branches)" "main" "both are gone"
cps branch unmerged "$tip" | has "Created branch unmerged at $tip" && ok "the undo command brings it back" || fail "line $LINENO: a step before this check failed"
cps checkout unmerged >/dev/null && same "$(cat u.txt)" "u" "with its work intact" && cps checkout main >/dev/null || fail "line $LINENO: a step before this check failed"
cps merge unmerged >/dev/null && cps branch -d unmerged | has "Deleted branch unmerged" && ok "after merging, -d is enough" || fail "line $LINENO: a step before this check failed"
expect_fail "can't delete the branch you're on" "you're on main" cps branch -d main
expect_fail "missing branch is reported" "no branch called" cps branch -d nope

echo "== creating at a commit"
cps branch old HEAD~1 >/dev/null && cps checkout old >/dev/null && [[ ! -e u.txt ]] && ok "branch NAME REV starts at REV" && cps checkout main >/dev/null || fail "line $LINENO: a step before this check failed"

echo "== renaming"
cps branch -m old older | has "Renamed branch old to older" && same "$(branches)" "main,older" "rename another branch" || fail "line $LINENO: a step before this check failed"
cps branch -m trunk >/dev/null && same "$(cps status | head -1)" "On branch trunk" "rename the current branch: you stay on it" || fail "line $LINENO: a step before this check failed"
same "$(branches)" "older,trunk" "old name gone"
expect_fail "rename onto an existing name is refused" "already exists" cps branch -m older trunk
cps branch -M older trunk >/dev/null && same "$(branches)" "trunk" "-M replaces it" || fail "line $LINENO: a step before this check failed"
cps checkout trunk >/dev/null && echo more >> base.txt && cps commit -m more >/dev/null && ok "commits keep working after renames" || fail "line $LINENO: a step before this check failed"
cps branch -m main >/dev/null

echo "== guarded during a rebase"
cps branch side HEAD~1 >/dev/null && cps checkout side >/dev/null && echo conflict > base.txt && cps commit -m "side edit" >/dev/null
cps rebase main >/dev/null
expect_fail "can't rename the branch being rebased" "being rebased" cps branch -m side side2
cps rebase --abort >/dev/null && cps checkout -f main >/dev/null

echo "== deleting on a copse remote"
cd .. && cps init --bare hub.copse >/dev/null && cd repo
cps remote add origin ../hub.copse >/dev/null && cps push >/dev/null && cps push origin side >/dev/null
cps push origin --delete side | has "Deleted branch side on origin" && ok "delete a branch on a copse remote" || fail "line $LINENO: a step before this check failed"
[[ ! -e ../hub.copse/refs/side ]] && ok "it's gone from the remote" || fail "line $LINENO: a step before this check failed"
[[ "$(cps log -n 20)" != *"origin/side"* ]] && ok "and its origin/side name is gone here" || fail "line $LINENO: a step before this check failed"
expect_fail "deleting a branch the remote doesn't have is reported" "no branch called" cps push origin --delete nope
cd .. && cps clone hub.copse other >/dev/null && cd other && cps branch temp >/dev/null && cps push origin temp >/dev/null && cd ../repo
cps fetch | has "origin/temp" && ok "fetch sees the new branch" || fail "line $LINENO: a step before this check failed"
cd ../other && cps push origin --delete temp >/dev/null && cd ../repo
cps fetch | has "origin/temp  deleted on the server" && ok "fetch tidies away a branch deleted on the server" || fail "line $LINENO: a step before this check failed"
expect_fail "a branch checked out in a non-bare remote can't be deleted" "checked out" sh -c "cd ../other && '$COPSE' remote add peer ../repo >/dev/null && '$COPSE' push peer --delete main"

echo "== deleting on a git server"
cd .. && git init -q --bare -b main server.git && cd repo
cps remote add gh ../server.git >/dev/null && cps push gh >/dev/null && cps push gh side >/dev/null
cps tag -a v1 -m "one" >/dev/null && cps push gh v1 >/dev/null
cps push gh --delete side | has "Deleted branch side on gh" && ok "delete a branch on a git server" || fail "line $LINENO: a step before this check failed"
same "$(git -C ../server.git branch --list | tr -d ' *' | tr '\n' ',')" "main," "git agrees it's gone"
cps push gh --delete v1 | has "Deleted tag v1 on gh" && ok "--delete also removes a tag when there's no branch of that name" || fail "line $LINENO: a step before this check failed"
same "$(git -C ../server.git tag -l)" "" "git agrees the tag is gone"
git -C ../server.git fsck --strict >/dev/null 2>&1 && ok "server still passes fsck --strict" || fail "line $LINENO: a step before this check failed"
cd .. && git clone -q -b main server.git gitclone && cd gitclone && git push -q origin main:refs/heads/gtemp && cd ../repo
cps fetch gh | has "gh/gtemp" && ok "fetch from git sees a new branch" || fail "line $LINENO: a step before this check failed"
git -C ../gitclone push -q origin --delete gtemp
cps fetch gh | has "gh/gtemp  deleted on the server" && ok "and tidies it away once git deletes it" || fail "line $LINENO: a step before this check failed"


echo "== names with slashes (feature/login), as on GitHub"
cd "$WORK" && mkdir slashes && cd slashes && cps init >/dev/null && echo base > base.txt && cps commit -m base >/dev/null
cps branch feature/login | has "Created branch feature/login" && ok "create a branch with a slash" || fail "line $LINENO: a step before this check failed"
cps checkout feature/login >/dev/null && echo login > login.txt && cps commit -m "login work" >/dev/null && same "$(cps status | head -1)" "On branch feature/login" "work on it" || fail "line $LINENO: a step before this check failed"
cps branch | has "feature/login" && cps log -n 1 | has "(feature/login)" && ok "it shows in branch lists and log" || fail "line $LINENO: a step before this check failed"
expect_fail "feature and feature/login can't coexist (git can't store both)" "clashes with the existing branch feature/login" cps branch feature
cps branch features >/dev/null && ok "a similar but different name is fine" || fail "line $LINENO: a step before this check failed"
expect_fail "names still follow git's rules" "invalid name" cps branch "bad..name"
cps checkout main >/dev/null && cps branch -m feature/login users/me/login >/dev/null && same "$(cps branch | grep -c users/me/login)" "1" "rename to a deeper name"
[[ "$(cps reflog users/me/login)" == *"login work"* ]] && ok "its history follows the rename" || fail "line $LINENO: a step before this check failed"
cps tag release/1.0 users/me/login >/dev/null && cps checkout release/1.0 >/dev/null && [[ -f login.txt ]] && ok "tags with slashes work as revisions" && cps checkout main >/dev/null || fail "line $LINENO: a step before this check failed"
[[ "$(ls .copse/refs)" == *"users%2Fme%2Flogin"* && ! -d .copse/refs/users ]] && ok "stored as one file, no nested folders" || fail "line $LINENO: a step before this check failed"

cd "$WORK" && cps init --bare slashhub.copse >/dev/null && cd slashes && cps remote add origin ../slashhub.copse >/dev/null
cps push origin users/me/login | has "Pushed users/me/login" && ok "push a slashed branch to a copse remote" || fail "line $LINENO: a step before this check failed"
cd "$WORK" && cps clone slashhub.copse slashclone >/dev/null && cd slashclone
cps checkout origin/users/me/login >/dev/null && [[ -f login.txt ]] && ok "a clone sees origin/users/me/login and can check it out" || fail "line $LINENO: a step before this check failed"
cps checkout main >/dev/null 2>&1 || cps checkout -f main >/dev/null 2>&1 || true
cd ../slashes && cps push origin --delete users/me/login | has "Deleted branch users/me/login" && ok "delete a slashed branch on a copse remote" || fail "line $LINENO: a step before this check failed"
cd ../slashclone && cps fetch | has "origin/users/me/login  deleted on the server" && ok "and fetch tidies it away" || fail "line $LINENO: a step before this check failed"

cd "$WORK" && git init -q --bare -b main slash.git && cd slashes && cps remote add gh ../slash.git >/dev/null && cps push gh >/dev/null
cps branch -m users/me/login feature/login >/dev/null && cps push gh feature/login >/dev/null && cps push gh release/1.0 >/dev/null && ok "push a slashed branch and tag to a git server" || fail "line $LINENO: a step before this check failed"
same "$(git -C ../slash.git for-each-ref --format='%(refname)' | tr '\n' ',')" "refs/heads/feature/login,refs/heads/main,refs/tags/release/1.0," "git sees refs/heads/feature/login and refs/tags/release/1.0"
cd "$WORK" && git clone -q -b main slash.git slashgit && cd slashgit && git checkout -qb bugfix/crash && echo fix > fix.txt && git add -A && git commit -qm "fix crash" && git push -q origin bugfix/crash && git tag hotfix/1 && git push -q origin hotfix/1 && cd ../slashes
out=$(cps fetch gh); [[ "$out" == *"gh/bugfix/crash"* && "$out" == *"tag hotfix/1"* ]] && ok "fetch brings slashed branches and tags made with git" || fail "$out"
cps checkout gh/bugfix/crash >/dev/null && [[ -f fix.txt ]] && ok "and they check out" && cps checkout main >/dev/null || fail "line $LINENO: a step before this check failed"
cps push gh --delete bugfix/crash | has "Deleted branch bugfix/crash" && [[ -z "$(git -C ../slash.git branch --list bugfix/crash)" ]] && ok "delete a slashed branch on a git server" || fail "line $LINENO: a step before this check failed"
git -C ../slash.git fsck --strict >/dev/null 2>&1 && ok "git server still passes fsck --strict" || fail "line $LINENO: a step before this check failed"

echo
echo "All $pass checks passed."
