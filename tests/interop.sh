#!/usr/bin/env bash
# Signed commits (SSH and GPG, verified by git itself), git submodules, and
# partial clones.
# Usage: tests/interop.sh [work_dir]     (build first: cargo build --release; needs git, ssh-keygen, gpg)
set -euo pipefail
. "$(cd "$(dirname "$0")" && pwd)/portable.sh"   # sedi, mtime, days_ago, peak_mb, ... (macOS and Linux)

COPSE="$(cd "$(dirname "$0")/.." && pwd)/target/release/copse"
WORK="${1:-$(mktemp -d)}"
rm -rf "$WORK" && mkdir -p "$WORK" && cd "$WORK"
WORK="$(pwd -P)"
cps() { "$COPSE" "$@"; }
export COPSE_AUTHOR="Test Person" COPSE_EMAIL="test@example.com" COPSE_FSYNC=0
export HOME="$WORK/home" XDG_CONFIG_HOME="$WORK/home/.config"; mkdir -p "$HOME"
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1 GIT_AUTHOR_NAME="Test Person" GIT_AUTHOR_EMAIL=test@example.com GIT_COMMITTER_NAME="Test Person" GIT_COMMITTER_EMAIL=test@example.com
# gpg-agent's socket lives in GNUPGHOME, and socket paths are limited to ~100
# characters, so gpg gets a short folder of its own (removed at the end).
export GNUPGHOME="$(mktemp -d /tmp/copse-gpg.XXXXXX)"; chmod 700 "$GNUPGHOME"
trap 'gpgconf --kill gpg-agent >/dev/null 2>&1 || true; rm -rf "$GNUPGHOME"' EXIT
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
g() { git -c protocol.file.allow=always "$@"; }

echo "== signed commits (SSH)"
ssh-keygen -q -t ed25519 -N "" -f key -C test && echo "test@example.com $(cat key.pub)" > allowed
mkdir r && cd r && cps init >/dev/null
cps config user.signingKey ../key >/dev/null && cps config gpg.format ssh >/dev/null
echo a > a && cps commit -S -m "signed" >/dev/null
echo b > b && cps commit -m "not signed" >/dev/null
out=$(cps verify-commit HEAD~1); [[ "$out" == *"good signature"*"ED25519"* ]] && ok "commit -S signs with the SSH key, and verify-commit checks it" || fail "$out"
cps verify-commit HEAD >/dev/null && fail "an unsigned commit should fail verify-commit" || ok "an unsigned commit fails verify-commit (exit 1)"
cps config commit.gpgSign true >/dev/null && echo c > c && cps commit -m "signed by setting" >/dev/null
cps verify-commit HEAD >/dev/null && ok "commit.gpgSign signs every commit" || fail "line $LINENO: a step before this check failed"
echo d > d && cps commit --no-gpg-sign -m "opted out" >/dev/null
cps verify-commit HEAD >/dev/null && fail "--no-gpg-sign should leave it unsigned" || ok "--no-gpg-sign opts out"
cps config gpg.ssh.allowedSignersFile ../allowed >/dev/null
cps verify-commit HEAD~1 | has "for test@example.com" && ok "with gpg.ssh.allowedSignersFile, it also checks whose key it is" || fail "line $LINENO: a step before this check failed"
cd .. && git init -q --bare -b main signed.git && cd r && cps remote add gh ../signed.git >/dev/null && cps push gh >/dev/null
cd ../signed.git
same "$(git -c gpg.ssh.allowedSignersFile=../allowed log --format='%G? %s' main | tr '\n' ',')" "N opted out,G signed by setting,N not signed,G signed," "git itself verifies the signatures after a push (G = good)"
git -c gpg.ssh.allowedSignersFile=../allowed verify-commit main~1 2>&1 | has 'Good "git" signature for test@example.com' && ok "git verify-commit agrees" || fail "line $LINENO: a step before this check failed"
[[ -z "$(git fsck --strict 2>&1)" ]] && ok "and git fsck --strict is clean" || fail "line $LINENO: a step before this check failed"
cd .. && git clone -q signed.git signed-copy && cd signed-copy && git config user.signingkey ../key && git config gpg.format ssh && echo e > e && git add e && git commit -qS -m "signed by git" && git push -q origin main && cd ../r
cps pull gh >/dev/null && cps verify-commit HEAD | has "good signature" && ok "a commit signed by git verifies in copse" || fail "line $LINENO: a step before this check failed"
cps commit --amend -m "amended" --no-gpg-sign >/dev/null && ! cps verify-commit HEAD >/dev/null && ok "amending drops the old signature (it would no longer be valid)" || fail "line $LINENO: a step before this check failed"
cps undo >/dev/null
cps config user.signingKey ../missing-key >/dev/null && echo f > f
expect_fail "a missing key is reported, and nothing is committed" "couldn't sign" cps commit -m "won't happen"
cps status | has "A f" && ok "(the change is still uncommitted)" || fail "line $LINENO: a step before this check failed"
cps config --unset commit.gpgSign >/dev/null
cd ..

