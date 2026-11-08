#!/usr/bin/env bash
# merge --no-ff / --ff-only / -m, rebase --onto / --autosquash / exec (checked against
# git doing the same), and stashing only some files.
# Usage: tests/workflow.sh [work_dir]     (build first: cargo build --release; needs git)
set -euo pipefail
. "$(cd "$(dirname "$0")" && pwd)/portable.sh"   # sedi, mtime, days_ago, peak_mb, ... (macOS and Linux)

COPSE="$(cd "$(dirname "$0")/.." && pwd)/target/release/copse"
WORK="${1:-$(mktemp -d)}"
rm -rf "$WORK" && mkdir -p "$WORK" && cd "$WORK"
cps() { "$COPSE" "$@"; }
export COPSE_AUTHOR="Test Person" COPSE_EMAIL="test@example.com"  # fixed identity, same on every machine
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1 GIT_AUTHOR_NAME="Test Person" GIT_AUTHOR_EMAIL=test@example.com GIT_COMMITTER_NAME="Test Person" GIT_COMMITTER_EMAIL=test@example.com
export COPSE_EDITOR=true GIT_EDITOR=true GIT_SEQUENCE_EDITOR=true   # accept lists and messages as offered
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
titles() { cps log -n 100 --oneline --first-parent "$@" | sed -E 's/^[0-9a-f]{10} (\([^)]*\) )?//' | tr '\n' ',' | sed 's/,$//'; }
git_titles() { git log --first-parent --format=%s "$@" | tr '\n' ',' | sed 's/,$//'; }
parents() { cps show "${1:-HEAD}" | grep -c '^Merge:' || true; }
clean() { [[ "$(cps status | tail -1)" == "Nothing changed." ]]; }
files() { for f in "$@"; do printf '%s=%s ' "$f" "$(tr '\n' '.' < "$f")"; done | sed 's/ $//'; }

echo "== merge --no-ff, --ff-only, -m"
mkdir m && cd m && cps init >/dev/null && echo base > a && cps commit -m base >/dev/null
cps branch feat >/dev/null && cps checkout feat >/dev/null && echo f > f && cps commit -m "feat work" >/dev/null && cps checkout main >/dev/null
cps branch feat2 feat >/dev/null
out=$(cps merge --no-ff feat); [[ "$out" == *"Merged feat into main"* ]] && ok "--no-ff makes a merge commit even though a fast-forward was possible" || fail "$out"
same "$(titles)" "Merge feat into main,base" "the main line shows the merge, not the branch's commits"
same "$(cps log -n 10 | grep -c 'feat work')" "1" "the branch's commit is still in history, via the merge"
[[ -f f ]] && clean && ok "with the branch's files" || fail "line $LINENO: a step before this check failed"
cps reset --hard HEAD~1 >/dev/null
cps merge --ff-only feat | has "Fast-forward" && same "$(titles)" "feat work,base" "--ff-only fast-forwards when it can" || fail "line $LINENO: a step before this check failed"
cps checkout feat2 >/dev/null && echo g > g && cps commit -m "feat2 more" >/dev/null && cps checkout main >/dev/null && echo m > m && cps commit -m "main moves" >/dev/null
expect_fail "--ff-only refuses when the branches have diverged" "can't fast-forward" cps merge --ff-only feat2
clean && ok "and leaves everything as it was" || fail "line $LINENO: a step before this check failed"
cps merge -m "Bring in feat2" feat2 >/dev/null && same "$(titles -n 1)" "Bring in feat2" "-m sets the merge commit's message" || fail "line $LINENO: a step before this check failed"
expect_fail "--no-ff and --ff-only don't go together" "cannot be used with" cps merge --no-ff --ff-only feat
cd ..

