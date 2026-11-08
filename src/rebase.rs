//! `copse rebase [-i] <onto>`: replay the current branch's commits on top of <onto>.
//!
//! Each commit is re-applied with a three-way merge: base = the commit's parent,
//! ours = the rebuilt branch so far, theirs = the commit. What happens next depends
//! on the commit's action (always `pick` unless `-i` let you choose):
//!   pick    new commit with the original author, time and message
//!   reword  same, then edit the message
//!   edit    same, then pause so the commit can be changed or split
//!   squash  fold into the previous commit, then edit the combined message
//!   fixup   fold into the previous commit, keeping its message
//!   drop    leave the commit out
//!   exec    run a shell command; if it fails, pause so you can fix things
//! `--onto NEWBASE UPSTREAM` moves the commits after UPSTREAM onto NEWBASE, and
//! `--autosquash` moves `fixup! X` / `squash! X` commits under X, folding them in.
//! A conflict pauses the rebase (state in .copse/REBASE_STATE) until `--continue`,
//! `--skip` or `--abort`.
//!
//! The branch ref doesn't move until the very end, so `--abort` (or a crash)
//! always leaves it exactly where it was. While rebasing, HEAD is detached at the
//! rebuilt commits.

use crate::cmd::{apply_tree, paint, print_changes};
use crate::merge::{ancestors, is_ancestor, merge_trees, stage_as_changes, with_markers, Files, MergeState};
use crate::objects::{flatten, parse_hex, short, write_tree, Commit, Id};
use crate::repo::{write_atomic, Head, Repo};
use crate::worktree::{changes, snapshot, Index};
use anyhow::{bail, Context, Result};
use std::collections::HashSet;
use std::fs;
use std::io::ErrorKind;
use std::path::Path;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    Pick,
    Reword,
    Edit,
    Squash,
    Fixup,
    Drop,
    /// Apply a commit's changes backwards (copse revert).
    Revert,
    /// Run a shell command between steps.
    Exec,
}

/// One line of the plan: an action on a commit, or (for exec) a command to run.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Step {
    pub action: Action,
    pub id: Id,
    pub cmd: String,
}

impl Step {
    fn new(action: Action, id: Id) -> Step {
        Step { action, id, cmd: String::new() }
    }

    fn exec(cmd: &str) -> Step {
        Step { action: Action::Exec, id: Id::from_bytes([0; 32]), cmd: cmd.to_string() }
    }
}

/// Which command is driving the replay engine: they share it, conflicts and all.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Op {
    Rebase,
    CherryPick,
    Revert,
}

impl Op {
    pub fn name(self) -> &'static str {
        match self {
            Op::Rebase => "rebase",
            Op::CherryPick => "cherry-pick",
            Op::Revert => "revert",
        }
    }

    fn parse(s: &str) -> Op {
        match s {
            "cherry-pick" => Op::CherryPick,
            "revert" => Op::Revert,
            _ => Op::Rebase,
        }
    }

    pub fn doing(self) -> &'static str {
        match self {
            Op::Rebase => "Rebasing",
            Op::CherryPick => "Cherry-picking",
            Op::Revert => "Reverting",
        }
    }
}

impl Action {
    fn parse(word: &str) -> Option<Action> {
        Some(match word {
            "p" | "pick" => Action::Pick,
            "r" | "reword" => Action::Reword,
            "e" | "edit" => Action::Edit,
            "s" | "squash" => Action::Squash,
            "f" | "fixup" => Action::Fixup,
            "d" | "drop" => Action::Drop,
            "revert" => Action::Revert,
            "x" | "exec" => Action::Exec,
            _ => return None,
        })
    }

    fn name(self) -> &'static str {
        match self {
            Action::Pick => "pick",
            Action::Reword => "reword",
            Action::Edit => "edit",
            Action::Squash => "squash",
            Action::Fixup => "fixup",
            Action::Drop => "drop",
            Action::Revert => "revert",
            Action::Exec => "exec",
        }
    }

    fn folds(self) -> bool {
        matches!(self, Action::Squash | Action::Fixup)
    }
}

pub struct RebaseState {
    pub op: Op,
    /// For merge commits being picked or reverted: which parent (1-based) to compare with.
    mainline: usize,
    pub branch: String,
    orig: Id,
    pub onto_label: String,
    /// Tip of the rebuilt branch so far.
    current: Id,
    /// The step the rebase is paused on: a conflict, or an `edit` pause.
    stopped: Option<(Action, Id)>,
    /// Paused by an `edit` action (not by a conflict).
    pub paused_for_edit: bool,
    /// Steps still to run, oldest first.
    todo: Vec<Step>,
    pub done: usize,
    pub total: usize,
    pub conflicts: Vec<String>,
    /// Files the stopped step created that the rebuilt branch doesn't have (removed on abort/skip).
    added: Vec<String>,
    /// -X options, kept for --continue.
    options: crate::merge::Options,
}

impl RebaseState {
    fn path(repo: &Repo) -> std::path::PathBuf {
        repo.wt.join("REBASE_STATE")
    }

