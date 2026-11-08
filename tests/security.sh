#!/usr/bin/env bash
# Hostile repositories: crafted git histories that try to make a checkout write
# outside the project or over copse's own data, and addresses that try to smuggle
# options into ssh. Each attack must be refused, with nothing written where it
# shouldn't be.
# Usage: tests/security.sh [work_dir]     (build first: cargo build --release; needs git, python3)
set -euo pipefail

COPSE="$(cd "$(dirname "$0")/.." && pwd)/target/release/copse"
WORK="${1:-$(mktemp -d)}"
rm -rf "$WORK" && mkdir -p "$WORK" && cd "$WORK"
WORK="$(pwd -P)"
cps() { "$COPSE" "$@"; }
export COPSE_AUTHOR="Test Person" COPSE_EMAIL="test@example.com" COPSE_FSYNC=0
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1 GIT_AUTHOR_NAME=T GIT_AUTHOR_EMAIL=t@e GIT_COMMITTER_NAME=T GIT_COMMITTER_EMAIL=t@e
export HOME="$WORK/home" XDG_CONFIG_HOME="$WORK/home/.config"; mkdir -p "$HOME"
has() { grep -F -- "$1" >/dev/null; }
pass=0
ok() { echo "  ok: $1"; pass=$((pass + 1)); }
fail() { echo "  FAIL: $1"; exit 1; }
expect_fail() {
  local what="$1" want="$2"; shift 2
  local out; if out=$("$@" 2>&1); then fail "$what (should have failed)"; fi
  [[ "$out" == *"$want"* ]] || fail "$what (wrong error: $out)"
  ok "$what"
}

# A tree object written by hand, so it can hold names git itself would refuse.
# Usage: raw_tree GIT_DIR "MODE NAME SHA" ...   (NAME may contain \u escapes)
raw_tree() {
  local gd="$1"; shift
  python3 - "$@" <<'EOF' | GIT_DIR="$gd" git hash-object -t tree --literally -w --stdin
import sys
out = b""
for spec in sys.argv[1:]:
    mode, name, sha = spec.split(" ", 2)
    name = name.encode("utf-8").decode("unicode_escape").encode("utf-8")
    out += mode.encode() + b" " + name + b"\0" + bytes.fromhex(sha)
sys.stdout.buffer.write(out)
EOF
}
# A bare git repository whose main branch has TREE as its only commit.
hostile_repo() {
  local name="$1" tree_fn="$2"
  rm -rf "$name.git" && git init -q --bare -b main "$name.git"
  local blob tree commit
  blob=$(echo "pwned" | GIT_DIR="$name.git" git hash-object -w --stdin)
  tree=$($tree_fn "$name.git" "$blob")
  commit=$(GIT_DIR="$name.git" git commit-tree "$tree" -m "hostile")
  GIT_DIR="$name.git" git update-ref refs/heads/main "$commit"
}
nothing_escaped() { [[ ! -e "$WORK/pwned.txt" && -z "$(ls -A "$WORK/outside" 2>/dev/null)" ]]; }
mkdir outside

echo "== hostile names in a repository"
for evil in ".." ".copse" ".COPSE" ".copse." "COPSE~1" ".git" ".Git " ".g\\u200cit" ".\\ufeffcopse"; do
  t_inner() { raw_tree "$1" "100644 pwned.txt $2"; }
  t_outer() { local inner; inner=$(t_inner "$1" "$2"); raw_tree "$1" "40000 $evil $inner" "100644 ok.txt $2"; }
  hostile_repo "evil" t_outer
  out=$(cps clone "$WORK/evil.git" victim 2>&1) && fail "cloning a repository with a folder named $evil should be refused"
  [[ "$out" == *"unsafe file name"* ]] || fail "wrong error for $evil: $out"
  nothing_escaped && [[ ! -e victim ]] && ok "a folder named \"$evil\" is refused, and nothing is written (clone left no folder)" || fail "something was written for $evil"
done

echo "== a folder listing an entry twice (a symlink and a folder of the same name)"
t_dup() { local inner; inner=$(raw_tree "$1" "100644 pwned.txt $2"); local link; link=$(printf '../outside' | GIT_DIR="$1" git hash-object -w --stdin); raw_tree "$1" "120000 link $link" "40000 link $inner"; }
hostile_repo "dup" t_dup
expect_fail "a symlink and a folder with the same name are refused" "twice" cps clone "$WORK/dup.git" victim
nothing_escaped && ok "and nothing is written through the link" || fail "a file was written through the link"

