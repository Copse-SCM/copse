mod branches;
mod cmd;
mod config;
mod diffs;
mod durable;
mod ffwd;
mod gc;
mod githttp;
mod gitimport;
mod gitview;
mod gitwire;
mod hunks;
mod hooks;
mod identity;
mod integrity;
mod investigate;
mod lfs;
mod logs;
mod merge;
mod objects;
mod pack;
mod rebase;
mod reflog;
mod renames;
mod remote;
mod repo;
mod signing;
mod restore;
mod sparse;
mod stash;
mod submodules;
mod tags;
mod tools;
mod transport;
mod tui;
mod worktree;
mod worktrees;

use clap::{CommandFactory, Parser, Subcommand};
use std::path::PathBuf;

/// copse: a small, fast, personal version control system.
///
/// There is no staging area: `copse commit` snapshots every file that isn't
/// ignored by .gitignore or .copseignore.
#[derive(Parser)]
#[command(name = "copse", version)]
struct Cli {
    /// Leave out to open the interactive screen (in a terminal)
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Create a new repository (in the current directory, or DIR)
    Init {
        dir: Option<PathBuf>,
        /// No working directory: a shared copy to push to and pull from
        #[arg(long)]
        bare: bool,
    },
    /// Copy a repository (a path, or user@host:path over SSH)
    Clone {
        url: String,
        dir: Option<PathBuf>,
        /// Only fetch the newest N commits of history (copse remotes)
        #[arg(long, value_name = "N")]
        depth: Option<usize>,
        /// Also clone each submodule and check it out at its pinned commit
        #[arg(long = "recurse-submodules")]
        recurse: bool,
        /// blob:none fetches file contents only when they're needed (copse remotes)
        #[arg(long, value_name = "FILTER")]
        filter: Option<String>,
    },
    /// Show, check out, or add submodules (other repositories pinned inside this one)
    Submodule {
        #[command(subcommand)]
        action: Option<SubmoduleAction>,
    },
    /// List, add or remove remotes
    Remote {
        #[command(subcommand)]
        action: Option<RemoteAction>,
    },
    /// Download new commits from a remote without changing your files
    Fetch {
        #[arg(default_value = "origin")]
        remote: String,
        /// Only fetch the newest N commits of history (copse remotes)
        #[arg(long, value_name = "N", conflicts_with = "unshallow")]
        depth: Option<usize>,
        /// Fetch the rest of history, so a shallow clone becomes a full one
        #[arg(long)]
        unshallow: bool,
    },
    /// Fetch, then merge the remote's version of a branch (default: the current one)
    Pull {
        #[arg(default_value = "origin")]
        remote: String,
        branch: Option<String>,
        /// Rebase your commits on top of the remote's instead of merging
        #[arg(short, long)]
        rebase: bool,
    },
    /// Upload a branch (default: the current one) to a remote
    Push {
        #[arg(default_value = "origin")]
        remote: String,
        /// A branch or tag (default: the current branch)
        branch: Option<String>,
        /// Overwrite the remote branch even if it has commits you don't have (or replace a tag)
        #[arg(short, long)]
        force: bool,
        /// Push all tags (to git servers)
        #[arg(long)]
        tags: bool,
        /// Delete the named branch (or tag) on the remote instead
        #[arg(long, conflicts_with_all = ["tags", "force"])]
        delete: bool,
        /// Skip the pre-push hook
        #[arg(long = "no-verify")]
        no_verify: bool,
    },
    /// List tags, or name a commit (`copse tag v1.0`, or `-a -m "..."` for an annotated tag)
    Tag {
        name: Option<String>,
        /// The commit to tag (default: HEAD)
        rev: Option<String>,
        /// Make an annotated tag (with its own message, author and date)
        #[arg(short, long)]
        annotate: bool,
        /// The tag's message (implies --annotate)
        #[arg(short, long)]
        message: Option<String>,
        /// Sign the tag (implies --annotate; user.signingKey, gpg.format ssh or openpgp)
        #[arg(short = 's', long = "sign", conflicts_with = "no_sign")]
        sign: bool,
        /// Don't sign, even if tag.gpgSign is on
        #[arg(long = "no-sign")]
        no_sign: bool,
        /// Delete the tag
        #[arg(short, long)]
        delete: bool,
        /// Move a tag that already exists
        #[arg(short, long)]
        force: bool,
    },
    /// Pack all objects into one file (old versions as compact deltas) and remove unused ones
    Gc {
        /// How old unused objects must be before they're removed: now, never, or e.g. 14d, 2w (default 2w)
        #[arg(long, value_name = "WHEN")]
        prune: Option<String>,
    },
    /// Check every object is intact and everything branches and tags need is there
    Fsck,
    /// Show where HEAD (or a branch) has been: every commit, checkout, merge, rebase, pull...
    Reflog {
        /// A branch (default: HEAD)
        name: Option<String>,
        /// How many entries to show
        #[arg(short = 'n', long, default_value_t = 30)]
        limit: usize,
    },
    /// Put files or folders back as they were in the last commit (or in --from REV)
    Restore {
        #[arg(required = true)]
        paths: Vec<String>,
        /// Take them from this commit, branch or tag instead of the last commit
        #[arg(long, short = 's', visible_alias = "source")]
        from: Option<String>,
    },
    /// Move the current branch to another commit (keeping your files; --hard makes them match)
    Reset {
        /// Where to move to (default: HEAD)
        rev: Option<String>,
        /// Also make your files match it, discarding uncommitted changes (saved as a stash first)
        #[arg(long)]
        hard: bool,
        /// Keep your files as they are (the default)
        #[arg(long, conflicts_with = "hard")]
        soft: bool,
    },
    /// Undo the last change to the current branch (commit, merge, rebase, pull...); run again to redo
    Undo,
    /// Serve a repository over stdin/stdout (used by push, pull and fetch)
    #[command(hide = true)]
    Serve { path: PathBuf },
    /// Show what changed since the last commit
    #[command(alias = "st")]
    Status {
        /// A stable, plain format for scripts and editors (see the README)
        #[arg(long)]
        porcelain: bool,
    },
    /// Snapshot the whole working directory as a new commit
    #[command(alias = "ci")]
    Commit {
        /// Required, except when finishing a merge or amending
        #[arg(short, long)]
        message: Option<String>,
        /// Replace the last commit with your current files (keeping its message unless -m)
        #[arg(long)]
        amend: bool,
        /// Commit only changes under these files or folders; the rest stay uncommitted
        paths: Vec<String>,
        /// Skip the pre-commit and commit-msg hooks
        #[arg(short = 'n', long = "no-verify")]
        no_verify: bool,
        /// Pick which changes to commit, one at a time (like git add -p)
        #[arg(short = 'p', long = "patch")]
        patch: bool,
        /// Sign the commit (with user.signingKey; gpg.format ssh or openpgp)
        #[arg(short = 'S', long = "gpg-sign", conflicts_with = "no_sign")]
        sign: bool,
        /// Don't sign, even if commit.gpgSign is on
        #[arg(long = "no-gpg-sign")]
        no_sign: bool,
    },
    /// Git LFS: large files kept on an LFS server, with pointers in history
    Lfs {
        #[command(subcommand)]
        action: LfsAction,
    },
    /// The interactive screen (also what `copse` on its own opens)
    Ui {
        /// Press these keys (comma-separated) and print the screen, instead of running interactively
        #[arg(long, hide = true)]
        replay: Option<String>,
        /// Screen size for --replay, as WIDTHxHEIGHT
        #[arg(long, hide = true, default_value = "120x30")]
        size: String,
    },
    /// Keep a read-only .git folder up to date, for editors and tools that read .git
    GitView {
        #[command(subcommand)]
        action: Option<OnOff>,
    },
    /// Check commits' signatures (default: HEAD)
    VerifyCommit { revs: Vec<String> },
    /// Check tags' signatures
    VerifyTag { names: Vec<String> },
    /// Read or change settings (name, email, aliases, editor, ignore file, hooks folder)
    Config {
        /// A setting, such as user.name or alias.st
        key: Option<String>,
        /// The new value (leave out to show the current one)
        value: Option<String>,
        /// The settings file for every repository, instead of this one
        #[arg(long)]
        global: bool,
        /// Remove the setting
        #[arg(long, conflicts_with = "value")]
        unset: bool,
        /// Show every setting
        #[arg(short, long, conflicts_with_all = ["key", "unset"])]
        list: bool,
    },
    /// Show changes: your files vs the last commit, vs REV, or between two revisions (A B or A..B)
    Diff {
        /// Zero, one or two revisions (or A..B)
        revs: Vec<String>,
        /// A per-file summary instead of the full changes
        #[arg(long)]
        stat: bool,
        /// Only these files or folders (after --)
        #[arg(last = true)]
        paths: Vec<String>,
    },
    /// Show one commit (default: HEAD): who, when, its message, and what it changed
    Show {
        rev: Option<String>,
        /// A per-file summary instead of the full changes
        #[arg(long)]
        stat: bool,
    },
    /// Show commit history
    #[command(args_override_self = true)]
    Log {
        /// Where to start (default: HEAD)
        revs: Vec<String>,
        /// How many commits to show
        #[arg(short = 'n', long, default_value_t = 20)]
        limit: usize,
        /// One line per commit
        #[arg(long)]
        oneline: bool,
        /// Draw branch lines
        #[arg(long)]
        graph: bool,
        /// Show each commit's changes
        #[arg(short = 'p', long = "patch")]
        patch: bool,
        /// Show a per-file summary of each commit's changes
        #[arg(long)]
        stat: bool,
        /// Follow only the first parent of merges (the main line of history)
        #[arg(long)]
        first_parent: bool,
        /// Start from every branch, remote branch and tag
        #[arg(long)]
        all: bool,
        /// Only commits whose author contains this text
        #[arg(long)]
        author: Option<String>,
        /// Only commits whose message contains this text
        #[arg(long)]
        grep: Option<String>,
        /// Only commits that changed these files or folders (after --)
        #[arg(last = true)]
        paths: Vec<String>,
    },
    /// List branches, or create a new one at the current commit
    Branch {
        name: Option<String>,
        /// Where to create the branch (default: HEAD), or the new name with -m OLD NEW
        second: Option<String>,
        /// Delete the branch (only if its work is merged into the current branch)
        #[arg(short, long)]
        delete: bool,
        /// Delete the branch even if that leaves commits without a branch (same as -d --force)
        #[arg(short = 'D')]
        force_delete: bool,
        /// Rename a branch: -m NEW renames the current one, -m OLD NEW renames OLD
        #[arg(short = 'm', long = "move")]
        rename: bool,
        /// Rename even if the new name is taken (same as -m --force)
        #[arg(short = 'M')]
        force_rename: bool,
        /// With -d or -m: skip the safety check
        #[arg(short, long)]
        force: bool,
    },
    /// Replay the current branch's commits on top of another branch or commit
    Rebase {
        /// The branch or commit to rebase onto (with --onto: where the commits to move start)
        upstream: Option<String>,
        /// Move the commits after UPSTREAM onto this instead: copse rebase --onto NEW UPSTREAM
        #[arg(long, value_name = "NEWBASE")]
        onto: Option<String>,
        /// Choose what happens to each commit: reorder, reword, edit, squash, fixup, drop or exec
        #[arg(short, long)]
        interactive: bool,
        /// Fold `fixup! X` and `squash! X` commits into X
        #[arg(long)]
        autosquash: bool,
        /// Run a shell command after each commit; the rebase pauses if it fails
        #[arg(short = 'x', long = "exec", value_name = "CMD")]
        exec: Vec<String>,
        /// Skip the pre-rebase hook
        #[arg(long = "no-verify")]
        no_verify: bool,
        /// Carry on after fixing conflicts
        #[arg(long = "continue", conflicts_with_all = ["skip", "abort"])]
        cont: bool,
        /// Drop the commit that conflicted and carry on
        #[arg(long, conflicts_with = "abort")]
        skip: bool,
        /// Give up and put the branch back as it was
        #[arg(long)]
        abort: bool,
    },
    /// Put uncommitted changes aside (`copse stash`), or list / show / pop / apply / drop them
    #[command(args_conflicts_with_subcommands = true)]
    Stash {
        #[command(subcommand)]
        action: Option<StashAction>,
        /// A note to remember the stash by
        #[arg(short, long)]
        message: Option<String>,
        /// Stash only these files or folders (after --), leaving other changes in place
        #[arg(last = true)]
        paths: Vec<String>,
    },
    /// Remove new files that were never committed (lists them unless -f)
    Clean {
        /// Actually remove them (they're saved as a stash first)
        #[arg(short, long)]
        force: bool,
        /// Also remove ignored files, such as build output (these aren't backed up)
        #[arg(short = 'x', long = "ignored")]
        ignored: bool,
        /// Only under these files or folders
        paths: Vec<String>,
    },
    /// Search files (yours, or a revision's) with a regular expression
    Grep {
        pattern: String,
        /// Search this commit, branch or tag instead of your files
        rev: Option<String>,
        /// Ignore upper/lower case
        #[arg(short, long)]
        ignore_case: bool,
        /// List only the names of matching files
        #[arg(short = 'l', long = "files-with-matches")]
        files_only: bool,
        /// Count matching lines per file
        #[arg(short, long)]
        count: bool,
        /// Only these files or folders (after --)
        #[arg(last = true)]
        paths: Vec<String>,
    },
    /// Show who last changed each line of a file, and in which commit
    Blame {
        file: String,
        /// Blame the file as it is in this revision (default: your file as it is now)
        rev: Option<String>,
        /// Only these lines: START,END (END may be left off)
        #[arg(short = 'L', value_name = "START,END")]
        lines: Option<String>,
    },
    /// More working directories for this repository, each on its own branch
    Worktree {
        #[command(subcommand)]
        action: Option<WorktreeAction>,
    },
    /// Check out only some folders (sparse checkout), or all of them again
    Sparse {
        #[command(subcommand)]
        action: Option<SparseAction>,
    },
    /// Find the commit that introduced a problem, by binary search
    Bisect {
        #[command(subcommand)]
        action: Option<BisectAction>,
    },
    /// Copy commits from elsewhere onto the current branch
    CherryPick {
        /// Commits to apply, in order (or ranges A..B)
        revs: Vec<String>,
        /// For a merge commit: which parent to compare it with (1 = the branch it was merged into)
        #[arg(short = 'm', long, default_value_t = 0)]
        mainline: usize,
        /// Carry on after fixing conflicts
        #[arg(long = "continue", conflicts_with_all = ["skip", "abort"])]
        cont: bool,
        /// Drop the commit that conflicted and carry on
        #[arg(long, conflicts_with = "abort")]
        skip: bool,
        /// Give up and put the branch back as it was
        #[arg(long)]
        abort: bool,
    },
    /// Add commits that undo earlier commits
    Revert {
        /// Commits to apply, in order (or ranges A..B)
        revs: Vec<String>,
        /// For a merge commit: which parent to compare it with (1 = the branch it was merged into)
        #[arg(short = 'm', long, default_value_t = 0)]
        mainline: usize,
        /// Carry on after fixing conflicts
        #[arg(long = "continue", conflicts_with_all = ["skip", "abort"])]
        cont: bool,
        /// Drop the commit that conflicted and carry on
        #[arg(long, conflicts_with = "abort")]
        skip: bool,
        /// Give up and put the branch back as it was
        #[arg(long)]
        abort: bool,
    },
    /// Merge a branch or commit into the current branch
    Merge {
        target: Option<String>,
        /// Give up on a merge that stopped on conflicts
        #[arg(long)]
        abort: bool,
        /// Always make a merge commit, even when a fast-forward is possible
        #[arg(long = "no-ff", conflicts_with = "ff_only")]
        no_ff: bool,
        /// Only fast-forward; refuse if the branches have diverged
        #[arg(long = "ff-only")]
        ff_only: bool,
        /// Message for the merge commit
        #[arg(short, long)]
        message: Option<String>,
    },
    /// Switch to a branch or commit (a branch name, id prefix, HEAD~2, ...)
    #[command(alias = "co")]
    Checkout {
        target: String,
        /// Discard uncommitted changes to committed files (untracked files are kept)
        #[arg(short, long)]
        force: bool,
    },
}

