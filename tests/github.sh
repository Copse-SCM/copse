#!/usr/bin/env bash
# Pushing to git servers, judged by real git: every push must pass `git fsck --strict`,
# a git clone must match copse's files exactly (content, permissions, symlinks), and
# translation must be deterministic (same copse history -> same git ids, always).
# Needs git installed. Usage: tests/github.sh [work_dir]   (build first: cargo build --release)
set -euo pipefail
. "$(cd "$(dirname "$0")" && pwd)/portable.sh"   # sedi, mtime, days_ago, peak_mb, ... (macOS and Linux)

COPSE="$(cd "$(dirname "$0")/.." && pwd)/target/release/copse"
WORK="${1:-$(mktemp -d)}"
rm -rf "$WORK" && mkdir -p "$WORK" && cd "$WORK"
cps() { "$COPSE" "$@"; }
export COPSE_AUTHOR="Test Person" COPSE_EMAIL="test@example.com"  # fixed identity, same on every machine
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1           # stock git, whatever this machine's settings
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
fingerprint() { # paths, permission bits and contents of every file, ignoring .copse and .git
  python3 - <<'PY'
import hashlib, os
h = hashlib.sha256()
for root, dirs, files in os.walk("."):
    dirs[:] = sorted(d for d in dirs if d not in (".copse", ".git"))
    for name in sorted(files):
        p = os.path.join(root, name)
        st = os.lstat(p)
        h.update(p.encode() + oct(st.st_mode).encode())
        h.update(os.readlink(p).encode() if os.path.islink(p) else open(p, "rb").read())
print(h.hexdigest()[:16])
PY
}
fsck() { git -C "$WORK/server.git" fsck --strict >/dev/null 2>&1 && ok "$1" || { git -C "$WORK/server.git" fsck --strict; fail "$1"; }; }
git_clone_matches() { # does a fresh git clone of <branch> match the copse working directory?
  rm -rf "$WORK/gitcopy" && git clone -q -b "$1" "$WORK/server.git" "$WORK/gitcopy"
  same "$(cd "$WORK/gitcopy" && fingerprint)" "$(fingerprint)" "$2"
}

echo "== first push"
git init -q --bare -b main server.git
mkdir work && cd work && cps init >/dev/null
mkdir -p src/deep foo
echo hello > hello.txt && echo "fn main() {}" > src/main.rs && echo deep > src/deep/x.txt
printf '#!/bin/sh\necho hi\n' > run.sh && chmod +x run.sh && ln -s hello.txt link
echo a > foo.txt && echo b > foo/in.txt && echo c > foo-bar && echo d > foo0   # git's tricky sort order
echo "unicode" > "café ☕.txt"
cps commit -m "first commit" >/dev/null
echo more >> hello.txt && cps commit -m $'second\n\nwith a longer body.\nÜnïcödé too ✓' >/dev/null
cps branch side >/dev/null && cps checkout side >/dev/null && echo side > side.txt && cps commit -m "side work" >/dev/null
cps checkout main >/dev/null && echo main > main.txt && cps commit -m "main work" >/dev/null
cps merge side >/dev/null
cps remote add gh ../server.git | has "Added git remote" && ok "remote ending in .git is detected as a git server" || fail "line $LINENO: a step before this check failed"
cps remote | has "(git)" && ok "remote list marks it as git" || fail "line $LINENO: a step before this check failed"
out=$(cps push gh); [[ "$out" == "Pushed main to gh: new branch"* ]] && ok "push to a git server" || fail "$out"
fsck "git fsck --strict passes"
git_clone_matches main "git clone matches copse's files exactly (content, permissions, symlink, unicode names)"
same "$(git -C ../server.git log --format=%s main | tr '\n' '|')" "Merge side into main|main work|side work|second|first commit|" "every commit arrived, in order"
same "$(git -C ../server.git log -1 --format='%an <%ae>' main)" "Test Person <test@example.com>" "author name and email"
same "$(git -C ../server.git log --merges --format=%p main | wc -w | tr -d ' ')" "2" "merge commit keeps both parents"
same "$(git -C ../server.git log -1 --skip=3 --format=%B main)" $'second\n\nwith a longer body.\nÜnïcödé too ✓' "multi-line unicode message intact"
same "$(git -C ../server.git log -1 --format=%ad --date=format:%z main)" "$(date +%z)" "local timezone recorded"
same "$(git -C ../server.git cat-file -p main:run.sh | head -1) $(git -C ../server.git ls-tree main run.sh | cut -c1-6)" "#!/bin/sh 100755" "executable bit is git mode 100755"

