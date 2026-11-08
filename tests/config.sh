#!/usr/bin/env bash
# Settings (copse config), aliases, ignore files and hooks.
# Usage: tests/config.sh [work_dir]     (build first: cargo build --release)
set -euo pipefail

COPSE="$(cd "$(dirname "$0")/.." && pwd)/target/release/copse"
WORK="${1:-$(mktemp -d)}"
rm -rf "$WORK" && mkdir -p "$WORK" && cd "$WORK"
cps() { "$COPSE" "$@"; }
# A home of our own, so your real settings can't leak in (or be changed).
export HOME="$WORK/home" XDG_CONFIG_HOME="$WORK/home/.config"
unset COPSE_AUTHOR COPSE_EMAIL COPSE_CONFIG_GLOBAL COPSE_EDITOR VISUAL EDITOR
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1
mkdir -p "$HOME"
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
author() { cps show "${1:-HEAD}" | grep '^Author:' | sed 's/^Author: *//'; }
hook() { mkdir -p "$(dirname "$1")" && cat > "$1" && chmod +x "$1"; }

echo "== copse config"
mkdir r && cd r && cps init >/dev/null
cps config | has "No settings yet" && ok "no settings to begin with, and a list of what can be set" || fail "line $LINENO: a step before this check failed"
cps config --global user.name "Global Name" >/dev/null && cps config --global user.email global@example.com >/dev/null
echo a > a && cps commit -m one >/dev/null
same "$(author)" "Global Name <global@example.com>" "global settings name the author"
[[ -f "$HOME/.config/copse/config" ]] && ok "they're kept in ~/.config/copse/config" || fail "line $LINENO: a step before this check failed"
cps config user.name "Repo Name" >/dev/null
echo b > b && cps commit -m two >/dev/null
same "$(author)" "Repo Name <global@example.com>" "a repository's own setting wins over the global one"
echo c > c && COPSE_AUTHOR="Env Name" cps commit -m three >/dev/null
same "$(author)" "Env Name <global@example.com>" "and \$COPSE_AUTHOR wins over both"
same "$(cps config user.name)" "Repo Name" "reading a setting gives the one that applies"
same "$(cps config --global user.name)" "Global Name" "--global reads the global file"
cps config user.name >/dev/null 2>&1 && same "$(cps config nope.key; echo "exit $?")" "exit 1" "an unset setting prints nothing and exits 1" || fail "line $LINENO: a step before this check failed"
cps config --list | has "user.name=Repo Name  (repository)" && cps config --list | has "user.name=Global Name  (global)" && ok "--list shows every setting and where it's from" || fail "line $LINENO: a step before this check failed"
cps config --unset user.name >/dev/null && same "$(cps config user.name)" "Global Name" "--unset removes it, so the global one applies again" || fail "line $LINENO: a step before this check failed"
expect_fail "unsetting what isn't set is reported" "isn't set" cps config --unset user.name
expect_fail "a setting needs a section" "needs a section" cps config name x
cps config core.excludesFile "~/my ignore" >/dev/null && same "$(cps config core.excludesfile)" "~/my ignore" "names ignore case; values with spaces are kept" || fail "line $LINENO: a step before this check failed"
cps config --unset core.excludesFile >/dev/null
cps config made.up value 2>&1 | has "isn't a setting copse uses" && ok "unknown settings are saved with a note" || fail "line $LINENO: a step before this check failed"
printf '# my notes\n' | cat - .copse/config > t && mv t .copse/config && cps config user.email repo@example.com >/dev/null
head -1 .copse/config | has "# my notes" && ok "editing keeps comments and everything else in the file" || fail "line $LINENO: a step before this check failed"
cd .. && expect_fail "outside a repository, settings need --global" "use --global" cps config user.name x && cd r

