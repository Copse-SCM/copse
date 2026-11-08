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
use similar::{DiffTag, TextDiff};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::{BufRead, Write};

/// One line of a hunk.
#[derive(Clone, Debug)]
pub struct Line {
    /// ' ' context, '-' removed, '+' added
    pub mark: char,
    /// The line, without its newline.
    pub text: String,
    /// For a removed or added line: its number among all the file's changed lines
    /// (what a pick is made of).
    pub change: Option<usize>,
    /// Where it sits (0-based) in the old and new file; for an added line, `old`
    /// is the old line it comes before, and the other way round for a removed one.
    old: usize,
    new: usize,
}

/// One change within a file, with a few lines of context around it.
#[derive(Clone, Debug)]
pub struct Hunk {
    /// "@@ -10,7 +10,8 @@"
    pub header: String,
    pub lines: Vec<Line>,
    /// The changed lines it holds (numbers among all the file's changed lines).
    pub changes: std::ops::Range<usize>,
    /// Its separate runs of changes, for splitting it smaller (`s`).
    pieces: Vec<std::ops::Range<usize>>,
}

impl Hunk {
    fn from_lines(lines: Vec<Line>, pieces: Vec<std::ops::Range<usize>>) -> Hunk {
        let changed: Vec<usize> = lines.iter().filter_map(|l| l.change).collect();
        let changes = match (changed.first(), changed.last()) {
            (Some(a), Some(b)) => *a..*b + 1,
            _ => 0..0,
        };
        let old_n = lines.iter().filter(|l| l.mark != '+').count();
        let new_n = lines.iter().filter(|l| l.mark != '-').count();
        let (o0, n0) = lines.first().map(|l| (l.old, l.new)).unwrap_or((0, 0));
        let start = |at: usize, n: usize| if n == 0 { at } else { at + 1 };
        let header = format!("@@ -{},{old_n} +{},{new_n} @@", start(o0, old_n), start(n0, new_n));
        Hunk { header, lines, changes, pieces }
    }

    /// Whether `s` can split it into smaller hunks.
    pub fn can_split(&self) -> bool {
        self.pieces.len() > 1
    }

    /// The smaller hunks it splits into: one per run of changes, with the context
    /// lines around each (as git's `s` does).
    pub fn split(&self) -> Vec<Hunk> {
        self.pieces
            .iter()
            .map(|piece| {
                let inside = |l: &Line| l.change.is_some_and(|c| piece.contains(&c));
                let first = self.lines.iter().position(inside).unwrap_or(0);
                let last = self.lines.iter().rposition(inside).unwrap_or(0);
                let mut a = first;
                while a > 0 && first - a < 3 && self.lines[a - 1].mark == ' ' {
                    a -= 1;
                }
                let mut b = last;
                while b + 1 < self.lines.len() && b - last < 3 && self.lines[b + 1].mark == ' ' {
                    b += 1;
                }
                Hunk::from_lines(self.lines[a..=b].to_vec(), vec![piece.clone()])
            })
            .collect()
    }

    /// The hunk as diff text ("@@ ...", then " ", "-", "+" lines).
    pub fn text(&self) -> String {
        let mut t = format!("{}\n", self.header);
        for l in &self.lines {
            t += &format!("{}{}\n", l.mark, l.text);
        }
        t
    }
}

/// The hunks that turn `old` into `new`.
pub fn hunks(old: &str, new: &str) -> Vec<Hunk> {
    let diff = TextDiff::from_lines(old, new);
    let mut next_change = 0;
    diff.grouped_ops(3)
        .into_iter()
        .map(|group| {
            let mut lines = Vec::new();
            let mut pieces = Vec::new();
            for op in &group {
                let first = next_change;
                let (mut o, mut n) = (op.old_range().start, op.new_range().start);
                for change in diff.iter_changes(op) {
                    let text = change.value().trim_end_matches('\n').to_string();
                    let (mark, num) = match change.tag() {
                        similar::ChangeTag::Equal => (' ', None),
                        similar::ChangeTag::Delete => ('-', Some(next_change)),
                        similar::ChangeTag::Insert => ('+', Some(next_change)),
                    };
                    lines.push(Line { mark, text, change: num, old: o, new: n });
                    match mark {
                        ' ' => {
                            o += 1;
                            n += 1;
                        }
                        '-' => o += 1,
                        _ => n += 1,
                    }
                    if num.is_some() {
                        next_change += 1;
                    }
                }
                if next_change > first {
                    pieces.push(first..next_change);
                }
            }
            Hunk::from_lines(lines, pieces)
        })
        .collect()
}