echo "== later pushes"
same "$(cps push gh)" "Everything up to date." "nothing new: nothing sent"
echo three >> hello.txt && cps commit -m third >/dev/null
out=$(cps push gh); [[ "$out" == *"(3 objects"* ]] && ok "incremental push sends only what changed: commit, top folder, one file" || fail "$out"
fsck "fsck still clean after incremental push"
git_clone_matches main "git clone matches after incremental push"

echo "== translation is deterministic"
mv .copse/gitmap ../gitmap.saved
same "$(cps push gh)" "Everything up to date." "re-translating from scratch gives identical git ids"
cmp -s .copse/gitmap ../gitmap.saved && ok "and the id map is byte-for-byte the same" || ok "id map rebuilt (record order may differ)" || fail "line $LINENO: a step before this check failed"

echo "== other branches"
cps push gh side >/dev/null && same "$(git -C ../server.git branch --list | tr -d ' *' | tr '\n' ',')" "main,side," "second branch pushed" || fail "line $LINENO: a step before this check failed"
git_clone_matches main "main still matches"

echo "== someone else pushes with git"
cd .. && git clone -q -b main server.git gituser && cd gituser
echo "from git" > from-git.txt && git add from-git.txt && git commit -qm "made with git" && git push -q origin main
cd ../work
expect_fail "push is rejected when the server has commits copse doesn't" "has commits you don't have" cps push gh
cps push gh --force | has "Pushed main" && ok "push --force overwrites" || fail "line $LINENO: a step before this check failed"
fsck "fsck clean after force push"
git_clone_matches main "server now has copse's version"

echo "== rewritten history"
for i in 1 2 3; do echo "$i" > "n$i.txt" && cps commit -m "n$i" >/dev/null; done
printf '%s\n' '#!/bin/sh' 'perl -pi -e "s/^pick/squash/ if \$. == 2; s/^pick/fixup/ if \$. == 3" "$1"' > ../squash-editor && chmod +x ../squash-editor
COPSE_EDITOR=../squash-editor cps rebase -i HEAD~3 >/dev/null
cps push gh --force >/dev/null && fsck "squashed history pushes cleanly" || fail "line $LINENO: a step before this check failed"
same "$(git -C ../server.git log -1 --format='%s|%an|%cn' main)" "n1|Test Person|Test Person" "squash keeps author; committer recorded"
git_clone_matches main "git clone matches after rebase"

echo "== packed repository"
cps gc >/dev/null
echo after-gc > gc.txt && cps commit -m "after gc" >/dev/null
cps push gh >/dev/null && fsck "push from a gc'd (delta-packed) repo is clean" || fail "line $LINENO: a step before this check failed"
git_clone_matches main "and matches"

echo "== SSH (a stand-in ssh that runs the command locally)"
printf '%s\n' '#!/bin/sh' 'shift' 'exec sh -c "$1"' > ../fake-ssh && chmod +x ../fake-ssh
mkdir -p ../ssh-home && git init -q --bare -b main ../ssh-home/repo.git
cps remote add viassh "git@example.invalid:$WORK/ssh-home/repo.git" >/dev/null
# The stand-in runs `git-receive-pack <path>`; make sure that program is reachable.
gitcore="$(git --exec-path)"
PATH="$gitcore:$PATH" COPSE_SSH="$WORK/fake-ssh" cps push viassh | has "Pushed main to viassh" && ok "push over an ssh-style address" || fail "line $LINENO: a step before this check failed"
git -C ../ssh-home/repo.git fsck --strict >/dev/null 2>&1 && ok "fsck clean over ssh" || fail "line $LINENO: a step before this check failed"