echo "== aliases"
cps config alias.s status >/dev/null && cps config alias.last "log -n 1 --oneline" >/dev/null
same "$(cps s | head -1)" "$(cps status | head -1)" "an alias runs the command it stands for"
same "$(cps last)" "$(cps log -n 1 --oneline)" "with its arguments"
cps config alias.save 'commit -m "made by alias"' >/dev/null && echo d > d && cps save >/dev/null && same "$(cps log -n 1 --oneline | sed 's/^[0-9a-f]* ([^)]*) //')" "made by alias" "quoted arguments stay together" || fail "line $LINENO: a step before this check failed"
cps config alias.l "last" >/dev/null && same "$(cps l)" "$(cps last)" "aliases can use other aliases" || fail "line $LINENO: a step before this check failed"
cps config alias.loop1 loop2 >/dev/null && cps config alias.loop2 loop1 >/dev/null
expect_fail "aliases that refer to each other in a loop are caught" "loop" cps loop1
cps config alias.where '!echo "$(basename "$(pwd)")"' >/dev/null && mkdir -p sub && same "$(cd sub && cps where x y)" "r x y" "a ! alias runs a shell command from the top of the repository, with the arguments" || fail "line $LINENO: a step before this check failed"
cps config alias.no '!exit 3' >/dev/null; set +e; cps no; code=$?; set -e
same "$code" "3" "and copse exits with its status"
expect_fail "an alias can't replace a command" "already a copse command" cps config alias.log x
cps config --global alias.hello '!echo hello' >/dev/null && same "$(cps hello)" "hello" "global aliases work too" || fail "line $LINENO: a step before this check failed"
expect_fail "unknown commands are still unknown" "unrecognized subcommand" cps nonsense

echo "== ignore files"
mkdir -p "$HOME/.config/copse" && printf '*.log\nbuild/\n' > "$HOME/.config/copse/ignore"
echo x > debug.log && mkdir -p build && echo o > build/out.o && echo y > keep.txt
same "$(cps status | grep -E '^  [AMD] ' | sed 's/^ *//' | tr '\n' ',')" "A keep.txt," "~/.config/copse/ignore applies to every repository"
echo mine > notes.txt && echo notes.txt > .copse/exclude
same "$(cps status | grep -E '^  [AMD] ' | sed 's/^ *//' | tr '\n' ',')" "A keep.txt," ".copse/exclude ignores files in just this repository, without committing a rule"
printf '*.txt\n' > "$WORK/other-ignore" && cps config --global core.excludesFile "$WORK/other-ignore" >/dev/null
same "$(cps status | grep -E '^  [AMD] ' | sed 's/^ *//' | tr '\n' ',')" "A build/out.o,A debug.log," "core.excludesFile points at a different ignore file instead"
cps clean -x | has "keep.txt" && ok "clean -x sees those files as ignored" || fail "line $LINENO: a step before this check failed"
cps config --global --unset core.excludesFile >/dev/null && rm -rf debug.log build notes.txt && cps commit -m "keep" >/dev/null

