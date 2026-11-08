#!/usr/bin/env python3
"""Benchmark copse against git on a synthetic repository.

Usage: python3 bench/bench.py [work_dir] [num_files]
Builds copse in release mode first: `cargo build --release`.
"""
import os
import random
import shutil
import statistics
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
COPSE = os.path.join(HERE, "..", "target", "release", "copse")
WORK = os.path.abspath(sys.argv[1] if len(sys.argv) > 1 else os.path.join(HERE, "..", "target", "bench"))
NUM_FILES = int(sys.argv[2]) if len(sys.argv) > 2 else 50_000
FILES_PER_DIR = 20
RUNS = 5

# Run git with no user or system config, so results reflect stock git.
GIT_ENV = dict(os.environ, GIT_CONFIG_GLOBAL="/dev/null", GIT_CONFIG_NOSYSTEM="1",
               GIT_AUTHOR_NAME="bench", GIT_AUTHOR_EMAIL="b@x", GIT_COMMITTER_NAME="bench",
               GIT_COMMITTER_EMAIL="b@x")


def run(cmd, cwd, shell=True):
    subprocess.run(cmd, cwd=cwd, shell=shell, check=True, env=GIT_ENV,
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def timed(cmd, cwd):
    start = time.perf_counter()
    run(cmd, cwd)
    return time.perf_counter() - start


def median_of(cmd, cwd, runs=RUNS):
    run(cmd, cwd)  # warm-up
    return statistics.median(timed(cmd, cwd) for _ in range(runs))


def generate(root):
    rng = random.Random(42)
    words = [f"word{i}" for i in range(2000)]
    for i in range(NUM_FILES):
        d = os.path.join(root, f"pkg{i // (FILES_PER_DIR * 10):03d}", f"mod{i // FILES_PER_DIR:05d}")
        os.makedirs(d, exist_ok=True)
        lines = (" ".join(rng.choices(words, k=10)) for _ in range(rng.randint(20, 120)))
        with open(os.path.join(d, f"file{i:06d}.txt"), "w") as f:
            f.write("\n".join(lines) + "\n")


def touch_files(root, count, tag):
    rng = random.Random(tag)
    for i in rng.sample(range(NUM_FILES), count):
        p = os.path.join(root, f"pkg{i // (FILES_PER_DIR * 10):03d}", f"mod{i // FILES_PER_DIR:05d}", f"file{i:06d}.txt")
        with open(p, "a") as f:
            f.write(f"edit {tag}\n")


def main():
    shutil.rmtree(WORK, ignore_errors=True)
    src = os.path.join(WORK, "src")
    print(f"Generating {NUM_FILES:,} files in {WORK} ...")
    generate(src)
    size = sum(os.path.getsize(os.path.join(dp, f)) for dp, _, fs in os.walk(src) for f in fs)
    print(f"  {size / 1e6:.0f} MB of text\n")

    tools = {
        "git": dict(
            init="git init -q -b main && git add -A && git commit -q -m init",
            status="git status --porcelain",
            commit="git add -A && git commit -q -m change",
            back="git checkout -q HEAD~1",
            forward="git checkout -q main",
        ),
        "copse": dict(
            init=f"'{COPSE}' init && '{COPSE}' commit -m init",
            status=f"'{COPSE}' status",
            commit=f"'{COPSE}' commit -m change",
            back=f"'{COPSE}' checkout HEAD~1",
            forward=f"'{COPSE}' checkout main",
        ),
    }

    results = {}
    for name, c in tools.items():
        repo = os.path.join(WORK, name)
        shutil.copytree(src, repo)
        r = results[name] = {}
        r["first commit (all files)"] = timed(c["init"], repo)
        r["status, nothing changed"] = median_of(c["status"], repo)
        touch_files(repo, 100, "a")
        r["status, 100 files changed"] = median_of(c["status"], repo)
        r["commit 100 changed files"] = timed(c["commit"], repo)
        touch_files(repo, 5000, "b")
        r["commit 5,000 changed files"] = timed(c["commit"], repo)
        times = []
        for _ in range(3):
            times.append(timed(c["back"], repo) + timed(c["forward"], repo))
        r["checkout back and forth"] = statistics.median(times) / 2
        r["repository size (MB)"] = sum(
            os.path.getsize(os.path.join(dp, f))
            for dp, _, fs in os.walk(os.path.join(repo, ".git" if name == "git" else ".copse")) for f in fs
        ) / 1e6

    print(f"{'operation':<30}{'git':>10}{'copse':>10}{'speedup':>10}")
    for op in results["git"]:
        g, t = results["git"][op], results["copse"][op]
        if op.startswith("repository"):
            print(f"{op:<30}{g:>9.0f} {t:>9.0f} {'':>10}")
        else:
            print(f"{op:<30}{g * 1000:>8.0f}ms{t * 1000:>8.0f}ms{g / t:>9.1f}x")


if __name__ == "__main__":
    main()