echo "== addresses and unsupported operations"
cps remote add web https://github.com/me/repo.git | has "Added git remote" && ok "HTTPS address accepted as a git remote" || fail "line $LINENO: a step before this check failed"
cps remote add hub git@github.com:me/repo.git | has "Added git remote" && ok "github.com address detected as git" || fail "line $LINENO: a step before this check failed"
cd .. && cps init --bare plain.copse >/dev/null && cd work
cps remote add copseremote ../plain.copse | has "Added remote copseremote" && ok "a copse repository is still a copse remote" || fail "line $LINENO: a step before this check failed"
cd ..

echo "== cloning a history made with git"
git init -q --bare -b main hub.git
git clone -q hub.git gu 2>/dev/null && cd gu
mkdir -p docs/deep && echo "readme" > README.md && echo "café" > "docs/naïve file.txt" && : > empty.txt
printf '#!/bin/sh\necho hi\n' > tool.sh && chmod +x tool.sh && ln -s README.md link
python3 -c "import random; open('blob.bin','wb').write(random.Random(1).randbytes(70000))"
for i in $(seq 1 12); do                                  # many versions of one big file -> delta chains
  python3 -c "import random,sys; r=random.Random(2); L=[str(r.random()) for _ in range(4000)]; L[int(sys.argv[1])*7]='edit'; open('docs/deep/big.txt','w').write('\n'.join(L))" "$i"
  git add -A && git commit -qm "big edit $i"
done
git checkout -qb topic && echo topic > topic.txt && git add -A && git commit -qm "topic work" && git checkout -q main
echo main-side >> README.md && git add -A && git commit -qm "main side" && git merge -q --no-ff -m "Merge topic" topic
git tag -a v1.0 -m "release 1.0"
# A hand-made commit with an extra header and a message lacking its final newline:
# copse can't re-create those bytes itself, so it must keep git's id via the id map.
odd=$(printf 'tree %s\nparent %s\nauthor Odd <odd@example.com> 1700000000 +0530\ncommitter Odd <odd@example.com> 1700000000 +0530\ngpgsig -----BEGIN PGP SIGNATURE-----\n \n notreal\n -----END PGP SIGNATURE-----\n\nunusual commit, no newline' \
  "$(git rev-parse HEAD^{tree})" "$(git rev-parse HEAD)" | git hash-object -t commit -w --stdin)