#[derive(Subcommand)]
enum LfsAction {
    /// Store files matching these patterns with LFS (no patterns: list them)
    Track { patterns: Vec<String> },
    /// Stop storing these patterns with LFS
    Untrack {
        #[arg(required = true)]
        patterns: Vec<String>,
    },
    /// LFS files in the current commit (* = contents downloaded)
    LsFiles,
    /// Download missing LFS contents and write them into your files
    Pull,
    /// Upload the current branch's LFS contents
    Push { remote: Option<String> },
}

#[derive(Subcommand)]
enum OnOff {
    /// Turn it on
    On,
    /// Turn it off
    Off,
}

#[derive(Subcommand)]
enum SubmoduleAction {
    /// Each submodule, and whether it's on its pinned commit
    Status,
    /// Check each submodule out at its pinned commit
    Update {
        /// Clone the ones that aren't there yet
        #[arg(long)]
        init: bool,
    },
    /// Add a submodule (then commit)
    Add { url: String, path: String },
}

#[derive(Subcommand)]
enum WorktreeAction {
    /// List every working directory
    List,
    /// Check out a branch (default: one named after the folder) in a new folder
    Add {
        path: String,
        /// The branch (or with -b, where the new branch starts; or a commit, detached)
        rev: Option<String>,
        /// Create this new branch for the worktree
        #[arg(short = 'b', value_name = "NEW_BRANCH")]
        new_branch: Option<String>,
    },
    /// Delete a worktree's folder (refused if it has uncommitted changes, unless --force)
    Remove {
        path: String,
        #[arg(short, long)]
        force: bool,
    },
    /// Forget worktrees whose folders have been deleted
    Prune,
}

