//! `copse diff` and `copse show`.
//!
//!   copse diff                     your files against the last commit
//!   copse diff REV                 your files against REV
//!   copse diff A B   (or A..B)     between two commits, branches or tags
//!   copse show [REV]               one commit: who, when, why, and what it changed
//!
//! Both take `--stat` for git's per-file summary, and `diff` takes `-- PATHS` to
//! look at only some files or folders.

use crate::cmd::{civil_time, paint};
use crate::merge::Files;
use crate::objects::{flatten, short, Id, Kind, Mode, Tag};
use crate::repo::Repo;
use crate::worktree::{read_content, snapshot, Index};
use anyhow::{bail, Result};
use similar::{ChangeTag, TextDiff};
use std::collections::{BTreeSet, HashMap};

/// One side of a comparison: a commit's files, or the working directory.
struct Side {
    files: Files,
    worktree: bool,
}

impl Side {
    fn commit(repo: &Repo, id: &Id) -> Result<Side> {
        Ok(Side { files: flatten(&repo.store, &repo.read_commit(id)?.tree)?, worktree: false })
    }

    fn empty() -> Side {
        Side { files: Files::new(), worktree: false }
    }

    fn working_files(repo: &Repo) -> Result<Side> {
        if repo.is_bare() {
            bail!("this repository has no working files; compare two revisions instead (copse diff A B)");
        }
        let index = Index::load(&repo.wt)?;
        let files = snapshot(&repo.root, &index, None)?;
        Ok(Side { files: files.into_iter().map(|f| (f.path, (f.mode, f.id))).collect(), worktree: true })
    }

    fn content(&self, repo: &Repo, path: &str, mode: Mode, id: &Id) -> Result<Vec<u8>> {
        if mode == Mode::Submodule {
            // Shown the way git shows it.
            return Ok(format!("Subproject commit {}\n", crate::objects::gitlink_hex(id)).into_bytes());
        }
        if self.worktree {
            // An LFS file is compared as its pointer, as git (without git-lfs's diff driver) does.
            if matches!(mode, Mode::File | Mode::Exec) && crate::lfs::Attrs::load(&repo.root).tracked(path) {
                return crate::lfs::clean(&repo.dir, &repo.root.join(path), false);
            }
            Ok(read_content(&repo.root.join(path), mode)?)
        } else {
            repo.store.get_kind(id, Kind::Blob)
        }
    }
}

fn git_mode(mode: Mode) -> &'static str {
    match mode {
        Mode::File => "100644",
        Mode::Exec => "100755",
        Mode::Link => "120000",
        Mode::Dir => "040000",
        Mode::Submodule => "160000",
    }
}

fn as_text(bytes: &[u8]) -> Option<&str> {
    std::str::from_utf8(bytes).ok().filter(|t| !t.contains('\0'))
}

struct FileChange {
    path: String,
    old: Option<(Mode, Id)>,
    new: Option<(Mode, Id)>,
}

/// Every path that differs between the two sides (optionally only under `specs`).
fn changed(old: &Side, new: &Side, specs: &[String]) -> Vec<FileChange> {
    let wanted = |p: &str| specs.is_empty() || specs.iter().any(|s| s.is_empty() || p == s || p.starts_with(&format!("{s}/")));
    let paths: BTreeSet<&String> = old.files.keys().chain(new.files.keys()).filter(|p| wanted(p)).collect();
    paths
        .into_iter()
        .filter_map(|p| {
            let (o, n) = (old.files.get(p).copied(), new.files.get(p).copied());
            (o != n).then(|| FileChange { path: p.clone(), old: o, new: n })
        })
        .collect()
}

/// Print the differences between two sides, in full or as a `--stat` summary.
/// Returns the number of files that differ.
fn print_diff(repo: &Repo, old: &Side, new: &Side, specs: &[String], stat: bool) -> Result<usize> {
    let changes = changed(old, new, specs);
    let mut stats = Vec::new();
    for ch in &changes {
        let a = match ch.old {
            Some((m, id)) => old.content(repo, &ch.path, m, &id)?,
            None => Vec::new(),
        };
        let b = match ch.new {
            Some((m, id)) => new.content(repo, &ch.path, m, &id)?,
            None => Vec::new(),
        };
        if stat {
            stats.push((ch.path.clone(), line_counts(&a, &b)));
        } else {
            print_file(ch, &a, &b);
        }
    }
    if stat {
        print_stat(&stats);
    }
    Ok(changes.len())
}

