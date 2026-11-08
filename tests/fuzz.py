#!/usr/bin/env python3
"""Randomized stress test: copse's merge, rebase and cherry-pick against git's.

Each round builds the same random history in a git repository and a copse one:
a base commit, then two branches that each edit, insert and delete lines, add,
delete and rename files (sometimes editing them too). Then it merges (and in
other rounds rebases or cherry-picks) in both, and compares:

  - both clean: every file must be identical, byte for byte;
  - both conflicted: the files without conflict markers must be identical,
    and the same files must be conflicted;
  - one clean, one conflicted: counted separately ("disagreements"); the two
    line-merge algorithms aren't identical, so a few are expected, but each is
    printed so it can be looked at.

Folder moves (git's directory rename detection) are followed as git follows them.
Rounds where git reports a folder move are still counted on their own line, so a
regression there is easy to spot.

Usage: tests/fuzz.py [rounds] [seed] [work_dir]
Exits 1 if any round produced different files where both tools succeeded.
"""
import os
import random
import shutil
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
COPSE = os.path.join(HERE, "..", "target", "release", "copse")
ROUNDS = int(sys.argv[1]) if len(sys.argv) > 1 else 200
SEED = int(sys.argv[2]) if len(sys.argv) > 2 else 1
WORK = os.path.abspath(sys.argv[3]) if len(sys.argv) > 3 else tempfile.mkdtemp()

ENV = dict(
    os.environ,
    GIT_CONFIG_GLOBAL="/dev/null", GIT_CONFIG_NOSYSTEM="1",
    GIT_AUTHOR_NAME="Fuzz", GIT_AUTHOR_EMAIL="f@example.com",
    GIT_COMMITTER_NAME="Fuzz", GIT_COMMITTER_EMAIL="f@example.com",
    COPSE_AUTHOR="Fuzz", COPSE_EMAIL="f@example.com", COPSE_FSYNC="0", COPSE_EDITOR="true",
    HOME=os.path.join(WORK, "home"),
)
WORDS = ["alpha", "beta", "gamma", "delta", "omega", "lambda", "sigma", "kappa", "theta", "zeta"]


def sh(args, cwd, check=True):
    r = subprocess.run(args, cwd=cwd, env=ENV, capture_output=True, text=True)
    if check and r.returncode != 0:
        raise RuntimeError(f"{' '.join(args)} failed in {cwd}:\n{r.stdout}\n{r.stderr}")
    return r


def line(rng):
    return " ".join(rng.choice(WORDS) for _ in range(rng.randint(2, 6)))


def new_file(rng):
    return [f"{i}: {line(rng)}" for i in range(rng.randint(8, 40))]


class Tree:
    """The files of one branch, as lists of lines."""

    def __init__(self, files):
        self.files = {p: list(ls) for p, ls in files.items()}

    def copy(self):
        return Tree(self.files)

    def mutate(self, rng, ops):
        for _ in range(ops):
            kind = rng.choices(["edit", "insert", "delete", "add", "remove", "rename", "rename_edit"], [5, 3, 2, 1, 0.5, 0.7, 0.7])[0]
            paths = sorted(self.files)
            if not paths:
                kind = "add"
            if kind == "add":
                d = rng.choice(["", "src/", "lib/", "docs/deep/"])
                self.files[f"{d}f{rng.randint(0, 9999)}.txt"] = new_file(rng)
                continue
            p = rng.choice(paths)
            ls = self.files[p]
            if kind == "edit" and ls:
                ls[rng.randrange(len(ls))] = "edited " + line(rng)
            elif kind == "insert":
                ls.insert(rng.randint(0, len(ls)), "new " + line(rng))
            elif kind == "delete" and ls:
                start = rng.randrange(len(ls))
                del ls[start:start + rng.randint(1, 3)]
            elif kind == "remove" and len(paths) > 1:
                del self.files[p]
            elif kind in ("rename", "rename_edit"):
                d = rng.choice(["", "moved/", "src/", "other/place/"])
                to = f"{d}{os.path.basename(p) if rng.random() < 0.5 else 'r' + str(rng.randint(0, 999)) + '.txt'}"
                if to in self.files:
                    continue
                self.files[to] = self.files.pop(p)
                if kind == "rename_edit" and self.files[to]:
                    self.files[to][rng.randrange(len(self.files[to]))] = "renamed+edited " + line(rng)

    def write(self, root):
        for dp, dns, fns in os.walk(root):
            if ".git" in dp.split(os.sep) or ".copse" in dp.split(os.sep):
                continue
            for f in fns:
                os.remove(os.path.join(dp, f))
        for p, ls in self.files.items():
            full = os.path.join(root, p)
            os.makedirs(os.path.dirname(full), exist_ok=True)
            with open(full, "w") as f:
                f.write("".join(l + "\n" for l in ls))
        # Tidy empty folders (neither tool tracks them).
        for dp, dns, fns in sorted(os.walk(root), key=lambda t: -len(t[0])):
            if dp != root and not os.listdir(dp) and ".git" not in dp and ".copse" not in dp:
                os.rmdir(dp)


def snapshot(root):
    out = {}
    for dp, dns, fns in os.walk(root):
        dns[:] = [d for d in dns if d not in (".git", ".copse")]
        for f in fns:
            full = os.path.join(dp, f)
            with open(full, "rb") as fh:
                out[os.path.relpath(full, root)] = fh.read()
    return out


def conflicted(files):
    return {p for p, data in files.items() if b"<<<<<<<" in data}