echo "== a symlink replaced by a folder between commits"
rm -rf seq.git seqw && git init -q --bare -b main seq.git && git clone -q seq.git seqw 2>/dev/null
(cd seqw && ln -s ../outside link && git add link && git commit -qm "a link" && git push -q origin main)
cps clone "$WORK/seq.git" victim >/dev/null && [[ -L victim/link ]] && ok "(the first commit's symlink is checked out as a symlink)" || fail "line $LINENO: a step before this check failed"
(cd seqw && git rm -q link && mkdir link && echo pwned > link/pwned.txt && git add link && git commit -qm "now a folder" && git push -q origin main)
(cd victim && cps pull >/dev/null) && [[ -f victim/link/pwned.txt && ! -L victim/link ]] && nothing_escaped && ok "pulling the next commit replaces the link with a real folder; nothing goes through the old link" || fail "line $LINENO: a step before this check failed"
rm -rf victim && cps clone "$WORK/seq.git" victim >/dev/null && cd victim && cps checkout HEAD~1 >/dev/null && cps checkout main >/dev/null && cd .. && nothing_escaped && ok "switching between the two commits is safe too" || fail "line $LINENO: a step before this check failed"
rm -rf victim

echo "== your own symlinked folder"
rm -rf up.git upw && git init -q --bare -b main up.git && git clone -q up.git upw 2>/dev/null && (cd upw && echo a > a.txt && git add . && git commit -qm a && git push -q origin main)
cps clone "$WORK/up.git" victim >/dev/null && ln -s "$WORK/outside" victim/docs
(cd upw && mkdir docs && echo pwned > docs/pwned.txt && git add docs && git commit -qm docs && git push -q origin main)
out=$(cd victim && cps pull 2>&1) && fail "a pull writing into a symlinked folder should be refused"
[[ "$out" == *"is a symlink"* ]] && nothing_escaped && ok "a pull that would write through a symlinked folder of yours is refused" || fail "$out"
rm -rf victim

echo "== options smuggled into ssh"
printf '#!/bin/sh\ntouch "%s/ssh-ran"; exit 1\n' "$WORK" > fakessh && chmod +x fakessh && export COPSE_SSH="$WORK/fakessh"
expect_fail "a copse remote whose host starts with - is refused" "starts with '-'" cps clone "ssh://-oProxyCommand=touch$WORK/pwned/repo" victim
expect_fail "so is a git one" "starts with '-'" cps clone "ssh://-oProxyCommand=touch$WORK/pwned/repo.git" victim
expect_fail "and the short form" "starts with '-'" cps clone -- "-oProxyCommand=touch pwned:repo.git" victim2
[[ ! -e "$WORK/ssh-ran" && ! -e "$WORK/pwned" && ! -e pwned ]] && ok "ssh was never started" || fail "ssh was run"
unset COPSE_SSH

echo "== options smuggled in through a submodule"
rm -rf sm.git && git init -q --bare -b main sm.git
gm_blob=$(printf '[submodule "lib"]\n\tpath = lib\n\turl = -oProxyCommand=touch pwned\n' | GIT_DIR=sm.git git hash-object -w --stdin)
t_sm() { raw_tree "$1" "100644 .gitmodules $gm_blob" "160000 lib 1111111111111111111111111111111111111111"; }
blob=$(echo x | GIT_DIR=sm.git git hash-object -w --stdin); tree=$(t_sm sm.git "$blob"); commit=$(GIT_DIR=sm.git git commit-tree "$tree" -m sm); GIT_DIR=sm.git git update-ref refs/heads/main "$commit"
out=$(cps clone --recurse-submodules "$WORK/sm.git" victim 2>&1) || true
[[ "$out" == *"refusing the submodule URL"* && ! -e pwned && ! -e victim/pwned ]] && ok "a submodule URL starting with - is refused" || fail "$out"
rm -rf victim

echo "== your own files"
mkdir mine && cd mine && cps init >/dev/null && echo ok > ok.txt && echo x > "copse~1"
expect_fail "committing a file whose name Windows would treat as .copse is refused" "unsafe file name" cps commit -m x
same_log=$(cps log --oneline 2>&1 | head -1); [[ "$same_log" != *"x"* ]] && ok "and nothing is committed" || fail "line $LINENO: a step before this check failed"
rm "copse~1" && cps commit -m fine >/dev/null && cps fsck | has "everything is intact" && ok "ordinary names still commit (and .gitignore, .github and friends are fine)" || fail "line $LINENO: a step before this check failed"
cd ..

echo
echo "All $pass checks passed."
