#!/usr/bin/env bash
# Rebase tests: clean replays, conflicts (continue / skip / abort), already-applied
# commits, branches containing merges, pull --rebase, and the safety guards.
# Usage: tests/rebase.sh [work_dir]     (build first: cargo build --release)
set -euo pipefail

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
same() { [[ "$1" == "$2" ]] || fail "$3 (got '$1', want '$2')"; ok "$3"; }
expect_fail() { # expect_fail "description" "message substring" cmd...
  local what="$1" want="$2"; shift 2
  local out; if out=$("$@" 2>&1); then fail "$what (should have failed)"; fi
  [[ "$out" == *"$want"* ]] || fail "$what (wrong error: $out)"
  ok "$what"
}
history() { cps log -n 50 | grep -v UTC | sed 's/^ *//' | grep -v '^$' | tr '\n' ',' | sed 's/,$//'; }
fingerprint() {
  find . -path ./.copse -prune -o -type f -print | LC_ALL=C sort | while read -r f; do echo "$f $(shasum < "$f")"; done | shasum | cut -c1-16
}
lines() { tr '\n' ' ' < "$1" | sed 's/ $//'; }
commit() { cps commit -m "$1" >/dev/null; }

# A fresh repo: base commit with shared.txt (5 lines), then main gets two commits.
setup() {
  rm -rf "$1" && mkdir "$1" && cd "$1" && cps init >/dev/null
  printf '1\n2\n3\n4\n5\n' > shared.txt && commit base
  cps branch feature >/dev/null
  echo a1 > a.txt && commit A1
  sed -i '' 's/^1$/ONE-main/' shared.txt && commit A2
}

echo "== clean rebase"
setup clean
cps checkout feature >/dev/null
echo f1 > f.txt && commit F1
sed -i '' 's/^5$/FIVE-feature/' shared.txt && commit F2
echo f3 >> f.txt && commit F3
main_before=$(cat .copse/refs/main)
cps rebase main | tail -1 | has "Rebased feature onto main" && ok "rebase reports success" || fail "line $LINENO: a step before this check failed"
same "$(history)" "F3,F2,F1,A2,A1,base" "history is linear: feature's commits on top of main's"
same "$(lines shared.txt)" "ONE-main 2 3 4 FIVE-feature" "edits from both sides are combined"
same "$(lines f.txt) / $(cat a.txt)" "f1 f3 / a1" "files from both branches are present"
same "$(cat .copse/refs/main)" "$main_before" "main itself is untouched"
same "$(cps status | head -1)" "On branch feature" "HEAD is back on the branch"
same "$(cps status | tail -1)" "Nothing changed." "working directory is clean"
same "$(cps log -n 1 | head -1 | awk '{print $(NF-1), $NF}')" "Test Person" "original author kept"
cps checkout main >/dev/null && cps merge feature | has "Fast-forward" && ok "main can now fast-forward to feature" || fail "line $LINENO: a step before this check failed"
cd ..

echo "== nothing to do / fast-forward"
setup ff
same "$(cps rebase feature)" "Already up to date: main already contains feature." "rebase onto an ancestor does nothing"
cps checkout feature >/dev/null
cps rebase main | has "Fast-forwarded feature" && ok "branch with no own commits fast-forwards" || fail "line $LINENO: a step before this check failed"
same "$(history)" "A2,A1,base" "and ends up exactly at main"
cd ..

