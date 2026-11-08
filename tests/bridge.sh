#!/usr/bin/env bash
# The .git view (read by git itself) and Git LFS (against a local LFS server that
# follows the batch API, as GitHub's does).
# Usage: tests/bridge.sh [work_dir]     (build first: cargo build --release; needs git, python3, curl)
set -euo pipefail
. "$(cd "$(dirname "$0")" && pwd)/portable.sh"   # sedi, mtime, days_ago, peak_mb, ... (macOS and Linux)

HERE="$(cd "$(dirname "$0")" && pwd)"
COPSE="$HERE/../target/release/copse"
WORK="${1:-$(mktemp -d)}"
rm -rf "$WORK" && mkdir -p "$WORK" && cd "$WORK"
WORK="$(pwd -P)"
cps() { "$COPSE" "$@"; }
export COPSE_AUTHOR="Test Person" COPSE_EMAIL="test@example.com" COPSE_FSYNC=0 GIT_TERMINAL_PROMPT=0
export HOME="$WORK/home" XDG_CONFIG_HOME="$WORK/home/.config"; mkdir -p "$HOME"
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1 GIT_AUTHOR_NAME="Test Person" GIT_AUTHOR_EMAIL=test@example.com GIT_COMMITTER_NAME="Test Person" GIT_COMMITTER_EMAIL=test@example.com
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
# copse's changes and git's, as "<letter> <path>" lines, sorted (git's ?? is copse's A).
copse_changes() { cps status | grep -E '^  [AMD] ' | sed 's/^ *//' | sort; }
git_changes() { git status --porcelain | sed -E 's/^\?\? /A /; s/^ ?([AMD]) +/\1 /' | sort; }

echo "== the .git view"
mkdir v && cd v && cps init >/dev/null
mkdir -p src && echo a > src/a.txt && echo b > b.txt && ln -s b.txt link && printf '#!/bin/sh\n' > run.sh && chmod +x run.sh
cps commit -m one >/dev/null && cps branch feature >/dev/null && echo b2 > b.txt && cps commit -m two >/dev/null && cps tag -a v1 -m "v1" >/dev/null
out=$(cps git-view on); [[ "$out" == *"Made a .git view"* && -d .git ]] && ok "git-view on makes a .git folder" || fail "$out"
same "$(git log --format=%s | tr '\n' ',')" "two,one," "git log reads copse's history"
same "$(git branch --format='%(refname:short)' | tr '\n' ',')" "feature,main," "git sees the branches"
same "$(git describe --tags)" "v1" "and the tags (annotated ones too)"
[[ -z "$(git fsck --strict 2>&1)" ]] && ok "git fsck --strict finds nothing wrong" || fail "line $LINENO: a step before this check failed"
same "$(git status --porcelain)" "" "git status is clean when copse's is"
same "$(git ls-files -s run.sh link | awk '{print $1}' | tr '\n' ' ')" "120000 100755 " "modes (symlinks, executables) are right in git's index"
echo b3 > b.txt && rm src/a.txt && echo new > new.txt
same "$(git_changes)" "$(copse_changes)" "uncommitted changes show the same in git status (so editors mark them)"
same "$(git diff --stat | tail -1 | sed 's/^ *//')" "2 files changed, 1 insertion(+), 2 deletions(-)" "git diff shows them"
cps commit -m three >/dev/null
same "$(git log -n 1 --format=%s) $(git status --porcelain | wc -l | tr -d ' ')" "three 0" "a copse commit appears in git at once"
same "$(git blame --porcelain b.txt | awk 'NR==1 {print substr($1,1,7)}')" "$(git rev-parse --short=7 HEAD)" "git blame works"
cps checkout feature >/dev/null && same "$(git branch --show-current) $(git log -n 1 --format=%s)" "feature one" "switching branches in copse switches git's view" || fail "line $LINENO: a step before this check failed"
cps checkout main >/dev/null
echo dirty > b.txt
expect_fail "git can't switch branches in the view" "index.lock" git checkout feature
same "$(cat b.txt)" "dirty" "and doesn't touch your files trying"
expect_fail "git can't commit in it" "index.lock" git commit -am x
expect_fail "or make branches" "reference-transaction" git branch side
cps restore b.txt >/dev/null && same "$(git status --porcelain)" "" "it stays in step with copse"
cps branch -m feature feat2 >/dev/null && same "$(git branch --format='%(refname:short)' | tr '\n' ',')" "feat2,main," "renamed and deleted branches follow" || fail "line $LINENO: a step before this check failed"
cps sparse set src >/dev/null 2>&1 || true
same "$(git status --porcelain)" "" "with sparse checkout, git doesn't see the missing files as deleted"
cps sparse disable >/dev/null
same "$(cps git-view)" "The .git view is on: tools that read .git see copse's branches, history and changes." "git-view says it's on"
cps git-view off >/dev/null && [[ ! -e .git ]] && ok "git-view off removes it" || fail "line $LINENO: a step before this check failed"
mkdir -p .git && expect_fail "an existing .git folder is never overwritten" "already a .git folder" cps git-view on
rmdir .git
cd ..

