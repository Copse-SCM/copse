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

thread_local! {
    /// Put before every line of diff output: the graph's padding, when `log --graph`
    /// shows a commit's changes (as git draws the graph down beside them).
    static PREFIX: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };
}

/// A line of diff output, after the current prefix.
macro_rules! out {
    () => { PREFIX.with(|p| println!("{}", p.borrow())) };
    ($($arg:tt)*) => { PREFIX.with(|p| println!("{}{}", p.borrow(), format!($($arg)*))) };
}

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
                return crate::lfs::clean(&repo.dir, &crate::objects::disk_path(&repo.root, path), false);
            }
            Ok(read_content(&crate::objects::disk_path(&repo.root, path), mode)?)
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
            stats.push((ch.path.clone(), line_counts(&a, &b).ok_or((a.len(), b.len()))));
        } else {
            print_file(ch, &a, &b);
        }
    }
    if stat {
        print_stat(&stats);
    }
    Ok(changes.len())
}

/// (insertions, deletions), or None for a binary file: as in git, one with a
/// zero byte in its first 8000 (text in another encoding than UTF-8 is counted
/// line by line too).
fn line_counts(a: &[u8], b: &[u8]) -> Option<(usize, usize)> {
    let binary = |d: &[u8]| d[..d.len().min(8000)].contains(&0);
    if binary(a) || binary(b) {
        return None;
    }
    let (a, b) = (String::from_utf8_lossy(a), String::from_utf8_lossy(b));
    let (a, b) = (a.as_ref(), b.as_ref());
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
    out!("{}", paint("1", &format!("diff {}", crate::objects::quote_path(&ch.path))));
    match (ch.old, ch.new) {
        (None, Some((m, _))) => out!("new file, mode {}", git_mode(m)),
        (Some((m, _)), None) => out!("deleted file, mode {}", git_mode(m)),
        (Some((om, _)), Some((nm, _))) if om != nm => out!("mode changed from {} to {}", git_mode(om), git_mode(nm)),
        _ => {}
    }
    if a == b {
        return; // only the mode changed
    }
    let (Some(a_text), Some(b_text)) = (as_text(a), as_text(b)) else {
        out!("Binary file differs");
        return;
    };
    let a_name = if ch.old.is_some() { crate::objects::quote_path(&format!("a/{}", ch.path)).into_owned() } else { "/dev/null".into() };
    let b_name = if ch.new.is_some() { crate::objects::quote_path(&format!("b/{}", ch.path)).into_owned() } else { "/dev/null".into() };
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
        out!("{colored}");
    }
}

/// The width `--stat` lays itself out in, as git works it out: $COLUMNS, else the
/// terminal's width, else (output to a file or a pipe) 80.
fn stat_width() -> usize {
    if let Some(n) = std::env::var("COLUMNS").ok().and_then(|c| c.trim().parse::<usize>().ok()).filter(|n| *n > 0) {
        return n;
    }
    use std::io::IsTerminal;
    if std::io::stdout().is_terminal() {
        let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
        if unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) } == 0 && ws.ws_col > 0 {
            return ws.ws_col as usize;
        }
    }
    80
}

/// git's `scale_linear`: at least one column for any change.
fn scale(n: usize, width: usize, most: usize) -> usize {
    if n == 0 { 0 } else { 1 + n * (width - 1) / most }
}

