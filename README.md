<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/logo/copse-leaf-master-dark.svg">
    <img src="assets/logo/copse-leaf-master.svg" alt="The copse logo: a leaf whose veins are a commit graph" width="160">
  </picture>
</p>

# copse

A small, fast, personal version control system written in Rust. It runs on macOS and Linux: every push is built and tested on both by GitHub Actions (`.github/workflows/ci.yml`).

copse keeps its own history with copse: the project is versioned with itself and pushed with copse to [Copse-SCM/copse](https://github.com/Copse-SCM/copse) on GitHub (private).

```
cargo build --release          # binary at target/release/copse
cargo install --path .         # or put `copse` on your PATH
```

## The interactive screen

Run `copse` on its own (or `copse ui`) for a full-screen view of the repository:

| Screen | Shows | Keys |
|---|---|---|
| **1 Changes** | What changed, with the diff beside it | `space` pick a file · `a` pick all · `c` commit (asks for a message) · `x` discard a file's changes · `s` stash |
| **2 History** | The commit graph, with the selected commit's details and diff | `enter`/`o` check out · `v` revert · `p` cherry-pick · `b` make a branch here |
| **3 Branches** | Local and remote branches, with how far ahead or behind each is | `enter` switch · `n` new · `d` delete · `m` merge into the current branch |
| **4 Undo** | Every move of HEAD as a timeline | `u` undo the last change (again to redo) · `enter` go back to that point |

- **Anywhere:**
  - `1`–`4` or `tab` switch screens, and `j`/`k` or the arrow keys move.
  - `J`/`K` scroll the right-hand pane.
  - `F` fetch, `L` pull, `P` push, `r` refresh, `?` help, `q` quit.
- **The top line** shows the branch, whether you're in step with origin (`↑2 to push`, `↓1 to pull`), and how many changes you have.
- **Every action runs the same `copse` command** you'd type and shows what it said, so the screen does exactly what the command line does. Anything that could lose work asks first and keeps a backup.
- **It refreshes** every few seconds, so edits saved in your editor show up on their own.

## Commands

| Command | What it does |
|---|---|
| `copse init [dir]` | Create a repository in the current folder, or in `dir` |
| `copse init --bare <dir>` | Create a shared copy with no working files, to push to and pull from |
| `copse status` (`st`) | List added (A), modified (M) and deleted (D) files |
| `copse commit -m "msg" [paths...]` (`ci`) | Snapshot every file that isn't ignored. There is no staging step: give paths to commit only changes under them, leaving the rest uncommitted. |
| `copse commit -S` / `--no-gpg-sign` | Sign the commit with your SSH or GPG key (git's format, so GitHub shows it as Verified); `commit.gpgSign` signs every commit |
| `copse verify-commit [rev...]` | Check commits' signatures |
| `copse tag -s <name> -m "..."` / `copse verify-tag <name>` | Make a signed tag (git's format); check a tag's signature |
| `copse submodule` / `update [--init]` / `add <url> <path>` | Show submodules, check them out at their pinned commits, or add one |
| `copse commit --amend [-m "msg"] [paths...]` | Replace the last commit with your current files, keeping its author and date (and its message unless you give a new one) |
| `copse diff [rev [rev]] [--stat] [-- paths]` | Changes: your files against the last commit, against `rev`, or between two revisions (`A B` or `A..B`). `--stat` gives a per-file summary like git's; `-- paths` limits it to some files or folders. |
| `copse show [rev] [--stat]` | One commit (default: the latest): author, date in the author's timezone, message, and exactly what it changed. Annotated tags show their own message first; merges list their parents. |
| `copse log [rev...] [options] [-- paths]` | All of history, newest first, merged branches included. `--oneline`, `--graph`, `-p` (changes), `--stat`, `--first-parent`, `--all`, `--author`, `--grep`, `-n`, and `-- paths` for commits that touched those paths. |
| `copse branch [name [rev]]` | List branches, or create one (at the current commit, or at `rev`) |
| `copse branch -d <name>` / `-D <name>` | Delete a branch. `-d` refuses if its work isn't merged into your current branch; `-D` deletes anyway. Either way, copse prints the command that brings it back. |
| `copse branch -m [old] <new>` | Rename a branch (the current one if `old` is left out); `-M` replaces an existing name |
| `copse merge <rev>` | Merge a branch or commit into the current branch (see below) |
| `copse merge --abort` | Give up on a merge that stopped on conflicts |
| `copse merge --no-ff` / `--ff-only` / `-m msg` | Always make a merge commit (keeping the branch's commits visible as a group), only fast-forward, or set the merge message |
| `copse stash [-m note]` | Put all uncommitted changes aside and reset to the last commit |
| `copse stash -- <paths>` | Put aside only the changes to those files or folders, leaving the rest in place |
| `copse stash list` / `show [n]` | List stashes (0 is newest), or show which files one changes |
| `copse stash pop [n]` / `apply [n]` | Bring a stash back; `pop` also removes it, `apply` keeps it |
| `copse stash drop [n]` | Delete a stash |
| `copse tag` / `tag <name> [rev]` / `tag -a <name> -m "..."` / `tag -d <name>` | List tags; tag a commit (lightweight); make an annotated tag with its own message, author and date; delete a tag. Tags work anywhere a commit is expected (`checkout v1.0`, `v1.0~2`) and show in `log`. |
| `copse restore <file or folder>... [--from rev]` | Put files back as they were in the last commit (or in `rev`). Deleted files come back; new files are left alone. |
| `copse reset [rev]` / `reset --hard [rev]` | Move the current branch to `rev`, keeping your files as they are (so `reset HEAD~1` uncommits); `--hard` also makes your files match. `reset --hard` alone discards uncommitted changes. |
| `copse cherry-pick <commit>... \| A..B` | Copy commits onto the current branch, keeping their author and message. `--continue`/`--skip`/`--abort` after a conflict; `-m N` for a merge commit. |
| `copse revert <commit>... \| A..B` | Add commits that undo earlier ones (git's `Revert "…"` message). Same options as cherry-pick. |
| `copse clean [-f] [-x] [paths]` | Remove new files that were never committed. Without `-f` it only lists them; with `-f` they're saved as a stash first. `-x` also removes ignored files. |
| `copse grep <pattern> [rev] [-- paths]` | Search your files, or any revision's, with a regular expression. `-i`, `-l` (file names), `-c` (counts); exits 1 when nothing matches. |
| `copse blame <file> [rev] [-L start,end]` | Who last changed each line, in which commit and when, following history through merges. Without a revision it explains your file as it is now, marking lines you haven't committed. Matches `git blame` line for line. |
| `copse bisect start <bad> <good>` | Find the commit that introduced a problem by binary search. Then `copse bisect good` / `bad` / `skip` for each commit it checks out, or `copse bisect run <command>` to automate it (exit 0 = good, 125 = skip, other = bad). `copse bisect reset` takes you back. |
| `copse git-view on` / `off` | Keep a read-only `.git` folder up to date, so editors and tools that read `.git` see copse's history and changes |
| `copse lfs track <pattern>` / `untrack` / `ls-files` / `pull` / `push` | Git LFS: large files on an LFS server, pointers in history (see below) |
| `copse config <key> [value]` | Read or change a setting (`--global` for every repository, `--unset`, `--list`); see below |
| `copse undo` | Undo the last change to your current branch (a commit, merge, rebase, pull...), putting your files back too. Run it again to redo. |
| `copse reflog [branch]` | Every move HEAD (or a branch) has made, and what caused it. `HEAD@{2}` and `main@{1}` work anywhere a commit is expected. |
| `copse rebase <rev>` | Replay the current branch's commits on top of another branch (see below) |
| `copse rebase -i <rev>` | Interactive: reorder, reword, edit, squash, fixup or drop commits (e.g. `copse rebase -i HEAD~3`) |
| `copse rebase --onto <new> <upstream>` | Move only the commits after `upstream` onto `new` (e.g. a topic branch started from another feature branch) |
| `copse rebase --autosquash <rev>` | Fold `fixup! X` and `squash! X` commits into commit X |
| `copse rebase -x <cmd> <rev>` | Run a command after each commit (e.g. the tests); if it fails, the rebase pauses there so you can fix it |
| `copse rebase --continue` / `--skip` / `--abort` | Carry on after fixing conflicts, drop the conflicting commit, or undo the whole rebase |
| `copse checkout <rev>` (`co`) | Switch to a branch, a commit id prefix, `origin/main`, or `HEAD~N`. Refuses if you have uncommitted changes; `-f` discards them, but untracked files are always kept. |
| `copse gc [--prune when]` | Pack all objects into one file (older versions as compact deltas) and remove unused objects once they're two weeks old |
| `copse fsck` | Check that every object is intact and that everything branches, tags, stashes and the reflog need is there |
| `copse clone <url> [dir] [--depth N] [--filter=blob:none] [--recurse-submodules]` | Copy a repository and check out its main branch. `--depth N` fetches only the newest N commits; `--filter=blob:none` fetches file contents only when needed (both copse remotes); `--recurse-submodules` checks submodules out too |
| `copse remote` | List remotes |
| `copse remote add <name> <url> [--git]` / `remove <name>` | Add or remove a remote. GitHub, GitLab, Bitbucket and `*.git` addresses are recognized as git servers automatically. |
| `copse fetch --unshallow` | Fetch the rest of history, so a shallow clone becomes a full one |
| `copse worktree add <path> [-b new] [branch]` / `list` / `remove` / `prune` | More working directories for the same repository, each on its own branch (see below) |
| `copse sparse set <folders>` / `add` / `disable` | Check out only some folders; commits still contain every file (see below) |
| `copse fetch [remote]` | Download new commits without touching your files; updates `origin/<branch>`, and removes `origin/<branch>` names for branches deleted on the server |
| `copse pull [remote] [branch] [--rebase]` | Fetch, then merge `origin/<branch>` into the current branch. `--rebase` replays your commits on top of it instead, keeping history linear. |
| `copse push [remote] --delete <name>` | Delete a branch (or tag) on the remote: GitHub, any git server, or a copse remote |
| `copse push [remote] [branch or tag] [-f] [--tags]` | Upload the current branch, or a named branch or tag. `--tags` pushes all tags (to git servers). `-f` overwrites commits the remote has that you don't, or replaces a tag. |

`remote` defaults to `origin` and `branch` to the current branch.

copse honours `.gitignore` files and also reads `.copseignore`, which uses the same syntax. Commits record your name, email and timezone, as git does. The name comes from `$COPSE_AUTHOR`, then git's `user.name`, then `$USER`. The email comes from `$COPSE_EMAIL`, then git's `user.email`. If you already use git, your commits are attributed to the same account with no setup.

## Merging

`copse merge feature` needs a clean working directory. It finds the newest commit shared by both branches (the merge base), then:

- **Already contained:** if your branch already contains `feature`, it says "Already up to date".
- **Fast-forward:** if `feature` is directly ahead of you, your branch just moves forward to it.
- **Three-way merge:** otherwise each file is compared across base, yours and theirs. A file changed on only one side takes that side, including deletions. A file changed on both sides is merged line by line. If that's clean, copse makes a merge commit with two parents right away.
- **Conflicts:** overlapping edits get git-style markers:
  ```
  <<<<<<< main
  your version
  =======
  their version
  >>>>>>> feature
  ```
  Edit the files, then run `copse commit`; the message defaults to "Merge feature into main". `copse status` lists the files that still have markers, and `commit` refuses until they're gone. `copse merge --abort` puts everything back.
- **Files that can't be merged line by line:** binary files and symlinks changed on both sides, and files deleted on one side but edited on the other, are reported as conflicts. copse keeps one version (yours, or the edited one) for you to check.
- **Renames are followed**, as git follows them, in merge, rebase, cherry-pick, revert and stash. A file renamed on one side and edited on the other ends up renamed with both changes. A rename counts when the content is identical or at least 50% the same.
  - Renamed differently on each side: both names get both sides' changes, and it's reported as a conflict.
  - Renamed on one side and deleted on the other: the renamed file is kept, and it's reported.
  - Folder moves are followed too, as git does. If one side moves a whole folder (`lib/` to `src/`, or up to the top level) and the other side adds a file in the old folder, the new file goes to the folder's new place. This is reported as a conflict so you can check it, which is git's default.
- **Fast-forwards touch only what changed.** When the other branch is simply ahead (most pulls), copse checks and writes only the files that differ, so uncommitted edits to other files don't get in the way, as with git. A small pull in a 50,000-file repository takes about 30 ms (git: about 135 ms).

## Undo and the reflog

copse records every move of HEAD and of each branch (commits, checkouts, merges, rebases, pulls, deletes, renames), with when it happened and which command did it. This is the reflog. Commits are never deleted, so with this record nothing a command did is out of reach.

- **`copse undo`** reverses the last change to your current branch and puts your files back exactly as they were. Undo a commit, a squash, a merge or a pull. Run `copse undo` again to redo.
- **It never destroys work.** It refuses if your uncommitted changes would be overwritten (it's fine after a plain `reset`, when your files already match where it's going), and it points you at `--abort` during a merge or rebase.
- **Overwriting commands keep a copy.** `restore` and `reset --hard` are the commands that can overwrite uncommitted work. Before they do, they save it as a stash, so `copse stash apply` brings it back.
- **`copse reflog`** lists the history, newest first. `copse reflog feature` shows one branch's history.
- **Any past position is a revision.** `copse checkout HEAD@{3}` or `copse branch rescue main@{2}` take you back to any recorded point.
- **Deleted branches come back.** A branch's reflog outlives the branch, so `copse branch feature feature@{1}` restores one.
- **Servers keep a record too.** A bare repository logs every push it receives, so a commit replaced by someone's `push --force` can still be found there with `copse reflog main`.

## Stashing

`copse stash` puts your uncommitted work aside (edits, new files, deletions, permission changes and symlinks) and resets your files to the last commit. Ignored files are left alone. `copse stash pop` brings the work back as uncommitted changes, exactly as it was.

- **Several stashes stack up:** 0 is the newest. `copse stash list` shows them with their notes and times.
- **History can move on in between.** Bringing a stash back uses the same three-way merge as `copse merge`, so it works after you've made new commits or switched to another branch. That's also a handy way to move unfinished work to a different branch.
- **Stash only some files** with `copse stash -- src/parser.rs docs`. Your other changes stay where they are, and `copse stash pop` works with them still there, as long as they don't touch the stashed files.
- **If a stash conflicts** with what's there now, the files get the usual conflict markers and the stash is not removed, so nothing is lost. Fix the files, then `copse stash drop`.
- **Stash refuses to run** if there's nothing to stash, while a merge or rebase is in progress, or (for `pop`/`apply`) when you already have uncommitted changes.

A stash is stored as an ordinary commit whose parent is the commit you were on. The list lives in `.copse/stash`, and `copse gc` keeps stashes safe.

## Rebasing

`copse rebase main`, run on your feature branch, takes the commits main doesn't have yet and replays them one at a time on top of main. The result is a straight line of history, as if you'd started your work from today's main.

- **How each commit is replayed:** with the same three-way merge as `copse merge`. Each new commit keeps the original author, time and message.
- **Conflicts:** the rebase stops at the commit that conflicts and writes the usual conflict markers. `copse status` shows how far it got and which files still need fixing. Then:
  - `copse rebase --continue`: once the markers are gone, records your fix and carries on.
  - `copse rebase --skip`: drops that commit and carries on.
  - `copse rebase --abort`: puts the branch and your files back exactly as they were.
- **Your branch can't be lost mid-rebase.** It doesn't move until the rebase finishes, so an abort or a crash leaves it untouched. While a rebase is paused, `commit`, `merge` and `checkout` refuse to run, so it can't get tangled.
- **Commits that are already in the target** (the same change made there too) are skipped, so no empty commits are left behind.
- **Merge commits on your branch are flattened.** The commits they brought in are replayed in order, so nothing is dropped. Git does the same by default.
- **No rebase needed?** If main has nothing new, it says so. If your branch has no commits of its own, it just fast-forwards.

### Interactive rebase

`copse rebase -i HEAD~3` tidies up your last three commits. `copse rebase -i main` does the same while moving your branch onto main. Your editor opens with one line per commit, oldest first:

```
pick 1a2b3c4d5e Add login form
pick 6f7a8b9c0d Fix typo
pick 2e3f4a5b6c Add password reset
```

- **Pick an action per line.** Change the first word to `reword` (edit the message), `edit` (pause on that commit), `squash` (fold into the commit above and edit the combined message), `fixup` (fold in, keeping the message above) or `drop` (remove it). One-letter forms (`r`, `e`, `s`, `f`, `d`) work too.
- **Reorder the lines** to reorder commits.
- **Delete a line** to drop that commit. Delete every line to cancel.
- **The list is checked before anything changes.** An unknown action, a commit listed twice, or a squash with nothing above it is reported, and nothing happens.
- **An `edit` pause** stops right after applying that commit. Change files and run `copse rebase --continue` to fold the changes into it. To split the commit, run `copse commit` a few times first; your new commits slot in at that point.
- **Commits you leave unchanged stay exactly the same commits,** so a no-op rebase rewrites nothing.

copse uses `$COPSE_EDITOR`, then `$VISUAL`, then `$EDITOR`, then `vi`. Editors that take arguments work, for example `export EDITOR="code --wait"`.

`copse pull --rebase` fetches, then rebases your local commits onto the remote's. Then `copse push` works without `--force`.

## Settings, aliases and hooks

Settings live in `.copse/config` for one repository and `~/.config/copse/config` for all of them; the repository's file wins. The layout is the same as git's, and `copse config` edits both files without disturbing anything else in them:

```bash
copse config --global user.name "Ada Lovelace"
copse config --global user.email ada@example.com
copse config alias.last "log -n 1 --oneline"     # copse last
copse config alias.tidy '!cargo fmt'              # a ! alias runs a shell command from the top of the repository
copse config core.editor nano                    # for rebase lists and messages
```

- **Your name and email:** `$COPSE_AUTHOR` / `$COPSE_EMAIL` come first, then copse's settings, then git's `user.name` / `user.email`, then `$USER`.
- **Aliases** can't replace a real command. They can use other aliases, and quoted arguments stay together.
- **Ignore files:**
  - `~/.config/copse/ignore` applies to every repository; `core.excludesFile` points somewhere else instead.
  - `.copse/exclude` holds rules for one repository that aren't committed.
  - These sit on top of each folder's `.gitignore` and `.copseignore`.
- **Hooks** are executable scripts in `.copse/hooks/`, or in a folder of the project named by `core.hooksPath` so a team can share them. They run from the top of the repository:

  | Hook | When | Can stop it? |
  |---|---|---|
  | `pre-commit` | before committing; it may also tidy files, and the commit takes the tidied version | yes |
  | `commit-msg` | given a file holding the message, which it may rewrite | yes |
  | `post-commit` | after committing | no |
  | `pre-push` | given the remote's name and URL, and a `<branch> <commit>` line per ref on stdin | yes |
  | `pre-rebase` | given what's being rebased onto, and the branch | yes |
  | `post-merge`, `post-checkout` | after a merge, or a switch (given the old and new commits) | no |

  `--no-verify` on `commit` (also `-n`), `push` and `rebase` skips the hooks that can stop them.

## GitHub and other git servers

copse works directly with GitHub, GitLab, Bitbucket or any git server, over HTTPS or SSH. It can clone, fetch, pull (including `--rebase`) and push:

```bash
copse clone https://github.com/you/your-repo.git    # or git@github.com:you/your-repo.git for SSH
copse pull                                          # get what others pushed (with git or copse)
copse push                                          # send your work
```

To use an existing copse repository with GitHub, run `copse remote add github git@github.com:you/your-repo.git`, then `copse push github`.

To GitHub, copse looks like any other git client. Commits, authors, emails, timezones, merge commits, executable bits and symlinks all go both ways exactly as git expects, so you can work alongside teammates who use git.

**Tags and releases:** run `copse tag -a v1.0 -m "First release"`, then `copse push origin v1.0` (or `copse push --tags`). The tag arrives on GitHub as a real annotated tag, ready to turn into a release.
- **Fetching brings tags too.** Clones and fetches bring the server's tags down as local tags. Annotated tags keep their exact git ids, so pushing them back changes nothing.
- **Tags don't move by surprise.** Like git, copse won't replace a tag on the server without `--force`, and a tag from the server never overwrites one of your own.

**Branch and tag names** follow git's rules, so names with slashes such as `feature/login` or `release/1.0` work everywhere: locally, with copse remotes and with git servers. As in git, you can't have both `feature` and `feature/login` at once.

**Signing in:**
- **SSH** uses your SSH key, as git does.
- **HTTPS** uses git's own credential helpers, so a login you've already saved for git (for example in the macOS Keychain) works for copse too.
- **Asked only when needed.** copse asks for a login only when the server requires one: always for pushing, and for reading private repositories.
- **Saved and forgotten like git.** Logins that work are saved, and rejected ones are forgotten.
- **GitHub needs a token.** The password must be a personal access token, not your account password. If you've never signed in with git, create a token on GitHub, then push once with git, or with copse in a terminal where git can ask you for it.
- **A login written into the address** (`https://user:token@host/…`) also works. copse never prints the password, but it is stored in `.copse/remotes`, so a credential helper is the safer choice.

**How it works:**
- **Translation.** copse keeps its own fast storage. Objects are translated between git's exact format (SHA-1 ids) and copse's as they cross the wire.
- **Git's own protocols,** over SSH or git's "smart HTTP". Pushing talks to the server's `git-receive-pack` and sends a standard git pack. Fetching talks to `git-upload-pack`, says which commits copse already has, and reads the pack it sends back, applying git's binary delta format. Large downloads go straight into a single copse pack file.
- **Exact round trips.** Everything pulled is recorded with its *original* git id, so pushing it back always reproduces exactly the same history. The tests check this with hand-made unusual commits too.
- **Translate once.** The `.copse/gitmap` id map remembers every translation, so later pushes only handle new work.
- **Deterministic.** The same copse history always produces the same git ids. The tests check this by throwing the map away and confirming every id comes out identical.

**Safety:**
- **Push won't overwrite commits it doesn't know about.** If someone else pushed first, push refuses. Run `copse pull` (or `copse pull --rebase`), then push, or pass `--force`.
- **Downloads are checked.** Every pack's SHA-1 checksum is verified before anything is stored.
- **Verified by git itself.** Every push in the tests is checked with `git fsck --strict`, and a fresh `git clone` is compared byte for byte with copse's files.

**Speed** (50,000-file repository, with a git server on the same Mac):

| Operation | git | copse |
|---|---|---|
| Clone the whole repository | 6.3 s | 5.0 s |
| Push the whole history, first time | 2.7 s | 3.4 s (translating every object once) |
| Push 100 changed files | 0.27 s | 0.24 s |
| Pull 100 changed files | 0.29–0.47 s | 0.36–0.60 s |

**Not yet supported** (see [ROADMAP.md](ROADMAP.md)):
- **Submodules.** copse says so if a repository uses them.
- **File names and commit messages that aren't UTF-8.**
- **Tags between two copse repositories** (as opposed to git servers).
- **Pushing to github.com hasn't been tried yet.** Cloning a public github.com repository over HTTPS has been checked: identical files, branches and history, with every commit translating back exactly. Pushing is tested against real git servers locally, over SSH and HTTP, but not against a live GitHub repository, which needs your login. Try it first on a throwaway repository.

## Remotes: push and pull

A remote is another copse repository, reached one of two ways:

- **A folder:** `copse remote add origin /Volumes/Backup/myproject.copse`. It can be on an external drive, a synced folder, or anywhere else on this machine.
- **Another machine over SSH:** `copse remote add origin me@server:repos/myproject.copse`, or `ssh://server/srv/myproject.copse`. copse must be installed and on the `PATH` on that machine. If it isn't, set `COPSE_REMOTE_CMD` to its full path there. To use a different ssh program, set `COPSE_SSH`.

To share work between your machines, make one bare repository and push to it from everywhere:

```bash
copse init --bare /Volumes/Backup/myproject.copse     # once
copse remote add origin /Volumes/Backup/myproject.copse
copse push                                           # from this machine
copse clone /Volumes/Backup/myproject.copse           # on another machine
copse pull                                           # get the other machine's work
```

**Safety:**
- **Push won't lose work.** If the remote branch has commits you don't have, push refuses until you `pull`, unless you pass `-f`.
- **Concurrent pushes can't clobber each other.** The remote updates a branch under a lock, and only if it hasn't moved since you looked.
- **Checked-out branches are protected.** copse won't push into a branch that a non-bare repository has checked out, because that would change files under someone's feet.
- **Every transfer is verified.** Each received object is re-hashed before it's kept. Commits are written last, so an interrupted transfer never leaves a commit whose files are missing.

**Tags** travel too: `fetch`, `pull` and `clone` bring the remote's tags (yours are never overwritten; a tag that differs is reported and kept as yours), and `copse push origin v1.0` or `copse push --tags` sends them. A tag that already exists on the remote with a different commit is refused unless you pass `--force`.

**Shallow clones:**
- `copse clone --depth 1 <url>` fetches only the newest commit's worth of history, and the files to check it out.
- Pulls stay shallow, and you can commit and push as usual. A repository you push a shallow history into becomes shallow at the same place.
- Only tags that point into the fetched history come along.
- `copse fetch --unshallow` brings the rest.
- The boundary is kept in `.copse/shallow`. Every history walk (log, merge, gc, fsck) stops there.
- Shallow clones work between copse repositories. Pushing to a git server needs the full history, because git ids depend on all of it.

**How it works:** the client runs `copse serve <path>` on the other end, locally or through `ssh`, and talks to it over stdin/stdout. It sends only the objects the other side is missing, worked out by comparing each new commit's folders with its parent's and skipping unchanged folders entirely. Objects travel exactly as they're stored, already compressed, so there's no recompression. The protocol is described at the top of `src/transport.rs`.

## Signed commits

Sign with an SSH key (the simplest) or GPG, using the same settings as git:

```bash
copse config --global user.signingKey ~/.ssh/id_ed25519
copse config --global gpg.format ssh
copse commit -S -m "Signed"              # or: copse config --global commit.gpgSign true
copse verify-commit HEAD
```

- **What's signed** is the commit exactly as git sees it, and the signature is stored as git stores it (a `gpgsig` header). git verifies copse's signatures, copse verifies git's, and GitHub shows them as Verified once your key is on your account.
- **Whose key it is:** set `gpg.ssh.allowedSignersFile` (git's format) and `verify-commit` also checks the key belongs to the commit's author.
- **Your git settings are used** when copse's own aren't set: `user.signingKey`, `gpg.format`, `commit.gpgSign`, `tag.gpgSign`, `gpg.ssh.allowedSignersFile`. Signing you've set up for git just works.
- **Every commit copse makes is signed when it should be:** `commit`, and merge, revert, cherry-pick and rebase commits.
- **Signed tags:** `copse tag -s v1.0 -m "..."` signs a tag the way git does, and `copse verify-tag` checks one.
- **Checked on GitHub:** a copse-signed commit pushed to github.com showed as Verified (2026-11-07).
- **Amending without `-S`** (with signing off) drops a signature, since it would no longer match.

## Submodules

Repositories with git submodules clone, change and push back exactly as git expects:

```bash
copse clone --recurse-submodules https://github.com/you/app.git
copse submodule                    # each submodule, and whether it's on its pinned commit
copse submodule update --init      # check them out at their pinned commits
copse submodule add https://github.com/you/lib.git vendor/lib
```

- **Moving a submodule:** check out another commit inside it, then commit in the main repository. `status` and `diff` show the move the way git does (`Subproject commit …`).
- **Kept separate:** a submodule's files are never counted as the main repository's, and `clean` never touches them.

## The .git view, for editors

Editors and tools that read `.git` directly can work in a copse repository too. These include VS Code's change markers, source control panel, blame and history views, and anything that runs `git status` or `git log`:

```bash
copse git-view on     # makes .git, and keeps it up to date after every copse command
copse git-view off
```

- **What's in it:** real git objects, branches, tags, HEAD, and an index matching copse's. Uncommitted changes appear in git as unstaged changes, exactly as `copse status` lists them.
- **Kept up to date** after every copse command, writing only what's new.
- **Read-only for git.** git commands that would change files or branches (checkout, commit, merge, branch) stop before touching anything, so commit and switch with copse.
- **An existing `.git` folder is never overwritten.**

## Git LFS

Repositories that keep large files in Git LFS work as they do with git-lfs:

```bash
copse lfs track "*.psd"      # store matching files with LFS (writes .gitattributes, as git-lfs does)
copse commit -m "Add art"
copse push                   # uploads the files' contents to the LFS server, then pushes
```

- **History** holds git-lfs's small pointer files, byte for byte, and the contents live on the LFS server, with a local copy in `.copse/lfs/`.
- **`clone`, `pull` and `checkout`** download the contents they need and write the real files. `status` and `commit` turn your files back into pointers.
- **The server** is the one git-lfs would use for the remote (`<repository>.git/info/lfs`, over HTTPS with your saved credentials), or `lfs.url` in copse's settings or in `.lfsconfig`.
- **`copse lfs pull`** fetches anything missing. `copse lfs ls-files` lists the LFS files (`*` = downloaded).
- **Note:** copse itself handles big files well (they're streamed), so LFS is mainly for repositories that already use it.

## Partial clones

`copse clone --filter=blob:none <url>` downloads all of history's commits and folders, but only the file contents the checkout needs.

- **Fetched when needed:** older versions are downloaded when something reads them, such as `log -p`, `diff`, checking out an old commit, or a merge.
- **`fsck`** reports them as not downloaded yet, rather than missing.
- **Pushing** from a partial clone fetches whatever the other side needs on the way.
- **Works between copse repositories.** `.copse/promisor` names the remote to fetch from.

## Worktrees and sparse checkout

**Worktrees** give you a second (or third) working directory for the same repository, each on its own branch. That's handy for a hotfix while a feature is half done, or for running a long test on one branch while you edit another:

```bash
copse worktree add ../hotfix -b hotfix     # new folder, new branch from where you are
copse worktree add ../review feature-x     # an existing branch
copse worktree list
copse worktree remove ../hotfix            # refuses if it has uncommitted changes (--force to discard)
```

- **Shared:** history, branches, tags, stashes and settings, so a commit made in one is visible in the others at once.
- **Separate:** each has its own HEAD, files, index, and merge, rebase or bisect in progress.
- **One worktree per branch.** A branch can be checked out in only one worktree at a time, and can't be deleted or renamed while another worktree has it.
- **gc keeps every worktree's commits.**
- **Layout:** a worktree's folder has a `.copse` file pointing at `.copse/worktrees/<name>/` in the main repository.

**Sparse checkout** keeps only some folders on disk, which helps in a big repository where you work in one corner:

```bash
copse sparse set app/ui docs    # only these folders (plus top-level files) on disk
copse sparse add lib            # one more
copse sparse disable            # everything back
```

- **History doesn't change.** Commits still contain every file, and a file outside your folders that isn't on disk counts as unchanged, never as deleted.
- **Switching branches, merging, stashing and rebasing** leave the other folders off disk.
- **A conflict** in a file outside them is written out so you can fix it.
- **Leaving out a folder** that has uncommitted changes is refused.
- **The folder list** is per worktree, in `.copse/sparse`.

## Packfiles and `copse gc`

New commits are stored as one small file per object ("loose"), because that's fastest to write. Two things put objects into pack files instead (one `.pack` with a sorted `.idx` index, under `.copse/objects/pack/`):

- **Clones and large fetches.** Anything over 500 objects is written straight into a single pack instead of thousands of small files. It only becomes visible once it's complete, so an interrupted transfer leaves nothing behind.
- **`copse gc`.** This repacks everything into one pack. It stores each older version of a file as a delta against the version that replaced it, using zstd's `--patch-from` technique (the newer version is the reference). The newest version of every file stays whole, so checkout and clone never have to decode deltas. Older versions are at most 10 deltas deep. `gc` works in bare repositories too, and it never deletes anything until the new pack is safely written.

Run `copse gc` now and then. It's worth it after lots of commits, and on a shared bare repository.

**Removing unused objects.** Objects that nothing can reach any more go too, such as a dropped stash or file versions you never committed. "Reach" covers a branch, tag, remote branch, stash, an operation in progress, and the reflog, so `copse undo` and `branch@{n}` keep working. They're only removed once they're two weeks old, so `gc` can't race another command that has just written objects. `--prune now` removes them at once, `--prune never` keeps them, and `gc.pruneExpire` changes the default. `gc` refuses to remove anything from a repository that's missing objects.

## Repositories from other people

Cloning or pulling someone else's repository is safe from the tricks git itself guards against:

- **File and folder names are checked** before copse stores or uses them, whether they come from a git server, a copse remote or your own files. A name is refused if it's:
  - empty, `.` or `..`, or contains `/` or a null byte;
  - `.copse` or `.git` in any disguise a filesystem would accept: other capitals (`.COPSE`), trailing dots or spaces (`.git.`), Windows short names (`GIT~1`), or invisible characters macOS ignores.

  A hostile repository can't make a checkout write outside your project or over copse's data. A folder that lists the same name twice is refused too.
- **They're checked on arrival, too.** Every folder listing received over copse's protocol (a push into a shared repository, a fetch, a partial clone's downloads) is checked before it's stored. A hostile push is refused outright rather than planted for someone else to trip over.
- **Checkouts never write through a symlink.** If a folder on the way to a file is a link, the write is refused rather than following it.
- **Addresses can't smuggle in options.** An SSH address whose host starts with `-` (like `-oProxyCommand=…`) is refused before ssh is started, and so is a submodule URL starting with `-`.

`tests/security.sh` attacks copse with hand-crafted repositories that git itself would refuse. With the name check switched off, the `..` attack really does write outside the project, so the test catches genuine breakage.

## Checking and crash safety

- **`copse fsck`** re-hashes every object and checks it matches its id. It also opens every pack index and walks from every branch, tag, stash and reflog entry to confirm everything they need is there. It reports damaged and missing objects with what needs them, and exits 1 if there are any. Missing objects that only the reflog mentions are warnings.
- **Crash safety.** An object is flushed to disk (fsync) before it's renamed into place, and before any ref, HEAD or the index changes. Refs themselves are flushed fully (F_FULLFSYNC on macOS) and replaced in one rename. A crash can lose the last command's work, but it can't leave a branch pointing at a missing or half-written object. This costs about 10 ms on a commit. `COPSE_FSYNC=0` or `core.fsync = false` turns it off.
- **Large files** (32 MB and up) are hashed, compressed, checked and checked out in 1 MB pieces. Memory use stays flat: a 100 MB or 200 MB file commits, packs, verifies and restores in under 10 MB. Pushing and cloning still hold one file's compressed form in memory at a time.

## Why it's faster than git

- **No staging area.** Commit is one pass: scan, hash, store. There is no separate `git add`.
- **BLAKE3 and zstd** in place of SHA-1 and zlib, which are several times faster each.
- **Everything is parallel.** Scanning, hashing, compressing and checkout writes use all CPU cores.
- **A stat cache** (`.copse/index`): files whose size and modification time haven't changed are never read. Files modified within 2 seconds of the last save are always re-hashed, so a fast edit can't slip through unnoticed.
- **Atomic writes**: objects, refs and the index are written to a temporary file and then renamed into place.

## Layout

```
.copse/HEAD             "ref: main" or a commit id (detached)
.copse/refs/<branch>    commit id
.copse/objects/ab/cd…   kind byte + zstd(content), named by BLAKE3(kind ‖ content)
.copse/index            bincode stat cache of the files in the current commit
```

```
.copse/objects/pack/pack-<n>.pack   many objects; each is full (kind + zstd) or a delta (0x80|kind, base id, zstd against the base)
.copse/objects/pack/pack-<n>.idx    sorted (id, offset, length) records + a BLAKE3 checksum; written last, so it makes the pack visible
.copse/remotes                  "name url" per line
.copse/tracking/<remote>/<b>    where the remote's branch was when you last fetched or pushed (origin/main)
.copse/config                   this repository's settings (git's layout)
.copse/exclude                  ignore rules for this repository only
.copse/hooks/<name>             hook scripts
```

During a paused rebase, `.copse/REBASE_STATE` records the branch, where it started, the rebuilt commits so far and the commits still to replay. During a conflicted merge, `.copse/MERGE_STATE` records the commit being merged in, the conflicted files, and the files the merge added. A bare repository has the same layout as `.copse/`, but at the top level.

Source: `objects.rs` (store, trees, commits), `repo.rs` (HEAD, refs, remotes), `worktree.rs` (scan, stat cache, change detection), `merge.rs` (merge base, line-level three-way merge), `rebase.rs` (replaying commits: rebase, cherry-pick and revert), `stash.rs` (stashes), `transport.rs` (wire protocol, server, transfer streams), `remote.rs` (clone, fetch, pull, push), `gitwire.rs` (translation to git and git's push protocol), `gitimport.rs` (git's fetch protocol, pack and delta format, translation from git), `githttp.rs` (smart HTTP and sign-in), `tags.rs` (the tag command), `branches.rs` (listing, creating, deleting and renaming branches), `reflog.rs` (the reflog and undo), `restore.rs` (restore and reset), `diffs.rs` (diff and show), `logs.rs` (log), `tools.rs` (clean and grep), `investigate.rs` (blame and bisect), `identity.rs` (name, email, timezone), `config.rs` (settings, the config command, ignore files), `hooks.rs` (running hooks), `tui/` (the interactive screen), `renames.rs` (rename detection), `ffwd.rs` (fast-forwards that touch only what changed), `gitview.rs` (the .git view), `lfs.rs` (Git LFS), `signing.rs` (signed commits), `submodules.rs` (submodules), `worktrees.rs` (worktrees), `sparse.rs` (sparse checkout), `integrity.rs` (reachability and fsck), `durable.rs` (flushing writes to disk), `pack.rs` (pack file format), `gc.rs` (delta planning and repacking), `cmd.rs` (the other commands).

Tests: `tests/run-all.sh` runs every suite, each in a fresh folder, and summarizes (the scripts work with both macOS and Linux tools, through `tests/portable.sh`). CI runs it, plus `cargo test` and 150 stress-test rounds, on macOS and Linux for each push. `cargo test` runs unit tests for merging, the rebase list, timezones and git's formats (checked against hashes real git produces). `tests/remote.sh` runs 27 end-to-end checks of clone, push, pull, conflicts, force push, the SSH path and corruption detection. `tests/tags.sh` runs 20 checks of local tags. `tests/undo.sh` runs 23 checks of the reflog and undo. `tests/restore.sh` runs 33 checks of restore and reset, including the automatic backups. `tests/history.sh` runs 39 checks of cherry-pick, revert, clean and grep. The logo lives in `assets/logo/`: the original design, plus versions tuned for each size (heavier and simpler as they shrink, with dark-mode colours and an adaptive `favicon.svg`), rebuilt by `assets/logo/make-sizes.py`. `tests/security.sh` runs 23 checks with hostile repositories: names like `..`, `.copse` and their disguises, a symlink and a folder with the same name, a symlink replaced by a folder between commits, a symlinked folder of yours, ssh option injection, and a hostile submodule URL. `tests/tui.sh` runs 46 checks of the interactive screen, driven by `copse ui --replay KEYS` (which presses keys and prints the screen it draws), with every action's effect checked with the command line: committing picked files, discarding, stashing, reverting, branching, switching, merging, deleting, undo and redo, going back to a point, fetch, pull and push, and empty repositories. `tests/fuzz.py` is a randomized stress test. It builds the same random histories in git and copse (edits, inserts, deletions, new files, renames with and without edits), then merges, rebases and cherry-picks in both and compares the results file by file. Run `tests/fuzz.py 500 <seed>`. 2,000 rounds across eight seeds match git exactly, folder moves included. `tests/bridge.sh` runs 41 checks of the `.git` view (read by git itself: log, branches, tags, status, diff, blame, fsck, and that git can't change it) and Git LFS against a local LFS server (`tests/git-http-server.py` also speaks the batch API): uploading on push, pointers byte for byte, downloading on clone and checkout, files pushed by a git-lfs user, `lfs pull`. `tests/interop.sh` runs 42 checks of signed commits (SSH and GPG, verified by git itself, and git's verified by copse), submodules (cloning, updating, moving, adding, all checked by git) and partial clones (fetching on demand, fsck, gc, pull, push). `tests/layout.sh` runs 61 checks of tags between copse repositories, shallow clones (depth, pulling, pushing from one, unshallowing, which tags come along), sparse checkout (commits, diffs, branch switches, stashes, conflicts outside the folders) and worktrees (sharing, the one-branch-one-worktree rule, gc, remove and prune). `tests/safety.sh` runs 40 checks of fsck (damaged and missing objects, damaged packs, temp files), gc's pruning (the grace period, `--prune`, what's always kept, objects inside packs expiring on time), crash safety, and large files with measured memory use. `tests/config.sh` runs 48 checks of settings, aliases (including shell aliases and loops), every ignore file, and every hook, including `--no-verify` and `core.hooksPath`. `tests/workflow.sh` runs 40 checks of `merge --no-ff`/`--ff-only`/`-m`, `rebase --onto`, `--autosquash` and `exec` (`--onto` and `--autosquash` compared with git doing the same rebase), and stashing only some files. `tests/investigate.sh` runs 33 checks of blame (every line compared with `git blame`, across merges, older revisions and other branches) and bisect (by hand, `run`, skipping, and the guards). `tests/commit.sh` runs 30 checks of partial commits, `--amend` and `log` (with `--graph` and path filtering checked against git). `tests/diff.sh` runs 29 checks of diff and show, with `--stat` checked against git's own output for the same commits. `tests/branches.sh` runs 51 checks of deleting and renaming branches and of names with slashes, locally, on copse remotes and on a real git server. `tests/github.sh` runs 94 checks against real git servers (needs git and python3 installed), including a local smart-HTTP server (`tests/git-http-server.py`, built on `git http-backend`) with sign-in. Pushing: `fsck --strict` after every push, git clones matching copse byte for byte, deterministic ids. Pulling: cloning history made with git (delta chains of both kinds, binary, empty, unicode and executable files, symlinks, merges, an annotated tag, a hand-made commit with a signature header), pushing it straight back unchanged, and working alongside a git user through fast-forwards, merges and `pull --rebase`. `tests/stash.sh` runs 32 checks: save and restore of every kind of change, byte-for-byte; several stashes; restoring onto newer history or another branch; conflicts; guards; and surviving `gc`. `tests/rebase.sh` runs 78 checks: the earlier ones, plus 30 for interactive rebase covering every action, reordering, conflicts, edit pauses, splitting, cancelling and bad lists (clean rebases, conflicts with continue, skip and abort, duplicate commits, branches containing merges, `pull --rebase` and the guards). `tests/pack.sh` runs 20 checks: it packs a history with long edit chains, a reverted file, binary data and a large file, confirms every commit reads back byte-for-byte identical, then tests transfers from packed repositories and detection of damaged packs. Build with `cargo build --release` first.

## Benchmark

`python3 bench/bench.py [dir] [num_files]`. Results on an 8-core Mac with 50,000 files (296 MB):

| Operation | git | copse | Speedup |
|---|---|---|---|
| First commit (all files) | 33.0 s | 6.4 s | 5.1x |
| Status, nothing changed | 431 ms | 117 ms | 3.7x |
| Status, 100 files changed | 430 ms | 124 ms | 3.5x |
| Commit 100 changed files | 821 ms | 327 ms | 2.5x |
| Commit 5,000 changed files | 6.1 s | 1.07 s | 5.7x |
| Checkout (one direction) | 2.4 s | 0.65 s | 3.8x |

Before packing, copse's objects took 101 MB and git's 209 MB. See below for sizes after both are packed.

Transfers between two folders on the same Mac, with git using `--no-local` so it goes through its real transfer protocol:

| Operation | git | copse | Speedup |
|---|---|---|---|
| Push 100 changed files | 0.32 s | 0.14 s | 2.3x |
| Fetch 100 changed files | 0.17 s | 0.15 s | 1.1x |
| Full clone (50,000 files), source not packed | 6.4 s | 5.0 s | 1.3x |
| Full clone (50,000 files), source packed | 6.0 s | 2.6 s | 2.3x |
| Pack the repository (`gc`) | 1.3 s | 1.2 s | |

After packing, objects take 86 MB in copse and 83 MB in git. Git can also store a file as a delta against a different, similar file. copse deliberately doesn't, so the newest version of every file is always whole and fast to read. Where copse's deltas apply (repeated edits to the same files), they're very effective: the test history in `tests/pack.sh` shrinks from 48.5 MB to 9.3 MB.

## Not built yet

Windows support, shallow and partial clones from git servers, and signed tags. The roadmap lists everything that's left.
