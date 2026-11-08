#!/usr/bin/env bash
# Check copse against a real git host (GitHub, GitLab, Codeberg/Forgejo, ...), with
# git as the judge. Not part of tests/run-all.sh: it needs the network and a login.
#
# Usage: tests/hosted/check.sh URL [--read-only] [--no-lfs] [--no-partial]
#
# With a writable URL it pushes, pulls, branches, tags, merges, rebases, force-pushes,
# deletes, and checks shallow and partial clones and LFS. It only touches branches
# named copse-check/* and tags named copse-check-*, never anything else, and deletes
# them at the end. With --read-only it clones, fetches and pulls only.
# Sign-in comes from git's credential helpers, as for any copse push.
set -euo pipefail
URL="$1"; shift
READ_ONLY=0; LFS=1; PARTIAL=1
for a in "$@"; do
  case "$a" in --read-only) READ_ONLY=1 ;; --no-lfs) LFS=0 ;; --no-partial) PARTIAL=0 ;; esac
done
COPSE="$(cd "$(dirname "$0")/../.." && pwd)/target/release/copse"
WORK="$(mktemp -d)"; cd "$WORK"
cps() { "$COPSE" "$@"; }
export COPSE_AUTHOR="copse check" COPSE_EMAIL="copse-check@example.com" GIT_TERMINAL_PROMPT=0
export GIT_AUTHOR_NAME="git check" GIT_AUTHOR_EMAIL="git-check@example.com" GIT_COMMITTER_NAME="git check" GIT_COMMITTER_EMAIL="git-check@example.com"
pass=0
ok() { echo "  ok: $1"; pass=$((pass + 1)); }
fail() { echo "  FAIL: $1"; exit 1; }
same() { [[ "$1" == "$2" ]] || fail "$3 (got '$1', want '$2')"; ok "$3"; }
has() { grep -F -- "$1" >/dev/null; }
files() { (cd "$1" && find . -type f -not -path './.git/*' -not -path './.copse/*' | LC_ALL=C sort | while read -r f; do printf '%s %s\n' "$f" "$(shasum -a 256 < "$f" | cut -c1-16)"; done); }
echo "Checking copse against $URL (work folder $WORK)"

echo "== reading"
cps clone "$URL" c0 > clone.log 2>&1 || fail "copse clone: $(tail -3 clone.log)"
git clone -q "$URL" g0 2>/dev/null
same "$(files c0)" "$(files g0)" "a copse clone has exactly git's files"
same "$(cd c0 && cps log --format='%s|%an|%ad' -n 30)" "$(git -C g0 log --format='%s|%an|%ad' -n 30)" "and the same history (messages, authors, dates)"
(cd c0 && cps fsck | has "everything is intact") && ok "fsck is happy" || fail "fsck in the clone"
(cd c0 && cps fetch >/dev/null && cps pull >/dev/null) && ok "fetch and pull" || fail "fetch/pull"
if (cd c0 && [[ -n "$(cps log --format=%h -n 2 | sed -n 2p)" ]]); then
  if cps clone --depth 1 "$URL" cs > shallow.log 2>&1; then
    same "$(cd cs && cps log --format=%s | wc -l | tr -d ' ')" "1" "a shallow clone (--depth 1)"
    (cd cs && cps fetch --unshallow >/dev/null 2>&1) || fail "fetch --unshallow"
    same "$(cd cs && cps log --format=%s -n 30)" "$(git -C g0 log --format=%s -n 30)" "and --unshallow brings the rest"
  elif has '"dumb" protocol' < shallow.log; then
    ok "a dumb-protocol server can't do shallow clones, and copse says so"
  else
    fail "shallow clone: $(tail -2 shallow.log)"
  fi
else
  echo "  (one commit only, so shallow clones are checked on the copse-check branch below)"
fi
if [[ $PARTIAL == 1 ]]; then
  if cps clone --filter=blob:none "$URL" cp > partial.log 2>&1; then
    same "$(files cp)" "$(files g0)" "a partial clone (--filter=blob:none) checks out the same files"
  elif has "doesn't support partial clones" < partial.log || has '"dumb" protocol' < partial.log; then
    ok "this host doesn't offer partial clones, and copse says so"
  else
    fail "partial clone: $(tail -3 partial.log)"
  fi
fi
[[ $READ_ONLY == 1 ]] && { echo; echo "All $pass checks passed (read-only)."; exit 0; }

