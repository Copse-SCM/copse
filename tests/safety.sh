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

echo "== commands that change things run one at a time"
mkdir turns && cd turns && cps init >/dev/null && echo a > a && cps commit -m base >/dev/null && cps branch other >/dev/null
echo m > m && cps commit -m M >/dev/null && cps checkout other >/dev/null && echo o > o && cps commit -m O >/dev/null && cps checkout main >/dev/null
printf '#!/bin/sh\np=$(cat)\ncase "$p" in *"Merge "*) sleep 2;; esac\nprintf -- "-----BEGIN PGP SIGNATURE-----\\nAAAA\\n-----END PGP SIGNATURE-----\\n"\n' > ../slowsign && chmod +x ../slowsign
cps config gpg.program "$PWD/../slowsign" >/dev/null && cps config commit.gpgsign true >/dev/null
cps merge other >/dev/null 2>&1 & sleep 0.5
echo important > work.txt && cps commit -m "important work" >/dev/null; wait
same "$(cps log --format=%s -n 2 | tr '\n' ',')" "important work,Merge other into main," "a commit made while a merge is signing waits its turn, and lands on top instead of being lost"
cps status | has "Nothing changed" && ok "and the index matches (no file left looking untracked)" || fail "line $LINENO: $(cps status)"
cps config --unset commit.gpgsign >/dev/null
mkdir -p .copse/hooks && printf '#!/bin/sh\n"%s" tag from-hook >/dev/null\n' "$COPSE" > .copse/hooks/post-commit && chmod +x .copse/hooks/post-commit
echo h > h && start=$(date +%s) && cps commit -m hooked >/dev/null && [[ $(( $(date +%s) - start )) -lt 5 && -n "$(cps tag | grep from-hook)" ]] && ok "a copse command run by a hook shares its command's turn instead of waiting for it" || fail "line $LINENO: a step before this check failed"
rm .copse/hooks/post-commit
sleep 30 & holder=$!; echo "$holder merge" > .copse/operation.lock; echo h2 > h; touch -t 202001010000 h; before=$(ls -l .copse/index)
cps status | has "M h" && [[ "$(ls -l .copse/index)" == "$before" && -e .copse/operation.lock ]] && ok "status still works while another command is busy, and leaves the index alone" || fail "line $LINENO: a step before this check failed"
kill $holder; wait $holder 2>/dev/null || true; rm -f .copse/operation.lock
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

echo "== a checkout blocked by a file copse doesn't track"
mkdir blocked && cd blocked && cps init >/dev/null && echo a > a && echo z1 > z && cps commit -m main >/dev/null
cps branch third >/dev/null && cps checkout third >/dev/null && mkdir x && echo y > x/y && cps commit -m third >/dev/null && cps checkout main >/dev/null
cps branch other >/dev/null && cps checkout other >/dev/null && rm a && mkdir a && echo b > a/b && echo z2 > z && cps commit -m other >/dev/null
printf '*.log\nx\n' > .copse/exclude && echo mine > a/notes.log
out=$(cps checkout main 2>&1) && fail "a checkout that has to replace a folder holding an ignored file should be refused"
[[ "$out" == *"a/notes.log is in the way of a"* && $(cat z) == z2 && -f a/b && -f a/notes.log ]] && cps status | has "Nothing changed" && ok "an ignored file inside a folder that must become a file stops the checkout before anything changes" || fail "$out"
mv a/notes.log .. && cps checkout main >/dev/null && [[ $(cat a) == a && $(cat z) == z1 ]] && ok "with it moved away, the checkout goes through" || fail "line $LINENO: a step before this check failed"
echo mine > x
out=$(cps checkout third 2>&1) && fail "a checkout that needs a folder where an ignored file is should be refused"
[[ "$out" == *"x is in the way of x/y"* && $(cat x) == mine ]] && cps status | has "On branch main" && ok "so does an ignored file where a folder has to go" || fail "$out"
cd ..