echo "== signed tags, and signing that follows git's settings"
cd r && cps config gpg.format ssh >/dev/null && cps config user.signingKey ../key >/dev/null
cps tag -s v1.0 -m "Release 1.0" | has "(annotated, signed)" && cps verify-tag v1.0 | has "good signature" && ok "tag -s signs a tag, and verify-tag checks it" || fail "line $LINENO: a step before this check failed"
cps tag -a v0.9 -m "unsigned" >/dev/null && ! cps verify-tag v0.9 >/dev/null && ok "an unsigned tag fails verify-tag" || fail "line $LINENO: a step before this check failed"
cps push gh v1.0 >/dev/null && git -C ../signed.git -c gpg.ssh.allowedSignersFile=../allowed verify-tag v1.0 2>&1 | has 'Good "git" signature for test@example.com' && ok "git verifies copse's signed tag after a push" || fail "line $LINENO: a step before this check failed"
cd ../signed-copy && git -c gpg.format=ssh -c user.signingkey=../key tag -s -m "git signed" v2.0 && git push -q origin v2.0 && cd ../r && cps fetch gh >/dev/null && cps verify-tag v2.0 | has "good signature" && ok "a tag signed by git verifies in copse" || fail "line $LINENO: a step before this check failed"
cps config --unset gpg.format >/dev/null && cps config --unset user.signingKey >/dev/null && cps config --unset gpg.ssh.allowedSignersFile >/dev/null
printf '[gpg]\n\tformat = ssh\n[user]\n\tsigningkey = %s\n[commit]\n\tgpgsign = true\n' "$WORK/key" > "$WORK/gitconfig-signing"
export GIT_CONFIG_GLOBAL="$WORK/gitconfig-signing"
echo g > g && cps commit -m "signed via git's settings" >/dev/null && cps verify-commit HEAD | has "good signature" && ok "with no copse settings, git's signing settings are used (commit.gpgSign signs every commit)" || fail "line $LINENO: a step before this check failed"
cps revert HEAD >/dev/null && cps verify-commit HEAD | has "good signature" && ok "revert commits are signed too" || fail "line $LINENO: a step before this check failed"
cps branch sidebranch HEAD~1 >/dev/null && cps checkout sidebranch >/dev/null && echo side > side.txt && cps commit -m side >/dev/null && cps checkout main >/dev/null
cps cherry-pick sidebranch >/dev/null && cps verify-commit HEAD | has "good signature" && ok "and cherry-picks" || fail "line $LINENO: a step before this check failed"
echo m2 > m2 && cps commit -m m2 >/dev/null && cps checkout sidebranch >/dev/null && echo s2 > s2 && cps commit -m s2 >/dev/null && cps checkout main >/dev/null
cps merge sidebranch >/dev/null && cps verify-commit HEAD | has "good signature" && ok "and merge commits" || fail "line $LINENO: a step before this check failed"
cps checkout sidebranch >/dev/null && echo s3 > s3 && cps commit -m s3 >/dev/null && cps rebase main >/dev/null && cps verify-commit HEAD | has "good signature" && ok "and rebased commits" || fail "line $LINENO: a step before this check failed"
cps checkout main >/dev/null
export GIT_CONFIG_GLOBAL=/dev/null
cd ..

