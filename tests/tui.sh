#!/usr/bin/env bash
# The interactive screen, driven by `copse ui --replay KEYS` (which presses the keys
# and prints the screen), with every action's effect checked with the command line.
# Usage: tests/tui.sh [work_dir]     (build first: cargo build --release)
set -euo pipefail
. "$(cd "$(dirname "$0")" && pwd)/portable.sh"   # sedi, mtime, days_ago, peak_mb, ... (macOS and Linux)

COPSE="$(cd "$(dirname "$0")/.." && pwd)/target/release/copse"
WORK="${1:-$(mktemp -d)}"
rm -rf "$WORK" && mkdir -p "$WORK" && cd "$WORK"
cps() { "$COPSE" "$@"; }
export COPSE_AUTHOR="Ada Lovelace" COPSE_EMAIL="ada@example.com" COPSE_FSYNC=0
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1
export HOME="$WORK/home" XDG_CONFIG_HOME="$WORK/home/.config"; mkdir -p "$HOME"
has() { grep -F -- "$1" >/dev/null; }
pass=0
ok() { echo "  ok: $1"; pass=$((pass + 1)); }
fail() { echo "  FAIL: $1"; exit 1; }
same() { [[ "$1" == "$2" ]] || fail "$3 (got '$1', want '$2')"; ok "$3"; }
ui() { cps ui --replay "$1" --size "${2:-110x24}"; }
msg() { cps show --stat | awk '/^Date:/{f=1;getline;next} f&&/^    /{print substr($0,5);next} f{exit}'; }
titles() { cps log -n 100 --oneline "$@" | sed -E 's/^[0-9a-f]{10} (\([^)]*\) )?//' | tr '\n' ',' | sed 's/,$//'; }

mkdir r && cd r && cps init >/dev/null
mkdir src && printf 'fn main() {\n    println!("hi");\n}\n' > src/main.rs && echo "# Demo" > README.md && cps commit -m "Start" >/dev/null
cps branch feature >/dev/null && cps checkout feature >/dev/null && echo "pub fn add() {}" > src/lib.rs && cps commit -m "Add lib" >/dev/null
cps checkout main >/dev/null && echo "docs" >> README.md && cps commit -m "Docs" >/dev/null

echo "== screens"
printf 'fn main() {\n    println!("hello");\n}\n' > src/main.rs && echo notes > notes.txt
out=$(ui ""); [[ "$out" == *"copse  ·  r  ·  main  ·  2 changes"* ]] && ok "the top line shows the repository, branch and how many changes" || fail "$out"
echo "$out" | has "[ ] A notes.txt" && echo "$out" | has "[ ] M src/main.rs" && ok "Changes lists them, marked added and modified" || fail "line $LINENO: a step before this check failed"
echo "$out" | has "+new" || echo "$out" | has "+notes" && ok "with the selected file's diff beside it" || fail "line $LINENO: a step before this check failed"
ui "j" | has '+    println!("hello");' && ok "moving down shows the next file's diff" || fail "line $LINENO: a step before this check failed"
out=$(ui "2"); echo "$out" | has "* " && echo "$out" | has "(main) Docs" && echo "$out" | has "Add lib" && ok "History shows the commit graph" || fail "$out"
echo "$out" | has "Author: Ada Lovelace" && ok "with the selected commit's details" || fail "line $LINENO: a step before this check failed"
ui "2" | grep "▶" | has "(main)" && ok "and starts on the commit you're on, wherever it is in the graph" || fail "line $LINENO: a step before this check failed"
out=$(ui "3"); echo "$out" | has "* main" && echo "$out" | has "feature" && ok "Branches lists them, with the current one marked" || fail "$out"
echo "$out" | has "▶ * main" && ok "and starts on the current branch" || fail "line $LINENO: a step before this check failed"
ui "4" | has "HEAD@{0}" && ok "Undo shows the timeline" || fail "line $LINENO: a step before this check failed"
ui "tab,tab" | has "Branches" && ui "backtab" | has "Undo timeline" && ok "tab and shift-tab move between screens" || fail "line $LINENO: a step before this check failed"
ui "?" | has "Every action runs the same copse command" && ok "? shows the help" || fail "line $LINENO: a step before this check failed"

