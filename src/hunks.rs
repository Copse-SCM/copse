//! Committing part of a file: `copse commit -p` asks about each change ("hunk") in
//! turn, and only the ones you pick go into the commit. Your files on disk aren't
//! touched; what you didn't pick stays an uncommitted change.
//!
//! The same splitting into hunks, and building a file from the ones picked, is
//! what the TUI uses to pick changes on the Changes screen.

use crate::cmd::paint;
use crate::objects::{Id, Kind, Mode};
use crate::repo::Repo;
use crate::worktree::{Changes, File, Index};
use anyhow::{bail, Result};
use similar::{DiffOp, DiffTag, TextDiff};
use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, Write};

/// One change within a file, with a few lines of context around it.
pub struct Hunk {
    /// "@@ -10,7 +10,8 @@"
    pub header: String,
    /// (' ' context, '-' removed, '+' added, line text without its newline)
    pub lines: Vec<(char, String)>,
}

fn groups<'a>(diff: &TextDiff<'a, 'a, 'a, str>) -> Vec<Vec<DiffOp>> {
    diff.grouped_ops(3)
}

/// The hunks that turn `old` into `new`.
pub fn hunks(old: &str, new: &str) -> Vec<Hunk> {
    let diff = TextDiff::from_lines(old, new);
    groups(&diff)
        .into_iter()
        .map(|group| {
            let (first, last) = (group.first().unwrap(), group.last().unwrap());
            let (o0, o1) = (first.old_range().start, last.old_range().end);
            let (n0, n1) = (first.new_range().start, last.new_range().end);
            let header = format!("@@ -{},{} +{},{} @@", o0 + 1, o1 - o0, n0 + 1, n1 - n0);
            let mut lines = Vec::new();
            for op in &group {
                for change in diff.iter_changes(op) {
                    let mark = match change.tag() {
                        similar::ChangeTag::Equal => ' ',
                        similar::ChangeTag::Delete => '-',
                        similar::ChangeTag::Insert => '+',
                    };
                    lines.push((mark, change.value().trim_end_matches('\n').to_string()));
                }
            }
            Hunk { header, lines }
        })
        .collect()
}

/// `old` with only the hunks marked in `take` applied (in the order `hunks` gives them).
pub fn apply(old: &str, new: &str, take: &[bool]) -> String {
    let diff = TextDiff::from_lines(old, new);
    // Which hunk each change belongs to.
    let mut owner: HashMap<(usize, usize), usize> = HashMap::new();
    for (g, group) in groups(&diff).iter().enumerate() {
        for op in group.iter().filter(|op| op.tag() != DiffTag::Equal) {
            owner.insert((op.old_range().start, op.new_range().start), g);
        }
    }
    let (olds, news) = (diff.old_slices(), diff.new_slices());
    let mut out = String::with_capacity(new.len().max(old.len()));
    for op in diff.ops() {
        let taken = op.tag() != DiffTag::Equal && owner.get(&(op.old_range().start, op.new_range().start)).is_some_and(|g| take.get(*g) == Some(&true));
        let lines = if taken { &news[op.new_range()] } else { &olds[op.old_range()] };
        for l in lines {
            out.push_str(l);
        }
    }
    out
}

pub fn text(data: &[u8]) -> Option<&str> {
    std::str::from_utf8(data).ok().filter(|t| !t.contains('\0'))
}

fn show_hunk(path: &str, h: &Hunk, n: usize, of: usize) {
    println!("{} {}  {}", paint("1", path), paint("36", &h.header), paint("2", &format!("(change {n} of {of})")));
    for (mark, line) in &h.lines {
        let s = format!("{mark}{line}");
        println!("{}", match mark {
            '+' => paint("32", &s),
            '-' => paint("31", &s),
            _ => s,
        });
    }
}

/// What `pick` asks about.
pub enum Question<'a> {
    /// A whole file: new, deleted, not text, or only its mode changed. Answers y/n/q.
    Whole { path: &'a str, prompt: String },
    /// One hunk of a text file (`n` of `of`, from 1). Answers y/n/a/d/q.
    Hunk { path: &'a str, hunk: &'a Hunk, n: usize, of: usize },
}