def setup(rng, rnd):
    g, c = os.path.join(WORK, f"r{rnd}-git"), os.path.join(WORK, f"r{rnd}-copse")
    for d in (g, c):
        shutil.rmtree(d, ignore_errors=True)
        os.makedirs(d)
    sh(["git", "init", "-q", "-b", "main"], g)
    sh([COPSE, "init"], c)
    base = Tree({f"{d}f{i}.txt": new_file(rng) for i, d in enumerate(rng.choices(["", "src/", "lib/"], k=rng.randint(2, 6)))})
    return g, c, base


def commit(tree, g, c, msg):
    tree.write(g)
    sh(["git", "add", "-A"], g)
    sh(["git", "commit", "-q", "--allow-empty", "-m", msg], g)
    tree.write(c)
    sh([COPSE, "commit", "-m", msg], c, check=False)  # "Nothing to commit" is fine


def branch_both(name, g, c, start="main"):
    sh(["git", "branch", name, start], g)
    sh([COPSE, "branch", name, start], c)


def checkout_both(name, g, c):
    sh(["git", "checkout", "-q", name], g)
    sh([COPSE, "checkout", name], c)


def compare(kind, rnd, g_ok, c_ok, g, c, stats, detail, git_out=""):
    gs, cs = snapshot(g), snapshot(c)
    if g_ok and c_ok:
        if gs != cs:
            stats["different"] += 1
            only_g, only_c = set(gs) - set(cs), set(cs) - set(gs)
            diff = [p for p in set(gs) & set(cs) if gs[p] != cs[p]]
            print(f"  DIFFERENT ({kind}, round {rnd}): only in git {sorted(only_g)}, only in copse {sorted(only_c)}, contents differ {sorted(diff)}\n    {detail}")
        else:
            stats["same"] += 1
    elif not g_ok and not c_ok:
        gc, cc = conflicted(gs), conflicted(cs)
        clean_g = {p: d for p, d in gs.items() if p not in gc}
        clean_c = {p: d for p, d in cs.items() if p not in cc}
        if gc == cc and clean_g == clean_c:
            stats["both_conflict"] += 1
        elif "CONFLICT (file location)" in git_out:
            stats["dir_rename"] += 1
            print(f"  folder move ({kind}, round {rnd}): git {sorted(gc)} copse {sorted(cc)}; other files differ: "
                  f"{sorted(p for p in set(clean_g) | set(clean_c) if clean_g.get(p) != clean_c.get(p))}\n    {detail}")
        else:
            stats["conflict_mismatch"] += 1
            print(f"  conflicts differ ({kind}, round {rnd}): git {sorted(gc)} copse {sorted(cc)}; "
                  f"other files differ: {sorted(p for p in set(clean_g) | set(clean_c) if clean_g.get(p) != clean_c.get(p))}\n    {detail}")
    elif "CONFLICT (file location)" in git_out:
        stats["dir_rename"] += 1  # git moved a file into a folder the other side renamed
    else:
        stats["disagree"] += 1
        print(f"  disagreement ({kind}, round {rnd}): git {'clean' if g_ok else 'conflict'}, copse {'clean' if c_ok else 'conflict'}\n    {detail}")


def run_round(rnd, rng, stats):
    g, c, base = setup(rng, rnd)
    commit(base, g, c, "base")
    a, b = base.copy(), base.copy()
    a.mutate(rng, rng.randint(1, 6))
    b.mutate(rng, rng.randint(1, 6))
    branch_both("theirs", g, c)
    commit(a, g, c, "ours")
    checkout_both("theirs", g, c)
    commit(b, g, c, "theirs")
    checkout_both("main", g, c)
    kind = rng.choice(["merge", "merge", "rebase", "cherry-pick"])
    detail = f"reproduce: tests/fuzz.py with seed {SEED}, round {rnd} (dirs {g}, {c})"
    if kind == "merge":
        gr = sh(["git", "merge", "--no-edit", "theirs"], g, check=False)
        g_ok, git_out = gr.returncode == 0, gr.stdout + gr.stderr
        r = sh([COPSE, "merge", "theirs"], c, check=False)
        c_ok = r.returncode == 0 and "conflict" not in r.stdout
    elif kind == "rebase":
        gr = sh(["git", "rebase", "theirs"], g, check=False)
        g_ok, git_out = gr.returncode == 0, gr.stdout + gr.stderr
        r = sh([COPSE, "rebase", "theirs"], c, check=False)
        c_ok = r.returncode == 0 and "Conflict" not in r.stdout
    else:
        gr = sh(["git", "cherry-pick", "theirs"], g, check=False)
        g_ok, git_out = gr.returncode == 0, gr.stdout + gr.stderr
        if "is now empty" in git_out:
            # The change is already there: git stops on the empty commit, copse skips it. Same files.
            sh(["git", "cherry-pick", "--skip"], g, check=False)
            g_ok = True
        r = sh([COPSE, "cherry-pick", "theirs"], c, check=False)
        c_ok = r.returncode == 0 and "Conflict" not in r.stdout
    compare(kind, rnd, g_ok, c_ok, g, c, stats, detail, git_out)
    if g_ok and c_ok:
        shutil.rmtree(g)
        shutil.rmtree(c)


def main():
    os.makedirs(ENV["HOME"], exist_ok=True)
    rng = random.Random(SEED)
    stats = dict(same=0, both_conflict=0, dir_rename=0, different=0, conflict_mismatch=0, disagree=0)
    for rnd in range(ROUNDS):
        run_round(rnd, rng, stats)
    total = sum(stats.values())
    print(f"\n{total} rounds (seed {SEED}): {stats['same']} identical, {stats['both_conflict']} conflicted alike, "
          f"{stats['dir_rename']} where git moved a file into a renamed folder, "
          f"{stats['disagree']} clean/conflict disagreements, {stats['conflict_mismatch']} conflicted differently, "
          f"{stats['different']} DIFFERENT results")
    sys.exit(1 if stats["different"] else 0)


if __name__ == "__main__":
    main()