    pub fn load(repo: &Repo) -> Result<Option<RebaseState>> {
        let text = match fs::read_to_string(Self::path(repo)) {
            Ok(t) => t,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let zero = Id::from_bytes([0; 32]);
        let mut s = RebaseState {
            op: Op::Rebase,
            mainline: 0,
            branch: String::new(),
            orig: zero,
            onto_label: String::new(),
            current: zero,
            stopped: None,
            paused_for_edit: false,
            todo: vec![],
            done: 0,
            total: 0,
            conflicts: vec![],
            added: vec![],
            options: crate::merge::Options::default(),
        };
        // "<action> <id>", or just "<id>" (a pick) for state written by older versions.
        let step = |value: &str| -> Result<(Action, Id)> {
            match value.split_once(' ') {
                Some((a, id)) => Ok((Action::parse(a).context("corrupt REBASE_STATE")?, parse_hex(id)?)),
                None => Ok((Action::Pick, parse_hex(value)?)),
            }
        };
        for line in text.lines() {
            let (key, value) = line.split_once(' ').context("corrupt REBASE_STATE")?;
            match key {
                "branch" => s.branch = value.to_string(),
                "orig" => s.orig = parse_hex(value)?,
                "onto" => s.onto_label = value.to_string(),
                "current" => s.current = parse_hex(value)?,
                "stopped" => s.stopped = Some(step(value)?),
                "paused" => s.paused_for_edit = value == "edit",
                "op" => s.op = Op::parse(value),
                "mainline" => s.mainline = value.parse().unwrap_or(0),
                "todo" => match value.strip_prefix("exec ") {
                    Some(cmd) => s.todo.push(Step::exec(cmd)),
                    None => {
                        let (action, id) = step(value)?;
                        s.todo.push(Step::new(action, id));
                    }
                },
                "done" => s.done = value.parse()?,
                "total" => s.total = value.parse()?,
                "C" => s.conflicts.push(value.to_string()),
                "A" => s.added.push(value.to_string()),
                "options" => s.options = crate::merge::Options::from_words(value),
                _ => {}
            }
        }
        if s.branch.is_empty() || s.orig == zero || s.current == zero {
            bail!("corrupt REBASE_STATE");
        }
        // Carrying on keeps the -X options the operation started with.
        crate::merge::set_options(s.options);
        Ok(Some(s))
    }

    fn save(&self, repo: &Repo) -> Result<()> {
        let mut t = format!(
            "branch {}\norig {}\nonto {}\ncurrent {}\ndone {}\ntotal {}\n",
            self.branch,
            self.orig.to_hex(),
            self.onto_label.replace('\n', " "),
            self.current.to_hex(),
            self.done,
            self.total
        );
        if let Some((a, id)) = self.stopped {
            t += &format!("stopped {} {}\n", a.name(), id.to_hex());
        }
        if self.paused_for_edit {
            t += "paused edit\n";
        }
        t += &format!("op {}\nmainline {}\n", self.op.name(), self.mainline);
        for step in &self.todo {
            if step.action == Action::Exec {
                t += &format!("todo exec {}\n", step.cmd.replace('\n', " "));
            } else {
                t += &format!("todo {} {}\n", step.action.name(), step.id.to_hex());
            }
        }
        for p in &self.conflicts {
            t += &format!("C {p}\n");
        }
        for p in &self.added {
            t += &format!("A {p}\n");
        }
        let words = self.options.to_words();
        if !words.is_empty() {
            t += &format!("options {words}\n");
        }
        write_atomic(&Self::path(repo), t.as_bytes())
    }

    fn clear(repo: &Repo) -> Result<()> {
        let _ = fs::remove_file(repo.wt.join("REBASE_TODO"));
        let _ = fs::remove_file(repo.wt.join("COMMIT_EDITMSG"));
        match fs::remove_file(Self::path(repo)) {
            Err(e) if e.kind() != ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        }
    }

    /// Every commit the operation in progress still needs (for gc and fsck).
    pub fn ids(&self) -> Vec<Id> {
        let mut out = vec![self.orig, self.current];
        out.extend(self.stopped.map(|(_, id)| id));
        out.extend(self.todo.iter().filter(|s| s.action != Action::Exec).map(|s| s.id));
        out
    }

    pub fn unresolved(&self, repo: &Repo) -> Vec<String> {
        with_markers(repo, &self.conflicts)
    }

    /// Short description of the commit the rebase is paused on, for `status`.
    pub fn paused_on(&self, repo: &Repo) -> Option<String> {
        let (_, id) = self.stopped?;
        let title = repo.read_commit(&id).ok()?.message.lines().next().unwrap_or_default().to_string();
        Some(format!("{} {title}", short(&id)))
    }
}

fn how_to_go_on(op: Op) -> String {
    let n = op.name();
    format!(
        "Fix the conflicts, then run `copse {n} --continue`.\n\
         To drop this commit instead: `copse {n} --skip`. To undo the whole {n}: `copse {n} --abort`."
    )
}

impl RebaseState {
    /// Why other commands have to wait, and what to run.
    pub fn busy(&self) -> String {
        let n = self.op.name();
        format!("a {n} is in progress: fix any conflicts and run `copse {n} --continue`, or `copse {n} --abort`")
    }
}

#[derive(Default)]
pub struct RebaseArgs<'a> {
    /// The branch or commit whose history the current branch should sit on.
    pub upstream: Option<&'a str>,
    /// `--onto NEWBASE`: put the commits after `upstream` on NEWBASE instead.
    pub onto: Option<&'a str>,
    pub interactive: bool,
    pub autosquash: bool,
    /// `-x CMD`: run CMD after each commit.
    pub exec: &'a [String],
    /// Skip the pre-rebase hook.
    pub no_verify: bool,
    pub cont: bool,
    pub skip: bool,
    pub abort: bool,
}

pub fn rebase(args: RebaseArgs) -> Result<()> {
    let repo = Repo::find()?;
    if args.cont {
        return continue_rebase(&repo);
    }
    if args.skip {
        return skip_commit(&repo);
    }
    if args.abort {
        return abort_rebase(&repo);
    }
    start(&repo, &args)
}

