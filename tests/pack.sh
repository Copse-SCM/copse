#!/usr/bin/env bash
# Packfile and gc tests: every commit must read back byte-for-byte identical after
# packing, transfers must work from packed repositories, and damage must be caught.
# Usage: tests/pack.sh [work_dir]     (build first: cargo build --release)
set -euo pipefail

COPSE="$(cd "$(dirname "$0")/.." && pwd)/target/release/copse"
WORK="${1:-$(mktemp -d)}"
rm -rf "$WORK" && mkdir -p "$WORK" && cd "$WORK"
cps() { "$COPSE" "$@"; }
export COPSE_AUTHOR="Test Person" COPSE_EMAIL="test@example.com"  # fixed identity, same on every machine
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1  # your own git settings (signing, identity) stay out of the tests
pass=0
ok() { echo "  ok: $1"; pass=$((pass + 1)); }
fail() { echo "  FAIL: $1"; exit 1; }
same() { [[ "$1" == "$2" ]] || fail "$3 (got '$1', want '$2')"; ok "$3"; }
fingerprint() { # hash of every file's path, mode and content (one process, so it stays fast)
  python3 - <<'PY'
import hashlib, os
h = hashlib.sha256()
for root, dirs, files in os.walk("."):
    dirs[:] = sorted(d for d in dirs if not (root == "." and d == ".copse"))
    for name in sorted(files):
        p = os.path.join(root, name)
        st = os.lstat(p)
        h.update(p.encode() + oct(st.st_mode).encode())
        h.update(os.readlink(p).encode() if os.path.islink(p) else open(p, "rb").read())
print(h.hexdigest()[:16])
PY
}

echo "== build a history"
mkdir repo && cd repo && cps init >/dev/null
python3 - <<'PY'
import os, random
r = random.Random(7)
os.makedirs("src/deep", exist_ok=True)
words = [f"w{i}" for i in range(500)]
for i in range(40):
    open(f"src/file{i}.txt", "w").write("\n".join(" ".join(r.choices(words, k=12)) for _ in range(200)) + "\n")
open("big.bin", "wb").write(r.randbytes(3 << 20))  # 3 MB incompressible
open("src/deep/notes.md", "w").write("# notes\n")
os.makedirs("many", exist_ok=True)
for i in range(600):  # enough objects that a clone arrives as a pack, not loose files
    open(f"many/f{i:03d}.txt", "w").write(f"small file {i}\n")
PY
ln -s src/deep/notes.md link
cps commit -m "c0" >/dev/null
for i in $(seq 1 15); do  # 15 edits to the same files: longer than the delta chain cap
  python3 - "$i" <<'PY'
import sys, random
i = int(sys.argv[1]); r = random.Random(i)
for f in (f"src/file{r.randrange(40)}.txt" for _ in range(5)):
    lines = open(f).read().splitlines(); lines[r.randrange(len(lines))] = f"edit {i}"
    open(f, "w").write("\n".join(lines) + "\n")
with open("src/deep/notes.md", "a") as f: f.write(f"- note {i}\n")
b = bytearray(open("big.bin", "rb").read()); b[i * 1000] ^= 0xFF; open("big.bin", "wb").write(b)
PY
  cps commit -m "c$i" >/dev/null
done
cp src/file0.txt /tmp/copse-pack-test-saved.txt
echo "changed" > src/file0.txt && cps commit -m "change file0" >/dev/null
cp /tmp/copse-pack-test-saved.txt src/file0.txt && cps commit -m "revert file0" >/dev/null  # revert: tempts a delta cycle
chmod +x src/file1.txt && cps commit -m "make file1 executable" >/dev/null
cps branch side >/dev/null && cps checkout side >/dev/null && echo side > side.txt && cps commit -m side >/dev/null && cps checkout main >/dev/null
ok "built $(cps log -n 100 | grep -c UTC) commits on main plus a side branch"

ids=$(cps log -n 100 | grep UTC | cut -c1-10; echo side)
mkdir -p ../before
for id in $ids; do cps checkout -f "$id" >/dev/null; fingerprint > "../before/$id"; done
cps checkout -f main >/dev/null

echo "== gc"
out=$(cps gc); echo "  $out"
[[ $(find .copse/objects -type f -not -path '*/pack/*' | wc -l) -eq 0 ]] && ok "no loose objects left" || fail "loose objects remain"
same "$(ls .copse/objects/pack | grep -c '\.idx$')" "1" "exactly one pack"
[[ "$out" == *"stored as deltas"* && "$out" != *"(0 stored"* ]] && ok "some objects stored as deltas" || fail "no deltas: $out"
same "$(cps gc)" "Already packed." "second gc has nothing to do"