echo "== Git LFS"
mkdir srv && git init -q --bare -b main srv/proj.git
python3 "$HERE/git-http-server.py" srv > port.txt 2>server.log & SERVER=$!
trap 'kill $SERVER 2>/dev/null || true' EXIT
for _ in $(seq 50); do [[ -s port.txt ]] && break; perl -e 'select(undef,undef,undef,0.1)'; done
PORT=$(cat port.txt); URL="http://127.0.0.1:$PORT"
mkdir w && cd w && cps init >/dev/null
cps lfs track "*.bin" | has "Tracking \*.bin with LFS" && grep -q '^\*.bin filter=lfs diff=lfs merge=lfs -text$' .gitattributes && ok "lfs track writes git-lfs's .gitattributes line" || fail "line $LINENO: a step before this check failed"
head -c 400000 /dev/urandom > asset.bin && echo text > notes.txt && cps commit -m "add asset" >/dev/null
oid=$(sha256 asset.bin)
same "$(cps lfs ls-files)" "${oid:0:10} * asset.bin  (400000 bytes)" "lfs ls-files lists it, contents here"
same "$(cps status | tail -1)" "Nothing changed." "a tracked file counts as unchanged after committing"
cps remote add origin "http://tester:s3cret@127.0.0.1:$PORT/proj.git" >/dev/null
cps push 2>&1 | has "Uploaded 1 LFS file(s)" && ok "push uploads the contents to the LFS server" || fail "line $LINENO: a step before this check failed"
same "$(git -C ../srv/proj.git show main:asset.bin)" "version https://git-lfs.github.com/spec/v1
oid sha256:$oid
size 400000" "git holds a pointer exactly as git-lfs writes it"
cmp -s ../srv/.lfs-store/$oid asset.bin && ok "and the LFS server holds the real contents" || fail "line $LINENO: a step before this check failed"
cps push 2>&1 | has "Everything up to date" && ok "pushing again uploads nothing" || fail "line $LINENO: a step before this check failed"
cd .. && cps clone "$URL/proj.git" c2 2>&1 | has "Downloaded 1 LFS file(s)" && ok "clone downloads LFS contents" || fail "line $LINENO: a step before this check failed"
cmp -s c2/asset.bin w/asset.bin && ok "and checks out the real file" || fail "line $LINENO: a step before this check failed"
cd c2
same "$(cps status | tail -1)" "Nothing changed." "status is clean in the clone"
head -c 1000 /dev/urandom >> asset.bin && cps diff | has "+size 401000" && ok "a changed file diffs as its pointer (as git does)" || fail "line $LINENO: a step before this check failed"
cps commit -m "bigger asset" >/dev/null && cps remote add up "http://tester:s3cret@127.0.0.1:$PORT/proj.git" >/dev/null
cps push up 2>&1 | has "Uploaded 1 LFS file(s)" && ok "a new version uploads too" || fail "line $LINENO: a step before this check failed"
cps checkout HEAD~1 >/dev/null && cmp -s asset.bin ../w/asset.bin && ok "checking out an old commit gives that version's contents" || fail "line $LINENO: a step before this check failed"
cps checkout main >/dev/null
cd ..
# Someone using git with git-lfs: a pointer committed, its contents uploaded straight to the server.
git clone -q "$URL/proj.git" gw && cd gw
head -c 5000 /dev/urandom > other.bin && goid=$(sha256 other.bin)
curl -s -f -X PUT -H "Authorization: RemoteAuth upload-token-123" --data-binary @other.bin "$URL/lfs-store/$goid" >/dev/null
printf 'version https://git-lfs.github.com/spec/v1\noid sha256:%s\nsize 5000\n' "$goid" > other.pointer && cp other.bin ../other.orig && mv other.pointer other.bin
git add other.bin && git commit -qm "git-lfs user adds other.bin" && git push -q "http://tester:s3cret@127.0.0.1:$PORT/proj.git" main && cd ..
cd c2 && cps pull 2>&1 | has "Downloaded 1 LFS file(s)" && cmp -s other.bin ../other.orig && ok "an LFS file pushed by a git-lfs user arrives with its contents" || fail "line $LINENO: a step before this check failed"
rm -rf "$WORK/c2/.copse/lfs/objects" && printf 'version https://git-lfs.github.com/spec/v1\noid sha256:%s\nsize 5000\n' "$goid" > other.bin
same "$(cps status | tail -1)" "Nothing changed." "a file left as its pointer (contents not downloaded) isn't a change"
cps lfs pull | has "Downloaded 2 LFS file(s); wrote 1 into your files" && cmp -s other.bin ../other.orig && ok "lfs pull downloads and writes what's missing" || fail "line $LINENO: a step before this check failed"
cd ..
mkdir broken && cd broken && cps init >/dev/null && cps lfs track "*.bin" >/dev/null && echo x > x.bin && cps commit -m x >/dev/null
cps remote add origin "$WORK/srv/proj.git" >/dev/null
expect_fail "with no LFS server to use, the reason is clear" "no LFS server" cps lfs push
cps config lfs.url "http://tester:s3cret@127.0.0.1:$PORT/proj.git/info/lfs" >/dev/null
cps lfs push | has "Uploaded 1 LFS file(s)" && ok "lfs.url names one explicitly" || fail "line $LINENO: a step before this check failed"
cps lfs untrack "*.bin" >/dev/null && ! grep -q 'filter=lfs' .gitattributes && ok "lfs untrack removes the pattern" || fail "line $LINENO: a step before this check failed"

echo
echo "All $pass checks passed."