echo "== merges follow renames"
mkdir rn && cd rn && cps init >/dev/null && seq 1 30 > old.txt && seq 101 130 > other.txt && cps commit -m base >/dev/null
cps branch side >/dev/null && cps checkout side >/dev/null && mkdir lib && sed 's/^5$/five/' old.txt > lib/new.txt && rm old.txt && cps commit -m "rename and edit" >/dev/null
cps checkout main >/dev/null && sedi 's/^25$/twentyfive/' old.txt && cps commit -m "edit under the old name" >/dev/null
cps merge side >/dev/null && same "$(test -e old.txt && echo old || echo no-old) $(grep -c -E '^(five|twentyfive)$' lib/new.txt)" "no-old 2" "a rename on one side and an edit on the other combine in the renamed file" || fail "line $LINENO: a step before this check failed"
cps checkout side >/dev/null && cps merge main >/dev/null && grep -q twentyfive lib/new.txt && ok "and the other way round" || fail "line $LINENO: a step before this check failed"
cps checkout main >/dev/null && cps branch del >/dev/null && cps branch ren >/dev/null
cps checkout del >/dev/null && rm other.txt && cps commit -m "delete other" >/dev/null && cps checkout ren >/dev/null && mv other.txt moved.txt && cps commit -m "rename other" >/dev/null
out=$(cps merge del); [[ "$out" == *"renamed on one side, deleted on the other"* && -f moved.txt ]] && ok "renamed on one side, deleted on the other: the renamed file is kept and the conflict reported" || fail "$out"
cps merge --abort >/dev/null; cps checkout main >/dev/null
cps branch r1 >/dev/null && cps branch r2 >/dev/null
cps checkout r1 >/dev/null && sed 's/^102$/one-oh-two/' other.txt > a.txt && rm other.txt && cps commit -m "rename to a" >/dev/null
cps checkout r2 >/dev/null && sed 's/^128$/one-two-eight/' other.txt > b.txt && rm other.txt && cps commit -m "rename to b" >/dev/null
cps merge r1 | has "renamed differently on each side" && same "$(grep -c -E '^(one-oh-two|one-two-eight)$' a.txt) $(grep -c -E '^(one-oh-two|one-two-eight)$' b.txt)" "2 2" "renamed differently on each side: both names get both sides' changes (as git does)" || fail "line $LINENO: a step before this check failed"
cps merge --abort >/dev/null; cd ..

echo "== merges follow folder moves"
mkdir dm && cd dm && cps init >/dev/null && mkdir lib && echo a > lib/a.txt && echo b > lib/b.txt && echo top > top.txt && cps commit -m base >/dev/null
cps branch move >/dev/null && cps checkout move >/dev/null && mkdir src && mv lib/a.txt lib/b.txt src/ && rmdir lib && cps commit -m "lib/ becomes src/" >/dev/null
cps checkout main >/dev/null && echo new > lib/new.txt && cps commit -m "a new file in lib/" >/dev/null
out=$(cps merge move); [[ "$out" == *"added in a folder the other side moved"* ]] && [[ -f src/new.txt && ! -e lib ]] && ok "a file added in a folder the other side moved goes to the folder's new place, and it's reported" || fail "$out"
cps commit -m "merged" >/dev/null && same "$(cps show --stat HEAD | grep -c 'src/')" "3" "and the merge can be committed as it stands (all three files now in src/)" || fail "line $LINENO: a step before this check failed"
cps branch flatten HEAD~1 >/dev/null && cps checkout flatten >/dev/null && mv lib/a.txt lib/b.txt . && rm lib/new.txt && rmdir lib && cps commit -m "lib/ moved to the top" >/dev/null
cps checkout main >/dev/null 2>&1; cps reset --hard HEAD~1 >/dev/null 2>&1; echo more > lib/more.txt && cps commit -m "another lib file" >/dev/null
cps merge flatten >/dev/null; [[ -f more.txt && ! -e lib/more.txt ]] && ok "a folder moved to the top level: its new file follows it there" || fail "line $LINENO: more.txt didn't follow lib/ to the top"
cps merge --abort >/dev/null 2>&1 || true; cd ..

