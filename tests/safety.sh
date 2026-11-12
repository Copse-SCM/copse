#!/usr/bin/env bash
# fsck, gc removing unused objects, crash safety, and large files (streamed, so
# memory stays flat).
# Usage: tests/safety.sh [work_dir]     (build first: cargo build --release)
set -euo pipefail
. "$(cd "$(dirname "$0")" && pwd)/portable.sh"   # sedi, mtime, days_ago, peak_mb, ... (macOS and Linux)

COPSE="$(cd "$(dirname "$0")/.." && pwd)/target/release/copse"
WORK="${1:-$(mktemp -d)}"
rm -rf "$WORK" && mkdir -p "$WORK" && cd "$WORK"
cps() { "$COPSE" "$@"; }
export COPSE_AUTHOR="Test Person" COPSE_EMAIL="test@example.com"
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
loose() { find .copse/objects -type f -not -path '*/pack/*' -not -name '.tmp-*' | wc -l | tr -d ' '; }
obj_path() { local h; h=$(cps show "$1" | awk 'NR==1 {print $2}'); echo ".copse/objects/${h:0:2}/${h:2}"; }
old() { touch -t 202001010000 "$@"; }   # make files look years old

echo "== fsck"
mkdir r && cd r && cps init >/dev/null
echo one > a.txt && mkdir d && echo deep > d/x.txt && cps commit -m one >/dev/null
echo two > a.txt && cps commit -m two >/dev/null
cps tag v1 HEAD~1 >/dev/null
out=$(cps fsck); [[ "$out" == *"everything is intact"* ]] && ok "a healthy repository checks out" || fail "$out"
echo scratch > s.txt && cps stash >/dev/null && cps stash drop >/dev/null
cps fsck | has "aren't used by anything" && ok "unused objects (a dropped stash) are counted" || fail "line $LINENO: a step before this check failed"
cp -R . ../backup
victim=$(obj_path HEAD~1)
chmod u+w "$victim" && printf 'garbage' > "$victim"
out=$(cps fsck 2>&1) && fail "a damaged object should fail fsck" || true
[[ "$out" == *"damaged object"* && "$out" == *"problem(s) found"* ]] && ok "a damaged commit is found, and fsck exits 1" || fail "$out"
rm "$victim"
out=$(cps fsck 2>&1) || true
[[ "$out" == *"missing object $(cd ../backup && cps show HEAD~1 | awk 'NR==1 {print $2}')"* && "$out" == *"needed by"* ]] && ok "a missing commit is named, with what needs it" || fail "$out"
expect_fail "gc refuses to touch a repository with missing objects" "nothing was changed" cps gc --prune now
cd .. && rm -rf r && mv backup r && cd r
cps fsck >/dev/null && ok "the backup copy is intact" || fail "line $LINENO: a step before this check failed"
cps gc --prune now >/dev/null && idx=$(ls .copse/objects/pack/*.idx)
cp "$idx" ../idx.bak && printf 'XXXX' | dd of="$idx" bs=1 seek=20 conv=notrunc 2>/dev/null
out=$(cps fsck 2>&1) || true
[[ "$out" == *"damaged pack"* ]] && ok "a damaged pack index is reported" || fail "$out"
cp ../idx.bak "$idx"
cps branch gone HEAD~1 >/dev/null && cps branch -D gone >/dev/null
cps fsck >/dev/null && ok "fsck is happy again once it's repaired" || fail "line $LINENO: a step before this check failed"
mkdir -p .copse/objects/ab && touch .copse/objects/ab/.tmp-999-1
cps fsck | has "temporary file(s) left by interrupted writes" && ok "temp files from interrupted writes are noted" || fail "line $LINENO: a step before this check failed"

echo "== gc removes unused objects, after a grace period"
echo junk1 > j.txt && cps stash >/dev/null && cps stash drop >/dev/null   # 3 unused objects: blob, tree, stash commit
n_unused=$(cps fsck | sed -n 's/^\([0-9]*\) object(s) aren.t used.*/\1/p')
out=$(cps gc); [[ "$out" == *"$n_unused unused object(s) are kept for now"* ]] && ok "by default, new unused objects are kept (another command may still need them)" || fail "$out"
same "$(loose)" "$n_unused" "they stay as loose files; everything else is packed"
old .copse/objects/??/*
out=$(cps gc); [[ "$out" == *"Removed $n_unused unused object(s)"* ]] && ok "once they're more than two weeks old, gc removes them" || fail "$out"
same "$(loose)" "0" "leaving nothing loose"
cps fsck | has "everything is intact" && ok "and the repository is intact" || fail "line $LINENO: a step before this check failed"
[[ ! -e .copse/objects/ab/.tmp-999-1 ]] || { old .copse/objects/ab/.tmp-999-1; cps gc --prune now >/dev/null; }
[[ ! -e .copse/objects/ab/.tmp-999-1 ]] && ok "old temp files are cleared" || fail "line $LINENO: a step before this check failed"
echo junk2 > j.txt && cps stash >/dev/null && cps stash drop >/dev/null
cps gc --prune never | has "kept for now" && ok "--prune never keeps them" || fail "line $LINENO: a step before this check failed"
cps gc --prune now | has "Removed" && same "$(loose)" "0" "--prune now removes them at once" || fail "line $LINENO: a step before this check failed"
expect_fail "a bad --prune value is explained" "--prune takes now, never" cps gc --prune soon
echo junk3 > j.txt && cps stash >/dev/null && cps stash drop >/dev/null
cps config gc.pruneExpire now >/dev/null && cps gc | has "Removed" && ok "gc.pruneExpire sets the default" || fail "line $LINENO: a step before this check failed"
cps config --unset gc.pruneExpire >/dev/null

echo "== what gc keeps"
echo keep > k.txt && cps stash -m keepme >/dev/null
cps branch topic >/dev/null && cps checkout topic >/dev/null && echo t > t.txt && cps commit -m "topic work" >/dev/null && cps checkout main >/dev/null
cps branch -D topic >/dev/null
echo three > a.txt && cps commit -m three >/dev/null && cps commit --amend -m "three, reworded" >/dev/null
cps gc --prune now >/dev/null
cps stash pop >/dev/null && same "$(cat k.txt)" "keep" "stashes are kept" || fail "line $LINENO: a step before this check failed"
cps commit -m "with k" >/dev/null
cps branch topic 'topic@{1}' >/dev/null && same "$(cps log -n 1 --oneline topic | sed 's/^[0-9a-f]* ([^)]*) //')" "topic work" "a deleted branch's commits are kept (the reflog still has them)" || fail "line $LINENO: a step before this check failed"
same "$(cps log -n 1 --oneline 'main@{2}' | sed -E 's/^[0-9a-f]{10} (\([^)]*\) )?//')" "three" "the commit an amend replaced is kept, so undo can still reach it"
same "$(cps show v1 | awk 'NR==1 {print $2}' | cut -c1-6)" "$(cps log --oneline -n 100 | tail -1 | cut -c1-6)" "tags are kept"
cps fsck | has "everything is intact" && ok "fsck agrees after all that pruning" || fail "line $LINENO: a step before this check failed"

echo "== unused objects inside a pack still expire on time"
echo packed > p.txt && cps stash -m packed >/dev/null && cps gc --prune now >/dev/null
cps stash drop >/dev/null
old .copse/objects/pack/*.pack
before=$(cps fsck | sed -n 's/^\([0-9]*\) object(s) aren.t used.*/\1/p')
cps gc | has "Removed $before unused" && ok "unused objects in a two-week-old pack are removed" || fail "line $LINENO: a step before this check failed"
echo packed2 > p.txt && cps stash -m packed2 >/dev/null && cps gc --prune now >/dev/null && cps stash drop >/dev/null
week_ago=$(days_ago 7) && touch -t "$week_ago" .copse/objects/pack/*.pack
pack_time=$(mtime .copse/objects/pack/*.pack)
echo more > m.txt && cps commit -m "more" >/dev/null   # something new, so gc rewrites the pack
cps gc | has "kept for now" && ok "in a week-old pack they're kept, as loose files, when gc rewrites it" || fail "line $LINENO: a step before this check failed"
times=$(find .copse/objects -type f -not -path '*/pack/*' | while read -r f; do mtime "$f"; done | sort -u | tr '\n' ' ')
same "$times" "$pack_time " "carrying the pack's age, so they still expire a week from now"
cps fsck | has "everything is intact" && ok "nothing needed was lost" || fail "line $LINENO: a step before this check failed"

echo "== crash safety"
[[ -z "$(find .copse -name '*.lock')" ]] && ok "no half-written state files are left behind" || fail "line $LINENO: a step before this check failed"
echo f > f.txt && COPSE_FSYNC=0 cps commit -m "no fsync" >/dev/null && cps fsck >/dev/null && ok "COPSE_FSYNC=0 turns flushing off (for test runs)" || fail "line $LINENO: a step before this check failed"
cps config core.fsync false >/dev/null && echo g > g.txt && cps commit -m "core.fsync false" >/dev/null && cps config --unset core.fsync >/dev/null && ok "and so does core.fsync = false" || fail "line $LINENO: a step before this check failed"
# An interrupted object write leaves only a temp file: never a damaged object under a real id.
printf 'half an object' > .copse/objects/ab/.tmp-12345-7
cps fsck | has "everything is intact" && ok "a half-written object is just a temp file, not a damaged object" || fail "line $LINENO: a step before this check failed"

echo "== large files are streamed"
cd .. && mkdir big && cd big && cps init >/dev/null
head -c 100000000 /dev/urandom > big.bin && echo small > small.txt && cp big.bin ../big.orig
mb=$(peak_mb "$COPSE" commit -m big); [[ $mb -lt 40 ]] && ok "committing a 100 MB file uses ${mb} MB of memory" || fail "commit used $mb MB"
mb=$(peak_mb "$COPSE" status); [[ $mb -lt 40 ]] && ok "status: ${mb} MB" || fail "status used $mb MB"
mb=$(peak_mb "$COPSE" fsck); [[ $mb -lt 40 ]] && ok "fsck: ${mb} MB" || fail "fsck used $mb MB"
mb=$(peak_mb "$COPSE" gc); [[ $mb -lt 40 ]] && ok "gc: ${mb} MB" || fail "gc used $mb MB"
rm big.bin
mb=$(peak_mb "$COPSE" restore big.bin); [[ $mb -lt 40 ]] && ok "restoring it from a pack: ${mb} MB" || fail "restore used $mb MB"
cmp -s big.bin ../big.orig && ok "and it comes back byte for byte" || fail "line $LINENO: a step before this check failed"
printf 'x' >> big.bin && cps commit -m "bigger" >/dev/null && cps checkout HEAD~1 >/dev/null && cmp -s big.bin ../big.orig && ok "switching between versions of it works" || fail "line $LINENO: a step before this check failed"
cps checkout main >/dev/null
cd .. && cps clone big big2 >/dev/null && cmp -s big2/small.txt big/small.txt && [[ $(wc -c < big2/big.bin) -eq 100000001 ]] && ok "it clones" || fail "line $LINENO: a step before this check failed"

echo "== commits at the same time"
mkdir race && cd race && cps init >/dev/null && echo base > base && cps commit -m base >/dev/null
for i in $(seq 1 20); do echo "$i" > "p$i"; done
for i in $(seq 1 20); do (cps commit "p$i" -m "p$i" >/dev/null 2>&1; echo $? > "../code$i") & done; wait
log=$(cps log --oneline -100); lost=""
for i in $(seq 1 20); do [[ $(cat "../code$i") == 0 ]] && echo "$log" | grep -qw "p$i" || lost+=" p$i"; done
[[ -z "$lost" && $(echo "$log" | wc -l) -eq 21 && ! -e .copse/operation.lock ]] && ok "20 commits started at once all succeed, and every one is on the branch" || fail "lost or failed:$lost"
echo "999999 commit" > .copse/operation.lock && echo x > x && cps commit -m x >/dev/null && ok "a lock left by a command that's gone is taken over" || fail "line $LINENO: a step before this check failed"
cd ..

echo "== edits that keep a file's size and modification time"
mkdir samestat && cd samestat && cps init >/dev/null
printf 'aaaa\n' > f && touch -t 202001010000 f && cps commit -m a >/dev/null && sleep 3 && cps status >/dev/null
printf 'bbbb\n' > f && touch -t 202001010000 f
cps status | has "1 modified" && ok "new contents of the same size, with the old modification time put back, are seen" || fail "line $LINENO: the edit was missed"
cps commit -m b >/dev/null && sleep 3 && cps status >/dev/null && cps config core.trustctime false >/dev/null
printf 'cccc\n' > f && touch -t 202001010000 f
cps status | has "Nothing changed" && ok "with core.trustctime = false the change time is ignored, as in git" || fail "line $LINENO: core.trustctime was ignored"
cd ..

echo
echo "All $pass checks passed."