/// Ask a question; returns the first letter answered (EOF counts as q).
fn ask(input: &mut dyn BufRead, question: &str) -> char {
    print!("{question} ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    match input.read_line(&mut line) {
        Ok(0) | Err(_) => {
            println!();
            'q'
        }
        Ok(_) => line.trim().chars().next().unwrap_or('?').to_ascii_lowercase(),
    }
}

/// Answers typed at the terminal: `copse commit -p`.
pub fn ask_terminal(input: &mut dyn BufRead) -> impl FnMut(&Question) -> Result<char> + '_ {
    move |q| {
        Ok(match q {
            Question::Whole { prompt, .. } => ask(input, &format!("{prompt} [y]es [n]o [q]uit:")),
            Question::Hunk { path, hunk, n, of } => {
                show_hunk(path, hunk, *n, *of);
                ask(input, "Commit this change? [y]es [n]o [a]ll the rest of this file [d]one with this file [q]uit:")
            }
        })
    }
}

/// What to take from one file, picked beforehand (the TUI's Changes screen).
#[derive(Clone, Debug, PartialEq)]
pub enum Choice {
    All,
    /// One entry per hunk, in order.
    Hunks(Vec<bool>),
}

/// "*:PATH" (the whole file) or "0110:PATH" (one digit per hunk): `commit --pick`.
pub fn parse_choice(spec: &str) -> Result<(String, Choice)> {
    let Some((how, path)) = spec.split_once(':') else { bail!("--pick takes *:PATH or 0101:PATH, not {spec:?}") };
    let choice = match how {
        "*" => Choice::All,
        bits if !bits.is_empty() && bits.bytes().all(|b| b == b'0' || b == b'1') => Choice::Hunks(bits.bytes().map(|b| b == b'1').collect()),
        _ => bail!("--pick takes *:PATH or 0101:PATH, not {spec:?}"),
    };
    Ok((path.to_string(), choice))
}

/// Answers decided beforehand. A file whose hunks no longer match what was picked
/// (it changed since) is refused rather than guessed at.
pub fn answers(choices: &BTreeMap<String, Choice>) -> impl FnMut(&Question) -> Result<char> + '_ {
    move |q| {
        Ok(match q {
            Question::Whole { path, .. } => match choices.get(*path) {
                Some(Choice::All) => 'y',
                Some(Choice::Hunks(_)) => bail!("{path} can only be committed whole now (it changed since its changes were picked); pick it again"),
                None => 'n',
            },
            Question::Hunk { path, n, of, .. } => match choices.get(*path) {
                Some(Choice::All) => 'a',
                Some(Choice::Hunks(take)) if take.len() == *of => if take[n - 1] { 'y' } else { 'n' },
                Some(Choice::Hunks(take)) => bail!("{path} now has {of} change(s), not the {} that were picked from (it changed since); pick again", take.len()),
                None => 'd',
            },
        })
    }
}