echo "== conflict, then --continue"
setup cont
cps checkout feature >/dev/null
echo f1 > f.txt && commit F1
sed -i '' 's/^1$/ONE-feature/' shared.txt && echo new > added-by-F2.txt && commit F2
echo f3 >> f.txt && commit F3
out=$(cps rebase main)
[[ "$out" == *"Conflict while applying"*"F2"*"(2/3)"* ]] && ok "stops at the conflicting commit (2 of 3)" || fail "no stop: $out"
grep -q '^<<<<<<< main$' shared.txt && grep -q 'ONE-feature' shared.txt && ok "conflict markers written" || fail "line $LINENO: a step before this check failed"
cps status | has "Rebasing feature onto main (1 of 3 commits done)" && ok "status shows rebase progress" || fail "line $LINENO: a step before this check failed"
expect_fail "commit is refused mid-rebase" "rebase is in progress" cps commit -m x
expect_fail "checkout is refused mid-rebase" "rebase is in progress" cps checkout main
expect_fail "merge is refused mid-rebase" "rebase is in progress" cps merge main
expect_fail "starting another rebase is refused" "already in progress" cps rebase main
expect_fail "continue with markers left is refused" "still have conflict markers" cps rebase --continue
printf 'ONE-both\n2\n3\n4\n5\n' > shared.txt
cps status | has "Conflicts are fixed" && ok "status notices the fix" || fail "line $LINENO: a step before this check failed"
cps rebase --continue | tail -1 | has "Rebased feature onto main" && ok "continue finishes the rebase" || fail "line $LINENO: a step before this check failed"
same "$(history)" "F3,F2,F1,A2,A1,base" "all three commits replayed"
same "$(lines shared.txt) | $(lines f.txt) | $(cat added-by-F2.txt)" "ONE-both 2 3 4 5 | f1 f3 | new" "resolution and later commits all present"
[[ ! -f .copse/REBASE_STATE ]] && ok "rebase state cleaned up" || fail "state left behind"
cd ..

echo "== conflict, then --skip"
setup skip
cps checkout feature >/dev/null
sed -i '' 's/^1$/ONE-feature/' shared.txt && echo x > from-skipped.txt && commit F1
echo f2 > f.txt && commit F2
cps rebase main >/dev/null
cps rebase --skip | tail -1 | has "Rebased feature onto main" && ok "skip drops the commit and finishes" || fail "line $LINENO: a step before this check failed"
same "$(history)" "F2,A2,A1,base" "skipped commit is gone, the rest replayed"
[[ ! -f from-skipped.txt ]] && ok "files from the skipped commit removed" || fail "skipped commit's file left behind"
same "$(lines shared.txt)" "ONE-main 2 3 4 5" "main's version kept"
cd ..

echo "== conflict, then --abort"
setup abort
cps checkout feature >/dev/null
echo f1 > f.txt && commit F1
sed -i '' 's/^1$/ONE-feature/' shared.txt && echo new > added-by-F2.txt && commit F2
orig=$(cat .copse/refs/feature); before=$(fingerprint)
cps rebase main >/dev/null
cps rebase --abort | has "back at" && ok "abort reports success" || fail "line $LINENO: a step before this check failed"
same "$(cat .copse/refs/feature)" "$orig" "branch points where it did before"
same "$(fingerprint)" "$before" "files are exactly as before"
same "$(cps status | head -1)" "On branch feature" "HEAD is back on the branch"
same "$(history)" "F2,F1,base" "history unchanged"
expect_fail "abort with no rebase is an error" "nothing to abort" cps rebase --abort
cd ..

echo "== commit already in the target is skipped"
setup dup
cps checkout feature >/dev/null
echo a1 > a.txt && commit "same change as A1"
echo f > f.txt && commit F
out=$(cps rebase main)
[[ "$out" == *"skipped"*"already there"* ]] && ok "duplicate change detected and skipped" || fail "$out"
same "$(history)" "F,A2,A1,base" "no empty commit left behind"
cd ..

echo "== branch containing a merge"
setup merged
cps checkout feature >/dev/null
echo f1 > f.txt && commit F1
cps branch side >/dev/null && cps checkout side >/dev/null && echo s > s.txt && commit S1 && cps checkout feature >/dev/null
echo f2 >> f.txt && commit F2
cps merge side >/dev/null
before=$(fingerprint)
out=$(cps rebase main)
[[ "$out" == *"1 merge commit(s) flattened away"* ]] && ok "merge commit flattened" || fail "$out"
same "$(history | tr ',' '\n' | grep -c -E '^(F1|F2|S1)$')" "3" "side-branch commit replayed, not dropped"
[[ "$(cat s.txt)" == "s" && "$(lines f.txt)" == "f1 f2" && "$(cat a.txt)" == "a1" ]] && ok "all content present after flattening" || fail "content"
cd ..