/// How many changed lines (removed plus added) turn `old` into `new`.
pub fn changed_lines(hunks: &[Hunk]) -> usize {
    hunks.last().map_or(0, |h| h.changes.end)
}

/// `old` with only the changed lines marked in `take` applied (numbered as in
/// `hunks`): an untaken removal keeps the old line, an untaken addition leaves the
/// new one out.
pub fn apply(old: &str, new: &str, take: &[bool]) -> String {
    apply_mapped(old, new, take).0
}

/// `apply`, plus where each old line landed in the result (byte range), for the
/// old lines that were kept.
fn apply_mapped(old: &str, new: &str, take: &[bool]) -> (String, Vec<Option<(usize, usize)>>) {
    let diff = TextDiff::from_lines(old, new);
    // The lines the diff numbered (each with its newline, as from_lines splits them).
    let olds: Vec<&str> = old.split_inclusive('\n').collect();
    let news: Vec<&str> = new.split_inclusive('\n').collect();
    let mut at: Vec<Option<(usize, usize)>> = vec![None; olds.len()];
    let mut out = String::with_capacity(new.len().max(old.len()));
    // A kept last line without a newline, followed by an added line, gets one
    // (otherwise the two would run together).
    let push = |out: &mut String, l: &str| {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(l);
    };
    let mut k = 0;
    for op in diff.ops() {
        if op.tag() == DiffTag::Equal {
            for n in op.old_range() {
                push(&mut out, olds[n]);
                at[n] = Some((out.len() - olds[n].len(), out.len()));
            }
            continue;
        }
        for n in op.old_range() {
            if take.get(k) != Some(&true) {
                push(&mut out, olds[n]);
                at[n] = Some((out.len() - olds[n].len(), out.len()));
            }
            k += 1;
        }
        for l in &news[op.new_range()] {
            if take.get(k) == Some(&true) {
                push(&mut out, l);
            }
            k += 1;
        }
    }
    (out, at)
}

/// One hunk the user rewrote in the editor (`e`): its lines in the old file are
/// replaced by `text`.
#[derive(Clone, Debug)]
pub struct Edit {
    pub old: std::ops::Range<usize>,
    pub text: String,
}

impl Hunk {
    /// The old file's lines this hunk covers (its context and removed lines).
    fn old_range(&self) -> std::ops::Range<usize> {
        let olds: Vec<usize> = self.lines.iter().filter(|l| l.mark != '+').map(|l| l.old).collect();
        match (olds.first(), olds.last()) {
            (Some(a), Some(b)) => *a..*b + 1,
            _ => self.lines.first().map_or(0..0, |l| l.old..l.old),
        }
    }
}

/// `apply`, then the edited hunks' old lines replaced by what the user wrote.
/// (Edited hunks' own changed lines must be untaken, so their old lines are all kept.)
pub fn apply_with_edits(old: &str, new: &str, take: &[bool], edits: &[Edit]) -> String {
    let (mut out, at) = apply_mapped(old, new, take);
    let mut edits: Vec<&Edit> = edits.iter().collect();
    edits.sort_by_key(|e| std::cmp::Reverse(e.old.start));
    for e in edits {
        let span = (e.old.start..e.old.end).filter_map(|n| at.get(n).copied().flatten());
        let (mut from, mut to) = (None, None);
        for (a, b) in span {
            from = Some(from.map_or(a, |f: usize| f.min(a)));
            to = Some(to.map_or(b, |t: usize| t.max(b)));
        }
        let (from, to) = match (from, to) {
            (Some(f), Some(t)) => (f, t),
            // An empty old range (adding to an empty file): insert where it would go.
            _ => {
                let pos = (0..e.old.start).rev().find_map(|n| at.get(n).copied().flatten().map(|(_, b)| b)).unwrap_or(0);
                (pos, pos)
            }
        };
        out.replace_range(from..to, &e.text);
    }
    out
}

