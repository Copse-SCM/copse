#!/usr/bin/env bash
# Revision names (^, ^N, @, ^{tree}, REV:path), show REV:path, and the plumbing
# commands (cat-file, rev-parse, ls-tree, ls-files, hash-object), checked against
# git on a history made with git and cloned with copse. Ids are copse's own, so they
# are blanked out before comparing.
# Usage: tests/plumbing.sh [work_dir]     (build first: cargo build --release; needs git)
set -euo pipefail
. "$(cd "$(dirname "$0")" && pwd)/portable.sh"

COPSE="$(cd "$(dirname "$0")/.." && pwd)/target/release/copse"
WORK="${1:-$(mktemp -d)}"
rm -rf "$WORK" && mkdir -p "$WORK" && cd "$WORK"
cps() { "$COPSE" "$@"; }
export COPSE_AUTHOR="Test Person" COPSE_EMAIL="test@example.com"
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1 GIT_AUTHOR_NAME="Test Person" GIT_AUTHOR_EMAIL=test@example.com GIT_COMMITTER_NAME="Test Person" GIT_COMMITTER_EMAIL=test@example.com
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
ids() { sed -E 's/[0-9a-f]{40,64}/ID/g'; }
# Same output (ids blanked) from git in ../g and copse here.
alike() { local what="$1"; shift; [[ "$(cps "$@" | ids)" == "$(git -C ../g "$@" | ids)" ]] || fail "$what: $(diff <(cps "$@" | ids) <(git -C ../g "$@" | ids))"; ok "$what"; }

git init -q -b main g && (
  cd g && mkdir -p src/deep && echo hi > src/a.txt && echo d > src/deep/d.txt
  printf '#!/bin/sh\n' > run.sh && chmod +x run.sh && ln -s src/a.txt link
  git add -A && git commit -qm one && echo two >> src/a.txt && git commit -qam two
  git checkout -qb br && echo b > b && git add b && git commit -qm br
  git checkout -q main && echo m > m && git add m && git commit -qm m && git merge -q --no-edit br
  git tag -a v1 -m "the first release"
)
cps clone g c >/dev/null 2>&1 && cd c

echo "== revision names"
alike "REV:path shows a file as it was" show HEAD~1:src/a.txt
alike "^ and ^2 follow the first and second parents" show HEAD^2:b
alike "^:path" show HEAD^:m
alike "@ is HEAD" show @:src/a.txt
alike ":path is the current commit's" show :src/a.txt
alike "a tag:path" show v1:run.sh
alike "REV:folder lists the folder, as git does" show HEAD:src
alike "REV: lists the top folder" show HEAD:
same "$(cps rev-parse HEAD^1)" "$(cps rev-parse HEAD~1)" "^1 and ~1 are the same commit"
same "$(cps rev-parse 'v1^{}')" "$(cps rev-parse HEAD)" "^{} goes from a tag to its commit"
same "$(cps rev-parse 'HEAD^{tree}')" "$(cps rev-parse HEAD:)" "^{tree} is the commit's top folder"
same "$(cps log --format=%s -n 1 HEAD^2)" "br" "log takes them too"
expect_fail "a missing path says so" "isn't in" cps show HEAD:nope.txt
expect_fail "a missing parent says so" "has no parent number 3" cps rev-parse HEAD^3

echo "== cat-file"
for spec in HEAD HEAD:src/a.txt HEAD:src v1; do
  [[ "$(cps cat-file -t "$spec")" == "$(git -C ../g cat-file -t "$spec")" ]] || fail "cat-file -t $spec"
done
ok "-t names the type: commit, blob, tree, tag"
alike "-p prints a commit as git does" cat-file -p HEAD
alike "-p prints a tag as git does" cat-file -p v1
alike "-p prints a folder as git does (modes, types, executable, symlink)" cat-file -p HEAD:src
alike "-p prints a file's bytes" cat-file -p HEAD:run.sh
alike "TYPE OBJECT prints the raw contents" cat-file blob HEAD:link
same "$(cps cat-file -s HEAD:src/a.txt)" "$(git -C ../g cat-file -s HEAD:src/a.txt)" "-s gives a file's size"
cps cat-file -e HEAD:b && ! cps cat-file -e HEAD:nope && ok "-e answers with the exit status" || fail "line $LINENO: a step before this check failed"

echo "== ls-tree"
alike "a commit's top folder" ls-tree HEAD
alike "-r lists every file" ls-tree -r HEAD
alike "a path picks one entry" ls-tree HEAD src
alike "-r with a path lists under it" ls-tree -r HEAD src
alike "-d lists only folders" ls-tree -d HEAD
alike "--name-only" ls-tree --name-only -r HEAD

echo "== ls-files, rev-parse, hash-object"
alike "ls-files lists the tracked files" ls-files
echo more >> b && rm run.sh && echo n > new.txt
same "$(cps ls-files -m | tr '\n' ' ')|$(cps ls-files -d)|$(cps ls-files -o)" "b run.sh |run.sh|new.txt" "-m modified (deletions too, as git), -d deleted, -o untracked"
same "$(cps rev-parse --abbrev-ref HEAD)" "main" "rev-parse --abbrev-ref HEAD gives the branch"
same "$(cps rev-parse --show-toplevel)" "$(pwd -P)" "--show-toplevel"
same "$(cps rev-parse --is-inside-work-tree)" "true" "--is-inside-work-tree"
same "$(cps rev-parse --short HEAD | wc -c | tr -d ' ')" "11" "--short gives copse's short id"
cps rev-parse --verify -q nope >/dev/null 2>&1 && fail "--verify -q should fail" || ok "--verify -q fails quietly for a missing revision"
same "$(cps hash-object new.txt)" "$(echo n | cps hash-object --stdin)" "hash-object gives the same id for a file and stdin"
id=$(cps hash-object -w new.txt) && same "$(cps cat-file -p "$id")" "n" "-w stores it, so cat-file can read it back"

echo
echo "All $pass checks passed."