#[derive(Subcommand)]
enum SparseAction {
    /// Check out only these folders (plus top-level files)
    Set {
        #[arg(required = true)]
        folders: Vec<String>,
    },
    /// Add folders to the ones checked out
    Add {
        #[arg(required = true)]
        folders: Vec<String>,
    },
    /// Check every folder out again
    Disable,
}

#[derive(Subcommand)]
enum BisectAction {
    /// Begin: copse bisect start BAD GOOD... (both optional; mark them later)
    Start { bad: Option<String>, good: Vec<String> },
    /// The checked-out commit (or REV) doesn't have the problem
    Good { rev: Option<String> },
    /// The checked-out commit (or REV) has the problem
    Bad { rev: Option<String> },
    /// The checked-out commit (or REV) can't be tested; try another
    Skip { rev: Option<String> },
    /// End the bisect and go back to where you started
    Reset,
    /// Test each commit automatically: exit 0 = good, 125 = skip, anything else = bad
    Run {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        command: Vec<String>,
    },
}

#[derive(Subcommand)]
enum StashAction {
    /// List stashes, newest (0) first
    List,
    /// Show which files a stash changes
    Show {
        #[arg(default_value_t = 0)]
        n: usize,
    },
    /// Bring a stash back and remove it from the list
    Pop {
        #[arg(default_value_t = 0)]
        n: usize,
    },
    /// Bring a stash back but keep it in the list
    Apply {
        #[arg(default_value_t = 0)]
        n: usize,
    },
    /// Delete a stash
    Drop {
        #[arg(default_value_t = 0)]
        n: usize,
    },
}