echo "== merge strategies: -s ours, -X ours / theirs (checked against git)"
# The same history in git and copse: both sides edit line 3 (a conflict), each edits
# another line of its own (merges cleanly), both change a binary file, and theirs
# deletes a file ours edited (stays a conflict, as in git).
build() {  # $1 = git or copse
  mkdir "$1" && cd "$1"
  local c=cps; [[ $1 == git ]] && { git init -q -b main; c=git; } || cps init >/dev/null
  seq 1 10 > f.txt; printf 'bin\0base' > b.bin; echo keep > d.txt
  if [[ $c == git ]]; then git add -A && git commit -qm base && git branch side; else cps commit -m base >/dev/null && cps branch side >/dev/null; fi
  sedi -e 's/^3$/three-ours/' -e 's/^8$/eight-ours/' f.txt; printf 'bin\0ours' > b.bin; echo edited > d.txt
  if [[ $c == git ]]; then git commit -qam ours && git checkout -q side; else cps commit -m ours >/dev/null && cps checkout side >/dev/null; fi
  sedi -e 's/^3$/three-theirs/' -e 's/^10$/ten-theirs/' f.txt; printf 'bin\0theirs' > b.bin; rm d.txt
  if [[ $c == git ]]; then git commit -qam theirs && git checkout -q main; else cps commit -m theirs >/dev/null && cps checkout main >/dev/null; fi
  cd ..
}
mkdir strat && cd strat && build git && build copse
for x in ours theirs; do
  cp -R git "git-$x" && cp -R copse "copse-$x"
  (cd "git-$x" && git merge -q -X "$x" side -m m >/dev/null 2>&1 || true)
  out=$(cd "copse-$x" && cps merge -X "$x" side -m m 2>&1 || true)
  same "$(cd "copse-$x" && files f.txt) $(hashin < "copse-$x/b.bin")" "$(cd "git-$x" && files f.txt) $(hashin < "git-$x/b.bin")" "-X $x: the clashing line and the binary file go the $x side's way, other edits from both sides are kept (same as git)"
  [[ "$out" == *"d.txt"* && "$out" == *"deleted on one side"* && -f "copse-$x/d.txt" ]] && ok "-X $x: a file deleted on one side and edited on the other is still a conflict, as in git" || fail "$out"
done
cp -R git git-s && cp -R copse copse-s
(cd git-s && git merge -q -s ours side -m "keep ours")
(cd copse-s && cps merge -s ours side -m "keep ours" >/dev/null)
same "$(cd copse-s && files f.txt d.txt) $(cd copse-s && parents)" "$(cd git-s && files f.txt d.txt) 1" "-s ours records the merge but keeps the current files exactly (same as git)"
(cd copse-s && clean) && ok "and leaves nothing to commit" || fail "line $LINENO: a step before this check failed"
mkdir ffs && cd ffs && cps init >/dev/null && echo a > a && cps commit -m a >/dev/null && cps branch s >/dev/null && cps checkout s >/dev/null && echo b > b && cps commit -m b >/dev/null && cps checkout main >/dev/null
cps merge -s ours s >/dev/null && [[ ! -e b ]] && same "$(parents)" "1" "-s ours makes a merge commit even where a fast-forward would do, as git does"
cd ..
(cd copse && expect_fail "-s theirs is refused with a pointer to -X theirs" "-X theirs" cps merge -s theirs side)
(cd copse && expect_fail "unknown -X options are refused" "isn't supported" cps merge -X patience side)
cp -R git git-cp && cp -R copse copse-cp
(cd git-cp && git cherry-pick -X theirs side >/dev/null 2>&1 || true)
(cd copse-cp && cps cherry-pick -X theirs side >/dev/null 2>&1 || true)
same "$(cd copse-cp && files f.txt)" "$(cd git-cp && files f.txt)" "cherry-pick -X theirs (same as git)"
cp -R git git-rb && cp -R copse copse-rb
(cd git-rb && git checkout -q side && git rebase -q -X ours main >/dev/null 2>&1 || true)
(cd copse-rb && cps checkout side >/dev/null && cps rebase -X ours main >/dev/null 2>&1 || true)
same "$(cd copse-rb && files f.txt)" "$(cd git-rb && files f.txt)" "rebase -X ours (ours is the branch being rebased onto, as in git)"
mkdir rbc && cd rbc && cps init >/dev/null && seq 1 5 > f.txt && echo keep > d.txt && cps commit -m base >/dev/null && cps branch topic >/dev/null
sedi 's/^3$/main-3/' f.txt && echo edited > d.txt && cps commit -m "main edits" >/dev/null
cps checkout topic >/dev/null && rm d.txt && cps commit -m "drop d" >/dev/null && sedi 's/^3$/topic-3/' f.txt && cps commit -m "topic edits 3" >/dev/null
out=$(cps rebase -X theirs main 2>&1 || true); [[ "$out" == *"d.txt"* ]] || fail "the rebase should stop on d.txt: $out"
rm d.txt && cps rebase --continue >/dev/null 2>&1 && same "$(sed -n 3p f.txt) $(titles -n 1)" "topic-3 topic edits 3" "rebase --continue keeps -X theirs for the steps after a pause"
cd ..
cd ..