echo "== Changes: commit, discard, stash"
out=$(ui "space,c,text:Add notes"); echo "$out" | has "Commit 1 picked file(s)" && echo "$out" | has "Summary  Add notes▏" && echo "$out" | has "tab to write it" && ok "space picks a file; c asks for a summary (and offers a description)" || fail "$out"
ui "space,c,text:Add notes,esc" >/dev/null && same "$(titles -n 1)" "Docs" "esc cancels without committing"
out=$(ui "space,c,text:Add notes,enter"); echo "$out" | has "[main " && echo "$out" | has "1 other change(s) left uncommitted" && ok "enter commits just the picked file, and shows copse's output" || fail "$out"
same "$(titles -n 1) | $(cps status | grep -E '^  [AMD] ' | sed 's/^ *//')" "Add notes | M src/main.rs" "the command line agrees: committed, and the other change is still there"
ui "c,enter" >/dev/null && same "$(titles -n 1)" "Add notes" "an empty message commits nothing"
ui "x" | has "Discard your changes to src/main.rs?" && ok "x asks before discarding" || fail "line $LINENO: a step before this check failed"
ui "x,n" >/dev/null && grep -q hello src/main.rs && ok "n keeps the change" || fail "line $LINENO: a step before this check failed"
ui "x,y" >/dev/null && grep -q '"hi"' src/main.rs && cps stash list | has "before restoring" && ok "y discards it, keeping a backup as a stash" || fail "line $LINENO: a step before this check failed"
echo temp > temp.txt && ui "x,y" >/dev/null && [[ ! -e temp.txt ]] && ok "x on a new file deletes it (also backed up)" || fail "line $LINENO: a step before this check failed"
echo wip > wip.txt && ui "s,y" >/dev/null && [[ ! -e wip.txt ]] && cps stash show | has "wip.txt" && ok "s stashes everything" || fail "line $LINENO: a step before this check failed"
cps stash pop >/dev/null && rm -f wip.txt
echo a > a.txt && echo b > b.txt && ui "a,c,text:Both,enter" >/dev/null && same "$(cps show --stat | grep -c '|')" "2" "a picks everything" || fail "line $LINENO: a step before this check failed"

echo "== Changes: a description under the summary"
echo c > c.txt
out=$(ui "c,text:Add c,tab,text:Why it's needed,enter,text:and more"); echo "$out" | has "Why it's needed" && echo "$out" | has "and more▏" && echo "$out" | has "ctrl-s commit" && ok "tab moves to the description, where enter starts a new line" || fail "$out"
ui "c,text:Add c,tab,text:Why it's needed,enter,text:and more,ctrl-s" >/dev/null
same "$(msg)" "$(printf "Add c\n\nWhy it's needed\nand more")" "ctrl-s commits, with the summary, a blank line, then the description"
echo d > d.txt && ui "c,text:Add d,tab,text:body,tab,enter" >/dev/null && same "$(msg)" "$(printf "Add d\n\nbody")" "tab goes back to the summary, where enter commits"
echo e > e.txt && ui "c,text:Add e,tab,alt-enter" >/dev/null && same "$(msg)" "Add e" "an empty description adds nothing (alt-enter commits too)"