/// (insertions, deletions), or None for a binary file.
fn line_counts(a: &[u8], b: &[u8]) -> Option<(usize, usize)> {
    let (a, b) = (as_text(a)?, as_text(b)?);
    let diff = TextDiff::from_lines(a, b);
    let (mut ins, mut del) = (0, 0);
    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Insert => ins += 1,
            ChangeTag::Delete => del += 1,
            ChangeTag::Equal => {}
        }
    }
    Some((ins, del))
}

fn print_file(ch: &FileChange, a: &[u8], b: &[u8]) {
    println!("{}", paint("1", &format!("diff {}", ch.path)));
    match (ch.old, ch.new) {
        (None, Some((m, _))) => println!("new file, mode {}", git_mode(m)),
        (Some((m, _)), None) => println!("deleted file, mode {}", git_mode(m)),
        (Some((om, _)), Some((nm, _))) if om != nm => println!("mode changed from {} to {}", git_mode(om), git_mode(nm)),
        _ => {}
    }
    if a == b {
        return; // only the mode changed
    }
    let (Some(a_text), Some(b_text)) = (as_text(a), as_text(b)) else {
        println!("Binary file differs");
        return;
    };
    let a_name = if ch.old.is_some() { format!("a/{}", ch.path) } else { "/dev/null".into() };
    let b_name = if ch.new.is_some() { format!("b/{}", ch.path) } else { "/dev/null".into() };
    let text = TextDiff::from_lines(a_text, b_text).unified_diff().context_radius(3).header(&a_name, &b_name).to_string();
    for line in text.lines() {
        let colored = if line.starts_with("+++") || line.starts_with("---") {
            paint("1", line)
        } else if line.starts_with("@@") {
            paint("36", line)
        } else if line.starts_with('+') {
            paint("32", line)
        } else if line.starts_with('-') {
            paint("31", line)
        } else {
            line.to_string()
        };
        println!("{colored}");
    }
}

/// git's summary: ` path | 12 +++++++---` per file, then totals.
fn print_stat(stats: &[(String, Option<(usize, usize)>)]) {
    if stats.is_empty() {
        return;
    }
    let width = stats.iter().map(|(p, _)| p.chars().count()).max().unwrap_or(0);
    let most = stats.iter().filter_map(|(_, c)| c.map(|(i, d)| i + d)).max().unwrap_or(0);
    let num_width = most.to_string().len();
    const BAR: usize = 40;
    let (mut total_ins, mut total_del) = (0, 0);
    for (path, counts) in stats {
        match counts {
            None => println!(" {path:<width$} | {:>num_width$}", "Bin"),
            Some((ins, del)) => {
                total_ins += ins;
                total_del += del;
                let (mut i, mut d) = (*ins, *del);
                if most > BAR {
                    // Scale the bar, but never hide a change completely.
                    i = if *ins > 0 { (ins * BAR / most).max(1) } else { 0 };
                    d = if *del > 0 { (del * BAR / most).max(1) } else { 0 };
                }
                println!(" {path:<width$} | {:>num_width$} {}{}", ins + del, paint("32", &"+".repeat(i)), paint("31", &"-".repeat(d)));
            }
        }
    }
    let plural = |n: usize, one: &str, many: &str| if n == 1 { format!("{n} {one}") } else { format!("{n} {many}") };
    // git's wording: both counts when both are zero, otherwise only the non-zero ones.
    let mut summary = format!(" {}", plural(stats.len(), "file changed", "files changed"));
    let both_zero = total_ins == 0 && total_del == 0;
    if total_ins > 0 || both_zero {
        summary += &format!(", {}", plural(total_ins, "insertion(+)", "insertions(+)"));
    }
    if total_del > 0 || both_zero {
        summary += &format!(", {}", plural(total_del, "deletion(-)", "deletions(-)"));
    }
    println!("{summary}");
}

// ---------------------------------------------------------------- commands

/// `copse diff [REV [REV]] [--stat] [-- PATHS]`
pub fn diff(revs: &[String], paths: &[String], stat: bool) -> Result<()> {
    let repo = Repo::find_any()?;
    let commit = |rev: &str| -> Result<Side> {
        let rev = if rev.is_empty() { "HEAD" } else { rev };
        Side::commit(&repo, &repo.resolve(rev)?)
    };
    let (old, new) = match revs {
        [] => {
            let head = match repo.head_commit()? {
                Some(h) => Side::commit(&repo, &h)?,
                None => Side::empty(),
            };
            (head, Side::working_files(&repo)?)
        }
        [range] if range.contains("..") => {
            let (a, b) = range.split_once("..").unwrap();
            (commit(a)?, commit(b)?)
        }
        [rev] => (commit(rev)?, Side::working_files(&repo)?),
        [a, b] => (commit(a)?, commit(b)?),
        _ => bail!("give at most two revisions: copse diff A B"),
    };
    let specs = paths.iter().map(|p| crate::restore::repo_path(&repo, p)).collect::<Result<Vec<_>>>()?;
    print_diff(&repo, &old, &new, &specs, stat)?;
    Ok(())
}