#[derive(Subcommand)]
enum RemoteAction {
    /// Add a remote: a folder path, or user@host:path over SSH
    Add {
        name: String,
        url: String,
        /// Treat it as a git server (detected automatically for GitHub, GitLab, *.git, ...)
        #[arg(long)]
        git: bool,
    },
    /// Remove a remote
    Remove { name: String },
}

/// Exit quietly when our output goes to a pipe that closes early (`copse log | head`),
/// like other command-line tools, instead of panicking on the failed print.
///
/// This deliberately doesn't restore the default SIGPIPE signal: that would also kill
/// copse when a *remote* closes its end mid-transfer, before it could report why.
/// Broken pipes to a remote stay ordinary errors; only a broken stdout exits quietly.
fn quiet_on_closed_stdout() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let message = info
            .payload()
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| info.payload().downcast_ref::<&str>().copied())
            .unwrap_or_default();
        if message.starts_with("failed printing to stdout") && message.contains("Broken pipe") {
            std::process::exit(141); // the conventional exit status for "killed by SIGPIPE"
        }
        default_hook(info);
    }));
}

/// Whether `name` is one of copse's own commands (or their short forms).
pub fn is_builtin_command(name: &str) -> bool {
    name == "help" || Cli::command().get_subcommands().any(|c| c.get_name() == name || c.get_all_aliases().any(|a| a == name))
}