/// Go through the changes in scope, asking about each. Returns what to commit for
/// each path the user picked something in: Some((mode, blob)) or None (deleted).
pub fn pick(repo: &Repo, index: &Index, files: &[File], ch: &Changes, in_scope: &dyn Fn(&str) -> bool, answer: &mut dyn FnMut(&Question) -> Result<char>) -> Result<BTreeMap<String, Option<(Mode, Id)>>> {
    let by_path: HashMap<&str, &File> = files.iter().map(|f| (f.path.as_str(), f)).collect();
    let mut paths: Vec<(&String, char)> = Vec::new();
    paths.extend(ch.modified.iter().map(|p| (p, 'M')));
    paths.extend(ch.added.iter().map(|p| (p, 'A')));
    paths.extend(ch.deleted.iter().map(|p| (p, 'D')));
    paths.retain(|(p, _)| in_scope(p));
    paths.sort();
    let mut chosen = BTreeMap::new();
    'files: for (path, kind) in paths {
        let mut whole = |prompt: String| answer(&Question::Whole { path, prompt });
        match kind {
            'A' => {
                let f = by_path[path.as_str()];
                match whole(format!("{} {path}: commit this new file?", paint("32", "A")))? {
                    'y' => {
                        chosen.insert(path.clone(), Some((f.mode, f.id)));
                    }
                    'q' => break 'files,
                    _ => {}
                }
            }
            'D' => match whole(format!("{} {path}: commit its deletion?", paint("31", "D")))? {
                'y' => {
                    chosen.insert(path.clone(), None);
                }
                'q' => break 'files,
                _ => {}
            },
            _ => {
                let f = by_path[path.as_str()];
                let e = &index.entries[path.as_str()];
                let (old_id, old_mode) = (Id::from_bytes(e.id), e.mode);
                let old = repo.store.get_kind(&old_id, Kind::Blob)?;
                let new = repo.store.get_kind(&f.id, Kind::Blob)?;
                let (Some(old_t), Some(new_t)) = (text(&old), text(&new)) else {
                    // Binary files, links: all or nothing.
                    match whole(format!("{} {path} (not text): commit the change?", paint("33", "M")))? {
                        'y' => {
                            chosen.insert(path.clone(), Some((f.mode, f.id)));
                        }
                        'q' => break 'files,
                        _ => {}
                    }
                    continue;
                };
                let hs = hunks(old_t, new_t);
                if hs.is_empty() {
                    // Only the mode changed (e.g. made executable).
                    match whole(format!("{} {path}: commit the change of mode?", paint("33", "M")))? {
                        'y' => {
                            chosen.insert(path.clone(), Some((f.mode, f.id)));
                        }
                        'q' => break 'files,
                        _ => {}
                    }
                    continue;
                }
                let mut take = vec![false; hs.len()];
                let mut rest: Option<bool> = None; // a = take the rest, d = skip the rest
                for (i, h) in hs.iter().enumerate() {
                    if let Some(all) = rest {
                        take[i] = all;
                        continue;
                    }
                    match answer(&Question::Hunk { path, hunk: h, n: i + 1, of: hs.len() })? {
                        'y' => take[i] = true,
                        'a' => {
                            take[i] = true;
                            rest = Some(true);
                        }
                        'd' => rest = Some(false),
                        'q' => {
                            if take.iter().any(|t| *t) {
                                chosen.insert(path.clone(), Some(build(repo, old_t, new_t, &take, old_mode, f)?));
                            }
                            break 'files;
                        }
                        _ => {}
                    }
                }
                if take.iter().any(|t| *t) {
                    chosen.insert(path.clone(), Some(build(repo, old_t, new_t, &take, old_mode, f)?));
                }
            }
        }
    }
    Ok(chosen)
}

/// The file as it would be with only the picked hunks: stored as a blob.
fn build(repo: &Repo, old: &str, new: &str, take: &[bool], old_mode: Mode, f: &File) -> Result<(Mode, Id)> {
    if take.iter().all(|t| *t) {
        return Ok((f.mode, f.id)); // everything: exactly the file on disk
    }
    let content = apply(old, new, take);
    let id = repo.store.put(Kind::Blob, content.as_bytes())?;
    Ok((if f.mode == old_mode { old_mode } else { f.mode }, id))
}

#[cfg(test)]
mod tests {
    use super::{apply, hunks};

    #[test]
    fn picks_some_hunks() {
        let old: String = (1..=30).map(|i| format!("line {i}\n")).collect();
        let new = old.replace("line 3\n", "LINE THREE\n").replace("line 25\n", "LINE 25\nextra\n");
        let hs = hunks(&old, &new);
        assert_eq!(hs.len(), 2, "two separate changes make two hunks");
        assert_eq!(apply(&old, &new, &[true, true]), new);
        assert_eq!(apply(&old, &new, &[false, false]), old);
        let first_only = apply(&old, &new, &[true, false]);
        assert!(first_only.contains("LINE THREE") && !first_only.contains("extra") && first_only.contains("line 25\n"));
        let second_only = apply(&old, &new, &[false, true]);
        assert!(!second_only.contains("LINE THREE") && second_only.contains("LINE 25\nextra\n"));
        assert!(hs[0].header.starts_with("@@ -1,6 +1,6 @@"));
    }

    #[test]
    fn end_of_file_without_newline() {
        let old = "a\nb\nc";
        let new = "a\nB\nc\nd";
        let hs = hunks(old, new);
        assert_eq!(apply(old, new, &vec![true; hs.len()]), new);
        assert_eq!(apply(old, new, &vec![false; hs.len()]), old);
    }
}