/// `copse cherry-pick` and `copse revert`: replay the given commits (or ranges A..B)
/// onto the current branch, as picks or as reverts.
pub fn replay(revs: &[String], op: Op, mainline: usize, cont: bool, skip: bool, abort: bool) -> Result<()> {
    let repo = Repo::find()?;
    if cont || skip || abort {
        if let Some(state) = RebaseState::load(&repo)? {
            if state.op != op {
                bail!("the operation in progress is a {}, not a {}; use `copse {} --continue` (or --skip, --abort)", state.op.name(), op.name(), state.op.name());
            }
        }
        return if cont { continue_rebase(&repo) } else if skip { skip_commit(&repo) } else { abort_rebase(&repo) };
    }
    if revs.is_empty() {
        bail!("say which commit(s): copse {} COMMIT...", op.name());
    }
    if let Some(state) = RebaseState::load(&repo)? {
        bail!("{}", state.busy());
    }
    if MergeState::load(&repo)?.is_some() {
        bail!("a merge is in progress; finish it with `copse commit` or undo it with `copse merge --abort` first");
    }
    let Head::Branch(branch) = repo.head()? else {
        bail!("HEAD is detached; switch to a branch first");
    };
    let orig = repo.branch(&branch)?.context("this branch has no commits yet")?;
    let index = Index::load(&repo.wt)?;
    let ch = changes(&index, &snapshot(&repo.root, &index, None)?);
    if !ch.is_empty() {
        print_changes(&ch);
        bail!("commit or stash your {} change(s) first", ch.len());
    }

    // Commits in the order given; a range A..B means B's commits that A doesn't have, oldest first.
    let action = if op == Op::Revert { Action::Revert } else { Action::Pick };
    let mut todo = Vec::new();
    for rev in revs {
        match rev.split_once("..") {
            Some((a, b)) => {
                let exclude = ancestors(&repo, repo.resolve(if a.is_empty() { "HEAD" } else { a })?)?;
                let (mut commits, _) = commits_to_replay(&repo, repo.resolve(if b.is_empty() { "HEAD" } else { b })?, &exclude)?;
                if op == Op::Revert {
                    commits.reverse(); // undo newest first
                }
                todo.extend(commits.into_iter().map(|id| Step::new(action, id)));
            }
            None => todo.push(Step::new(action, repo.resolve(rev)?)),
        }
    }
    if todo.is_empty() {
        println!("Nothing to {}.", op.name());
        return Ok(());
    }
    repo.set_head(&Head::Detached(orig))?;
    let state = RebaseState {
        op,
        mainline,
        branch: branch.clone(),
        orig,
        onto_label: branch.clone(),
        current: orig,
        stopped: None,
        paused_for_edit: false,
        total: todo.len(),
        todo,
        done: 0,
        conflicts: vec![],
        added: vec![],
        options: crate::merge::options(),
    };
    state.save(&repo)?;
    let prep = if op == Op::Revert { "on" } else { "onto" };
    println!("{} {} commit(s) {prep} {branch}", op.doing(), state.total);
    run(&repo, state)
}

fn start(repo: &Repo, args: &RebaseArgs) -> Result<()> {
    let interactive = args.interactive;
    let upstream = match (args.upstream, args.onto) {
        (Some(u), _) => u,
        (None, Some(o)) => o,
        (None, None) => bail!("say what to rebase onto, e.g. `copse rebase main` or `copse rebase -i HEAD~3`"),
    };
    // Where the commits go: --onto, or the upstream itself.
    let onto = args.onto.unwrap_or(upstream);
    // Lists that may rewrite or run things don't take the shortcuts below.
    let planned = interactive || args.autosquash || !args.exec.is_empty() || args.onto.is_some();
    if RebaseState::load(repo)?.is_some() {
        bail!("a rebase is already in progress: use `copse rebase --continue`, `--skip` or `--abort`");
    }
    if MergeState::load(repo)?.is_some() {
        bail!("a merge is in progress; finish it with `copse commit` or undo it with `copse merge --abort` first");
    }
    let Head::Branch(branch) = repo.head()? else {
        bail!("HEAD is detached; switch to the branch you want to rebase first");
    };
    let orig = repo.branch(&branch)?.context("this branch has no commits yet")?;
    let onto_id = repo.resolve(onto)?;
    let upstream_id = repo.resolve(upstream)?;
    if !args.no_verify {
        crate::hooks::check(repo, "pre-rebase", &[upstream, &branch], None, "rebase")?;
    }

    let mut index = Index::load(&repo.wt)?;
    let files = snapshot(&repo.root, &index, None)?;
    let ch = changes(&index, &files);
    if !ch.is_empty() {
        print_changes(&ch);
        bail!("commit or discard your {} change(s) before rebasing", ch.len());
    }

    // Interactive rebases onto an ancestor (`-i HEAD~3`) are how you edit recent history.
    if !planned && is_ancestor(repo, onto_id, orig)? {
        println!("Already up to date: {branch} already contains {onto}.");
        return Ok(());
    }
    if args.onto.is_none() && is_ancestor(repo, orig, onto_id)? && orig != onto_id {
        let tree = flatten(&repo.store, &repo.read_commit(&onto_id)?.tree)?;
        apply_tree(repo, &mut index, &files, &tree, &[])?;
        index.save(&repo.wt)?;
        repo.set_branch(&branch, &onto_id)?;
        println!("Fast-forwarded {branch} to {onto} ({}); it had no commits of its own.", short(&onto_id));
        return Ok(());
    }

    // Every non-merge commit on this branch that the upstream doesn't have, parents first.
    let upstream_has = ancestors(repo, upstream_id)?;
    let (commits, merges) = commits_to_replay(repo, orig, &upstream_has)?;
    if commits.is_empty() {
        if args.onto.is_some() && orig != onto_id {
            let tree = flatten(&repo.store, &repo.read_commit(&onto_id)?.tree)?;
            apply_tree(repo, &mut index, &files, &tree, &[])?;
            index.save(&repo.wt)?;
            repo.set_branch(&branch, &onto_id)?;
            println!("{branch} had no commits after {upstream}, so it now points at {onto} ({}).", short(&onto_id));
        } else {
            println!("Nothing to rebase: {branch} has no commits after {upstream}.");
        }
        return Ok(());
    }
    let mut plan: Vec<Step> = commits.iter().map(|id| Step::new(Action::Pick, *id)).collect();
    if args.autosquash {
        plan = autosquash(repo, plan)?;
    }
    if !args.exec.is_empty() {
        plan = add_execs(plan, args.exec);
    }
    let todo = if interactive {
        match plan_interactively(repo, &branch, onto, &plan)? {
            Some(todo) => todo,
            None => {
                println!("Nothing to do: the list was empty, so the rebase was cancelled.");
                return Ok(());
            }
        }
    } else {
        plan
    };
    let total = todo.len();

    let tree = flatten(&repo.store, &repo.read_commit(&onto_id)?.tree)?;
    apply_tree(repo, &mut index, &files, &tree, &[])?;
    index.save(&repo.wt)?;
    repo.set_head(&Head::Detached(onto_id))?;
    let state = RebaseState {
        op: Op::Rebase,
        mainline: 0,
        branch: branch.clone(),
        orig,
        onto_label: onto.to_string(),
        current: onto_id,
        stopped: None,
        paused_for_edit: false,
        todo,
        done: 0,
        total,
        conflicts: vec![],
        added: vec![],
        options: crate::merge::options(),
    };
    state.save(repo)?;

    let note = if merges > 0 { format!(" ({merges} merge commit(s) flattened away)") } else { String::new() };
    let commit_count = state.todo.iter().filter(|s| s.action != Action::Exec).count();
    println!("Rebasing {commit_count} commit(s) from {branch} onto {onto}{note}");
    run(repo, state)
}