/// Replace an alias (`copse st` with alias.st = status) with what it stands for.
/// An alias starting with `!` is a shell command: it runs from the top of the
/// repository with the remaining arguments, and copse exits with its status.
fn expand_aliases(mut args: Vec<String>) -> Vec<String> {
    for _ in 0..10 {
        let Some(name) = args.get(1).filter(|a| !a.starts_with('-') && !is_builtin_command(a)).cloned() else { return args };
        let Some((_, value)) = config::aliases().into_iter().find(|(n, _)| *n == name) else { return args };
        if let Some(shell) = value.strip_prefix('!') {
            let dir = repo::Repo::find_any().map(|r| r.root).unwrap_or_else(|_| PathBuf::from("."));
            let status = std::process::Command::new("sh")
                .arg("-c")
                .arg(format!("{shell} \"$@\""))
                .arg(format!("copse {name}"))
                .args(&args[2..])
                .current_dir(dir)
                .status();
            match status {
                Ok(s) => std::process::exit(s.code().unwrap_or(128)),
                Err(e) => {
                    eprintln!("copse: can't run alias {name}: {e}");
                    std::process::exit(1);
                }
            }
        }
        match config::split_words(&value) {
            Ok(words) if !words.is_empty() => {
                args.splice(1..2, words);
            }
            Ok(_) => {
                eprintln!("copse: alias {name} is empty");
                std::process::exit(1);
            }
            Err(e) => {
                eprintln!("copse: {e}");
                std::process::exit(1);
            }
        }
    }
    eprintln!("copse: aliases refer to each other in a loop");
    std::process::exit(1);
}

