#!/usr/bin/env bash
# Local tag tests: lightweight and annotated tags, using tags as revisions, moving,
# deleting, guards, and tags surviving gc. (Pushing and fetching tags to git servers
# is covered in tests/github.sh.)
# Usage: tests/tags.sh [work_dir]     (build first: cargo build --release)
set -euo pipefail
. "$(cd "$(dirname "$0")" && pwd)/portable.sh"   # sedi, mtime, days_ago, peak_mb, ... (macOS and Linux)

COPSE="$(cd "$(dirname "$0")/.." && pwd)/target/release/copse"
WORK="${1:-$(mktemp -d)}"
rm -rf "$WORK" && mkdir -p "$WORK" && cd "$WORK"
cps() { "$COPSE" "$@"; }
export COPSE_AUTHOR="Test Person" COPSE_EMAIL="test@example.com"  # fixed identity, same on every machine
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1  # your own git settings (signing, identity) stay out of the tests
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

mkdir repo && cd repo && cps init >/dev/null
for i in 1 2 3; do echo "$i" > f.txt && cps commit -m "c$i" >/dev/null; done
c3=$(cps log -n 1 | head -1 | cut -c1-10)
c2=$(cps log -n 2 | grep UTC | tail -1 | cut -c1-10)

echo "== creating tags"
same "$(cps tag)" 'No tags. Create one with `copse tag v1.0` or `copse tag -a v1.0 -m "First release"`.' "no tags yet"
cps tag light | has "as light" && ok "lightweight tag on HEAD" || fail "line $LINENO: a step before this check failed"
cps tag -a v1.0 -m "First release" HEAD~1 | has "as v1.0 (annotated)" && ok "annotated tag on an older commit" || fail "line $LINENO: a step before this check failed"
expect_fail "annotated tag needs a message" "needs a message" cps tag -a v2.0
cps tag release/1 >/dev/null && cps tag -d release/1 >/dev/null && ok "tag names can have slashes, like release/1" || fail "line $LINENO: a step before this check failed"
expect_fail "invalid tag names are refused" "invalid name" cps tag "bad..tag"
expect_fail "existing tag isn't silently moved" "already exists" cps tag light HEAD~2
list=$(cps tag)
[[ "$list" == *"light  $c3  c3"* && "$list" == *"v1.0  $c2  First release"* ]] && ok "list shows each tag's commit and message" || fail "$list"

echo "== tags as revisions"
cps log -n 3 | has "tag: v1.0" && ok "log shows tags" || fail "line $LINENO: a step before this check failed"
cps checkout v1.0 >/dev/null && same "$(cat f.txt)" "2" "checkout an annotated tag" && cps checkout main >/dev/null || fail "line $LINENO: a step before this check failed"
cps checkout light~2 >/dev/null && same "$(cat f.txt)" "1" "tag~N works" && cps checkout main >/dev/null || fail "line $LINENO: a step before this check failed"
cps branch from-tag >/dev/null && ok "branching still works with tags around" || fail "line $LINENO: a step before this check failed"

echo "== moving and deleting"
cps tag -f light HEAD~2 >/dev/null && [[ "$(cps tag)" == *"light  "*"  c1"* ]] && ok "--force moves a tag" || fail "line $LINENO: a step before this check failed"
cps tag -d light | has "Deleted tag light" && ok "delete a tag" || fail "line $LINENO: a step before this check failed"
expect_fail "deleting a missing tag is reported" "no tag called" cps tag -d light
[[ "$(cps tag)" != *"light"* ]] && ok "deleted tag is gone" || fail "line $LINENO: a step before this check failed"

echo "== tags survive gc"
cps gc >/dev/null
same "$(cps tag)" "v1.0  $c2  First release" "annotated tag intact after gc"
cps checkout v1.0 >/dev/null && same "$(cat f.txt)" "2" "and still checks out" && cps checkout main >/dev/null || fail "line $LINENO: a step before this check failed"

echo "== copse-to-copse remotes"
cd .. && cps init --bare hub.copse >/dev/null && cd repo && cps remote add origin ../hub.copse >/dev/null
cps push | has "Pushed main" && ok "pushing branches to copse remotes is unaffected" || fail "line $LINENO: a step before this check failed"
cps push --tags | has "tag(s) to origin" && ok "tags can be pushed to copse remotes too (tests/layout.sh covers the details)" || fail "line $LINENO: a step before this check failed"

echo
echo "All $pass checks passed."