echo "== signed commits (GPG)"
gpg --batch --passphrase '' --quick-gen-key "Test Person <test@example.com>" ed25519 sign never 2>/dev/null
mkdir rg && cd rg && cps init >/dev/null && cps config user.signingKey test@example.com >/dev/null
echo a > a && cps commit -S -m "gpg signed" >/dev/null
cps verify-commit | has "good signature" && ok "commit -S signs with GPG (the default format, as in git)" || fail "line $LINENO: a step before this check failed"
cd .. && git init -q --bare -b main gpg.git && cd rg && cps remote add gh ../gpg.git >/dev/null && cps push gh >/dev/null
git -C ../gpg.git verify-commit main 2>&1 | has "Good signature" && ok "git verifies the GPG signature too" || fail "line $LINENO: a step before this check failed"
cd ..

echo "== submodules"
git init -q --bare -b main lib.git && git init -q --bare -b main app.git
g clone -q lib.git libw 2>/dev/null && cd libw && echo v1 > lib.txt && git add . && git commit -qm "lib v1" && echo v2 > lib.txt && git commit -qam "lib v2" && git push -q origin main && cd ..
g clone -q app.git appw 2>/dev/null && cd appw && echo app > app.txt && g submodule add -q ../lib.git vendor/lib && (cd vendor/lib && git checkout -q HEAD~1) && git add . && git commit -qm "app with lib v1" && git push -q origin main && cd ..
v1=$(git -C lib.git rev-parse main~1) && v2=$(git -C lib.git rev-parse main)
cps clone "$WORK/app.git" app >/dev/null && cd app
[[ -d vendor/lib && -z "$(ls -A vendor/lib)" ]] && ok "a plain clone leaves an empty folder for the submodule, as git does" || fail "line $LINENO: a step before this check failed"
same "$(cps status | tail -1)" "Nothing changed." "and doesn't count it as a change"
same "$(cps submodule | sed 's/  */ /g')" "-${v1:0:10} vendor/lib (not checked out; \`copse submodule update --init\`)" "submodule status says it isn't checked out"
cps submodule update --init | has "vendor/lib: checked out ${v1:0:10}" && same "$(cat vendor/lib/lib.txt)" "v1" "submodule update --init clones it at the pinned commit" || fail "line $LINENO: a step before this check failed"
same "$(cps status | tail -1)" "Nothing changed." "its files aren't counted as this repository's"
cd vendor/lib && cps checkout main >/dev/null && cd ../..
cps submodule | has "+${v2:0:10}" && ok "moving it shows in submodule status" || fail "line $LINENO: a step before this check failed"
cps status | has "M vendor/lib" && ok "and as a change to the submodule" || fail "line $LINENO: a step before this check failed"
cps diff | has "+Subproject commit $v2" && ok "diff shows it the way git does" || fail "line $LINENO: a step before this check failed"
cps commit -m "lib to v2" >/dev/null && cps push >/dev/null
same "$(git -C ../app.git ls-tree main vendor/lib | awk '{print $1, $2, $3}')" "160000 commit $v2" "committing pins the new commit, and git sees it after a push"
[[ -z "$(git -C ../app.git fsck --strict 2>&1)" ]] && ok "git fsck --strict is clean" || fail "line $LINENO: a step before this check failed"
cps gc --prune now >/dev/null && cps fsck | has "everything is intact" && ok "gc and fsck handle submodule entries" || fail "line $LINENO: a step before this check failed"
cps clean -x | has "Nothing to clean" && ok "clean never touches a submodule's files" || fail "line $LINENO: a step before this check failed"
cd .. && cps clone --recurse-submodules "$WORK/app.git" app2 >/dev/null && same "$(cat app2/vendor/lib/lib.txt)" "v2" "clone --recurse-submodules checks submodules out too" || fail "line $LINENO: a step before this check failed"
cd app2 && cps checkout HEAD~1 >/dev/null && cps submodule update | has "checked out ${v1:0:10}" && same "$(cat vendor/lib/lib.txt)" "v1" "after switching commits, submodule update moves it to that commit's pin" || fail "line $LINENO: a step before this check failed"
cd .. && git init -q --bare -b main extra.git && g clone -q extra.git extraw 2>/dev/null && (cd extraw && echo x > x && git add x && git commit -qm x && git push -q origin main)
cd app && cps submodule add "$WORK/extra.git" vendor/extra | has "Added submodule vendor/extra" && cps commit -m "add extra" >/dev/null && ok "submodule add, then commit" || fail "line $LINENO: a step before this check failed"
cps push >/dev/null && cd ../appw && g pull -q 2>/dev/null && g submodule update -q --init 2>/dev/null && [[ -f vendor/extra/x ]] && ok "and git can check out the submodule copse added" || fail "line $LINENO: a step before this check failed"
grep -q 'path = vendor/extra' .gitmodules && ok "(.gitmodules is in git's format)" || fail "line $LINENO: a step before this check failed"
cd ..