echo "== every commit reads back identically"
bad=0
for id in $ids; do cps checkout -f "$id" >/dev/null; [[ "$(fingerprint)" == "$(cat "../before/$id")" ]] || { echo "  mismatch at $id"; bad=1; }; done
cps checkout -f main >/dev/null
[[ $bad -eq 0 ]] && ok "all $(echo $ids | wc -w | tr -d ' ') snapshots match byte-for-byte" || fail "snapshot mismatch"
same "$(cps status | tail -1)" "Nothing changed." "status clean after gc"
cps diff >/dev/null && cps log -n 3 >/dev/null && cps checkout "$(cps log -n 1 | head -1 | cut -c1-6)" >/dev/null && cps checkout main >/dev/null && ok "diff, log, short-id checkout work on packed objects" || fail "line $LINENO: a step before this check failed"

echo "== new work on top of a pack, then gc again"
echo more >> src/file2.txt && cps commit -m "after gc" >/dev/null
cps gc >/dev/null && same "$(ls .copse/objects/pack | grep -c '\.idx$')" "1" "repack merges into one pack" || fail "line $LINENO: a step before this check failed"
old_tip=$(echo $ids | cut -d' ' -f1)
cps checkout -f HEAD~1 >/dev/null && same "$(fingerprint)" "$(cat "../before/$old_tip")" "previous commit still intact after second gc" || fail "line $LINENO: a step before this check failed"
cps checkout -f main >/dev/null

echo "== transfers from and into packs"
cd .. && cps init --bare remote.copse >/dev/null
cd repo && cps remote add origin ../remote.copse >/dev/null && cps push >/dev/null && cps push origin side >/dev/null && ok "push from a delta-packed repo" || fail "line $LINENO: a step before this check failed"
cd .. && cps clone remote.copse copy >/dev/null
same "$(ls copy/.copse/objects/pack | grep -c '\.idx$')" "1" "large clone arrives as one pack"
same "$(find copy/.copse/objects -type f -not -path '*/pack/*' | wc -l | tr -d ' ')" "0" "and no loose objects"
cd copy && same "$(fingerprint)" "$(cd ../repo && fingerprint)" "clone matches the original" || fail "line $LINENO: a step before this check failed"
echo tiny > tiny.txt && cps commit -m tiny >/dev/null && cps push >/dev/null
cd ../repo && cps pull >/dev/null && [[ -f tiny.txt ]] && ok "small fetch still works (stored loose)" || fail "line $LINENO: a step before this check failed"
cps gc >/dev/null && (cd ../remote.copse && cps gc >/dev/null) && ok "gc works in a bare repository" || fail "line $LINENO: a step before this check failed"
cd .. && rm -rf copy2 && cps clone remote.copse copy2 >/dev/null && cd copy2 && same "$(fingerprint)" "$(cd ../repo && fingerprint)" "clone from a gc'd bare repo matches" || fail "line $LINENO: a step before this check failed"

echo "== damage is caught"
cd ../repo && idx=$(ls .copse/objects/pack/*.idx) && cp "$idx" /tmp/copse-idx-backup
printf 'X' | dd of="$idx" bs=1 seek=100 conv=notrunc 2>/dev/null
out=$(cps log -n 1 2>&1 || true)
[[ "$out" == *"corrupt"* ]] && ok "corrupted pack index is reported" || fail "corrupt idx not reported: $out"
cp /tmp/copse-idx-backup "$idx"
pack=$(ls .copse/objects/pack/*.pack)
size=$(stat -f %z "$pack"); printf '\x00\x00\x00\x00\x00\x00\x00\x00' | dd of="$pack" bs=1 seek=$((size / 2)) conv=notrunc 2>/dev/null
out=$(cd .. && rm -rf copy3 && cps clone repo copy3 2>&1 || true)
# Depending on where the damage lands, either the sender notices (and says so) or
# the receiver's hash check does. Either way the cause must be shown.
[[ "$out" == *"copse (remote):"* || "$out" == *"does not match"* ]] && ok "corrupted pack data is reported with its cause, not silently copied" || fail "corrupt pack not caught: $out"
[[ ! -e ../copy3 ]] && ok "failed clone leaves nothing behind" || fail "partial clone left behind"

rm -f /tmp/copse-pack-test-saved.txt /tmp/copse-idx-backup
echo
echo "All $pass checks passed."
