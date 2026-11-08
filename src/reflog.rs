//! The reflog: a record of every move of HEAD and of each branch, so nothing a
//! command did is ever out of reach. `copse undo` uses it to put things back.
//!
//! `.copse/logs/HEAD` and `.copse/logs/refs/<branch>`, one line per move:
//!   <old id> <new id> <unix time> <tz> <what did it>
//! A missing side (a branch being created or deleted) is all zeros. A branch's log
//! outlives the branch, so a deleted branch can be found again (`name@{1}`).

use crate::cmd::{apply_tree, format_time, paint, print_changes};
use crate::merge::MergeState;
use crate::objects::{flatten, parse_hex, short, Id};
use crate::rebase::RebaseState;
use crate::repo::{validate_branch, Head, Repo};
use crate::worktree::{changes, snapshot, Index};
use anyhow::{bail, Context, Result};
use std::fs;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const NONE: &str = "0000000000000000000000000000000000000000000000000000000000000000";

static REASON: Mutex<String> = Mutex::new(String::new());

/// What's currently moving refs (shown in `copse reflog`). Set from the command line
/// at startup, and more precisely by commands such as commit.
pub fn set_reason(reason: impl Into<String>) {
    *REASON.lock().unwrap() = reason.into().replace('\n', " ");
}

fn reason() -> String {
    REASON.lock().unwrap().clone()
}

fn log_path(dir: &Path, refname: &str) -> PathBuf {
    if refname == "HEAD" {
        dir.join("logs").join("HEAD")
    } else {
        dir.join("logs").join("refs").join(crate::repo::ref_file(refname))
    }
}

/// Record that `refname` ("HEAD" or a branch) moved from `old` to `new`.
pub fn record(dir: &Path, refname: &str, old: Option<Id>, new: Option<Id>) -> Result<()> {
    let path = log_path(dir, refname);
    fs::create_dir_all(path.parent().unwrap())?;
    let (time, tz) = crate::identity::now();
    let id = |x: Option<Id>| x.map_or_else(|| NONE.to_string(), |i| i.to_hex().to_string());
    let line = format!("{} {} {time} {tz} {}\n", id(old), id(new), reason());
    fs::OpenOptions::new().create(true).append(true).open(&path)?.write_all(line.as_bytes())?;
    Ok(())
}

/// Carry a branch's history over to its new name.
pub fn rename(dir: &Path, old: &str, new: &str) -> Result<()> {
    let (from, to) = (log_path(dir, old), log_path(dir, new));
    match fs::rename(&from, &to) {
        Err(e) if e.kind() != ErrorKind::NotFound => Err(e.into()),
        _ => Ok(()),
    }
}

pub struct Entry {
    pub old: Option<Id>,
    pub new: Option<Id>,
    pub time: i64,
    pub reason: String,
}

/// The log of `refname`, newest first.
pub fn read(dir: &Path, refname: &str) -> Result<Vec<Entry>> {
    let text = match fs::read_to_string(log_path(dir, refname)) {
        Ok(t) => t,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let id = |s: &str| -> Result<Option<Id>> { if s == NONE { Ok(None) } else { parse_hex(s).map(Some) } };
    let mut out = Vec::new();
    for line in text.lines().filter(|l| !l.is_empty()) {
        let mut parts = line.splitn(5, ' ');
        let (Some(old), Some(new), Some(time), Some(_tz)) = (parts.next(), parts.next(), parts.next(), parts.next()) else {
            continue; // a torn line from a crash: skip it
        };
        out.push(Entry { old: id(old)?, new: id(new)?, time: time.parse().unwrap_or(0), reason: parts.next().unwrap_or("").to_string() });
    }
    out.reverse();
    Ok(out)
}

/// Where `refname`'s log lives: HEAD's belongs to the working directory (each
/// worktree has its own), branches' are shared.
fn log_dir<'a>(repo: &'a Repo, refname: &str) -> &'a Path {
    if refname == "HEAD" { &repo.wt } else { &repo.dir }
}

/// Every commit any reflog mentions, with the log it's in (for gc and fsck): these
/// stay, so `copse undo` and `name@{n}` keep working.
pub fn all_ids(dir: &Path) -> Result<Vec<(Id, String)>> {
    let mut logs = vec![("HEAD".to_string(), dir.join("logs").join("HEAD"))];
    // Each extra worktree's HEAD log too.
    if let Ok(entries) = fs::read_dir(dir.join("worktrees")) {
        for e in entries.flatten() {
            logs.push((format!("HEAD of worktree {}", e.file_name().to_string_lossy()), e.path().join("logs").join("HEAD")));
        }
    }
    if let Ok(entries) = fs::read_dir(dir.join("logs").join("refs")) {
        for e in entries.flatten() {
            let name = crate::repo::ref_name(&e.file_name().to_string_lossy());
            logs.push((name, e.path()));
        }
    }
    let mut out = Vec::new();
    for (name, path) in logs {
        let Ok(text) = fs::read_to_string(&path) else { continue };
        for line in text.lines() {
            for word in line.split(' ').take(2) {
                if word != NONE {
                    if let Ok(id) = parse_hex(word) {
                        out.push((id, format!("reflog of {name}")));
                    }
                }
            }
        }
    }
    Ok(out)
}