/// Non-merge commits reachable from `tip` but not in `exclude`, in an order where
/// every commit comes after its parents. Returns them and the number of merges skipped.
fn commits_to_replay(repo: &Repo, tip: Id, exclude: &HashSet<Id>) -> Result<(Vec<Id>, usize)> {
    let mut order = Vec::new();
    let mut merges = 0;
    let mut visited = HashSet::new();
    // Iterative post-order DFS: (commit, parents already pushed?)
    let mut stack = vec![(tip, false)];
    while let Some((id, expanded)) = stack.pop() {
        if expanded {
            let c = repo.read_commit(&id)?;
            if c.parents.len() > 1 {
                merges += 1;
            } else {
                order.push(id);
            }
            continue;
        }
        if exclude.contains(&id) || !visited.insert(id) {
            continue;
        }
        stack.push((id, true));
        let c = repo.read_commit(&id)?;
        // Push in reverse so the first parent's history is replayed first.
        for p in c.parents.iter().rev() {
            stack.push((*p, false));
        }
    }
    Ok((order, merges))
}

/// `--autosquash`: move each `fixup! X` / `squash! X` (or `amend! X`) commit to just
/// after the commit it names (X is its title, an id prefix, or the start of its
/// title) and mark it fixup or squash. Commits naming nothing in the list stay put.
fn autosquash(repo: &Repo, plan: Vec<Step>) -> Result<Vec<Step>> {
    let mut titles = Vec::new();
    for step in &plan {
        titles.push(repo.read_commit(&step.id)?.message.lines().next().unwrap_or_default().to_string());
    }
    let plain = |j: usize| fold_prefix(&titles[j]).is_none();
    // For each commit: the folds that go after it, in their original order.
    let mut folds: Vec<Vec<(usize, Action)>> = vec![Vec::new(); plan.len()];
    let mut moved = vec![false; plan.len()];
    for i in 0..plan.len() {
        let Some((kind, mut rest)) = fold_prefix(&titles[i]) else { continue };
        // "fixup! fixup! X" folds into whatever X is.
        while let Some((_, inner)) = fold_prefix(rest) {
            rest = inner;
        }
        let rest = rest.trim();
        let target = (0..i)
            .find(|&j| plain(j) && titles[j] == rest)
            .or_else(|| (0..i).find(|&j| plain(j) && rest.len() >= 4 && plan[j].id.to_hex().starts_with(&rest.to_ascii_lowercase())))
            .or_else(|| (0..i).find(|&j| plain(j) && !rest.is_empty() && titles[j].starts_with(rest)));
        if let Some(j) = target {
            folds[j].push((i, kind));
            moved[i] = true;
        }
    }
    let mut out = Vec::with_capacity(plan.len());
    for i in 0..plan.len() {
        if moved[i] {
            continue;
        }
        out.push(plan[i].clone());
        for &(f, kind) in &folds[i] {
            out.push(Step::new(kind, plan[f].id));
        }
    }
    Ok(out)
}

/// The fold a commit title asks for: "fixup! X" -> (Fixup, "X").
fn fold_prefix(title: &str) -> Option<(Action, &str)> {
    if let Some(rest) = title.strip_prefix("fixup! ").or_else(|| title.strip_prefix("amend! ")) {
        return Some((Action::Fixup, rest));
    }
    title.strip_prefix("squash! ").map(|rest| (Action::Squash, rest))
}