/// What one commit changed (compared with its first parent), for `log -p` / `--stat`.
pub fn commit_changes(repo: &Repo, id: &Id, specs: &[String], stat: bool) -> Result<()> {
    let c = repo.read_commit(id)?;
    let parent = match c.parents.first() {
        Some(p) => Side::commit(repo, p)?,
        None => Side::empty(),
    };
    let new = Side { files: flatten(&repo.store, &c.tree)?, worktree: false };
    print_diff(repo, &parent, &new, specs, stat)?;
    Ok(())
}

/// A date in the timezone it was recorded in, like git: "2026-11-07 14:02 -0500".
fn local_date(secs: i64, tz: &str) -> String {
    let sign = if tz.starts_with('-') { -1 } else { 1 };
    let digits = tz.trim_start_matches(['+', '-']);
    let offset = match (digits.get(..2).and_then(|h| h.parse::<i64>().ok()), digits.get(2..4).and_then(|m| m.parse::<i64>().ok())) {
        (Some(h), Some(m)) => sign * (h * 3600 + m * 60),
        _ => 0,
    };
    format!("{} {tz}", civil_time(secs + offset))
}

/// `copse show [REV] [--stat]`
pub fn show(rev: Option<&str>, stat: bool) -> Result<()> {
    let repo = Repo::find_any()?;
    let rev = rev.unwrap_or("HEAD");
    // REV:path, or a file's or folder's own id: its contents, as git shows them.
    if rev.contains(':') || matches!(repo.resolve_object(rev).and_then(|id| Ok(repo.store.get(&id)?.0)), Ok(Kind::Blob | Kind::Tree)) {
        return crate::plumbing::show_path(&repo, rev);
    }

    // An annotated tag shows its own details first.
    if let Some(tag_id) = repo.tag(rev)? {
        let (kind, data) = repo.store.get(&tag_id)?;
        if kind == Kind::Tag {
            let tag = Tag::decode(&data)?;
            println!("{}", paint("33", &format!("tag {}", tag.name)));
            if let Some(t) = &tag.tagger {
                println!("Tagger: {}", t.who);
                println!("Date:   {}", local_date(t.time, &t.tz));
            }
            println!();
            for line in tag.message.lines() {
                println!("    {line}");
            }
            println!();
        }
    }

    let id = repo.resolve(rev)?;
    let c = repo.read_commit(&id)?;
    let mut labels: HashMap<Id, Vec<String>> = HashMap::new();
    for (name, bid) in repo.branches()?.into_iter().chain(repo.tracking_refs()?) {
        labels.entry(bid).or_default().push(name);
    }
    for (name, tid) in repo.tags()? {
        if let Ok(commit) = repo.peel(tid) {
            labels.entry(commit).or_default().push(format!("tag: {name}"));
        }
    }
    let deco = labels.get(&id).map(|n| paint("32", &format!(" ({})", n.join(", ")))).unwrap_or_default();
    println!("{}{deco}", paint("33", &format!("commit {}", id.to_hex())));
    if c.parents.len() > 1 {
        println!("Merge:  {}", c.parents.iter().map(short).collect::<Vec<_>>().join(" "));
    }
    println!("Author: {}", c.author.who);
    println!("Date:   {}", local_date(c.author.time, &c.author.tz));
    if c.committer.who != c.author.who {
        println!("Commit: {}", c.committer.who);
    }
    println!();
    for line in c.message.lines() {
        println!("    {line}");
    }
    println!();

    let parent = match c.parents.first() {
        Some(p) => Side::commit(&repo, p)?,
        None => Side::empty(),
    };
    if c.parents.len() > 1 {
        println!("{}", paint("2", &format!("(changes compared with the first parent, {})", short(&c.parents[0]))));
    }
    let new = Side { files: flatten(&repo.store, &c.tree)?, worktree: false };
    print_diff(&repo, &parent, &new, &[], stat)?;
    Ok(())
}