echo "== guards"
setup guards
echo dirty >> shared.txt
expect_fail "dirty working directory is refused" "before rebasing" cps rebase feature
tip=$(cps log -n 1 | awk "NR==1" | cut -c1-10)
cps checkout -f main >/dev/null
expect_fail "missing target is reported" "unknown revision" cps rebase nosuchbranch
cps checkout "$tip" >/dev/null
expect_fail "detached HEAD is refused" "HEAD is detached" cps rebase feature
cd ..

echo "== pull --rebase"
cps init --bare hub.copse >/dev/null
mkdir alice && cd alice && cps init >/dev/null && printf '1\n2\n3\n' > notes.txt && commit start
cps remote add origin ../hub.copse >/dev/null && cps push >/dev/null
cd .. && cps clone hub.copse bob >/dev/null
cd bob && echo bob > bob.txt && commit "bob's work" && cps push >/dev/null
cd ../alice && sed -i '' 's/^3$/THREE/' notes.txt && commit "alice's work"
expect_fail "plain push is rejected (diverged)" "has commits you don't have" cps push
cps pull --rebase | tail -1 | has "Rebased main onto origin/main" && ok "pull --rebase replays local work on top" || fail "line $LINENO: a step before this check failed"
same "$(history)" "alice's work,bob's work,start" "history is linear, no merge commit"
cps push | has "Pushed main" && ok "push now succeeds without --force" || fail "line $LINENO: a step before this check failed"
cd ../bob && cps pull | has "Fast-forward" && same "$(lines notes.txt) | $(cat bob.txt)" "1 2 THREE | bob" "other clone fast-forwards to the result" || fail "line $LINENO: a step before this check failed"
cd ..


echo "== interactive rebase"
# A stand-in editor. For the commit list it writes $PLAN, e.g. "pick:F2,squash:F3",
# looking commits up by title; for commit messages it writes $NEW_MSG (if set).
cat > fake-editor <<'PY'
#!/usr/bin/env python3
import os, sys
path = sys.argv[1]
if path.endswith("REBASE_TODO"):
    ids = {}
    for line in open(path):
        parts = line.split(None, 2)
        if len(parts) == 3 and parts[0] == "pick":
            ids[parts[2].strip()] = parts[1]
    plan = [p for p in os.environ.get("PLAN", "").split(",") if p]
    open(path, "w").write("".join(f"{a} {ids.get(t, t)} {t}\n" for a, t in (p.split(":", 1) for p in plan)))
elif os.environ.get("NEW_MSG"):
    open(path, "w").write(os.environ["NEW_MSG"] + "\n# comment lines are ignored\n")
PY
chmod +x fake-editor
export COPSE_EDITOR="$WORK/fake-editor"

# Base repo with three independent commits F1..F3 on top of base (one file each).
isetup() {
  rm -rf "$1" && mkdir "$1" && cd "$1" && cps init >/dev/null
  echo base > base.txt && commit base
  for i in 1 2 3; do echo "f$i" > "f$i.txt" && commit "F$i"; done
}
ri() { PLAN="$1" NEW_MSG="${2:-}" cps rebase -i HEAD~3; }

isetup reorder
ri "pick:F3,pick:F1,pick:F2" >/dev/null
same "$(history)" "F2,F1,F3,base" "reorder commits"
same "$(cat f1.txt f2.txt f3.txt | tr '\n' ' ')" "f1 f2 f3 " "reordered history has all files"
cd ..

isetup drop
ri "pick:F1,drop:F2,pick:F3" >/dev/null
same "$(history)" "F3,F1,base" "drop a commit"
[[ ! -e f2.txt ]] && ok "dropped commit's file is gone" || fail "f2.txt still there"
cd ..

isetup deleteline
ri "pick:F1,pick:F3" >/dev/null
same "$(history)" "F3,F1,base" "deleting a line drops that commit"
cd ..

isetup squash
ri "pick:F1,squash:F2,pick:F3" "F1 and F2 together" >/dev/null
same "$(history)" "F3,F1 and F2 together,base" "squash folds a commit in with a new message"
same "$(cat f1.txt f2.txt | tr '\n' ' ')" "f1 f2 " "squashed commit has both changes"
cd ..

isetup fixup
ri "pick:F1,fixup:F2,fixup:F3" >/dev/null
same "$(history)" "F1,base" "fixup folds commits in, keeping the first message"
same "$(cat f1.txt f2.txt f3.txt | tr '\n' ' ')" "f1 f2 f3 " "fixed-up commit has all changes"
cd ..