/// `-x CMD`: run the commands after each finished commit. A commit with folds after
/// it isn't finished until the last fold.
fn add_execs(plan: Vec<Step>, cmds: &[String]) -> Vec<Step> {
    let mut out = Vec::new();
    for (i, step) in plan.iter().enumerate() {
        out.push(step.clone());
        let next_folds = plan.get(i + 1).map_or(false, |n| n.action.folds());
        if step.action != Action::Drop && !next_folds {
            out.extend(cmds.iter().map(|c| Step::exec(c)));
        }
    }
    out
}

// ---------------------------------------------------------------- the editor

/// $COPSE_EDITOR, then core.editor, then $VISUAL, $EDITOR, and finally vi.
fn editor() -> String {
    let env = |v: &str| std::env::var(v).ok().filter(|s| !s.trim().is_empty());
    env("COPSE_EDITOR")
        .or_else(|| crate::config::get("core.editor").filter(|s| !s.trim().is_empty()))
        .or_else(|| env("VISUAL"))
        .or_else(|| env("EDITOR"))
        .unwrap_or_else(|| "vi".to_string())
}

/// Open `path` in the user's editor and wait for it to close. The editor setting may
/// include arguments (e.g. "code --wait"), so it runs through the shell.
pub(crate) fn run_editor(path: &Path) -> Result<()> {
    let ed = editor();
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("{ed} \"$1\""))
        .arg("sh")
        .arg(path)
        .status()
        .with_context(|| format!("starting the editor ({ed})"))?;
    if !status.success() {
        bail!("the editor ({ed}) exited with an error");
    }
    Ok(())
}

/// Let the user edit a commit message. An empty result keeps `initial`.
fn edit_message(repo: &Repo, initial: &str, heading: &str) -> Result<String> {
    let path = repo.wt.join("COMMIT_EDITMSG");
    fs::write(
        &path,
        format!("{}\n\n# {heading}\n# Edit the message above. Lines starting with # are ignored;\n# an empty message keeps the original.\n", initial.trim_end()),
    )?;
    run_editor(&path)?;
    let text = fs::read_to_string(&path)?;
    let message: String = text.lines().filter(|l| !l.starts_with('#')).collect::<Vec<_>>().join("\n");
    let message = message.trim();
    Ok(if message.is_empty() { initial.trim().to_string() } else { message.to_string() })
}

/// Write the todo list, let the user edit it, and parse the result.
/// None means the user emptied the list (cancel).
fn plan_interactively(repo: &Repo, branch: &str, onto: &str, plan: &[Step]) -> Result<Option<Vec<Step>>> {
    let mut text = String::new();
    let mut commits = Vec::new();
    for step in plan {
        if step.action == Action::Exec {
            text += &format!("exec {}\n", step.cmd);
            continue;
        }
        commits.push(step.id);
        let title = repo.read_commit(&step.id)?.message.lines().next().unwrap_or_default().to_string();
        text += &format!("{} {} {title}\n", step.action.name(), short(&step.id));
    }
    text += &format!(
        "\n# Rebasing {} commit(s) from {branch} onto {onto}, oldest first.\n\
         # Change the word at the start of a line to choose what happens to that commit:\n\
         #   pick   = keep the commit\n\
         #   reword = keep it, but edit its message\n\
         #   edit   = keep it, then pause so you can change it (or split it with more commits)\n\
         #   squash = fold it into the commit above, and edit the combined message\n\
         #   fixup  = fold it into the commit above, keeping that commit's message\n\
         #   drop   = remove it (deleting the line does the same)\n\
         #   exec CMD = run CMD in the shell; if it fails, the rebase pauses so you can fix things\n\
         # Reorder the lines to reorder the commits. Delete every line to cancel.\n",
        commits.len()
    );
    let path = repo.wt.join("REBASE_TODO");
    fs::write(&path, text)?;
    run_editor(&path)?;
    let edited = fs::read_to_string(&path)?;
    let _ = fs::remove_file(&path);
    parse_todo(&edited, &commits)
}

fn parse_todo(text: &str, commits: &[Id]) -> Result<Option<Vec<Step>>> {
    let mut todo = Vec::new();
    let mut seen = HashSet::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut words = line.split_whitespace();
        let (word, prefix) = (words.next().unwrap_or_default(), words.next().unwrap_or_default());
        let unknown = || format!("line {}: unknown action {word:?} (use pick, reword, edit, squash, fixup, drop or exec)", n + 1);
        let action = Action::parse(word).filter(|a| *a != Action::Revert).with_context(unknown)?;
        if action == Action::Exec {
            let cmd = line[word.len()..].trim();
            if cmd.is_empty() {
                bail!("line {}: exec needs a command to run", n + 1);
            }
            todo.push(Step::exec(cmd));
            continue;
        }
        let prefix = prefix.to_ascii_lowercase();
        let matches: Vec<&Id> = commits.iter().filter(|id| !prefix.is_empty() && id.to_hex().starts_with(&prefix)).collect();
        let id = match matches.as_slice() {
            [id] => **id,
            [] => bail!("line {}: {prefix:?} isn't one of the commits being rebased", n + 1),
            _ => bail!("line {}: {prefix:?} matches more than one commit", n + 1),
        };
        if !seen.insert(id) {
            bail!("line {}: commit {prefix} is listed twice", n + 1);
        }
        todo.push(Step::new(action, id));
    }
    if todo.iter().all(|s| s.action == Action::Exec) {
        return Ok(None);
    }
    if let Some(Step { action, .. }) = todo.iter().find(|s| !matches!(s.action, Action::Drop | Action::Exec)) {
        if action.folds() {
            bail!("the first kept commit can't be `{}`: there's no commit above it to fold into", action.name());
        }
    }
    Ok(Some(todo))
}

