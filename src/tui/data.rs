//! What the TUI shows, read from the repository, and how it acts: every action
//! runs the real `copse` command (the same code the command line uses) and
//! captures what it printed.

use crate::objects::{short, Id};
use crate::repo::{Head, Repo};
use crate::worktree::{changes, snapshot, Index};
use anyhow::Result;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Clone, Debug, PartialEq)]
pub struct Change {
    pub path: String,
    /// 'A' added, 'M' modified, 'D' deleted.
    pub kind: char,
}

#[derive(Clone, Debug)]
pub struct LogRow {
    /// The graph drawing at the start of the line ("* ", "| * ", "|/ "...).
    pub graph: String,
    /// The commit's short id (None for lines that only continue the graph).
    pub id: Option<String>,
    pub labels: String,
    pub title: String,
}

#[derive(Clone, Debug)]
pub struct BranchRow {
    pub name: String,
    pub id: Id,
    pub current: bool,
    pub remote: bool,
    /// Commits ahead of / behind its remote branch (local branches with one).
    pub ahead_behind: Option<(usize, usize)>,
    pub title: String,
}

#[derive(Clone, Debug)]
pub struct UndoRow {
    pub n: usize,
    pub id: Option<Id>,
    pub time: i64,
    pub reason: String,
    pub title: String,
}

/// Run `copse ARGS` in the repository. Returns (succeeded, everything it printed).
pub fn run_copse(exe: &Path, root: &Path, args: &[&str]) -> (bool, String) {
    let out = Command::new(exe)
        .args(args)
        .current_dir(root)
        .env("COPSE_EDITOR", "true") // nothing may open an editor under the TUI
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .output();
    match out {
        Ok(o) => {
            let mut text = String::from_utf8_lossy(&o.stdout).into_owned();
            let err = String::from_utf8_lossy(&o.stderr);
            if !err.trim().is_empty() {
                if !text.is_empty() && !text.ends_with('\n') {
                    text.push('\n');
                }
                text.push_str(&err);
            }
            (o.status.success(), text)
        }
        Err(e) => (false, format!("couldn't run copse: {e}")),
    }
}