/// `name@{n}`: where `name` ("HEAD" or a branch) was `n` moves ago.
pub fn resolve(repo: &Repo, name: &str, n: usize) -> Result<Id> {
    let name = if name.is_empty() { "HEAD" } else { name };
    let entries = read(log_dir(&repo, name), name)?;
    if entries.is_empty() {
        bail!("{name} has no recorded history yet");
    }
    // Position 0 is where it is now; each older entry's "new" is where it was before.
    let positions: Vec<Option<Id>> = entries.iter().map(|e| e.new).chain(entries.last().map(|e| e.old)).collect();
    match positions.get(n) {
        Some(Some(id)) => Ok(*id),
        Some(None) => bail!("{name}@{{{n}}}: {name} didn't exist at that point"),
        None => bail!("{name}@{{{n}}}: the history only goes back {} moves", positions.len() - 1),
    }
}

fn ago(then: i64) -> String {
    let secs = (crate::identity::now().0 - then).max(0);
    match secs {
        s if s < 60 => "just now".to_string(),
        s if s < 3600 => format!("{} min ago", s / 60),
        s if s < 86400 => format!("{} h ago", s / 3600),
        s => format!("{} days ago", s / 86400),
    }
}

// ---------------------------------------------------------------- commands

/// `copse reflog [branch]`
pub fn show(name: Option<&str>, limit: usize) -> Result<()> {
    let repo = Repo::find_any()?;
    let name = name.unwrap_or("HEAD");
    if name != "HEAD" {
        validate_branch(name)?;
    }
    let entries = read(log_dir(&repo, name), name)?;
    if entries.is_empty() {
        println!("No recorded history for {name} yet.");
    }
    for (i, e) in entries.iter().take(limit).enumerate() {
        let at = e.new.map_or_else(|| "(deleted) ".to_string(), |id| short(&id));
        println!("{}  {}  {}  {}", paint("33", &format!("{name}@{{{i}}}")), at, paint("2", &format_time(e.time)), e.reason);
    }
    Ok(())
}

/// `copse undo`: put the current branch (and your files) back where they were before
/// the last thing that moved it. Running it again redoes.
pub fn undo() -> Result<()> {
    let repo = Repo::find()?;
    if MergeState::load(&repo)?.is_some() {
        bail!("a merge is in progress; to undo it, run `copse merge --abort`");
    }
    if let Some(r) = RebaseState::load(&repo)? {
        bail!("a {} is in progress; to undo it, run `copse {} --abort`", r.op.name(), r.op.name());
    }
    let head = repo.head()?;
    let refname = match &head {
        Head::Branch(b) => b.clone(),
        Head::Detached(_) => "HEAD".to_string(),
    };
    let entries = read(log_dir(&repo, &refname), &refname)?;
    let last = entries.iter().find(|e| e.old != e.new).with_context(|| format!("nothing to undo: {refname} has no recorded moves"))?;
    let Some(target) = last.old else {
        bail!("nothing to undo: {refname} was created by `{}`", last.reason);
    };
    let tree = flatten(&repo.store, &repo.read_commit(&target)?.tree)?;

    // Uncommitted changes are only a problem if undo would overwrite them. If your
    // files already match where undo is going (say, after `copse reset`), it's safe.
    let mut index = Index::load(&repo.wt)?;
    let files = snapshot(&repo.root, &index, None)?;
    let ch = changes(&index, &files);
    let already_there = files.len() == tree.len() && files.iter().all(|f| tree.get(&f.path) == Some(&(f.mode, f.id)));
    if !ch.is_empty() && !already_there {
        print_changes(&ch);
        bail!("you have {} uncommitted change(s); commit or stash them first, so undo can't lose them", ch.len());
    }

    set_reason(format!("undo: {}", last.reason));
    apply_tree(&repo, &mut index, &files, &tree, &[])?;
    index.save(&repo.wt)?;
    match head {
        Head::Branch(b) => repo.set_branch(&b, &target)?,
        Head::Detached(_) => repo.set_head(&Head::Detached(target))?,
    }
    let title = repo.read_commit(&target)?.message.lines().next().unwrap_or_default().to_string();
    println!("Undid `{}` ({}).", last.reason, ago(last.time));
    println!("  {refname} is back at {} {title}", paint("33", &short(&target)));
    println!("  Run `copse undo` again to redo it. `copse reflog` shows the full history.");
    Ok(())
}