// ---------------------------------------------------------------- replaying

/// Run the remaining steps until done, a conflict, or an `edit` pause.
fn run(repo: &Repo, mut state: RebaseState) -> Result<()> {
    while !state.todo.is_empty() {
        let step = state.todo.remove(0);
        let (action, id) = (step.action, step.id);
        if action == Action::Exec {
            state.done += 1;
            state.save(repo)?;
            println!("  {}/{} exec {}", state.done, state.total, step.cmd);
            let status = std::process::Command::new("sh")
                .arg("-c")
                .arg(&step.cmd)
                .current_dir(&repo.root)
                .status()
                .with_context(|| format!("can't run `{}`", step.cmd))?;
            let index = Index::load(&repo.wt)?;
            let dirty = !changes(&index, &snapshot(&repo.root, &index, None)?).is_empty();
            if !status.success() || dirty {
                // Pause on the commit just made, like `edit`: fix it, then --continue
                // folds the fix in and carries on with the rest.
                state.stopped = Some((Action::Edit, state.current));
                state.paused_for_edit = true;
                state.save(repo)?;
                let why = if status.success() {
                    "left uncommitted changes".to_string()
                } else {
                    format!("failed ({})", status.code().map_or("killed".to_string(), |c| format!("exit {c}")))
                };
                println!("`{}` {why}; paused at {}.", step.cmd, short(&state.current));
                println!("  Fix things, then `copse rebase --continue` (your changes are folded into this commit).");
                println!("  `copse rebase --abort` undoes the whole rebase.");
                return Ok(());
            }
            continue;
        }
        let commit = repo.read_commit(&id)?;
        let title = commit.message.lines().next().unwrap_or_default().to_string();
        if action == Action::Drop {
            state.done += 1;
            state.save(repo)?;
            println!("  {}/{} dropped {} {title}", state.done, state.total, short(&id));
            continue;
        }
        let mut index = Index::load(&repo.wt)?;
        let files = snapshot(&repo.root, &index, None)?;

        // Already sitting on this commit's parent: its tree applies as it is.
        if commit.parents.first() == Some(&state.current) && action != Action::Revert && commit.parents.len() <= 1 {
            let tree = flatten(&repo.store, &commit.tree)?;
            apply_tree(repo, &mut index, &files, &tree, &[])?;
            index.save(&repo.wt)?;
            if matches!(action, Action::Pick | Action::Edit) {
                // Keep the very same commit: nothing about it changes.
                state.current = id;
                state.done += 1;
                repo.set_head(&Head::Detached(id))?;
                println!("  {}/{} {} {title}", state.done, state.total, paint("33", &short(&id)));
                if action == Action::Edit {
                    pause_for_edit(repo, &mut state, id, &title)?;
                    return Ok(());
                }
                state.save(repo)?;
                continue;
            }
            finish_step(repo, &mut state, action, &commit, id, commit.tree)?;
            if state.paused_for_edit {
                return Ok(());
            }
            continue;
        }

        // A merge commit's changes depend on which parent they're measured against.
        let parent_index = if commit.parents.len() > 1 {
            if state.mainline == 0 {
                bail!("{} is a merge commit; say which parent to compare it with, e.g. --mainline 1", short(&id));
            }
            state.mainline - 1
        } else {
            0
        };
        let parent = match commit.parents.get(parent_index) {
            Some(p) => flatten(&repo.store, &repo.read_commit(p)?.tree)?,
            None if commit.parents.is_empty() => Files::new(),
            None => bail!("{} has no parent number {}", short(&id), state.mainline),
        };
        let ours = flatten(&repo.store, &repo.read_commit(&state.current)?.tree)?;
        let this = flatten(&repo.store, &commit.tree)?;
        // Reverting is applying the change backwards: from the commit to its parent.
        let (base, theirs, theirs_label) = if action == Action::Revert {
            (this, parent, format!("reverting {} {title}", short(&id)))
        } else {
            (parent, this, format!("{} {title}", short(&id)))
        };
        let (merged, conflicts) = merge_trees(repo, &base, &ours, &theirs, &state.onto_label, &theirs_label)?;

        if conflicts.is_empty() && merged == ours && action != Action::Squash {
            state.done += 1;
            state.save(repo)?;
            let why = if action == Action::Revert { "already undone" } else { "its changes are already there" };
            println!("  {}/{} skipped {} {title} ({why})", state.done, state.total, short(&id));
            continue;
        }

        apply_tree(repo, &mut index, &files, &merged, &[])?;

        if !conflicts.is_empty() {
            crate::sparse::write_conflicts(repo, &merged, conflicts.iter().map(|(p, _)| p))?;
            stage_as_changes(&mut index, &merged, &ours);
            index.save(&repo.wt)?;
            state.stopped = Some((action, id));
            state.conflicts = conflicts.iter().map(|(p, _)| p.clone()).collect();
            state.added = merged.keys().filter(|p| !ours.contains_key(*p)).cloned().collect();
            state.save(repo)?;
            println!("Conflict while applying {} {title} ({}/{}):", short(&id), state.done + 1, state.total);
            for (path, reason) in &conflicts {
                println!("  {} {path}  {}", paint("31", "C"), paint("2", reason));
            }
            println!("{}", how_to_go_on(state.op));
            return Ok(());
        }

        let tree = write_tree(&repo.store, merged.iter().map(|(p, (m, i))| (p.as_str(), *m, *i)))?;
        index.save(&repo.wt)?;
        finish_step(repo, &mut state, action, &commit, id, tree)?;
        if state.paused_for_edit {
            return Ok(());
        }
    }
    finish(repo, state)
}