pub fn load_changes(repo: &Repo) -> Result<Vec<Change>> {
    let index = Index::load(&repo.wt)?;
    let files = snapshot(&repo.root, &index, None)?;
    let ch = changes(&index, &files);
    let mut out: Vec<Change> = Vec::with_capacity(ch.len());
    out.extend(ch.added.into_iter().map(|path| Change { path, kind: 'A' }));
    out.extend(ch.modified.into_iter().map(|path| Change { path, kind: 'M' }));
    out.extend(ch.deleted.into_iter().map(|path| Change { path, kind: 'D' }));
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

fn is_hex_id(s: &str) -> bool {
    s.len() == 10 && s.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// The commit graph, from `copse log --graph --oneline --all` (the same drawing as the command line).
pub fn load_history(exe: &Path, root: &Path, limit: usize) -> Vec<LogRow> {
    let n = limit.to_string();
    let (_, text) = run_copse(exe, root, &["log", "--graph", "--oneline", "--all", "-n", &n]);
    text.lines()
        .filter(|l| !l.is_empty())
        .map(|line| {
            // The id is the first 10-hex-digit word; everything before it is graph.
            let mut at = None;
            let mut pos = 0;
            for word in line.split(' ') {
                if is_hex_id(word) {
                    at = Some(pos);
                    break;
                }
                pos += word.len() + 1;
            }
            match at {
                None => LogRow { graph: line.to_string(), id: None, labels: String::new(), title: String::new() },
                Some(p) => {
                    let graph = line[..p].to_string();
                    let rest = &line[p + 10..];
                    let rest = rest.strip_prefix(' ').unwrap_or(rest);
                    let (labels, title) = match rest.strip_prefix('(').and_then(|r| r.split_once(") ")) {
                        Some((l, t)) => (l.to_string(), t.to_string()),
                        None => (String::new(), rest.to_string()),
                    };
                    LogRow { graph, id: Some(line[p..p + 10].to_string()), labels, title }
                }
            }
        })
        .collect()
}

/// Commits reachable from `from` and not from `not`, counted up to a limit (so a
/// huge history can't stall the screen). None past the limit.
fn count_only_in(repo: &Repo, from: Id, not: Id) -> Option<usize> {
    const LIMIT: usize = 20_000;
    let reach = |start: Id| -> Option<HashSet<Id>> {
        let mut seen = HashSet::new();
        let mut stack = vec![start];
        while let Some(id) = stack.pop() {
            if seen.insert(id) {
                if seen.len() > LIMIT {
                    return None;
                }
                if let Ok(c) = repo.read_commit(&id) {
                    stack.extend(c.parents);
                }
            }
        }
        Some(seen)
    };
    let a = reach(from)?;
    let b = reach(not)?;
    Some(a.difference(&b).count())
}

pub fn ahead_behind(repo: &Repo, local: Id, remote: Id) -> Option<(usize, usize)> {
    if local == remote {
        return Some((0, 0));
    }
    Some((count_only_in(repo, local, remote)?, count_only_in(repo, remote, local)?))
}

fn title_of(repo: &Repo, id: &Id) -> String {
    repo.read_commit(id).map(|c| c.message.lines().next().unwrap_or_default().to_string()).unwrap_or_default()
}

pub fn load_branches(repo: &Repo) -> Result<Vec<BranchRow>> {
    let current = match repo.head()? {
        Head::Branch(b) => Some(b),
        Head::Detached(_) => None,
    };
    let mut out = Vec::new();
    for (name, id) in repo.branches()? {
        let ahead_behind = repo.tracking("origin", &name).ok().flatten().filter(|t| repo.store.contains(t)).and_then(|t| ahead_behind(repo, id, t));
        out.push(BranchRow { current: current.as_deref() == Some(name.as_str()), title: title_of(repo, &id), name, id, remote: false, ahead_behind });
    }
    for (name, id) in repo.tracking_refs()? {
        out.push(BranchRow { name, title: title_of(repo, &id), id, current: false, remote: true, ahead_behind: None });
    }
    Ok(out)
}

pub fn load_undo(repo: &Repo) -> Result<Vec<UndoRow>> {
    Ok(crate::reflog::read(&repo.wt, "HEAD")?
        .into_iter()
        .take(300)
        .enumerate()
        .map(|(n, e)| UndoRow { n, id: e.new, time: e.time, title: e.new.map(|i| title_of(repo, &i)).unwrap_or_default(), reason: e.reason })
        .collect())
}

/// "repo · main · ↑1 ↓2 · 3 changes": the top line.
pub fn header(repo: &Repo, changes: usize) -> String {
    let name = repo.root.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let mut parts = vec![name];
    match repo.head() {
        Ok(Head::Branch(b)) => {
            let mut s = b.clone();
            if let (Ok(Some(local)), Ok(Some(remote))) = (repo.branch(&b), repo.tracking("origin", &b)) {
                if let Some((a, bh)) = ahead_behind(repo, local, remote) {
                    s += &match (a, bh) {
                        (0, 0) => "  in step with origin".to_string(),
                        (a, 0) => format!("  ↑{a} to push"),
                        (0, b) => format!("  ↓{b} to pull"),
                        (a, b) => format!("  ↑{a} ↓{b}"),
                    };
                }
            }
            parts.push(s);
        }
        Ok(Head::Detached(id)) => parts.push(format!("detached at {}", short(&id))),
        Err(_) => {}
    }
    parts.push(match changes {
        0 => "no changes".to_string(),
        1 => "1 change".to_string(),
        n => format!("{n} changes"),
    });
    parts.join("  ·  ")
}

pub fn ago(then: i64) -> String {
    let secs = (crate::identity::now().0 - then).max(0);
    match secs {
        s if s < 60 => "just now".into(),
        s if s < 3600 => format!("{}m ago", s / 60),
        s if s < 86_400 => format!("{}h ago", s / 3600),
        s => format!("{}d ago", s / 86_400),
    }
}

pub fn exe() -> PathBuf {
    std::env::current_exe().unwrap_or_else(|_| PathBuf::from("copse"))
}
