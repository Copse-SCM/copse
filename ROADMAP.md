# copse roadmap

How copse compares with git, and the plan for what to build next.

Status: ✅ done · 🟡 partial · ❌ not yet · ➖ not needed by design

## 1. GitHub compatibility (in progress)

**Goal:** keep copse's own fast storage locally, but speak git's real protocol, so copse can push to and pull from GitHub, GitLab or any git server. GitHub sees an ordinary git client. Meta's Sapling and git-cinnabar work the same way.

**How it works:**
- **Translation.** Every copse commit, folder and file is translated into git's exact byte format, with its git (SHA-1) id.
- **An id map.** A permanent copse id ↔ git id map (`.copse/gitmap`) means each object is translated once, so later pushes only handle new work.
- **Exact round trips.** Commits must survive a round trip byte for byte. A commit pulled from GitHub and pushed back must keep its git id, or the history diverges from everyone else's.

| Step | What | Status |
|---|---|---|
| 1 | Commits store the full git identity: name, email, timezone, a separate committer, and extra git headers kept verbatim. Identity comes from `$COPSE_AUTHOR`/`$COPSE_EMAIL`, else git's own settings | ✅ |
| 2 | **Push to git servers over SSH**: git object encoding, the id map, the git pack format, and git's push conversation (`git-receive-pack`) | ✅ Verified against real git (`fsck --strict`, byte-for-byte clones, deterministic ids) locally and via a stand-in SSH; **pushed to github.com over HTTPS** (2026-11-07; see row 4). Pushing over SSH to github.com is still untried |
| 3 | **Clone, fetch and pull from git servers**: read git pack files, including git's delta format, and translate them into copse objects | ✅ Verified against real git: history made with git clones byte-for-byte and pushes back unchanged; two-way work with a git user through merges and rebases. **On github.com (2026-11-07):** pulled commits made on GitHub's side (as the web UI makes them) as a fast-forward, with an unrelated uncommitted edit, as a merge, with `pull --rebase`, and through a conflict that was resolved and pushed. GitHub's own commit ids were kept exactly (they appear unchanged as parents of copse's merges), and a fresh git clone passed `fsck --strict` with identical files and history |
| 4 | HTTPS remotes, using git's saved credentials or a GitHub token | ✅ Tested locally against `git http-backend` with sign-in (good and bad logins, private repositories), and **on github.com (2026-11-07)**: cloned a private repository with keychain credentials, committed an edit and a new file, pushed; GitHub's API showed the commit, a fresh git clone passed `fsck --strict` and matched copse's files byte for byte, and existing commit ids were unchanged |
| 5 | Optionally keep a `.git` view, so tools that read `.git` directly (VS Code's change markers and others) still work | ✅ `copse git-view on`: git reads copse's history, branches, tags and changes (checked with `git status`, `log`, `blame`, `fsck --strict`); read-only for git |

**Known limits of the git bridge:**
- **Submodules** round-trip exactly (git's 160000 entries and `.gitmodules`); `copse submodule update --init` checks them out.
- **Signed commits** (SSH or GPG) verify on git and GitHub. Rebased and amended commits lose their signature unless signed again; git behaves the same.
- **File names and commit messages that aren't UTF-8** round-trip byte for byte (same git ids); Latin-1 messages display readably.
- **Older "dumb" HTTP servers** (cgit): clone, fetch and pull (read-only by nature). Tested against git.symlinx.net (cgit): four repositories cloned with identical files and git ids.
- **Tags** travel to and from git servers (lightweight and annotated, exact round trips), and between copse repositories.
- **Pulling is faster than git's**: a fast-forward checks and writes only the files that changed (about 30 ms against git's 135 ms for a small pull in a 50,000-file repository).
- **Signed commits** pushed to github.com show as Verified (tested 2026-11-07). Pushing over SSH to github.com is untested (this machine's SSH key isn't registered with GitHub).

## 2. Feature parity with git

### Everyday work
| Feature | Status | Notes |
|---|---|---|
| `init`, `init --bare` | ✅ | |
| `status` | ✅ | Where you stand against origin (ahead/behind); `--porcelain` for scripts and editors |
| Staging area (`add`) | ➖ | `commit` snapshots everything that isn't ignored |
| Commit only some files (`commit <paths>`) | ✅ | Whole files or folders, or pick changes within files with `commit -p` (like `add -p`, with split and edit), or single lines in the TUI |
| `commit -m` | ✅ | |
| `commit --amend` | ✅ | Keeps author and date; with or without a new message; can take paths |
| `diff` | ✅ | Your files or any revision vs any revision, `A..B`, `--stat` (matches git's), `-- paths` |
| `log` | ✅ | Whole history, `--oneline`, `--graph` (matches git's drawing), `-p`, `--stat`, `--first-parent`, `--all`, `--author`, `--grep`, `-- paths`, `--pretty`/`--format` (git's named formats and placeholders), `--date` |
| `show` | ✅ | Commits and annotated tags, `--stat`, `REV:path` |
| Plumbing (`cat-file`, `rev-parse`, `ls-tree`, `ls-files`, `hash-object`) | ✅ | git's output formats; revisions with `^N`, `@`, `^{tree}`, `REV:path` |
| `mv` / `rm` | ➖ | Renames and deletions are picked up automatically |
| `.gitignore` | ✅ | Per-folder `.gitignore` and `.copseignore`, a global ignore file (`core.excludesFile`), and `.copse/exclude` |
| `clean` | ✅ | Lists by default; `-f` backs up to a stash first; `-x` for ignored files |

### Branches and history
| Feature | Status | Notes |
|---|---|---|
| `branch` list and create | ✅ | |
| `branch -d` / `-D` / `-m` / `-M` | ✅ | Unmerged branches protected; prints an undo command |
| `checkout` / `switch` | ✅ | Branches, ids, `HEAD~N`, `origin/main` |
| `restore` (files or folders, from any commit) | ✅ | Saves uncommitted edits as a stash before overwriting them |
| `reset` (default and `--hard`) | ✅ | Default keeps your files (git's `--soft`/`--mixed` are the same without a staging area); `--hard` backs up uncommitted work first; `undo` reverses either |
| `merge` | ✅ | `--no-ff`, `--ff-only`, `-m`; follows renames and folder moves, as git does (also in rebase, cherry-pick, revert, stash); fast-forwards touch only changed files. `-s ours`; `-X ours`/`theirs`/`no-renames` (also on pull, rebase, cherry-pick, revert) |
| `rebase`, `rebase -i` | ✅ | All six actions plus `exec`, `--onto`, `--autosquash`, `-x` |
| `cherry-pick`, `revert` | ✅ | Single commits and ranges, conflicts with continue/skip/abort, merges with `-m N` |
| `stash` | ✅ | Including only some files (`copse stash -- paths`) |
| `tag` (lightweight and annotated) | ✅ | Pushed to and fetched from git servers and copse remotes |
| `reflog`, plus `copse undo` | ✅ | Every HEAD and branch move recorded, including pushes servers receive; undo and redo |
| `grep` | ✅ | Regular expressions, your files or any revision |
| `blame` | ✅ | Follows merges, `-L`, marks uncommitted lines; matches `git blame` line for line |
| `bisect` | ✅ | `start`, `good`, `bad`, `skip`, `reset`, `run` (git's exit-code rules) |

### Remotes
| Feature | Status | Notes |
|---|---|---|
| copse ↔ copse: clone, fetch, pull (`--rebase`), push (`-f`) | ✅ | Local folders and SSH |
| Push to GitHub and other git servers | ✅ | See section 1; first push of a big history is slower than git (one-time translation), later pushes are on par |
| Clone, fetch, pull (`--rebase`) from GitHub and other git servers | ✅ | Section 1, step 3 |
| SSH and HTTPS to git servers, with git's saved logins | ✅ | Section 1, step 4 |
| Delete a remote branch (`push --delete`) | ✅ | git servers and copse remotes; fetch tidies away deleted branches |
| Shallow clone (`--depth`, `--unshallow`) | ✅ | Copse repositories and git servers (deepening, unshallowing, pushing from one); checked against GitHub |
| Worktrees | ✅ | `add`, `list`, `remove`, `prune`; one branch per worktree |
| Sparse checkout | ✅ | Folder-based (like git's cone mode), per worktree |
| Partial clone (`--filter=blob:none`) | ✅ | Copse repositories and git servers (contents fetched by git id when needed); checked against GitHub |
| Submodules | ✅ | Round trip with git, `submodule status/update/add`, `clone --recurse-submodules` |

### Storage, configuration and platform
| Feature | Status | Notes |
|---|---|---|
| Packfiles, `gc` with deltas | ✅ | Deltas only between versions of the same file |
| Removing unreachable objects | ✅ | `gc`, after a two-week grace period (`--prune`, `gc.pruneExpire`); reflog-reachable objects are kept |
| `fsck` | ✅ | Re-hashes every object, checks packs and that everything refs need is there |
| Crash safety | ✅ | Objects flushed before refs move; refs flushed fully and replaced atomically |
| Large files | ✅ | Streamed in 1 MB pieces locally (flat memory); transfers still hold one compressed file at a time |
| Large files (LFS) | ✅ | git-lfs's pointer format and batch API; `lfs track/pull/push/ls-files`; uploads before git pushes. Tested against a spec-following local server, not GitHub's |
| Identity | ✅ | `$COPSE_AUTHOR`/`$COPSE_EMAIL`, else copse's `user.name`/`user.email`, else git's, else `$USER` |
| Config file (`copse config`) | ✅ | `.copse/config` and `~/.config/copse/config`, in git's layout |
| Aliases | ✅ | Command aliases and `!` shell aliases |
| Hooks | ✅ | `pre-commit`, `commit-msg`, `post-commit`, `pre-push`, `pre-rebase`, `post-merge`, `post-checkout`; `core.hooksPath`; `--no-verify` |
| Signed commits and tags | ✅ | SSH and GPG, in git's format, following git's signing settings; every commit copse makes; `tag -s`, `verify-commit`, `verify-tag`. Verified on github.com |
| macOS, Linux | ✅ | CI (GitHub Actions) builds and runs every suite plus a stress test on both, for each push; first green run 2026-11-08 |
| Windows | ❌ | |

## 3. Plan: full feature parity first, then a TUI

Work through the remaining gaps in batches, testing each against real git wherever possible:

1. ✅ **Committing:** partial commit (`copse commit <paths>`), `commit --amend`, and full `log` (all of history, `--oneline`, `--graph`, `-p`, `--stat`, filtering by file, author or message, `--all`)
2. ✅ **History tools:** `cherry-pick`, `revert`, `clean`, `grep`
3. ✅ **Investigation:** `blame`, `bisect`
4. ✅ **Merge and rebase options:** `merge --no-ff`, `rebase --onto`, `--autosquash`, `exec`; stashing only some files
5. ✅ **Configuration:** a config file, aliases, a global ignore file, hooks
6. ✅ **Safety:** an `fsck` command, `gc` removing unreachable objects, crash safety (forcing writes to disk), streaming large files
7. **The big ones**, in three parts:
   - 7a ✅ tags between copse repositories, shallow clones, worktrees, sparse checkout
   - 7b ✅ signed commits, submodules, partial clones (shallow and partial clones from git servers remain: copse ids depend on full history)
   - 7c: ✅ LFS, ✅ a `.git` view for editors; Windows still to do (needs a Windows toolchain to check the build)

8. ✅ **Security:** file and folder names checked as git checks them (no `..`, no `.copse`/`.git` in any disguise, no repeated names), never writing through a symlink, no option injection through ssh hosts or submodule URLs; `tests/security.sh` attacks with hand-crafted repositories
9. ✅ **Hardening:** rename detection in merges; randomized stress tests against git (`tests/fuzz.py`: 2,000 rounds across eight seeds match git exactly, folder moves included); signed commits checked on github.com; fast-forward pulls that touch only changed files (now faster than git); signed tags; signing that follows git's settings

**Self-hosting (2026-11-07):** copse now versions its own source with copse, pushed to Copse-SCM/copse on GitHub (public since 2026-11-08, with secret scanning and push protection, CodeQL, Dependabot, private vulnerability reporting, and `main` protected from force-pushes and deletion). A fresh git clone of it builds from scratch and passes the tests.

Before relying on copse for real work: weeks of use on a backed-up project (this one, now). Still open: Windows, pushing to github.com over SSH, LFS against GitHub's server.

## 4. After parity: a built-in TUI ✅

`copse` on its own (or `copse ui`) opens a terminal interface (ratatui). Every action runs the real `copse` command and shows its output; `tests/tui.sh` drives it with `copse ui --replay` and checks each action with the command line:
- **Changes:** what changed, the diff alongside, pick files (or single changes within a file) to commit, write a summary and description.
- **History:** the commit graph; select a commit to see what it changed.
- **Branches:** switch, create, delete, merge; fetch, pull and push run in the background.
- **Undo timeline:** the reflog as a timeline, with one-key undo. No other version-control tool makes undo this visible.