/// Record a replayed step whose resulting tree is `tree` (already in the working
/// directory and index), according to its action.
fn finish_step(repo: &Repo, state: &mut RebaseState, action: Action, original: &Commit, original_id: Id, tree: Id) -> Result<()> {
    let title = original.message.lines().next().unwrap_or_default().to_string();
    let (new, verb) = match action {
        Action::Pick | Action::Edit => (replay_commit(repo, original, tree, state.current, None)?, ""),
        Action::Reword => {
            let message = edit_message(repo, &original.message, &format!("New message for {} {title}", short(&original_id)))?;
            (replay_commit(repo, original, tree, state.current, Some(message))?, "reworded ")
        }
        Action::Squash | Action::Fixup => {
            let previous = repo.read_commit(&state.current)?;
            let message = if action == Action::Squash {
                // A "squash! X" title only said where the commit goes; leave it out.
                let body = match original.message.split_once('\n') {
                    Some((first, rest)) if first.starts_with("squash! ") => rest.trim().to_string(),
                    None if original.message.starts_with("squash! ") => String::new(),
                    _ => original.message.trim_end().to_string(),
                };
                let combined = if body.is_empty() {
                    previous.message.trim_end().to_string()
                } else {
                    format!("{}\n\n{body}", previous.message.trim_end())
                };
                edit_message(repo, &combined, "Message for the combined commit")?
            } else {
                previous.message.clone()
            };
            let commit = Commit::rewritten(&previous, tree, previous.parents.clone(), message);
            let verb = if action == Action::Squash { "squashed " } else { "fixed up " };
            (crate::signing::store(repo, commit)?, verb)
        }
        Action::Revert => {
            // Name the commit the way the people who'll read this know it: by its git id
            // if it has been to (or come from) a git server, else by copse's.
            let named = crate::gitwire::GitMap::load(repo)
                .ok()
                .and_then(|m| m.git(&original_id))
                .map_or_else(|| original_id.to_hex().to_string(), |g| crate::gitwire::hex(&g));
            let message = format!("Revert \"{title}\"\n\nThis reverts commit {named}.\n");
            let commit = Commit::new(tree, vec![state.current], message);
            (crate::signing::store(repo, commit)?, "reverted as ")
        }
        Action::Drop | Action::Exec => unreachable!("drops and execs never reach finish_step"),
    };
    state.current = new;
    state.done += 1;
    repo.set_head(&Head::Detached(new))?;
    let shown = if action.folds() { format!("{} into {}", short(&original_id), short(&new)) } else { short(&new) };
    println!("  {}/{} {verb}{} {title}", state.done, state.total, paint("33", &shown));
    if action == Action::Edit {
        return pause_for_edit(repo, state, original_id, &title);
    }
    state.save(repo)
}

fn pause_for_edit(repo: &Repo, state: &mut RebaseState, original_id: Id, title: &str) -> Result<()> {
    state.stopped = Some((Action::Edit, original_id));
    state.paused_for_edit = true;
    state.save(repo)?;
    println!("Paused at {} {title} so you can change it.", short(&state.current));
    println!("  Edit files, then `copse rebase --continue` to fold the changes into this commit.");
    println!("  To split it, `copse commit` new commits here first. `copse rebase --abort` undoes everything.");
    Ok(())
}

/// A copy of `original` with a new tree and parent, keeping its author and authored
/// time (and its message, unless a new one is given).
fn replay_commit(repo: &Repo, original: &Commit, tree: Id, parent: Id, message: Option<String>) -> Result<Id> {
    let commit = Commit::rewritten(original, tree, vec![parent], message.unwrap_or_else(|| original.message.clone()));
    crate::signing::store(repo, commit)
}

fn finish(repo: &Repo, state: RebaseState) -> Result<()> {
    repo.set_branch(&state.branch, &state.current)?;
    repo.set_head(&Head::Branch(state.branch.clone()))?;
    RebaseState::clear(repo)?;
    let to = paint("33", &short(&state.current));
    // How many commits were actually added (skipped ones don't count).
    let mut added = 0;
    let mut at = state.current;
    while at != state.orig {
        added += 1;
        match repo.read_commit(&at)?.parents.first() {
            Some(p) => at = *p,
            None => break,
        }
    }
    match state.op {
        Op::Rebase => println!("Rebased {} onto {}: {} → {to}", state.branch, state.onto_label, short(&state.orig)),
        Op::CherryPick => println!("Cherry-picked {added} commit(s) onto {}: now at {to}", state.branch),
        Op::Revert => println!("Reverted {added} commit(s) on {}: now at {to}", state.branch),
    }
    Ok(())
}

// ---------------------------------------------------------------- continue / skip / abort