echo "== Changes: picking some of a file's changes"
seq 1 30 | sed 's/^/line /' > long.txt && cps commit -m "long" >/dev/null
seq 1 30 | sed 's/^/line /' | sed -e 's/^line 3$/LINE THREE/' -e 's/^line 25$/LINE 25/' > long.txt
on_disk=$(sha256 long.txt)
out=$(ui "enter"); echo "$out" | has "long.txt: 0 of 2 change(s) picked" && echo "$out" | has "▶ [ ] @@ -1,6 +1,6 @@" && ok "enter on a modified file lists its changes, to pick from" || fail "$out"
out=$(ui "enter,space"); echo "$out" | has "[x] @@ -1,6" && echo "$out" | has "▶ [ ] @@ -22" && ok "space picks a change and moves to the next" || fail "$out"
ui "enter,space,enter" | has "[~] M long.txt" && ok "a file with some changes picked is marked [~]" || fail "line $LINENO: a step before this check failed"
ui "enter,space,space,enter" | has "[x] M long.txt" && ok "picking every change picks the whole file" || fail "line $LINENO: a step before this check failed"
ui "enter,a,a,enter" | has "[ ] M long.txt" && ok "a picks all, and again none" || fail "line $LINENO: a step before this check failed"
out=$(ui "enter,space,enter,c,text:Third line,enter"); echo "$out" | has "(picked changes)" && ok "c commits only the picked changes" || fail "$out"
cps show | has "+LINE THREE" && ! cps show | grep "LINE 25" >/dev/null && ok "the commit has the first change and not the second" || fail "line $LINENO: a step before this check failed"
same "$(sha256 long.txt) $(cps status --porcelain | grep long)" "$on_disk M long.txt" "the file on disk is untouched, and the rest is still a change"
cps diff -- long.txt | has "+LINE 25" && ! cps diff -- long.txt | grep "LINE THREE" >/dev/null && ok "and its diff is just the change left" || fail "line $LINENO: a step before this check failed"
cps commit -m "rest of long first" >/dev/null && seq 1 30 | sed 's/^/line /' > long.txt && cps commit -m "plain long" >/dev/null
seq 1 30 | sed 's/^/line /' | sed -e 's/^line 3$/LINE THREE/' -e 's/^line 4$/LINE FOUR/' > long.txt
out=$(ui "enter,l"); echo "$out" | has "▸[ ] -line 3" && echo "$out" | has "single lines" && ok "l goes into a change, to pick single lines" || fail "$out"
out=$(ui "enter,l,j,space,enter"); echo "$out" | has "[~] M long.txt" && ok "space picks one line" || fail "$out"
ui "enter,l,j,j,space,enter,c,text:Just a line,enter" >/dev/null
same "$(cps show HEAD:long.txt | sed -n '3,6p' | tr '\n' ',')" "line 3,line 4,LINE THREE,line 5," "c commits just that line (LINE THREE added; the old lines 3 and 4 kept)"
cps commit -m "rest again" >/dev/null && seq 1 30 | sed 's/^/line /' > long.txt && cps commit -m "plain long again" >/dev/null
seq 1 30 | sed 's/^/line /' | sed -e 's/^line 3$/LINE THREE/' -e 's/^line 8$/LINE EIGHT/' > long.txt
out=$(ui "enter,s"); echo "$out" | has "Split into 2 changes." && echo "$out" | has "(1 of 2)" && ok "s splits a change in two" || fail "$out"
ui "enter,s,j,space,enter,c,text:Eighth,enter" >/dev/null && cps show | has "+LINE EIGHT" && ! cps show | has "+LINE THREE" && ok "and each part can be picked on its own" || fail "line $LINENO: a step before this check failed"
cps commit -m "rest of split" >/dev/null
echo new > newfile.txt && ui "j,enter" | has "newfile.txt can only be committed whole" && ok "new files can only be picked whole" || fail "line $LINENO: a step before this check failed"
rm newfile.txt
seq 1 30 | sed 's/^/line /' | sed -e 's/^line 3$/LINE THREE/' -e 's/^line 25$/LINE 25/' -e 's/^line 14$/LINE 14/' > long.txt
{ cps commit -m "stale" --pick=1:long.txt 2>&1 || true; } | has "now has 2 change(s), not the 1" && same "$(titles -n 1)" "rest of split" "a pick made before the file changed again is refused, not guessed at" || fail "line $LINENO: a step before this check failed"
cps commit -m "rest of long" >/dev/null