echo "== checkout --force keeps a copy of what it discards"
mkdir forced && cd forced && cps init >/dev/null && echo one > a && echo keep > b && cps commit -m one >/dev/null
cps branch two >/dev/null && cps checkout two >/dev/null && echo two > a && echo theirs > n && cps commit -m two >/dev/null && cps checkout main >/dev/null
echo edited > a && rm b && echo mine > n && echo scratch > u
out=$(cps checkout -f two 2>&1) && [[ "$out" == *"stash 0"* && $(cat a) == two && $(cat n) == theirs && $(cat u) == scratch ]] || fail "line $LINENO: $out"
rm u && cps checkout main >/dev/null && rm -f n && cps stash apply 0 >/dev/null && [[ $(cat a) == edited && ! -e b && $(cat n) == mine && $(cat u) == scratch ]] && ok "an edit, a deletion and a new file the target replaced all come back with stash apply" || fail "line $LINENO: a step before this check failed"
cps reset --hard >/dev/null && cps stash drop 0 >/dev/null && cps stash drop 0 >/dev/null && rm -f n u && before=$(cps stash list | wc -l)
echo scratch > u && cps checkout -f two >/dev/null && [[ $(cps stash list | wc -l) == "$before" && $(cat u) == scratch ]] && ok "untracked files it keeps don't make a stash" || fail "line $LINENO: a step before this check failed"
cd ..

echo "== a symlinked folder of yours is never gone through"
mkdir linked && cd linked && cps init >/dev/null && mkdir vendor keep && echo lib > vendor/lib.c && echo k > keep/k && cps commit -m one >/dev/null
cps branch other >/dev/null && cps checkout other >/dev/null && rm vendor/lib.c && echo more > keep/more && cps commit -m "drop lib" >/dev/null && cps checkout main >/dev/null
mkdir ../shared && echo lib > ../shared/lib.c && rm -rf vendor && ln -s ../shared vendor
out=$(cps merge other 2>&1) && fail "a fast-forward removing a file inside a symlinked folder should be refused"
[[ "$out" == *"vendor/lib.c (vendor is a symlink here)"* && -f ../shared/lib.c && ! -e keep/more ]] && ok "a fast-forward won't remove a file through a symlinked folder, and changes nothing" || fail "$out"
rm vendor && mkdir vendor && echo lib > vendor/lib.c && cps status | has "Nothing changed" || fail "line $LINENO: $(cps status)"
rm -rf vendor && ln -s ../shared vendor
cps sparse set keep >/dev/null && [[ -f ../shared/lib.c ]] && ok "sparse set never deletes a file through a symlinked folder" || fail "line $LINENO: ../shared/lib.c was deleted"
out=$(cps sparse disable 2>&1) && fail "sparse disable writing through a symlinked folder should be refused"
[[ "$out" == *"Nothing was changed"* && -L vendor && -f ../shared/lib.c && ! -e ../shared/k ]] && ok "and won't write through one either" || fail "$out"
cd ..

echo "== a fast-forward happens completely or not at all"
mkdir ffa && cd ffa && cps init >/dev/null && mkdir a && echo x > a/x && for i in $(seq 1 30); do echo $i > f$i; done && cps commit -m main >/dev/null
cps branch other >/dev/null && cps checkout other >/dev/null && rm -rf a && echo file > a && for i in $(seq 1 30); do echo new$i > f$i; done && cps commit -m other >/dev/null && cps checkout main >/dev/null
echo '*.log' > .copse/exclude && echo mine > a/notes.log
out=$(cps merge other 2>&1) && fail "a fast-forward that has to replace a folder holding an ignored file should be refused"
[[ "$out" == *"a/notes.log is in the way of a"* && -f a/x && $(cat f3) == 3 && $(cat f30) == 30 ]] && cps status | has "Nothing changed" && ok "an ignored file inside a folder that must become a file stops it before anything changes" || fail "$out"
rm a/notes.log && cps merge other >/dev/null && [[ $(cat a) == file && $(cat f30) == new30 ]] && ok "with it moved away, the fast-forward goes through" || fail "line $LINENO: a step before this check failed"
cd ..

echo
echo "All $pass checks passed."
