#!/usr/bin/env bash
# End-to-end test of clone, push, pull, fetch, merge-on-pull and safety checks.
# Usage: tests/remote.sh [work_dir]     (build first: cargo build --release)
set -euo pipefail
. "$(cd "$(dirname "$0")" && pwd)/portable.sh"   # sedi, mtime, days_ago, peak_mb, ... (macOS and Linux)

COPSE="$(cd "$(dirname "$0")/.." && pwd)/target/release/copse"
WORK="${1:-$(mktemp -d)}"
rm -rf "$WORK" && mkdir -p "$WORK" && cd "$WORK"
cps() { "$COPSE" "$@"; }
export COPSE_AUTHOR="Test Person" COPSE_EMAIL="test@example.com"  # fixed identity, same on every machine
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1  # your own git settings (signing, identity) stay out of the tests
has() { grep -- "$1" >/dev/null; }  # like grep -q, but reads all input (no early pipe close)
pass=0
ok() { echo "  ok: $1"; pass=$((pass + 1)); }
fail() { echo "  FAIL: $1"; exit 1; }
expect_fail() { # expect_fail "description" "message substring" cmd...
  local what="$1" want="$2"; shift 2
  local out; if out=$("$@" 2>&1); then fail "$what (should have failed)"; fi
  [[ "$out" == *"$want"* ]] || fail "$what (wrong error: $out)"
  ok "$what"
}
same() { [[ "$1" == "$2" ]] || fail "$3 (got '$1', want '$2')"; ok "$3"; }

echo "== shared bare remote, first push"
cps init --bare drive/proj.copse >/dev/null
mkdir laptop && cd laptop && cps init >/dev/null
mkdir src && printf 'a\nb\nc\nd\ne\n' > src/notes.txt && echo hi > readme.md
chmod +x readme.md && ln -s readme.md link
cps commit -m first >/dev/null
cps remote add origin ../drive/proj.copse >/dev/null
cps push >/dev/null && ok "push new branch" || fail "line $LINENO: a step before this check failed"

echo "== clone"
cd .. && cps clone drive/proj.copse desktop >/dev/null
same "$(cat desktop/src/notes.txt | tr '\n' ' ')" "a b c d e " "clone has the files"
[[ -x desktop/readme.md && -L desktop/link ]] && ok "clone keeps exec bit and symlink" || fail "modes"
same "$(cd desktop && cps status | tail -1)" "Nothing changed." "clone is clean"

echo "== fast-forward pull"
cd desktop && echo desk > desk.txt && cps commit -m desk >/dev/null && cps push >/dev/null
cd ../laptop && cps pull | has "Fast-forward" && ok "pull fast-forwards" || fail "line $LINENO: a step before this check failed"
[[ -f desk.txt ]] && ok "pulled file arrived" || fail "desk.txt missing"

echo "== diverged: push rejected, pull merges, push succeeds"
sedi 's/^a$/A-laptop/' src/notes.txt && cps commit -m "laptop edit" >/dev/null
cd ../desktop && sedi 's/^e$/E-desktop/' src/notes.txt && cps commit -m "desktop edit" >/dev/null && cps push >/dev/null
cd ../laptop
expect_fail "push of diverged branch is rejected" "has commits you don't have" cps push
cps pull | has "Merged origin/main" && ok "pull makes a merge commit" || fail "line $LINENO: a step before this check failed"
same "$(cat src/notes.txt | tr '\n' ' ')" "A-laptop b c d E-desktop " "both edits merged"
cps push >/dev/null && ok "push after pull" || fail "line $LINENO: a step before this check failed"
cd ../desktop && cps pull >/dev/null
same "$(cat src/notes.txt | tr '\n' ' ')" "A-laptop b c d E-desktop " "other side fast-forwards to the merge"

echo "== nothing to do"
same "$(cps push)" "Everything up to date." "push when up to date"
same "$(cps fetch)" "" "fetch when up to date prints nothing"

echo "== force push"
cd ../laptop && echo x > x.txt && cps commit -m x >/dev/null && cps push >/dev/null
cd ../desktop && echo y > y.txt && cps commit -m y >/dev/null
expect_fail "push without --force is rejected" "has commits you don't have" cps push
cps push --force >/dev/null && ok "push --force overwrites" || fail "line $LINENO: a step before this check failed"
cd ../laptop && cps fetch >/dev/null
cps checkout origin/main >/dev/null
[[ -f y.txt && ! -f x.txt ]] && ok "remote now has the forced version" || fail "force push result"
cps checkout main >/dev/null

echo "== non-bare remote"
cd .. && cps clone desktop p2 >/dev/null && cd p2 && echo z > z.txt && cps commit -m z >/dev/null
expect_fail "push to a checked-out branch is refused" "checked out in that repository" cps push origin main
cps branch side >/dev/null && cps push origin side >/dev/null && ok "push to another branch of a non-bare repo works" || fail "line $LINENO: a step before this check failed"

echo "== errors"
cps remote add nowhere ../nope >/dev/null
same "$(cps remote | grep nowhere | sed 's/.*\///')" "nope" "remote path is tidied"
expect_fail "missing remote repo is reported" "no copse repository" cps fetch nowhere
expect_fail "unknown remote name is reported" "no remote called" cps fetch upstream
expect_fail "clone into non-empty dir is refused" "isn't empty" cps clone ../drive/proj.copse ../laptop

echo "== SSH transport (via a stand-in ssh that runs the command locally)"
cd .. && cat > fake-ssh <<'EOF'
#!/bin/sh
shift            # drop the host name
exec sh -c "$1"  # run the remote command, as sshd would
EOF
chmod +x fake-ssh
export COPSE_SSH="$WORK/fake-ssh" COPSE_REMOTE_CMD="'$COPSE'"
cps clone "somehost:$WORK/drive/proj.copse" viassh >/dev/null && ok "clone over ssh-style url" || fail "line $LINENO: a step before this check failed"
cd viassh && echo s > s.txt && cps commit -m s >/dev/null && cps push >/dev/null && ok "push over ssh-style url" || fail "line $LINENO: a step before this check failed"
cd ../laptop && cps pull >/dev/null && [[ -f s.txt ]] && ok "commit pushed over ssh is visible to others" || fail "line $LINENO: a step before this check failed"
unset COPSE_SSH COPSE_REMOTE_CMD

echo "== tampered object is caught by the receiver"
cd .. && python3 - drive/proj.copse/objects <<'EOF'
import os, sys
files = [os.path.join(d, f) for d, _, fs in os.walk(sys.argv[1]) for f in fs]
blobs = [p for p in files if open(p, "rb").read(1) == b"\x01"]
open(blobs[0], "wb").write(open(blobs[1], "rb").read())
EOF
expect_fail "corrupt object rejected on clone" "does not match its id" cps clone drive/proj.copse broken
[[ ! -e broken ]] && ok "failed clone leaves nothing behind" || fail "partial clone left behind"

echo
echo "All $pass checks passed."