echo "== fast-forward pulls touch only what changed"
mkdir ff && cd ff && cps init >/dev/null && mkdir -p src docs && echo a > src/a.txt && echo b > src/b.txt && echo d > docs/d.txt && echo k > keep.txt && cps commit -m base >/dev/null
cd .. && cps clone ff ff2 >/dev/null && cd ff
echo a2 > src/a.txt && rm docs/d.txt && mkdir -p new/deep && echo n > new/deep/n.txt && cps commit -m "upstream work" >/dev/null && cd ../ff2
echo "my edit" > keep.txt
out=$(cps pull); [[ "$out" == *"Fast-forward"* ]] && ok "pull fast-forwards with an uncommitted edit to an unrelated file (as git allows)" || fail "$out"
same "$(cat src/a.txt) $(cat new/deep/n.txt) $(test -e docs && echo docs || echo no-docs) $(cat keep.txt)" "a2 n no-docs my edit" "changed, added and deleted files arrive (emptied folders too); your edit is untouched"
same "$(cps status | grep -E '^  [AMD] ' | sed 's/^ *//')" "M keep.txt" "and it's still your only change"
cd ../ff && echo a3 > src/a.txt && cps commit -m "a again" >/dev/null && cd ../ff2
echo "local a" > src/a.txt
expect_fail "a pull that would overwrite an uncommitted edit is refused" "src/a.txt (changed here)" cps pull
same "$(cat src/a.txt) $(cps log --oneline -n 1 | sed -E 's/^[0-9a-f]{10} (\([^)]*\) )?//')" "local a upstream work" "and nothing was changed"
cps restore src/a.txt >/dev/null && cd ../ff && echo x > src/c.txt && cps commit -m "add c" >/dev/null && cd ../ff2 && echo mine > src/c.txt
expect_fail "so is one that would overwrite an untracked file" "src/c.txt (untracked here)" cps pull
rm src/c.txt && cps pull >/dev/null && same "$(cat src/a.txt) $(cat src/c.txt)" "a3 x" "once out of the way, it goes through" || fail "line $LINENO: a step before this check failed"
cps fsck | has "everything is intact" && cps status | grep -E '^  [AMD] ' | sed 's/^ *//' | has "M keep.txt" && ok "the index stays exact (status and fsck agree)" || fail "line $LINENO: a step before this check failed"
cd ..

echo "== rebase --onto"
# main -> next (two commits) -> topic (two commits); move topic onto main, leaving next's commits behind.
mkdir o && cd o && cps init >/dev/null && echo 1 > base.txt && cps commit -m "o: base" >/dev/null
cps branch next >/dev/null && cps checkout next >/dev/null && echo n1 > n1 && cps commit -m "next 1" >/dev/null && echo n2 > n2 && cps commit -m "next 2" >/dev/null
cps branch topic >/dev/null && cps checkout topic >/dev/null && echo t1 > t1 && cps commit -m "topic 1" >/dev/null && echo t2 > t2 && cps commit -m "topic 2" >/dev/null
cps checkout main >/dev/null && echo m >> base.txt && cps commit -m "main 2" >/dev/null
cd .. && git init -q --bare -b main o.git && cd o && cps remote add gh ../o.git >/dev/null && cps push gh main >/dev/null && cps push gh next >/dev/null && cps push gh topic >/dev/null && cd .. && git clone -q o.git og && cd o
cps checkout topic >/dev/null
out=$(cps rebase --onto main next); [[ "$out" == *"Rebasing 2 commit(s) from topic onto main"* ]] && ok "--onto NEWBASE UPSTREAM moves only the commits after UPSTREAM" || fail "$out"
(cd ../og && git checkout -q topic && git rebase -q --onto origin/main origin/next)
same "$(titles)" "$(cd ../og && git_titles)" "same commits, in the same order, as git rebase --onto"
same "$(ls | tr '\n' ' ')" "$(cd ../og && ls | tr '\n' ' ')" "and the same files (next's are gone)"
cps checkout next >/dev/null && cps branch empty-after next >/dev/null && cps checkout empty-after >/dev/null
cps rebase --onto main next | has "now points at main" && same "$(titles -n 1)" "main 2" "a branch with nothing after UPSTREAM just moves to NEWBASE" || fail "line $LINENO: a step before this check failed"
cd ..