/// git's summary: ` path | 12 +++++++---` per file (binary files: ` path | Bin
/// 3 -> 5 bytes`), laid out and scaled exactly as git's `show_stats` does, then
/// the totals. Counts are (insertions, deletions), or a binary file's sizes.
fn print_stat(stats: &[(String, Result<(usize, usize), (usize, usize)>)]) {
    if stats.is_empty() {
        return;
    }
    let stats: Vec<(String, Result<(usize, usize), (usize, usize)>)> = stats.iter().map(|(p, c)| (crate::objects::quote_path(p).into_owned(), *c)).collect();
    let digits = |n: usize| n.to_string().len();
    let width_of = crate::pretty::display_width;
    let max_len = stats.iter().map(|(p, _)| width_of(p)).max().unwrap_or(0);
    let most = stats.iter().filter_map(|(_, c)| c.ok().map(|(i, d)| i + d)).max().unwrap_or(0);
    let bin_width = stats.iter().filter_map(|(_, c)| c.err().map(|(old, new)| 14 + digits(old) + digits(new))).max().unwrap_or(0);
    let mut number_width = if bin_width > 0 { 3 } else { 0 };
    number_width = number_width.max(digits(most));
    let width = stat_width().max(16 + 6 + number_width);
    let mut graph_width = if most + 4 > bin_width { most } else { bin_width - 4 };
    let mut name_width = max_len;
    if name_width + number_width + 6 + graph_width > width {
        let three_eighths = (width * 3 / 8).saturating_sub(number_width + 6);
        if graph_width > three_eighths {
            graph_width = three_eighths.max(6);
        }
        let room = width.saturating_sub(number_width + 6 + graph_width);
        if name_width > room {
            name_width = room;
        } else {
            graph_width = width - number_width - 6 - name_width;
        }
    }
    let (mut total_ins, mut total_del) = (0, 0);
    for (path, counts) in &stats {
        // A name too long for its column keeps its end, from a `/` where it can.
        let (prefix, name) = if width_of(path) > name_width {
            let tail = crate::pretty::fit(path, name_width.saturating_sub(3), true);
            ("...", tail.find('/').map_or(tail.clone(), |i| tail[i..].to_string()))
        } else {
            ("", path.clone())
        };
        let pad = name_width.saturating_sub(prefix.len() + width_of(&name));
        let shown = format!("{prefix}{name}{}", " ".repeat(pad));
        match counts {
            Err((old, new)) => {
                if old == new && *old == 0 {
                    out!(" {shown} | {:>number_width$}", "Bin");
                } else {
                    out!(" {shown} | {:>number_width$} {} -> {} bytes", "Bin", paint("31", &old.to_string()), paint("32", &new.to_string()));
                }
            }
            Ok((ins, del)) => {
                total_ins += ins;
                total_del += del;
                let (mut i, mut d) = (*ins, *del);
                if graph_width <= most {
                    let mut total = scale(ins + del, graph_width, most);
                    if total < 2 && *ins > 0 && *del > 0 {
                        total = 2;
                    }
                    if ins < del {
                        i = scale(*ins, graph_width, most);
                        d = total - i;
                    } else {
                        d = scale(*del, graph_width, most);
                        i = total - d;
                    }
                }
                let space = if ins + del > 0 { " " } else { "" };
                out!(" {shown} | {:>number_width$}{space}{}{}", ins + del, paint("32", &"+".repeat(i)), paint("31", &"-".repeat(d)));
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
    out!("{summary}");
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
/// A commit's changes (against its first parent), each line after `prefix`.
pub fn commit_changes(repo: &Repo, id: &Id, specs: &[String], stat: bool, prefix: &str) -> Result<()> {
    PREFIX.with(|p| *p.borrow_mut() = prefix.to_string());
    let result = commit_changes_inner(repo, id, specs, stat);
    PREFIX.with(|p| p.borrow_mut().clear());
    result
}

fn commit_changes_inner(repo: &Repo, id: &Id, specs: &[String], stat: bool) -> Result<()> {
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
            out!("{}", paint("33", &format!("tag {}", tag.name)));
            if let Some(t) = &tag.tagger {
                out!("Tagger: {}", t.who);
                out!("Date:   {}", local_date(t.time, &t.tz));
            }
            out!();
            for line in tag.message.lines() {
                out!("    {line}");
            }
            out!();
        }
    }

    let id = repo.resolve(rev)?;
    let c = repo.read_commit(&id)?.for_display();
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
    out!("{}{deco}", paint("33", &format!("commit {}", id.to_hex())));
    if c.parents.len() > 1 {
        out!("Merge:  {}", c.parents.iter().map(short).collect::<Vec<_>>().join(" "));
    }
    out!("Author: {}", c.author.who);
    out!("Date:   {}", local_date(c.author.time, &c.author.tz));
    if c.committer.who != c.author.who {
        out!("Commit: {}", c.committer.who);
    }
    out!();
    for line in c.message.lines() {
        out!("    {line}");
    }
    out!();

    let parent = match c.parents.first() {
        Some(p) => Side::commit(&repo, p)?,
        None => Side::empty(),
    };
    if c.parents.len() > 1 {
        out!("{}", paint("2", &format!("(changes compared with the first parent, {})", short(&c.parents[0]))));
    }
    let new = Side { files: flatten(&repo.store, &c.tree)?, worktree: false };
    print_diff(&repo, &parent, &new, &[], stat)?;
    Ok(())
}