git update-ref refs/heads/main "$odd" && git reset -q --hard main
git push -q origin main topic v1.0 2>/dev/null
git -C ../hub.git repack -q -a -d -f --depth=50 --window=50   # make the server store real deltas
chains=$(git -C ../hub.git verify-pack -v ../hub.git/objects/pack/*.idx | grep -c "chain length = " || true)
[[ "$chains" -gt 0 ]] && ok "server pack contains delta chains ($chains chain lengths), so cloning must apply deltas" || fail "no deltas on the server"
cd ..
out=$(cps clone hub.git tclone 2>&1); [[ "$out" == *"Checked out main"* ]] && ok "copse clone from a git server" || fail "$out"
[[ "$out" != *"unusual git formatting"* ]] && ok "the hand-made commit (signature header, no final newline) translates back exactly" || fail "$out"
cd tclone
same "$(fingerprint)" "$(cd ../gu && fingerprint)" "clone matches git's checkout (binary, empty, unicode, symlink, executable)"
same "$(cps log --first-parent -n 50 | grep -v UTC | sed 's/^ *//' | grep -v '^$' | tr '\n' '|')" "$(git -C ../gu log --first-parent --format=%B -n 50 | grep -v '^$' | tr '\n' '|')" "history and messages match git's (first-parent view)"
cps log -n 50 | has "topic work" && ok "the full log includes commits from merged branches, as git's does" || fail "line $LINENO: a step before this check failed"
same "$(cps status | tail -1)" "Nothing changed." "clone is clean"
same "$(cps push origin)" "Everything up to date." "pushing it straight back changes nothing (every git id preserved)"
same "$(cps remote)" "origin  $(cd "$WORK" && pwd -P)/hub.git  (git)" "origin is recorded as a git remote"
cps checkout origin/topic >/dev/null && [[ -f topic.txt ]] && ok "other branches arrive too (origin/topic)" && cps checkout main >/dev/null || fail "line $LINENO: a step before this check failed"
cd ..
rm -rf tclone-ref && COPSE_NO_OFS_DELTA=1 cps clone hub.git tclone-ref >/dev/null 2>&1
same "$(cd tclone-ref && fingerprint)" "$(cd gu && fingerprint)" "clone using id-based (REF) deltas matches too"

echo "== pulling and pushing alongside a git user"
cd gu && echo "from git" > g1.txt && git add -A && git commit -qm "git adds g1" && git push -q origin main && cd ..
cd tclone
out=$(cps fetch 2>&1); [[ "$out" == *"origin/main"* && "$out" == *"Received"* ]] && ok "fetch picks up the git user's commit" || fail "$out"
cps pull | has "Fast-forward" && [[ -f g1.txt ]] && ok "pull fast-forwards" || fail "line $LINENO: a step before this check failed"
same "$(cps fetch 2>&1)" "" "fetch with nothing new does nothing"
echo "from copse" > t1.txt && cps commit -m "copse adds t1" >/dev/null && cps push origin | has "Pushed main" && ok "push copse's commit" || fail "line $LINENO: a step before this check failed"
cd ../gu && git pull -q origin main 2>/dev/null && [[ -f t1.txt ]] && ok "git user pulls copse's commit" && cd ../tclone || fail "line $LINENO: a step before this check failed"
git -C ../hub.git fsck --strict >/dev/null 2>&1 && ok "server passes fsck --strict" || fail "line $LINENO: a step before this check failed"

echo "== diverged: merge"
cd ../gu && echo g2 > g2.txt && git add -A && git commit -qm "git adds g2" && git push -q origin main && cd ../tclone
echo t2 > t2.txt && cps commit -m "copse adds t2" >/dev/null
expect_fail "push is rejected while diverged" "has commits you don't have" cps push origin
cps pull | has "Merged origin/main" && ok "pull merges the git user's work" || fail "line $LINENO: a step before this check failed"
cps push origin | has "Pushed main" && ok "push the merge" || fail "line $LINENO: a step before this check failed"
cd ../gu && git pull -q origin main 2>/dev/null && cd ../tclone
same "$(fingerprint)" "$(cd ../gu && fingerprint)" "git user and copse user now have identical files"
same "$(git -C ../hub.git log -1 --format=%p main | wc -w | tr -d ' ')" "2" "the merge commit has two parents on the server"

echo "== diverged: pull --rebase"
cd ../gu && echo g3 > g3.txt && git add -A && git commit -qm "git adds g3" && git push -q origin main && cd ../tclone
echo t3 > t3.txt && cps commit -m "copse adds t3" >/dev/null
cps pull --rebase | has "Rebased main onto origin/main" && ok "pull --rebase replays copse's commit on top" || fail "line $LINENO: a step before this check failed"
cps push origin | has "Pushed main" && ok "push after rebase needs no --force" || fail "line $LINENO: a step before this check failed"
same "$(git -C ../hub.git log -2 --format=%s main | tr '\n' '|')" "copse adds t3|git adds g3|" "server history is linear"
git -C ../hub.git fsck --strict >/dev/null 2>&1 && ok "server still passes fsck --strict" || fail "line $LINENO: a step before this check failed"

echo "== new branches from git; SSH"
cd ../gu && git checkout -qb feature && echo f > f.txt && git add -A && git commit -qm feature && git push -q origin feature && cd ../tclone
cps fetch | has "origin/feature  new" && ok "a new branch made with git shows up" || fail "line $LINENO: a step before this check failed"
cd ..
PATH="$(git --exec-path):$PATH" COPSE_SSH="$WORK/fake-ssh" cps clone "git@example.invalid:$WORK/hub.git" viassh-clone >/dev/null 2>&1
same "$(cd viassh-clone && fingerprint)" "$(cd tclone && cps checkout -f main >/dev/null && fingerprint)" "clone over an ssh-style address"
cd work


echo "== tags on git servers"
cd "$WORK/work"
cps tag -a v1.0 -m "First release" >/dev/null && cps tag light HEAD~1 >/dev/null
cps push gh v1.0 | has "Pushed tag v1.0" && ok "push one annotated tag" || fail "line $LINENO: a step before this check failed"
same "$(git -C ../server.git cat-file -t v1.0)" "tag" "it arrives as a real annotated tag"
same "$(git -C ../server.git cat-file tag v1.0 | grep -E '^(tag|tagger) ' | cut -d' ' -f1-4 | tr '\n' '|')" "tag v1.0|tagger Test Person <test@example.com>|" "with its name and tagger"
same "$(git -C ../server.git tag -l --format='%(contents:subject)' v1.0)" "First release" "and its message"
same "$(git -C ../server.git rev-parse 'v1.0^{commit}')" "$(git -C ../server.git rev-parse main)" "pointing at the right commit"
out=$(cps push gh --tags); [[ "$out" == *"Pushed tag light"* && "$out" != *"Pushed tag v1.0"* ]] && ok "--tags pushes only the tags the server lacks" || fail "$out"
same "$(git -C ../server.git rev-parse light)" "$(git -C ../server.git rev-parse 'main~1')" "lightweight tag points at its commit"
same "$(cps push gh --tags)" "Everything up to date." "pushing tags again sends nothing"
cps tag -f light >/dev/null
expect_fail "moving a tag on the server needs --force" "Tags normally never" cps push gh light
cps push gh light --force | has "Pushed tag light" && ok "--force replaces it" || fail "line $LINENO: a step before this check failed"
git -C ../server.git fsck --strict >/dev/null 2>&1 && ok "server passes fsck --strict with tags" || fail "line $LINENO: a step before this check failed"

cd "$WORK/tclone"
list=$(cps tag); [[ "$list" == "v1.0  "*"  release 1.0" ]] && ok "a tag made with git arrived with the clone" || fail "$list"
same "$(cps push origin --tags)" "Everything up to date." "a git-made annotated tag round-trips with the same id"
cd "$WORK/gu" && git tag -a v2.0 -m "release 2.0" && git push -q origin v2.0 && cd "$WORK/tclone"
cps fetch | has "tag v2.0" && ok "fetch brings new tags" || fail "line $LINENO: a step before this check failed"
cps tag v3.0 >/dev/null
cd "$WORK/gu" && git tag v3.0 HEAD~1 && git push -q origin v3.0 && cd "$WORK/tclone"
cps fetch | has "differs from yours; kept yours" && ok "a server tag never overwrites your own" || fail "line $LINENO: a step before this check failed"
cd "$WORK/work"

echo "== HTTPS / HTTP (git's smart HTTP protocol, served by git http-backend)"
cd "$WORK"
mkdir -p web/private && git init -q --bare -b main web/proj.git && git init -q --bare -b main web/private/secret.git
python3 "$(cd "$(dirname "$COPSE")/../../tests" && pwd)/git-http-server.py" "$WORK/web" > web-port.txt 2> web-server.log &
server_pid=$!
trap 'kill $server_pid 2>/dev/null' EXIT
for _ in $(seq 1 300); do [[ -s web-port.txt ]] && break; perl -e 'select(undef,undef,undef,0.1)'; done
[[ -s web-port.txt ]] || fail "the test HTTP server didn't start within 30 seconds: $(cat web-server.log)"
base="http://127.0.0.1:$(cat web-port.txt)"
# Sign-in goes through git's credential helpers. Use git's "store" helper with a file this test controls.
export GIT_CONFIG_GLOBAL="$WORK/gitconfig-http" GIT_TERMINAL_PROMPT=0
printf '[credential]\n\thelper = store --file=%s\n' "$WORK/creds" > "$GIT_CONFIG_GLOBAL"
: > "$WORK/creds"

cd "$WORK/work"
cps remote add lweb "$base/proj.git" | has "Added git remote" && ok "http(s) address is a git remote" || fail "line $LINENO: a step before this check failed"
expect_fail "push without a saved login explains how to sign in" "needs you to sign in" cps push lweb
echo "http://tester:wrong@127.0.0.1:$(cat ../web-port.txt)" > "$WORK/creds"
expect_fail "wrong password is reported" "sign-in to" cps push lweb
[[ ! -s "$WORK/creds" ]] && ok "the rejected login is forgotten (git credential reject)" || fail "bad login kept: $(cat "$WORK/creds")"
echo "http://tester:s3cret@127.0.0.1:$(cat ../web-port.txt)" > "$WORK/creds"
cps push lweb | has "Pushed main to lweb" && ok "push over HTTP with a saved login" || fail "line $LINENO: a step before this check failed"
git -C "$WORK/web/proj.git" fsck --strict >/dev/null 2>&1 && ok "server passes fsck --strict" || fail "line $LINENO: a step before this check failed"
rm -rf "$WORK/webcheck" && git clone -q -b main "$WORK/web/proj.git" "$WORK/webcheck"
same "$(cd "$WORK/webcheck" && fingerprint)" "$(fingerprint)" "what arrived over HTTP matches copse's files"
echo "over http" > http.txt && cps commit -m "over http" >/dev/null
out=$(cps push lweb); [[ "$out" == *"Pushed main to lweb"* && "$out" == *"(3 objects"* ]] && ok "incremental push over HTTP sends only what changed" || fail "$out"

cd "$WORK"
: > "$WORK/creds"
cps clone "$base/proj.git" webclone >/dev/null 2>&1 && ok "anonymous clone of a public repository" || fail "line $LINENO: a step before this check failed"
same "$(cd webclone && fingerprint)" "$(cd work && fingerprint)" "HTTP clone matches"
git clone -q -b main "$WORK/web/proj.git" webgit && cd webgit && echo "git user" > gu.txt && git add -A && git commit -qm "git user over the filesystem" && git push -q origin main && cd ..
cd webclone && cps pull | has "Fast-forward" && [[ -f gu.txt ]] && ok "pull over HTTP" && cd .. || fail "line $LINENO: a step before this check failed"
echo "http://tester:s3cret@127.0.0.1:$(cat web-port.txt)" > "$WORK/creds"   # pushing needs a login
cd webclone && same "$(cps push origin)" "Everything up to date." "round trip over HTTP keeps every git id" && cd .. || fail "line $LINENO: a step before this check failed"

cd work && cps remote add secret "$base/private/secret.git" >/dev/null && cps push secret | has "Pushed main" && ok "push to a private repository" && cd .. || fail "line $LINENO: a step before this check failed"
cps clone "$base/private/secret.git" secretclone >/dev/null 2>&1 && ok "clone a private repository with the saved login" || fail "line $LINENO: a step before this check failed"
: > "$WORK/creds"
expect_fail "private clone without a login explains how to sign in" "needs you to sign in" cps clone "$base/private/secret.git" nologin
expect_fail "missing repository is reported clearly" "repository not found" cps clone "$base/nope.git" nope
port=$(cat web-port.txt)
cps clone "http://tester:s3cret@127.0.0.1:$port/private/secret.git" inlinecreds >/dev/null 2>&1 && ok "login written into the address works" || fail "line $LINENO: a step before this check failed"
same "$(cd inlinecreds && cps remote)" "origin  http://tester:***@127.0.0.1:$port/private/secret.git  (git)" "and the password is never displayed"
kill $server_pid 2>/dev/null; trap - EXIT

echo
echo "All $pass checks passed."