B="copse-check/$(date +%s)"; T="copse-check-$(date +%s)"
cleanup() {
  [[ -d "$WORK/w" ]] || return 0
  cd "$WORK/w"
  for b in "$B" "$B-feature" "$B-renamed" "$B-rebase"; do cps push origin "$b" --delete >/dev/null 2>&1 || true; done
  for t in "$T" "$T-annotated"; do git -C "$WORK/g1" push -q origin ":refs/tags/$t" 2>/dev/null || true; done
}
trap cleanup EXIT

echo "== writing (branch $B)"
cps clone "$URL" w >/dev/null 2>&1 && cd w
cps branch "$B" >/dev/null 2>&1 || true; cps checkout "$B" >/dev/null 2>&1 || { cps init >/dev/null; }
mkdir -p check/sub && printf 'hello from copse\n' > check/a.txt && printf 'bin\0ary\x01\x02' > check/b.bin
printf '#!/bin/sh\necho hi\n' > check/run.sh && chmod +x check/run.sh && ln -sf a.txt check/link
printf 'ünïcödé ✓\n' > 'check/sub/ünï cödé.txt' && head -c 300000 /dev/urandom > check/big.bin
cps commit -m "copse check: files of every kind" >/dev/null
out=$(cps push origin "$B" 2>&1) || fail "push a new branch: $out"
ok "push a new branch (binary, executable, symlink, unicode name, 300 KB file)"
git clone -q -b "$B" "$URL" ../g1 2>/dev/null
same "$(files ../g1)" "$(files .)" "git clones exactly what copse pushed"
(cd ../g1 && git fsck --strict >/dev/null 2>&1) && ok "git fsck --strict passes on it" || fail "git fsck --strict"
same "$(git -C ../g1 log -1 --format=%s)" "copse check: files of every kind" "the commit arrived as it was made"
same "$(git -C ../g1 ls-files -s check/run.sh | cut -c1-6) $(git -C ../g1 ls-files -s check/link | cut -c1-6)" "100755 120000" "the executable bit and the symlink survive"

echo "== working alongside a git user"
(cd ../g1 && echo "from git" > check/from-git.txt && git add -A && git commit -qm "a git user's commit" && git push -q origin "$B")
cps pull >/dev/null 2>&1 && [[ -f check/from-git.txt ]] && ok "pull brings a git user's commit (fast-forward)" || fail "pull fast-forward"
(cd ../g1 && git pull -q origin "$B" && echo "git again" >> check/from-git.txt && git commit -qam "git edits" && git push -q origin "$B")
echo "copse side" > check/copse.txt && cps commit -m "copse edits" >/dev/null
cps pull >/dev/null 2>&1 && cps push origin "$B" >/dev/null 2>&1 && ok "diverged: pull merges, then push" || fail "pull with a merge, then push"
(cd ../g1 && git pull -q --no-rebase origin "$B" 2>/dev/null) || fail "git pull of the merge"
same "$(git -C ../g1 log -1 --format=%P | wc -w | tr -d ' ')" "2" "git sees the merge commit, with both parents"
(cd ../g1 && echo "conflict-git" > check/a.txt && git commit -qam "git changes a.txt" && git push -q origin "$B")
echo "conflict-copse" > check/a.txt && cps commit -m "copse changes a.txt" >/dev/null
cps pull >/dev/null 2>&1 || true
grep -q "<<<<<<<" check/a.txt && ok "a conflicting edit is reported with markers" || fail "no conflict markers"
echo "resolved" > check/a.txt && cps commit -m "resolve" >/dev/null && cps push origin "$B" >/dev/null 2>&1 && ok "resolved and pushed" || fail "push after resolving"
(cd ../g1 && git pull -q --no-rebase origin "$B" 2>/dev/null && echo "rebase-git" > check/r.txt && git add -A && git commit -qm "git before rebase" && git push -q origin "$B")
echo "rebase-copse" > check/r2.txt && cps commit -m "copse before rebase" >/dev/null
cps pull --rebase >/dev/null 2>&1 && cps push origin "$B" >/dev/null 2>&1 && ok "pull --rebase, then push" || fail "pull --rebase"
(cd ../g1 && git pull -q --no-rebase origin "$B" 2>/dev/null) || fail "git pull of the rebase"
same "$(git -C ../g1 log -1 --format=%s)" "copse before rebase" "git sees the rebased commit on top"