echo "== History: revert, branch here, check out"
ui "2,v" | has 'Revert' && ok "v asks before reverting" || fail "line $LINENO: a step before this check failed"
ui "2,v,y" >/dev/null && same "$(titles -n 1)" "Revert \"rest of long\"" "y reverts the selected commit"
ui "2,G,b,text:from-start,enter" >/dev/null && same "$(cps log --oneline -n 1 from-start | sed -E 's/^[0-9a-f]{10} (\([^)]*\) )?//')" "Start" "b makes a branch at the selected commit"
ui "2,G,o,y" >/dev/null && cps status | head -1 | has "HEAD detached" && ok "o checks the selected commit out (detached)" || fail "line $LINENO: a step before this check failed"
cps checkout main >/dev/null

echo "== Branches: new, switch, merge, delete"
ui "3,n,text:topic,enter" >/dev/null && cps branch | has "topic" && ok "n makes a new branch" || fail "line $LINENO: a step before this check failed"
keys="3,G"   # branches are sorted, and topic is the last one (no remote branches yet)
ui "$keys" | grep "▶" | has "topic" || fail "line $LINENO: topic isn't the last branch"
ui "$keys,enter,y" >/dev/null && same "$(cps status | head -1)" "On branch topic" "enter switches to the selected branch"
echo t > t.txt && cps commit -m "topic work" >/dev/null && cps checkout main >/dev/null
ui "$keys,m,y" | has "Fast-forward" && titles -n 3 | has "topic work" && ok "m merges the selected branch into the current one" || fail "line $LINENO: a step before this check failed"
ui "$keys,d,y" >/dev/null && ! cps branch | grep -q " topic " && ok "d deletes it (merged, so it's allowed)" || fail "line $LINENO: a step before this check failed"

echo "== Undo"
before=$(titles -n 1)
ui "4,u" | has "Undo \"" && ok "u asks before undoing" || fail "line $LINENO: a step before this check failed"
ui "4,u,y" >/dev/null && [[ "$(titles -n 1)" != "$before" ]] && ok "y undoes the last change" || fail "line $LINENO: a step before this check failed"
ui "4,u,y" >/dev/null && same "$(titles -n 1)" "$before" "and again redoes it"
target=$(cps log --oneline -n 100 | tail -1 | cut -c1-10)
ui "4,G,enter" | has "Go back to" && ok "enter on an older point asks before going back there" || fail "line $LINENO: a step before this check failed"
ui "4,G,enter,y" >/dev/null && ok "and y goes back" || fail "line $LINENO: a step before this check failed"
cps undo >/dev/null && same "$(titles -n 1)" "$before" "which undo reverses"

echo "== remotes"
cd .. && cps init --bare hub >/dev/null && cd r && cps remote add origin ../hub >/dev/null && cps push >/dev/null
ui "" | has "in step with origin" && ok "the top line says when you're in step with origin" || fail "line $LINENO: a step before this check failed"
echo more > more.txt && cps commit -m more >/dev/null && ui "" | has "↑1 to push" && ok "and how many commits there are to push" || fail "line $LINENO: a step before this check failed"
ui "P" | has "Pushed main to origin" && ui "" | has "in step with origin" && ok "P pushes" || fail "line $LINENO: a step before this check failed"
cd .. && cps clone hub other >/dev/null && cd other && echo theirs > theirs.txt && cps commit -m theirs >/dev/null && cps push >/dev/null && cd ../r
ui "F" >/dev/null && ui "" | has "↓1 to pull" && ok "F fetches, and the top line shows what there is to pull" || fail "line $LINENO: a step before this check failed"
ui "L" | has "Fast-forward" && [[ -f theirs.txt ]] && ok "L pulls" || fail "line $LINENO: a step before this check failed"
ui "F,busy" | has "⟳ copse fetch… (keep going" && ok "fetch, pull and push run in the background, so the screen keeps working" || fail "line $LINENO: a step before this check failed"
ui "F,j,2,busy" | has "2 History" && ok "you can move around while one runs" || fail "line $LINENO: a step before this check failed"
ui "F,P,busy" | has "Wait: copse fetch is still running." && ok "and another can't start until it's done" || fail "line $LINENO: a step before this check failed"
ui "3" | has "origin/main" && ok "remote branches are listed" || fail "line $LINENO: a step before this check failed"
ui "F,q,busy" | has "still running. Quit anyway" && ok "quitting while one runs asks first" || fail "line $LINENO: a step before this check failed"
ui "F,q,j,busy" | has "Staying." && ok "and any other key stays" || fail "line $LINENO: a step before this check failed"