/// Each hunk's pick, as a pick of its changed lines.
pub fn lines_of(hunks: &[Hunk], take_hunks: &[bool]) -> Vec<bool> {
    let mut take = vec![false; changed_lines(hunks)];
    for (h, t) in hunks.iter().zip(take_hunks) {
        take[h.changes.clone()].iter_mut().for_each(|x| *x = *t);
    }
    take
}

pub fn text(data: &[u8]) -> Option<&str> {
    std::str::from_utf8(data).ok().filter(|t| !t.contains('\0'))
}

fn show_hunk(path: &str, h: &Hunk, n: usize, of: usize) {
    println!("{} {}  {}", paint("1", path), paint("36", &h.header), paint("2", &format!("(change {n} of {of})")));
    for l in &h.lines {
        let s = format!("{}{}", l.mark, l.text);
        println!("{}", match l.mark {
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
    /// A text file, before its hunks: answered with every line's pick at once
    /// (picked beforehand), or a key to go hunk by hunk.
    File { path: &'a str, hunks: &'a [Hunk] },
    /// One hunk of a text file (`n` of `of`, from 1). Answers y/n/a/d/q, s (split),
    /// e (edit), or the lines to take.
    Hunk { path: &'a str, hunk: &'a Hunk, n: usize, of: usize },
}

pub enum Answer {
    Key(char),
    /// The hunk rewritten in the editor: this text replaces its old lines.
    Edited(String),
    /// Which changed lines to take: for a File question, all of the file's; for a
    /// Hunk question, the hunk's own.
    Take(Vec<bool>),
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

/// Edit a hunk in the editor, as git's `e` does. The context and `-` lines must
/// stay as they were (they're what the file has now); the `+` side can be anything:
/// change, add or delete `+` lines, or turn a `-` into a space to keep that line.
/// Returns the text that replaces the hunk's old lines, or None if the user
/// deleted everything (leave the change out).
pub fn edit_hunk(repo: &Repo, path: &str, h: &Hunk) -> Result<Option<String>> {
    let file = repo.wt.join("HUNK_EDIT.diff");
    let mut text = format!(
        "# Editing a change in {path}. Change, add or delete + lines as you like.\n\
         # To keep a removed (-) line, change its '-' to a space. Context and -\n\
         # lines must otherwise stay as they are. Lines starting with # are\n\
         # ignored. Delete everything to leave the whole change out.\n"
    );
    text += &h.text();
    std::fs::write(&file, &text)?;
    let edited = crate::rebase::run_editor(&file).and_then(|_| Ok(std::fs::read_to_string(&file)?));
    let _ = std::fs::remove_file(&file);
    let edited = edited?;
    let lines: Vec<&str> = edited.lines().filter(|l| !l.starts_with('#') && !l.starts_with("@@")).collect();
    if lines.iter().all(|l| l.trim().is_empty()) {
        return Ok(None);
    }
    let (mut old_side, mut new_side): (Vec<&str>, Vec<&str>) = (Vec::new(), Vec::new());
    for l in &lines {
        // An editor may strip a context line's lone space, leaving it empty.
        let (mark, rest) = match l.chars().next() {
            None => (' ', ""),
            Some(c) => (c, &l[c.len_utf8()..]),
        };
        match mark {
            ' ' => {
                old_side.push(rest);
                new_side.push(rest);
            }
            '-' => old_side.push(rest),
            '+' => new_side.push(rest),
            _ => bail!("the line {l:?} doesn't start with a space, + or -"),
        }
    }
    let want: Vec<&str> = h.lines.iter().filter(|l| l.mark != '+').map(|l| l.text.as_str()).collect();
    if old_side != want {
        bail!("the context and - lines have to stay as they were (they're what the file has now); edit the + lines, or turn a - into a space to keep that line");
    }
    Ok(Some(new_side.iter().map(|l| format!("{l}\n")).collect()))
}

/// Answers typed at the terminal: `copse commit -p`.
pub fn ask_terminal<'a>(repo: &'a Repo, input: &'a mut dyn BufRead) -> impl FnMut(&Question) -> Result<Answer> + 'a {
    move |q| {
        Ok(match q {
            Question::Whole { prompt, .. } => Answer::Key(ask(input, &format!("{prompt} [y]es [n]o [q]uit:"))),
            Question::File { .. } => Answer::Key('?'), // go hunk by hunk
            Question::Hunk { path, hunk, n, of } => loop {
                show_hunk(path, hunk, *n, *of);
                let split = if hunk.can_split() { " [s]plit" } else { "" };
                match ask(input, &format!("Commit this change? [y]es [n]o [a]ll the rest of this file [d]one with this file{split} [e]dit [q]uit:")) {
                    'e' => match edit_hunk(repo, path, hunk) {
                        Ok(Some(text)) => break Answer::Edited(text),
                        Ok(None) => break Answer::Key('n'),
                        Err(e) => println!("{} {e:#}", paint("31", "Not used:")),
                    },
                    's' if !hunk.can_split() => println!("{}", paint("2", "This change can't be split any smaller.")),
                    c => break Answer::Key(c),
                }
            },
        })
    }
}

/// What to take from one file, picked beforehand (the TUI's Changes screen).
#[derive(Clone, Debug, PartialEq)]
pub enum Choice {
    All,
    /// One entry per hunk, in order.
    Hunks(Vec<bool>),
    /// One entry per changed line (removed and added lines, in order).
    Lines(Vec<bool>),
}

/// "*:PATH" (the whole file), "h0110:PATH" or "0110:PATH" (one digit per hunk), or
/// "l0011101:PATH" (one digit per changed line): `commit --pick`.
pub fn parse_choice(spec: &str) -> Result<(String, Choice)> {
    let bad = || anyhow::anyhow!("--pick takes *:PATH, 0101:PATH (per hunk) or l0101:PATH (per line), not {spec:?}");
    let (how, path) = spec.split_once(':').ok_or_else(bad)?;
    let bits = |b: &str| -> Option<Vec<bool>> { (!b.is_empty() && b.bytes().all(|c| c == b'0' || c == b'1')).then(|| b.bytes().map(|c| c == b'1').collect()) };
    let choice = match how {
        "*" => Choice::All,
        _ if how.starts_with('l') => Choice::Lines(bits(&how[1..]).ok_or_else(bad)?),
        _ if how.starts_with('h') => Choice::Hunks(bits(&how[1..]).ok_or_else(bad)?),
        _ => Choice::Hunks(bits(how).ok_or_else(bad)?),
    };
    Ok((path.to_string(), choice))
}

/// Answers decided beforehand. A file whose changes no longer match what was
/// picked (it changed since) is refused rather than guessed at.
pub fn answers(choices: &BTreeMap<String, Choice>) -> impl FnMut(&Question) -> Result<Answer> + '_ {
    let stale = |path: &str| anyhow::anyhow!("{path} changed since its changes were picked; pick again");
    move |q| {
        Ok(match q {
            Question::Whole { path, .. } => match choices.get(*path) {
                Some(Choice::All) => Answer::Key('y'),
                Some(_) => bail!("{path} can only be committed whole now (it changed since its changes were picked); pick it again"),
                None => Answer::Key('n'),
            },
            Question::File { path, hunks } => {
                let total = changed_lines(hunks);
                Answer::Take(match choices.get(*path) {
                    Some(Choice::All) => vec![true; total],
                    Some(Choice::Hunks(t)) if t.len() == hunks.len() => lines_of(hunks, t),
                    Some(Choice::Hunks(t)) => bail!("{path} now has {} change(s), not the {} that were picked from (it changed since); pick again", hunks.len(), t.len()),
                    Some(Choice::Lines(t)) if t.len() == total => t.clone(),
                    Some(Choice::Lines(_)) => return Err(stale(path)),
                    None => vec![false; total],
                })
            }
            Question::Hunk { .. } => Answer::Key('d'), // not reached: files are answered whole
        })
    }
}

/// Go through the changes in scope, asking about each. Returns what to commit for
/// each path the user picked something in: Some((mode, blob)) or None (deleted).
pub fn pick(repo: &Repo, index: &Index, files: &[File], ch: &Changes, in_scope: &dyn Fn(&str) -> bool, answer: &mut dyn FnMut(&Question) -> Result<Answer>) -> Result<BTreeMap<String, Option<(Mode, Id)>>> {
    let by_path: HashMap<&str, &File> = files.iter().map(|f| (f.path.as_str(), f)).collect();
    let mut paths: Vec<(&String, char)> = Vec::new();
    paths.extend(ch.modified.iter().map(|p| (p, 'M')));
    paths.extend(ch.added.iter().map(|p| (p, 'A')));
    paths.extend(ch.deleted.iter().map(|p| (p, 'D')));
    paths.retain(|(p, _)| in_scope(p));
    paths.sort();
    let mut chosen = BTreeMap::new();
    let key = |a: Answer| match a {
        Answer::Key(c) => c,
        Answer::Take(_) | Answer::Edited(_) => '?',
    };
    'files: for (path, kind) in paths {
        let mut whole = |prompt: String| -> Result<char> { Ok(key(answer(&Question::Whole { path, prompt })?)) };
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
                let mut take = vec![false; changed_lines(&hs)];
                let mut edits: Vec<Edit> = Vec::new();
                let mut quit = false;
                match answer(&Question::File { path, hunks: &hs })? {
                    Answer::Take(t) => take = t,
                    Answer::Edited(_) => {}
                    Answer::Key(_) => {
                        // Hunk by hunk; split hunks go to the front of the queue.
                        let mut queue: VecDeque<Hunk> = hs.iter().cloned().collect();
                        let (mut n, mut of) = (0, hs.len());
                        let mut rest: Option<bool> = None; // a = take the rest, d = skip the rest
                        while let Some(h) = queue.pop_front() {
                            if let Some(all) = rest {
                                take[h.changes.clone()].iter_mut().for_each(|x| *x = all);
                                continue;
                            }
                            n += 1;
                            match answer(&Question::Hunk { path, hunk: &h, n, of })? {
                                Answer::Take(t) => take[h.changes.clone()].copy_from_slice(&t),
                                Answer::Edited(text) => {
                                    take[h.changes.clone()].iter_mut().for_each(|x| *x = false);
                                    edits.push(Edit { old: h.old_range(), text });
                                }
                                Answer::Key('y') => take[h.changes.clone()].iter_mut().for_each(|x| *x = true),
                                Answer::Key('a') => {
                                    take[h.changes.clone()].iter_mut().for_each(|x| *x = true);
                                    rest = Some(true);
                                }
                                Answer::Key('d') => rest = Some(false),
                                Answer::Key('s') if h.can_split() => {
                                    let parts = h.split();
                                    of += parts.len() - 1;
                                    n -= 1;
                                    for p in parts.into_iter().rev() {
                                        queue.push_front(p);
                                    }
                                }
                                Answer::Key('q') => {
                                    quit = true;
                                    break;
                                }
                                Answer::Key(_) => {}
                            }
                        }
                    }
                }
                if !edits.is_empty() {
                    let content = apply_with_edits(old_t, new_t, &take, &edits);
                    if content != old_t {
                        let id = repo.store.put(Kind::Blob, content.as_bytes())?;
                        chosen.insert(path.clone(), Some((if f.mode == old_mode { old_mode } else { f.mode }, id)));
                    }
                } else if take.iter().any(|t| *t) {
                    chosen.insert(path.clone(), Some(build(repo, old_t, new_t, &take, old_mode, f)?));
                }
                if quit {
                    break 'files;
                }
            }
        }
    }
    Ok(chosen)
}