echo "== partial clones"
mkdir big && cd big && cps init >/dev/null
for i in 1 2 3 4 5; do head -c 30000 /dev/urandom | base64 > data.txt; echo "$i" > n.txt; cps commit -m "c$i" >/dev/null; done
cd .. && cps clone --filter=blob:none big part >/dev/null && cd part
same "$(cat n.txt)" "5" "clone --filter=blob:none checks out the newest files"
small=$(bytes_in .copse/objects); fullsize=$(bytes_in ../big/.copse/objects)
[[ $small -lt $((fullsize / 2)) ]] && ok "while downloading far less (${small} KB vs ${fullsize} KB)" || fail "partial clone is ${small} KB, full is ${fullsize} KB"
cps fsck | has "aren't downloaded yet" && cps fsck | has "everything is intact" && ok "fsck counts the rest as not downloaded yet, not missing" || fail "line $LINENO: a step before this check failed"
cps log -p -n 4 | has "^-1$" && ok "older versions are fetched when they're needed (log -p)" || fail "line $LINENO: a step before this check failed"
cps checkout HEAD~4 >/dev/null && same "$(cat n.txt)" "1" "checking out an old commit fetches its files" || fail "line $LINENO: a step before this check failed"
cps checkout main >/dev/null && echo 6 > n.txt && cps commit -m c6 >/dev/null
cps gc --prune now >/dev/null && cps fsck | has "everything is intact" && ok "gc works without downloading everything" || fail "line $LINENO: a step before this check failed"
cd ../big && echo upstream > other.txt && cps commit -m c7 >/dev/null && cd ../part
cps pull --rebase >/dev/null && same "$(cat n.txt) $(cat other.txt)" "6 upstream" "pull works (and stays partial)" || fail "line $LINENO: a step before this check failed"
[[ -f .copse/promisor ]] && ok "(.copse/promisor names where to fetch from)" || fail "line $LINENO: a step before this check failed"
cd .. && cps init --bare up >/dev/null && cd part && cps remote add up ../up >/dev/null && cps push up >/dev/null
cd .. && cps clone up full >/dev/null && cd full && cps fsck | tail -1 | has "everything is intact" && ! cps fsck | has "downloaded" && ok "pushing from a partial clone sends complete history" || fail "line $LINENO: a step before this check failed"
cd .. && expect_fail "--filter with a git server that doesn't allow filters says so" "doesn't support partial clones" cps clone --filter=blob:none "$WORK/app.git" gp
expect_fail "unknown filters are refused" "unsupported --filter" cps clone --filter=tree:0 big x

echo
echo "All $pass checks passed."