echo "== a push that needs a sign-in"
mkdir -p ../web/private && git init -q --bare -b main ../web/private/secret.git
python3 "$(cd "$(dirname "$COPSE")/../../tests" && pwd)/git-http-server.py" "$(cd ../web && pwd)" > ../web-port.txt 2> ../web-server.log &
server_pid=$!
trap 'kill $server_pid 2>/dev/null' EXIT
for _ in $(seq 1 300); do [[ -s ../web-port.txt ]] && break; perl -e 'select(undef,undef,undef,0.1)'; done
cps remote add web "http://127.0.0.1:$(cat ../web-port.txt)/private/secret.git" >/dev/null
printf '[credential]\n\thelper = store --file=%s\n' "$WORK/creds" > "$WORK/gitconfig-tui" && : > "$WORK/creds"
# P pushes to origin; point origin at the private repository for this check.
cps remote remove origin >/dev/null && cps remote add origin "http://127.0.0.1:$(cat ../web-port.txt)/private/secret.git" >/dev/null
out=$(GIT_CONFIG_GLOBAL="$WORK/gitconfig-tui" GIT_TERMINAL_PROMPT=0 ui "P" 110x30); echo "$out" | has "needs a sign-in, which can't be typed here" && echo "$out" | has "Press t to run" && ok "a push that needs a sign-in says so, and offers the terminal" || fail "$out"
GIT_CONFIG_GLOBAL="$WORK/gitconfig-tui" GIT_TERMINAL_PROMPT=0 ui "P,t" 110x30 | has "(would run copse push in the terminal)" && ok "t hands the push to the terminal" || fail "line $LINENO: a step before this check failed"
echo "http://tester:s3cret@127.0.0.1:$(cat ../web-port.txt)" > "$WORK/creds"
GIT_CONFIG_GLOBAL="$WORK/gitconfig-tui" ui "P" 110x30 | has "Pushed main to origin" && ok "once the login is saved, P works in the screen" || fail "line $LINENO: a step before this check failed"
kill $server_pid 2>/dev/null; trap - EXIT

echo "== robustness"
cd .. && mkdir empty && cd empty && cps init >/dev/null
ui "" | has "Nothing changed since the last commit." && ok "an empty repository shows calmly" || fail "line $LINENO: a step before this check failed"
ui "2,j,k,3,4,u,n,1,c" >/dev/null && ok "keys on empty screens do nothing harmful" || fail "line $LINENO: a step before this check failed"
ui "" 40x10 | head -1 | has "copse" && ok "a small terminal still draws" || fail "line $LINENO: a step before this check failed"
for size in 1x1 5x2 30x4 20x7; do for keys in "c" "c,tab,text:x,enter,enter" "?" "x" "enter" "2" "3" "4,u"; do
  cps ui --replay "$keys" --size $size >/dev/null 2>&1 || fail "the screen crashed at $size after $keys"
done; done
ok "tiny terminals never crash it (every box, down to 1x1)"
cps 2>&1 | has "Usage:" && ok "copse on its own, not in a terminal, prints the help" || fail "line $LINENO: a step before this check failed"

echo
echo "All $pass checks passed."