/// The file as it would be with only the picked lines: stored as a blob.
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
    use super::{apply, changed_lines, hunks, lines_of};

    #[test]
    fn picks_some_hunks() {
        let old: String = (1..=30).map(|i| format!("line {i}\n")).collect();
        let new = old.replace("line 3\n", "LINE THREE\n").replace("line 25\n", "LINE 25\nextra\n");
        let hs = hunks(&old, &new);
        assert_eq!(hs.len(), 2, "two separate changes make two hunks");
        let pick = |t: &[bool]| apply(&old, &new, &lines_of(&hs, t));
        assert_eq!(pick(&[true, true]), new);
        assert_eq!(pick(&[false, false]), old);
        let first_only = pick(&[true, false]);
        assert!(first_only.contains("LINE THREE") && !first_only.contains("extra") && first_only.contains("line 25\n"));
        let second_only = pick(&[false, true]);
        assert!(!second_only.contains("LINE THREE") && second_only.contains("LINE 25\nextra\n"));
        assert_eq!(hs[0].header, "@@ -1,6 +1,6 @@");
        assert_eq!(hs[1].header, "@@ -22,7 +22,8 @@");
    }

    #[test]
    fn single_lines() {
        let old = "a\nb\nc\n";
        let new = "a\nB\nX\nc\n";
        let hs = hunks(old, new);
        // changed lines, in order: -b, +B, +X
        assert_eq!(changed_lines(&hs), 3);
        assert_eq!(apply(old, new, &[false, false, true]), "a\nb\nX\nc\n", "keep b, add only X");
        assert_eq!(apply(old, new, &[true, false, false]), "a\nc\n", "just the removal");
        assert_eq!(apply(old, new, &[true, true, false]), "a\nB\nc\n");
    }

    #[test]
    fn splitting() {
        let old: String = (1..=12).map(|i| format!("{i}\n")).collect();
        let new = old.replace("3\n", "three\n").replace("8\n", "eight\n");
        let hs = hunks(&old, &new);
        assert_eq!(hs.len(), 1, "close changes share a hunk");
        assert!(hs[0].can_split());
        let parts = hs[0].split();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].header, "@@ -1,6 +1,6 @@");
        assert_eq!(parts[1].header, "@@ -5,7 +5,7 @@");
        assert!(!parts[0].can_split());
        let mut take = vec![false; changed_lines(&hs)];
        take[parts[1].changes.clone()].iter_mut().for_each(|t| *t = true);
        assert_eq!(apply(&old, &new, &take), old.replace("8\n", "eight\n"));
    }

    #[test]
    fn an_edited_hunk_replaces_its_lines() {
        let line = |i: u32| match i {
            3 => "three".to_string(),
            25 => "twenty-five".to_string(),
            i => i.to_string(),
        };
        let old: String = (1..=30).map(|i| format!("{i}\n")).collect();
        let new: String = (1..=30).map(|i| format!("{}\n", line(i))).collect();
        let hs = hunks(&old, &new);
        assert_eq!(hs.len(), 2);
        // Take the first change as it is, and write the second one by hand.
        let mut take = vec![false; changed_lines(&hs)];
        take[hs[0].changes.clone()].iter_mut().for_each(|t| *t = true);
        let replacement: String = hs[1].lines.iter().map(|l| if l.text == "twenty-five" { "XXV\n".to_string() } else if l.mark == '-' { String::new() } else { format!("{}\n", l.text) }).collect();
        let out = super::apply_with_edits(&old, &new, &take, &[super::Edit { old: hs[1].old_range(), text: replacement }]);
        let want: String = (1..=30).map(|i| match i { 3 => "three\n".to_string(), 25 => "XXV\n".to_string(), i => format!("{i}\n") }).collect();
        assert_eq!(out, want);
    }

    #[test]
    fn end_of_file_without_newline() {
        let old = "a\nb\nc";
        let new = "a\nB\nc\nd";
        let hs = hunks(old, new);
        let n = changed_lines(&hs);
        assert_eq!(apply(old, new, &vec![true; n]), new);
        assert_eq!(apply(old, new, &vec![false; n]), old);
    }
}