echo "== hooks"
hook .copse/hooks/pre-commit <<'EOF'
#!/bin/sh
if grep -rq "DO NOT COMMIT" --exclude-dir=.copse .; then echo "found DO NOT COMMIT" >&2; exit 1; fi
EOF
echo "DO NOT COMMIT" > wip.txt
expect_fail "a failing pre-commit hook stops the commit" "the pre-commit hook stopped the commit (exit 1)" cps commit -m wip
cps status | has "A wip.txt" && ok "and nothing is committed" || fail "line $LINENO: a step before this check failed"
cps commit -n -m "wip anyway" >/dev/null && same "$(cps log -n 1 --oneline | sed 's/^[0-9a-f]* ([^)]*) //')" "wip anyway" "--no-verify skips it" || fail "line $LINENO: a step before this check failed"
hook .copse/hooks/pre-commit <<'EOF'
#!/bin/sh
for f in *.txt; do tr a-z A-Z < "$f" > "$f.tmp" && mv "$f.tmp" "$f"; done
EOF
echo lower > fmt.txt && cps commit -m "formatted" >/dev/null && same "$(cps show HEAD | grep '^+LOWER')" "+LOWER" "a pre-commit hook can tidy files, and the commit takes the tidied version" || fail "line $LINENO: a step before this check failed"
rm .copse/hooks/pre-commit
hook .copse/hooks/commit-msg <<'EOF'
#!/bin/sh
grep -qE '^[A-Z]+-[0-9]+' "$1" || { echo "messages must start with a ticket, like ABC-12" >&2; exit 1; }
printf '\nReviewed-by: hook\n' >> "$1"
EOF
echo e > e
expect_fail "commit-msg can reject a message" "the commit-msg hook stopped the commit" cps commit -m "no ticket"
cps commit -m "ABC-12 with ticket" >/dev/null && cps log -n 1 | has "Reviewed-by: hook" && ok "and can add to the message" || fail "line $LINENO: a step before this check failed"
cps commit --no-verify -m "skip it" >/dev/null 2>&1 || true
rm .copse/hooks/commit-msg
hook .copse/hooks/post-commit <<'EOF'
#!/bin/sh
echo "committed" >> ../post-commit.log; exit 7
EOF
echo f > f && cps commit -m "post" >/dev/null && same "$(cat ../post-commit.log)" "committed" "post-commit runs after committing (its exit status doesn't matter)" || fail "line $LINENO: a step before this check failed"
rm .copse/hooks/post-commit
hook .copse/hooks/post-checkout <<'EOF'
#!/bin/sh
echo "$3 ${1%"${1#??????}"} ${2%"${2#??????}"}" >> ../checkout.log
EOF
before=$(cps log -n 1 --oneline | cut -c1-6)
cps branch other >/dev/null && cps checkout other >/dev/null
same "$(cat ../checkout.log)" "1 $before $before" "post-checkout gets the old and new commits"
rm .copse/hooks/post-checkout
echo g > g && cps commit -m "on other" >/dev/null && cps checkout main >/dev/null
hook .copse/hooks/post-merge <<'EOF'
#!/bin/sh
echo "merged $1" >> ../merge.log
EOF
cps merge other >/dev/null && same "$(cat ../merge.log)" "merged 0" "post-merge runs after a merge" || fail "line $LINENO: a step before this check failed"
rm .copse/hooks/post-merge
hook .copse/hooks/pre-rebase <<'EOF'
#!/bin/sh
[ "$2" = "protected" ] && { echo "don't rebase protected" >&2; exit 1; }; exit 0
EOF
cps branch protected HEAD~1 >/dev/null && cps checkout protected >/dev/null && echo p > p && cps commit -m p >/dev/null
expect_fail "pre-rebase can refuse a rebase" "the pre-rebase hook stopped the rebase" cps rebase main
cps rebase --no-verify main | has "Rebased" && ok "--no-verify skips it" || fail "line $LINENO: a step before this check failed"
cps checkout main >/dev/null && rm .copse/hooks/pre-rebase

cd .. && cps init --bare server.git >/dev/null && cd r && cps remote add origin ../server.git >/dev/null
hook .copse/hooks/pre-push <<'EOF'
#!/bin/sh
echo "$1 $(basename "$2")" > ../push.log; cat >> ../push.log
grep -q "^main " ../push.log && [ -f ../allow-push ] || exit 1
EOF
expect_fail "pre-push can stop a push" "the pre-push hook stopped the push" cps push origin main
head_id=$(cps show | awk 'NR==1 {print $2}')
same "$(cat ../push.log)" "origin server.git
main $head_id" "it's told the remote and each branch with its commit"
touch ../allow-push && cps push origin main >/dev/null && ok "and lets it through when it exits 0" || fail "line $LINENO: a step before this check failed"
rm ../allow-push && cps push --no-verify origin other >/dev/null && ok "push --no-verify skips it" || fail "line $LINENO: a step before this check failed"
rm .copse/hooks/pre-push
mkdir -p githooks && hook githooks/post-commit <<'EOF'
#!/bin/sh
echo shared > ../shared-hook.log
EOF
cps config core.hooksPath githooks >/dev/null && echo h > h && cps commit -m "shared hooks" >/dev/null && [[ -f ../shared-hook.log ]] && ok "core.hooksPath uses hooks from a folder in the project, so a team can share them" || fail "line $LINENO: a step before this check failed"
printf '#!/bin/sh\nexit 1\n' > githooks/pre-commit
echo i > i && out=$(cps commit -m "not executable" 2>&1) && [[ "$out" == *"isn't executable"* ]] && ok "a hook that isn't executable is skipped with a hint" || fail "line $LINENO: ${out:-a step before this check failed}"

echo "== a bare copse repository with settings is still copse, not git"
(cd ../server.git && cps config user.name "Server" >/dev/null)
cd .. && rm -rf clone2 && cps clone server.git clone2 >/dev/null && [[ -d clone2/.copse ]] && ok "cloning it still uses copse's own protocol" || fail "line $LINENO: a step before this check failed"

echo
echo "All $pass checks passed."