isetup reword
ri "reword:F1,pick:F2,pick:F3" "A better first message" >/dev/null
same "$(history)" "F3,F2,A better first message,base" "reword changes just the message"
cd ..

isetup keepids
before=$(cat .copse/refs/main)
ri "pick:F1,pick:F2,pick:F3" >/dev/null
same "$(cat .copse/refs/main)" "$before" "unchanged list keeps the very same commits"
cd ..

isetup edit
out=$(ri "pick:F1,edit:F2,pick:F3")
[[ "$out" == *"Paused at"*"F2"* ]] && ok "edit pauses at the commit" || fail "$out"
cps status | has "Paused to edit" && ok "status shows the edit pause" || fail "line $LINENO: a step before this check failed"
expect_fail "skip is refused during an edit pause" "paused for an edit" cps rebase --skip
echo "amended" >> f2.txt
cps rebase --continue | tail -1 | has "Rebased main" && ok "continue after editing finishes" || fail "line $LINENO: a step before this check failed"
same "$(history)" "F3,F2,F1,base" "edited history keeps its shape"
cps checkout -f HEAD~1 >/dev/null && same "$(cat f2.txt | tr '\n' ' ')" "f2 amended " "the change was folded into the edited commit" && cps checkout -f main >/dev/null || fail "line $LINENO: a step before this check failed"
cd ..

isetup split
ri "pick:F1,edit:F2,pick:F3" >/dev/null
echo extra > extra.txt && cps commit -m "extra commit" >/dev/null && ok "commit is allowed during an edit pause" || fail "line $LINENO: a step before this check failed"
cps rebase --continue >/dev/null
same "$(history)" "F3,extra commit,F2,F1,base" "new commit inserted where the rebase paused"
cd ..

isetup editabort
before=$(cat .copse/refs/main); fp=$(fingerprint)
ri "edit:F1,pick:F2,pick:F3" >/dev/null && echo junk > junk.txt && echo more >> f1.txt
cps rebase --abort >/dev/null
[[ "$(cat junk.txt)" == "junk" ]] && ok "abort keeps untracked files you created" || fail "junk.txt lost"
rm -f junk.txt
same "$(cat .copse/refs/main) $(fingerprint)" "$before $fp" "abort during an edit pause restores branch and tracked files"
cd ..

isetup conflict
# F1 and F2 both edit the same line in sequence, so swapping them conflicts.
printf 'a\n' > seq.txt && commit S0
printf 'b\n' > seq.txt && commit S1
printf 'c\n' > seq.txt && commit S2
out=$(PLAN="pick:S2,pick:S1" cps rebase -i HEAD~2)
[[ "$out" == *"Conflict while applying"*"S2"* ]] && ok "reordering into a conflict pauses" || fail "$out"
printf 'c\n' > seq.txt
out=$(cps rebase --continue)
[[ "$out" == *"Conflict while applying"*"S1"* ]] && ok "second swapped commit conflicts too, and pauses again" || fail "$out"
printf 'b\n' > seq.txt && cps rebase --continue | tail -1 | has "Rebased main" && ok "continue after the second fix finishes" || fail "line $LINENO: a step before this check failed"
same "$(history | cut -d, -f1-2) $(cat seq.txt)" "S1,S2 b" "commits end up in the new order with the resolutions"
cd ..

isetup cancel
before=$(cat .copse/refs/main)
same "$(ri "")" "Nothing to do: the list was empty, so the rebase was cancelled." "empty list cancels"
same "$(cat .copse/refs/main)" "$before" "cancel changes nothing"
expect_fail "unknown action is reported" "unknown action" env PLAN="frobnicate:F1" "$COPSE" rebase -i HEAD~3
expect_fail "squash with nothing above is refused" "first kept commit" env PLAN="squash:F1,pick:F2" "$COPSE" rebase -i HEAD~3
same "$(cat .copse/refs/main) $(cps status | tail -1)" "$before Nothing changed." "rejected lists change nothing"
cd ..
unset COPSE_EDITOR

echo
echo "All $pass checks passed."