fn main() {
    quiet_on_closed_stdout();
    let args = expand_aliases(std::env::args().collect());
    let cli = Cli::parse_from(&args);
    let Some(command) = &cli.command else {
        // `copse` on its own: the interactive screen in a terminal, the help otherwise.
        use std::io::IsTerminal;
        if std::io::stdout().is_terminal() && std::io::stdin().is_terminal() {
            if let Err(e) = tui::run() {
                eprintln!("copse: {e:#}");
                std::process::exit(1);
            }
        } else {
            let _ = Cli::command().print_help();
        }
        return;
    };
    // What the reflog records as the reason for any branch or HEAD move.
    reflog::set_reason(args[1..].join(" "));
    let result = match command {
        Command::Init { dir, bare } => cmd::init(dir.as_deref(), *bare),
        Command::Clone { url, dir, depth, recurse, filter } => remote::clone(url, dir.clone(), *depth, *recurse, filter.as_deref()),
        Command::Submodule { action } => submodules::submodule(match action {
            None | Some(SubmoduleAction::Status) => submodules::SubmoduleCmd::Status,
            Some(SubmoduleAction::Update { init }) => submodules::SubmoduleCmd::Update { init: *init },
            Some(SubmoduleAction::Add { url, path }) => submodules::SubmoduleCmd::Add { url, path },
        }),
        Command::Remote { action: None } => remote::list(),
        Command::Remote { action: Some(RemoteAction::Add { name, url, git }) } => remote::add(name, url, *git),
        Command::Remote { action: Some(RemoteAction::Remove { name }) } => remote::remove(name),
        Command::Fetch { remote, depth, unshallow } => remote::fetch(
            remote,
            match (depth, unshallow) {
                (_, true) => remote::Depth::Unshallow,
                (Some(n), _) => remote::Depth::Last(*n),
                _ => remote::Depth::Full,
            },
        ),
        Command::Pull { remote, branch, rebase } => remote::pull(remote, branch.as_deref(), *rebase),
        Command::Rebase { upstream, onto, interactive, autosquash, exec, no_verify, cont, skip, abort } => rebase::rebase(rebase::RebaseArgs {
            no_verify: *no_verify,
            upstream: upstream.as_deref(),
            onto: onto.as_deref(),
            interactive: *interactive,
            autosquash: *autosquash,
            exec,
            cont: *cont,
            skip: *skip,
            abort: *abort,
        }),
        Command::Push { remote, branch, force, tags, delete, no_verify } => {
            remote::push(remote, branch.as_deref(), *force, *tags, *delete, *no_verify)
        }
        Command::Tag { name, rev, annotate, message, sign, no_sign, delete, force } => tags::tag(tags::TagArgs {
            sign: if *sign { Some(true) } else if *no_sign { Some(false) } else { None },
            name: name.as_deref(),
            rev: rev.as_deref(),
            message: message.as_deref(),
            annotate: *annotate,
            delete: *delete,
            force: *force,
        }),
        Command::Serve { path } => transport::serve(path),
        Command::Gc { prune } => gc::gc(prune.as_deref()),
        Command::Fsck => integrity::fsck(),
        Command::Reflog { name, limit } => reflog::show(name.as_deref(), *limit),
        Command::Undo => reflog::undo(),
        Command::Clean { force, ignored, paths } => tools::clean(*force, *ignored, paths),
        Command::Grep { pattern, rev, ignore_case, files_only, count, paths } => tools::grep(tools::GrepArgs {
            pattern,
            rev: rev.as_deref(),
            paths,
            ignore_case: *ignore_case,
            files_only: *files_only,
            count: *count,
        }),
        Command::Blame { file, rev, lines } => investigate::blame(file, rev.as_deref(), lines.as_deref()),
        Command::Worktree { action } => worktrees::worktree(match action {
            None | Some(WorktreeAction::List) => worktrees::WorktreeCmd::List,
            Some(WorktreeAction::Add { path, rev, new_branch }) => {
                worktrees::WorktreeCmd::Add { path, rev: rev.as_deref(), new_branch: new_branch.as_deref() }
            }
            Some(WorktreeAction::Remove { path, force }) => worktrees::WorktreeCmd::Remove { path, force: *force },
            Some(WorktreeAction::Prune) => worktrees::WorktreeCmd::Prune,
        }),
        Command::Sparse { action } => sparse::sparse(match action {
            None => sparse::SparseCmd::Show,
            Some(SparseAction::Set { folders }) => sparse::SparseCmd::Set(folders),
            Some(SparseAction::Add { folders }) => sparse::SparseCmd::Add(folders),
            Some(SparseAction::Disable) => sparse::SparseCmd::Disable,
        }),
        Command::Bisect { action } => investigate::bisect(match action {
            None => investigate::BisectCmd::Status,
            Some(BisectAction::Start { bad, good }) => investigate::BisectCmd::Start { bad: bad.as_deref(), good },
            Some(BisectAction::Good { rev }) => investigate::BisectCmd::Good(rev.as_deref()),
            Some(BisectAction::Bad { rev }) => investigate::BisectCmd::Bad(rev.as_deref()),
            Some(BisectAction::Skip { rev }) => investigate::BisectCmd::Skip(rev.as_deref()),
            Some(BisectAction::Reset) => investigate::BisectCmd::Reset,
            Some(BisectAction::Run { command }) => investigate::BisectCmd::Run(command),
        }),
        Command::CherryPick { revs, mainline, cont, skip, abort } => {
            rebase::replay(revs, rebase::Op::CherryPick, *mainline, *cont, *skip, *abort)
        }
        Command::Revert { revs, mainline, cont, skip, abort } => rebase::replay(revs, rebase::Op::Revert, *mainline, *cont, *skip, *abort),
        Command::Restore { paths, from } => restore::restore(paths, from.as_deref()),
        Command::Reset { rev, hard, soft: _ } => restore::reset(rev.as_deref(), *hard),
        Command::Stash { action: None, message, paths } => stash::push(message.as_deref(), paths),
        Command::Stash { action: Some(StashAction::List), .. } => stash::list(),
        Command::Stash { action: Some(StashAction::Show { n }), .. } => stash::show(*n),
        Command::Stash { action: Some(StashAction::Pop { n }), .. } => stash::apply(*n, false),
        Command::Stash { action: Some(StashAction::Apply { n }), .. } => stash::apply(*n, true),
        Command::Stash { action: Some(StashAction::Drop { n }), .. } => stash::drop(*n),
        Command::Status { porcelain } => cmd::status(*porcelain),
        Command::Commit { message, amend, paths, no_verify, patch, sign, no_sign } => {
            let sign = if *sign { Some(true) } else if *no_sign { Some(false) } else { None };
            cmd::commit(message.as_deref(), paths, *amend, *no_verify, sign, *patch)
        }
        Command::VerifyCommit { revs } => signing::verify_commits(revs),
        Command::VerifyTag { names } => signing::verify_tags(names),
        Command::Lfs { action } => lfs::lfs(match action {
            LfsAction::Track { patterns } => lfs::LfsCmd::Track(patterns),
            LfsAction::Untrack { patterns } => lfs::LfsCmd::Untrack(patterns),
            LfsAction::LsFiles => lfs::LfsCmd::LsFiles,
            LfsAction::Pull => lfs::LfsCmd::Pull,
            LfsAction::Push { remote } => lfs::LfsCmd::Push(remote.as_deref()),
        }),
        Command::Ui { replay: Some(keys), size } => tui::replay(keys, size),
        Command::Ui { replay: None, .. } => tui::run(),
        Command::GitView { action } => gitview::git_view(action.as_ref().map(|a| matches!(a, OnOff::On))),
        Command::Config { key, value, global, unset, list } => config::config(config::ConfigArgs {
            key: key.as_deref(),
            value: value.as_deref(),
            global: *global,
            unset: *unset,
            list: *list,
        }),
        Command::Merge { target, abort, no_ff, ff_only, message } => {
            let ff = if *no_ff {
                merge::FastForward::Never
            } else if *ff_only {
                merge::FastForward::Only
            } else {
                merge::FastForward::Allow
            };
            merge::merge(target.as_deref(), *abort, ff, message.as_deref())
        }
        Command::Diff { revs, stat, paths } => diffs::diff(revs, paths, *stat),
        Command::Show { rev, stat } => diffs::show(rev.as_deref(), *stat),
        Command::Log { revs, limit, oneline, graph, patch, stat, first_parent, all, author, grep, paths } => logs::log(logs::LogArgs {
            revs,
            limit: *limit,
            oneline: *oneline,
            graph: *graph,
            patch: *patch,
            stat: *stat,
            first_parent: *first_parent,
            all: *all,
            author: author.as_deref(),
            grep: grep.as_deref(),
            paths,
        }),
        Command::Branch { name, second, delete, force_delete, rename, force_rename, force } => {
            branches::branch(branches::BranchArgs {
                name: name.as_deref(),
                second: second.as_deref(),
                delete: *delete || *force_delete,
                rename: *rename || *force_rename,
                force: *force || *force_delete || *force_rename,
            })
        }
        Command::Checkout { target, force } => cmd::checkout(target, *force),
    };
    if let Err(e) = result {
        eprintln!("copse: {e:#}");
        std::process::exit(1);
    }
    // Keep the .git view (if there is one) in step with whatever just changed.
    if !matches!(command, Command::Serve { .. } | Command::GitView { .. } | Command::Ui { .. }) {
        gitview::after_command();
    }
}
