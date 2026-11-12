<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/logo/copse-leaf-master-dark.svg">
    <img src="assets/logo/copse-leaf-master.svg" alt="The copse logo: a leaf whose veins are a commit graph" width="160">
  </picture>
</p>

# copse

A fast version control system written in Rust that works with git servers. You can clone from GitHub, commit, branch, merge and push, and your teammates using git won't notice anything different. It also has no staging area, an undo for everything, and a built-in terminal interface.

copse is versioned with itself and pushed with copse to [Copse-SCM/copse](https://github.com/Copse-SCM/copse). It runs on macOS and Linux, and every push is built and tested on both.

## Install

With [Homebrew](https://brew.sh) (macOS or Linux):

```bash
brew install copse-scm/copse/copse            # the latest release
brew install copse-scm/copse/copse-nightly    # or the latest daily build of main
```

Or download the archive for your machine from the [latest release](https://github.com/Copse-SCM/copse/releases/latest) (macOS on Apple Silicon or Intel, Linux on x86_64), unpack it, and put `copse` on your `PATH`:

```bash
tar xzf copse-*.tar.gz && sudo mv copse-*/copse /usr/local/bin/
copse version
```

On a Mac, a download from the browser is quarantined, and since copse isn't signed with an Apple developer certificate yet, macOS refuses to open it until you clear that: `xattr -d com.apple.quarantine /usr/local/bin/copse`. (Downloads with `curl` or `gh release download` aren't quarantined.)

`SHA256SUMS` in the release lets you check the download (`shasum -a 256 -c SHA256SUMS --ignore-missing`). Or build it yourself:

```bash
cargo install --git https://github.com/Copse-SCM/copse     # or, from a checkout:
cargo build --release                                      # binary at target/release/copse
```

**Versions** are dates: `2026.11.0` is the first release of November 2026, `2026.11.1` the next. A [nightly build](https://github.com/Copse-SCM/copse/releases/tag/nightly) of main is published each day it changes, named like `2026.11.09-dev+abc1234` (the date and the commit).

## A quick tour

```bash
copse clone https://github.com/you/project.git   # or git@github.com:you/project.git
cd project
copse status                  # what changed, and where you stand against origin
copse commit -m "Fix login"   # no staging step: commits everything that changed
copse commit -p               # or pick changes one at a time (s splits, e edits)
copse push
copse undo                    # changed your mind? undo the last move (again to redo)
copse                         # the interactive screen
```

## What's different

- **Faster than git** at the everyday things: 3–4x faster `status`, 2.5–6x faster commits, and 4x faster checkouts on a 50,000-file repository. It uses BLAKE3 and zstd, does everything in parallel, and never reads files it knows haven't changed.
- **No staging area.** `commit` takes everything, or the paths you name, or the changes you pick with `-p`.
- **Undo for everything.** `copse undo` reverses the last commit, merge, rebase, pull or reset on your branch (run it again to redo). Anything that would throw work away keeps a backup first.
- **Works with git.** It speaks git's protocols over HTTPS and SSH, with signed commits, tags, submodules, LFS, and a read-only `.git` view so editors and git tools keep working. Merges, rebases and cherry-picks give the same results as git; 2,000 randomized stress rounds match it exactly.
- **A built-in terminal interface.** Run `copse` on its own to get these screens:
  - **Changes:** pick files, or single changes within a file, and commit.
  - **History:** the commit graph.
  - **Branches:** switch, merge, pull and push.
  - **Undo:** a timeline you can jump back through.
- **Careful with what it's handed.** Hostile file names, symlink tricks and option injection are refused, both on checkout and when objects arrive.

## Documentation

- **[The guide](docs/guide.md):** every command, merging, rebasing, remotes, GitHub, the interactive screen, the storage format, benchmarks and how it's tested.
- **[The roadmap](ROADMAP.md):** git parity command by command, and what's left. Windows isn't supported yet.

## Status

The first release is 2026.11.0, for people who like trying new tools. 1,056 end-to-end checks and a randomized stress test against git run on every push. It's still young, though, so keep a git clone of anything you can't afford to lose.

## License

copse is licensed under the [Apache License 2.0](LICENSE): you can use, change and share it, including in forks. Keep the [NOTICE](NOTICE) file so the original stays credited. The name "copse" and the logo aren't covered by the license, so a fork needs a name and logo of its own.