echo "== rebase --autosquash"
mkdir s && cd s && cps init >/dev/null && echo base > base.txt && cps commit -m "s: base" >/dev/null
printf 'a\n' > a.txt && cps commit -m "add a" >/dev/null
printf 'b\n' > b.txt && cps commit -m "add b" >/dev/null
printf 'a fixed\n' > a.txt && cps commit -m "fixup! add a" >/dev/null
printf 'c\n' > c.txt && cps commit -m "add c" >/dev/null
printf 'b\nmore b\n' > b.txt && cps commit -m $'squash! add b\n\nexplain the extra b line' >/dev/null
printf 'a fixed twice\n' > a.txt && cps commit -m "fixup! fixup! add a" >/dev/null
cd .. && git init -q --bare -b main s.git && cd s && cps remote add gh ../s.git >/dev/null && cps push gh >/dev/null && cd .. && git clone -q s.git sg && cd s
out=$(cps rebase --autosquash HEAD~6); [[ "$out" == *"fixed up"* && "$out" == *"squashed"* ]] && ok "fixup! and squash! commits are folded in" || fail "$out"
(cd ../sg && git rebase -q -i --autosquash HEAD~6 >/dev/null)
same "$(titles)" "$(cd ../sg && git_titles)" "the same commits as git rebase --autosquash"
same "$(files a.txt b.txt c.txt)" "$(cd ../sg && files a.txt b.txt c.txt)" "with the same contents"
same "$(cps log -n 2 | grep -A3 'add b' | sed 's/^ *//' | grep -v '^$' | tr '\n' '|')" "add b|explain the extra b line|" "a squash keeps the body but drops the squash! title"
clean && ok "clean afterwards" || fail "line $LINENO: a step before this check failed"
cat > ../show-list.sh <<'EOF'
#!/bin/sh
cp "$1" ../list.txt
EOF
chmod +x ../show-list.sh
printf 'z\n' > a.txt && cps commit -m "fixup! add a" >/dev/null
COPSE_EDITOR=../show-list.sh cps rebase -i --autosquash HEAD~4 >/dev/null
same "$(grep -v '^#' ../list.txt | grep -v '^$' | awk '{print $1, $3, $4}' | tr '\n' ',')" "pick add a,fixup fixup! add,pick add b,pick add c," "with -i, the list arrives already arranged, for you to adjust"
cd ..