echo "== shallow clone of a longer history"
rm -rf ../cs2 && cps clone --depth 2 "$URL" ../cs2 > ../cs2.log 2>&1 || fail "copse clone --depth 2: $(tail -2 ../cs2.log)"
(cd ../cs2 && cps fetch --depth 3 > /dev/null 2>&1 && cps fetch --unshallow > /dev/null 2>&1) || fail "deepening the shallow clone"
same "$(cd ../cs2 && cps log --format=%s -n 50 origin/main)" "$(git -C ../g1 log --format=%s -n 50 origin/main)" "clone --depth, fetch --depth and --unshallow end with all of history"

echo "== branches and tags"
cps branch "$B-feature" >/dev/null && cps checkout "$B-feature" >/dev/null && echo f > check/f.txt && cps commit -m "feature" >/dev/null
cps push origin "$B-feature" >/dev/null 2>&1 && git ls-remote "$URL" "refs/heads/$B-feature" | has "$B-feature" && ok "push another branch" || fail "push another branch"
cps checkout "$B" >/dev/null && cps merge --no-ff "$B-feature" >/dev/null && cps push origin "$B" >/dev/null 2>&1 && ok "merge --no-ff and push" || fail "merge and push"
cps push origin "$B-feature" --delete >/dev/null 2>&1 && ! git ls-remote "$URL" "refs/heads/$B-feature" | has "$B-feature" && ok "delete a branch on the server" || fail "push --delete"
cps tag "$T" >/dev/null && cps tag -a "$T-annotated" -m "an annotated tag" >/dev/null
cps push origin "$T" >/dev/null 2>&1 && cps push origin "$T-annotated" >/dev/null 2>&1 && ok "push a lightweight and an annotated tag" || fail "push tags"
same "$(git ls-remote "$URL" "refs/tags/$T-annotated^{}" | cut -c1-40)" "$(git ls-remote "$URL" "refs/heads/$B" | cut -c1-40)" "the annotated tag points at the right commit"
(cd ../g1 && git fetch -q origin "refs/tags/$T-annotated:refs/tags/$T-annotated" && git cat-file -t "$T-annotated") | has tag && ok "git sees a real annotated tag" || fail "annotated tag type"

echo "== force push and undo"
git -C ../g1 fetch -q origin "$B"
before=$(git ls-remote "$URL" "refs/heads/$B" | cut -c1-40)
cps reset --hard HEAD~1 >/dev/null 2>&1 && cps commit -m "replacement" >/dev/null 2>&1 || { echo x > check/x.txt; cps commit -m "replacement" >/dev/null; }
cps push origin "$B" >/dev/null 2>&1 && fail "a non-fast-forward push should be refused without -f" || ok "a push that would lose commits is refused"
cps push -f origin "$B" >/dev/null 2>&1 && [[ "$(git ls-remote "$URL" "refs/heads/$B" | cut -c1-40)" != "$before" ]] && ok "push -f replaces it" || fail "push -f"

if [[ $LFS == 1 ]]; then
  echo "== Git LFS"
  cps lfs track "*.psd" >/dev/null && head -c 200000 /dev/urandom > check/art.psd && cps commit -m "an LFS file" >/dev/null
  if out=$(cps push origin "$B" 2>&1); then
    rm -rf ../glfs && git clone -q -b "$B" "$URL" ../glfs 2>/dev/null
    if command -v git-lfs >/dev/null; then
      same "$(shasum -a 256 < ../glfs/check/art.psd)" "$(shasum -a 256 < check/art.psd)" "an LFS file uploads, and git-lfs downloads the same bytes"
    else
      head -1 ../glfs/check/art.psd | has "git-lfs.github.com/spec" && ok "an LFS file uploads, and git sees the pointer (git-lfs isn't installed here)" || fail "LFS pointer"
    fi
    cps clone "$URL" ../clfs > ../clfs.log 2>&1 || fail "copse clone for LFS: $(tail -2 ../clfs.log)"
    (cd ../clfs && cps checkout "$B" > ../clfs-co.log 2>&1) || fail "checking out $B with an LFS file: $(tail -3 ../clfs-co.log)"
    same "$(shasum -a 256 < ../clfs/check/art.psd)" "$(shasum -a 256 < check/art.psd)" "a copse clone downloads the LFS file"
  else
    echo "  (LFS push failed: $(echo "$out" | tail -1))"; fail "LFS"
  fi
fi

echo
echo "All $pass checks passed against $URL."