fn continue_rebase(repo: &Repo) -> Result<()> {
    let mut state = RebaseState::load(repo)?.context("nothing to continue: no rebase, cherry-pick or revert is in progress")?;
    let Some((action, stopped)) = state.stopped else {
        return run(repo, state); // interrupted between steps; just carry on
    };

    if state.paused_for_edit {
        // The user may have committed during the pause (HEAD moved) and/or changed files.
        let head = repo.head_commit()?.context("HEAD has no commit")?;
        state.current = head;
        let mut index = Index::load(&repo.wt)?;
        let files = snapshot(&repo.root, &index, Some(&repo.store))?;
        if !changes(&index, &files).is_empty() {
            let c = repo.read_commit(&head)?;
            let tree = write_tree(&repo.store, files.iter().map(|f| (f.path.as_str(), f.mode, f.id)))?;
            let amended = Commit::rewritten(&c, tree, c.parents.clone(), c.message.clone());
            let new = crate::signing::store(repo, amended)?;
            index.entries = files.iter().map(|f| (f.path.clone(), f.entry())).collect();
            index.save(&repo.wt)?;
            state.current = new;
            repo.set_head(&Head::Detached(new))?;
            println!("  amended {} {}", paint("33", &short(&new)), c.message.lines().next().unwrap_or_default());
        }
        state.stopped = None;
        state.paused_for_edit = false;
        state.save(repo)?;
        return run(repo, state);
    }

    let unresolved = state.unresolved(repo);
    if !unresolved.is_empty() {
        for p in &unresolved {
            println!("  {} {p}", paint("31", "C"));
        }
        bail!("{} file(s) still have conflict markers; fix them, then run `copse rebase --continue` again", unresolved.len());
    }

    let commit = repo.read_commit(&stopped)?;
    let title = commit.message.lines().next().unwrap_or_default().to_string();
    let mut index = Index::load(&repo.wt)?;
    let files = snapshot(&repo.root, &index, Some(&repo.store))?;
    state.stopped = None;
    state.conflicts.clear();
    state.added.clear();
    if changes(&index, &files).is_empty() && action != Action::Squash {
        state.done += 1;
        println!("  {}/{} skipped {} {title} (nothing left after resolving)", state.done, state.total, short(&stopped));
        state.save(repo)?;
    } else {
        let tree = write_tree(&repo.store, files.iter().map(|f| (f.path.as_str(), f.mode, f.id)))?;
        index.entries = files.iter().map(|f| (f.path.clone(), f.entry())).collect();
        index.save(&repo.wt)?;
        finish_step(repo, &mut state, action, &commit, stopped, tree)?;
        if state.paused_for_edit {
            return Ok(());
        }
    }
    run(repo, state)
}

fn skip_commit(repo: &Repo) -> Result<()> {
    let mut state = RebaseState::load(repo)?.context("nothing to skip: no rebase, cherry-pick or revert is in progress")?;
    if state.paused_for_edit {
        bail!("the rebase is paused for an edit, not a conflict; run `copse rebase --continue` to carry on");
    }
    let (_, stopped) = state.stopped.context("the rebase isn't stopped on a commit, so there's nothing to skip")?;
    restore(repo, state.current, &state.added)?;
    state.done += 1;
    state.stopped = None;
    state.conflicts.clear();
    state.added.clear();
    state.save(repo)?;
    println!("  {}/{} skipped {}", state.done, state.total, short(&stopped));
    run(repo, state)
}

fn abort_rebase(repo: &Repo) -> Result<()> {
    let state = RebaseState::load(repo)?.context("nothing to abort: no rebase, cherry-pick or revert is in progress")?;
    restore(repo, state.orig, &state.added)?;
    repo.set_head(&Head::Branch(state.branch.clone()))?;
    RebaseState::clear(repo)?;
    let n = state.op.name();
    println!("{}{} aborted; {} is back at {}.", n[..1].to_uppercase(), &n[1..], state.branch, short(&state.orig));
    Ok(())
}

/// Put the working directory back to exactly `commit`, discarding a conflicted step.
fn restore(repo: &Repo, commit: Id, extra_remove: &[String]) -> Result<()> {
    let mut index = Index::load(&repo.wt)?;
    let files = snapshot(&repo.root, &index, None)?;
    let tree = flatten(&repo.store, &repo.read_commit(&commit)?.tree)?;
    apply_tree(repo, &mut index, &files, &tree, extra_remove)?;
    index.save(&repo.wt)
}

#[cfg(test)]
mod tests {
    use super::{parse_todo, Action, Step};
    use crate::objects::Id;

    fn ids() -> Vec<Id> {
        vec![Id::from_bytes([0xaa; 32]), Id::from_bytes([0xbb; 32]), Id::from_bytes([0xcc; 32])]
    }

    #[test]
    fn parses_actions_reordering_and_comments() {
        let todo = parse_todo("# comment\ns bbbbbb x\npick aaaa first\n\nd cccccc\n", &ids());
        // squash can't come first...
        assert!(todo.is_err());
        let todo = parse_todo("pick aaaa\nfixup bbbbbb\ndrop cccc\n", &ids()).unwrap().unwrap();
        assert_eq!(todo, vec![Step::new(Action::Pick, ids()[0]), Step::new(Action::Fixup, ids()[1]), Step::new(Action::Drop, ids()[2])]);
        let todo = parse_todo("r cccc\nexec make test\np aaaa\n", &ids()).unwrap().unwrap();
        assert_eq!(todo, vec![Step::new(Action::Reword, ids()[2]), Step::exec("make test"), Step::new(Action::Pick, ids()[0])]);
    }

    #[test]
    fn rejects_bad_lists() {
        assert!(parse_todo("bogus aaaa\n", &ids()).unwrap_err().to_string().contains("unknown action"));
        assert!(parse_todo("pick dddd\n", &ids()).unwrap_err().to_string().contains("isn't one of"));
        assert!(parse_todo("pick aaaa\npick aaaa\n", &ids()).unwrap_err().to_string().contains("listed twice"));
        assert!(parse_todo("drop aaaa\nfixup bbbb\n", &ids()).unwrap_err().to_string().contains("first kept commit"));
        assert!(parse_todo("pick aaaa\nexec\n", &ids()).unwrap_err().to_string().contains("needs a command"));
    }

    #[test]
    fn empty_list_cancels() {
        assert!(parse_todo("# nothing here\n\n", &ids()).unwrap().is_none());
    }
}