echo "== rebase exec"
mkdir x && cd x && cps init >/dev/null && echo 0 > n && cps commit -m "x: base" >/dev/null
for i in 1 2 3; do echo "$i" > n; cps commit -m "x$i" >/dev/null; done
out=$(cps rebase -x 'cat n >> ../ran.txt' HEAD~3); [[ "$out" == *"exec cat n"* ]] && ok "-x runs a command after each commit" || fail "$out"
same "$(tr '\n' ' ' < ../ran.txt)" "1 2 3 " "once per commit, with that commit checked out"
same "$(titles)" "x3,x2,x1,x: base" "commits that need no changes stay as they are"
out=$(cps rebase -x 'test "$(cat n)" != 2 || test -f ok.txt' HEAD~3); [[ "$out" == *"failed (exit 1); paused at"* ]] && ok "a failing command pauses the rebase on that commit" || fail "$out"
cps status | has "Paused to edit" && ok "status shows the pause" || fail "line $LINENO: a step before this check failed"
touch ok.txt
cps rebase --continue | has "Rebased" && cps show --stat HEAD~1 | has "ok.txt" && ok "--continue folds the fix into that commit and carries on" || fail "line $LINENO: a step before this check failed"
same "$(cat n),$(titles -n 3)" "3,x3,x2,x1" "and the later commits are replayed on top"
cat > ../add-exec.sh <<'EOF'
#!/bin/sh
awk 'NR==1 {print; print "exec echo between >> ../between.txt"; next} {print}' "$1" > "$1.new" && mv "$1.new" "$1"
EOF
chmod +x ../add-exec.sh
COPSE_EDITOR=../add-exec.sh cps rebase -i HEAD~2 >/dev/null && same "$(cat ../between.txt)" "between" "exec lines can be added to an interactive list" || fail "line $LINENO: a step before this check failed"
cps rebase -x 'echo dirty > dirt.txt' HEAD~1 | has "left uncommitted changes" && ok "a command that leaves changes behind also pauses" || fail "line $LINENO: a step before this check failed"
rm dirt.txt && cps rebase --continue >/dev/null && clean && ok "and --continue carries on" || fail "line $LINENO: a step before this check failed"
cd ..

echo "== stash only some files"
mkdir st && cd st && cps init >/dev/null && mkdir -p src docs && echo a > src/a.rs && echo b > src/b.rs && echo d > docs/d.md && echo r > README && cps commit -m base >/dev/null
echo a2 > src/a.rs && echo d2 > docs/d.md && echo new > src/new.rs && rm README
out=$(cps stash -- src); [[ "$out" == *"Stashed 2 change(s) as stash 0"* && "$out" == *"2 other change(s) left"* ]] && ok "stash -- PATH stashes only the changes there" || fail "$out"
same "$(files src/a.rs docs/d.md) $(test -e src/new.rs && echo new || echo no-new) $(test -e README && echo readme || echo no-readme)" "src/a.rs=a. docs/d.md=d2. no-new no-readme" "those files are back as committed; the rest is untouched"
cps stash show | has "src/new.rs" && ! cps stash show | has "docs/d.md" && ok "the stash holds only those changes" || fail "line $LINENO: a step before this check failed"
cps stash pop >/dev/null || fail "line $LINENO: pop with other changes present failed"
same "$(files src/a.rs docs/d.md src/new.rs)" "src/a.rs=a2. docs/d.md=d2. src/new.rs=new." "pop brings them back alongside your other changes"
same "$(cps status | grep -E '^  [AMD] ' | sed 's/^ *//' | tr '\n' ',')" "D README,M docs/d.md,M src/a.rs,A src/new.rs," "all four show as uncommitted changes again"
echo a9 > src/a.rs && cps stash -- src/a.rs >/dev/null && echo clash > src/a.rs
expect_fail "pop refuses if you've changed a file the stash also changes" "you've also changed" cps stash pop
echo a2 > src/a.rs && cps stash drop >/dev/null
out=$(cps stash -m "just docs" -- docs/d.md README); [[ "$out" == *"Stashed 2 change(s) as stash 0: just docs"* && -f README && "$(cat docs/d.md)" == "d" ]] && ok "several paths, a deletion, and a message" || fail "line $LINENO: a step before this check failed"
cps stash pop >/dev/null || fail "line $LINENO: pop failed"
[[ ! -e README && "$(cat docs/d.md)" == "d2" ]] && ok "popping it deletes the file again" || fail "line $LINENO: a step before this check failed"
expect_fail "a path with no changes is reported" "has no changes to stash" cps stash -- src/b.rs
mkdir -p tmp/deep && echo x > tmp/deep/x.txt && cps stash -- tmp >/dev/null && [[ ! -e tmp ]] && ok "folders emptied by stashing new files are tidied away" || fail "line $LINENO: a step before this check failed"
cd src && echo a3 > a.rs && cps stash -- a.rs >/dev/null && same "$(cat a.rs)" "a" "paths are relative to where you are" || fail "line $LINENO: a step before this check failed" && cd ..

echo
echo "All $pass checks passed."
