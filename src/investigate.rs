//! `copse blame` and `copse bisect`.
//!
//!   copse blame FILE [REV] [-L START,END]
//!       for each line: the commit that last changed it, who, and when
//!   copse bisect start BAD GOOD... | good [REV] | bad [REV] | skip [REV] | reset | run CMD...
//!       binary search for the commit that introduced a problem

use crate::cmd::{apply_tree, civil_time, paint, print_changes};
use crate::merge::{ancestors, MergeState};
use crate::objects::{flatten, parse_hex, short, Commit, Id, Kind, Mode};
use crate::rebase::RebaseState;
use crate::repo::{Head, Repo};
use crate::worktree::{changes, read_content, snapshot, Index};
use anyhow::{bail, Context, Result};
use similar::{capture_diff_slices, Algorithm, DiffTag};
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::fs;

// ---------------------------------------------------------------- blame

/// The lines of `path` in `commit`, or None if it isn't there (or isn't text).
fn lines_at(repo: &Repo, commit: &Commit, path: &str) -> Result<Option<Vec<String>>> {
    let files = flatten(&repo.store, &commit.tree)?;
    let Some(&(mode, id)) = files.get(path) else { return Ok(None) };
    if mode == Mode::Link || mode == Mode::Submodule {
        return Ok(None);
    }
    let bytes = repo.store.get_kind(&id, Kind::Blob)?;
    Ok(std::str::from_utf8(&bytes).ok().filter(|t| !t.contains('\0')).map(|t| t.lines().map(str::to_string).collect()))
}

/// For each line of `new`, the line of `old` it was carried over from, if unchanged.
fn carried_over(old: &[String], new: &[String]) -> Vec<Option<usize>> {
    let mut map = vec![None; new.len()];
    for op in capture_diff_slices(Algorithm::Myers, old, new) {
        let (tag, o, n) = op.as_tag_tuple();
        if tag == DiffTag::Equal {
            for (k, ni) in n.enumerate() {
                map[ni] = Some(o.start + k);
            }
        }
    }
    map
}

/// Lines of one commit's version of the file still waiting to be blamed:
/// (line in that version, line in the final file).
struct Pending {
    lines: Vec<String>,
    waiting: Vec<(usize, usize)>,
}

/// `copse blame FILE [REV] [-L START,END]`
pub fn blame(path: &str, rev: Option<&str>, range: Option<&str>) -> Result<()> {
    let repo = Repo::find_any()?;
    let spec = if repo.is_bare() { path.to_string() } else { crate::restore::repo_path(&repo, path)? };
    let start = repo.resolve(rev.unwrap_or("HEAD"))?;
    let start_commit = repo.read_commit(&start)?;

    // The text being explained: your file as it is now, or the file in REV.
    let (final_lines, from_worktree) = match rev {
        None if !repo.is_bare() => {
            let bytes = read_content(&crate::objects::disk_path(&repo.root, &spec), Mode::File).with_context(|| format!("can't read {path}"))?;
            let text = std::str::from_utf8(&bytes).ok().filter(|t| !t.contains('\0')).with_context(|| format!("{path} isn't a text file"))?;
            (text.lines().map(str::to_string).collect::<Vec<_>>(), true)
        }
        _ => (lines_at(&repo, &start_commit, &spec)?.with_context(|| format!("{path} isn't a text file in that commit"))?, false),
    };

    // Who's responsible for each final line: (commit, its line number there).
    let mut owner: Vec<Option<(Id, usize)>> = vec![None; final_lines.len()];
    let mut pending: HashMap<Id, Pending> = HashMap::new();
    let mut queue: BinaryHeap<(i64, [u8; 32])> = BinaryHeap::new();
    let mut not_committed = vec![false; final_lines.len()];

    let committed = lines_at(&repo, &start_commit, &spec)?;
    let first_waiting: Vec<(usize, usize)> = if from_worktree {
        // Lines that differ from the last commit haven't been committed yet.
        let map = committed.as_ref().map(|c| carried_over(c, &final_lines)).unwrap_or_else(|| vec![None; final_lines.len()]);
        map.iter().enumerate().filter_map(|(f, m)| match m {
            Some(c) => Some((*c, f)),
            None => {
                not_committed[f] = true;
                None
            }
        }).collect()
    } else {
        (0..final_lines.len()).map(|i| (i, i)).collect()
    };
    if let (Some(lines), false) = (committed, first_waiting.is_empty()) {
        pending.insert(start, Pending { lines, waiting: first_waiting });
        queue.push((start_commit.committer.time, *start.as_bytes()));
    }

    // Walk back through history, newest first, passing each unchanged line to the
    // parent it came from; a line no parent had was written by this commit.
    let mut seen = HashSet::new();
    while let Some((_, bytes)) = queue.pop() {
        let id = Id::from_bytes(bytes);
        if !seen.insert(id) {
            continue;
        }
        let Some(mut p) = pending.remove(&id) else { continue };
        let commit = repo.read_commit(&id)?;
        for parent in &commit.parents {
            if p.waiting.is_empty() {
                break;
            }
            let pc = repo.read_commit(parent)?;
            let Some(parent_lines) = lines_at(&repo, &pc, &spec)? else { continue };
            let map = carried_over(&parent_lines, &p.lines);
            let (passed, kept): (Vec<_>, Vec<_>) = p.waiting.iter().partition(|(local, _)| map[*local].is_some());
            if passed.is_empty() {
                continue;
            }
            let entry = pending.entry(*parent).or_insert_with(|| Pending { lines: parent_lines.clone(), waiting: Vec::new() });
            entry.waiting.extend(passed.iter().map(|(local, f)| (map[*local].unwrap(), *f)));
            queue.push((pc.committer.time, *parent.as_bytes()));
            p.waiting = kept;
        }
        for (local, f) in p.waiting {
            owner[f] = Some((id, local + 1));
        }
    }

    // Which lines to show.
    let (from, to) = match range {
        None => (1, final_lines.len()),
        Some(r) => {
            let (a, b) = r.split_once(',').context("-L takes START,END, e.g. -L 10,20")?;
            let a: usize = a.trim().parse().context("-L START must be a line number")?;
            let b: usize = if b.trim().is_empty() { final_lines.len() } else { b.trim().parse().context("-L END must be a line number")? };
            if a == 0 || a > b || b > final_lines.len() {
                bail!("-L {r} is outside the file (it has {} lines)", final_lines.len());
            }
            (a, b)
        }
    };
    let mut commits: HashMap<Id, Commit> = HashMap::new();
    let width = final_lines.len().to_string().len();
    let mut name_width = 0;
    for f in from - 1..to {
        if let Some((id, _)) = owner[f] {
            if !commits.contains_key(&id) {
                commits.insert(id, repo.read_commit(&id)?);
            }
            name_width = name_width.max(commits[&id].author.name().chars().count());
        }
    }
    name_width = name_width.max(if not_committed[from - 1..to].iter().any(|&b| b) { "Not committed yet".len() } else { 0 });
    for f in from - 1..to {
        let (id_text, who, when) = match owner[f] {
            Some((id, _)) => {
                let c = &commits[&id];
                (short(&id), c.author.name().to_string(), civil_time(c.author.time)[..10].to_string())
            }
            None => ("0000000000".to_string(), "Not committed yet".to_string(), String::from("          ")),
        };
        println!(
            "{} ({who:<name_width$} {when} {:>width$}) {}",
            paint("33", &id_text),
            f + 1,
            final_lines[f]
        );
    }
    Ok(())
}

// ---------------------------------------------------------------- bisect

struct Bisect {
    /// Where to go back to: a branch name, or a commit id if HEAD was detached.
    back_to: String,
    bad: Option<Id>,
    good: Vec<Id>,
    skipped: Vec<Id>,
}

impl Bisect {
    fn path(repo: &Repo) -> std::path::PathBuf {
        repo.wt.join("BISECT_STATE")
    }

    fn load(repo: &Repo) -> Result<Option<Bisect>> {
        let Ok(text) = fs::read_to_string(Self::path(repo)) else { return Ok(None) };
        let mut b = Bisect { back_to: String::new(), bad: None, good: vec![], skipped: vec![] };
        for line in text.lines() {
            let (key, value) = line.split_once(' ').unwrap_or((line, ""));
            match key {
                "back" => b.back_to = value.to_string(),
                "bad" => b.bad = Some(parse_hex(value)?),
                "good" => b.good.push(parse_hex(value)?),
                "skip" => b.skipped.push(parse_hex(value)?),
                _ => {}
            }
        }
        Ok(Some(b))
    }

    fn save(&self, repo: &Repo) -> Result<()> {
        let mut t = format!("back {}\n", self.back_to);
        if let Some(b) = self.bad {
            t += &format!("bad {}\n", b.to_hex());
        }
        for g in &self.good {
            t += &format!("good {}\n", g.to_hex());
        }
        for s in &self.skipped {
            t += &format!("skip {}\n", s.to_hex());
        }
        crate::repo::write_atomic(&Self::path(repo), t.as_bytes())
    }
}

/// Commits a bisect in progress refers to (for gc and fsck).
pub fn bisect_ids(repo: &Repo) -> Result<Vec<Id>> {
    let Some(b) = Bisect::load(repo)? else { return Ok(Vec::new()) };
    let mut out: Vec<Id> = b.bad.into_iter().chain(b.good).chain(b.skipped).collect();
    if let Ok(id) = parse_hex(&b.back_to) {
        out.push(id);
    }
    Ok(out)
}

/// Move the working directory and a detached HEAD to `id`.
fn move_to(repo: &Repo, id: &Id) -> Result<()> {
    let mut index = Index::load(&repo.wt)?;
    let files = snapshot(&repo.root, &index, None)?;
    let tree = flatten(&repo.store, &repo.read_commit(id)?.tree)?;
    apply_tree(repo, &mut index, &files, &tree, &[])?;
    index.save(&repo.wt)?;
    repo.set_head(&Head::Detached(*id))
}

fn require_clean(repo: &Repo, what: &str) -> Result<()> {
    let index = Index::load(&repo.wt)?;
    let ch = changes(&index, &snapshot(&repo.root, &index, None)?);
    if !ch.is_empty() {
        print_changes(&ch);
        bail!("commit or stash your {} change(s) before {what}", ch.len());
    }
    Ok(())
}

/// What's left to test, and the next commit to check out (or the answer).
enum Next {
    Test(Id, usize),
    Found(Id),
    OnlySkipped(Vec<Id>),
}

fn next_step(repo: &Repo, b: &Bisect) -> Result<Next> {
    let bad = b.bad.context("no bad commit yet")?;
    let mut known_good = HashSet::new();
    for g in &b.good {
        known_good.extend(ancestors(repo, *g)?);
    }
    // Suspects: the bad commit and its ancestors that no good commit has.
    let suspects: HashSet<Id> = ancestors(repo, bad)?.into_iter().filter(|c| !known_good.contains(c)).collect();
    let skipped: HashSet<Id> = b.skipped.iter().copied().collect();
    let testable: Vec<Id> = suspects.iter().copied().filter(|c| *c != bad && !skipped.contains(c)).collect();
    if testable.is_empty() {
        let unsure: Vec<Id> = suspects.iter().copied().filter(|c| *c != bad && skipped.contains(c)).collect();
        return Ok(if unsure.is_empty() { Next::Found(bad) } else { Next::OnlySkipped(unsure) });
    }
    // Pick the commit that splits the suspects most evenly: about half of them are
    // its ancestors, so either answer rules out half.
    let n = suspects.len();
    let mut best: Option<(usize, Id)> = None;
    for c in &testable {
        let below = ancestors(repo, *c)?.iter().filter(|a| suspects.contains(a)).count();
        let score = below.min(n - below);
        if best.map_or(true, |(s, id)| score > s || (score == s && c.as_bytes() < id.as_bytes())) {
            best = Some((score, *c));
        }
    }
    Ok(Next::Test(best.unwrap().1, n))
}

/// Check out the next commit to test, or announce the answer. Returns true when done.
fn advance(repo: &Repo, b: &Bisect) -> Result<bool> {
    if b.bad.is_none() || b.good.is_empty() {
        let need = if b.bad.is_none() { "a bad commit (copse bisect bad [REV])" } else { "a good commit (copse bisect good REV)" };
        println!("Waiting for {need}.");
        return Ok(false);
    }
    match next_step(repo, b)? {
        Next::Test(id, left) => {
            move_to(repo, &id)?;
            let steps = (usize::BITS - left.leading_zeros()).saturating_sub(1);
            let title = repo.read_commit(&id)?.message.lines().next().unwrap_or_default().to_string();
            println!("Bisecting: {} commit(s) left to test (about {steps} step(s))", left - 1);
            println!("[{}] {title}", paint("33", &short(&id)));
            println!("Test it, then run `copse bisect good` or `copse bisect bad` (or `skip`).");
            Ok(false)
        }
        Next::Found(id) => {
            let c = repo.read_commit(&id)?;
            println!("{} is the first bad commit", paint("33", &id.to_hex()));
            println!("Author: {}", c.author.who);
            println!("Date:   {}", civil_time(c.author.time));
            println!();
            for line in c.message.lines() {
                println!("    {line}");
            }
            println!();
            println!("Run `copse bisect reset` to go back to where you were.");
            Ok(true)
        }
        Next::OnlySkipped(ids) => {
            println!("Only skipped commits are left, so the first bad commit is one of these (or {}):", short(&b.bad.unwrap()));
            for id in ids {
                println!("  {}", short(&id));
            }
            Ok(true)
        }
    }
}

pub enum BisectCmd<'a> {
    Status,
    Start { bad: Option<&'a str>, good: &'a [String] },
    Good(Option<&'a str>),
    Bad(Option<&'a str>),
    Skip(Option<&'a str>),
    Reset,
    Run(&'a [String]),
}

pub fn bisect(cmd: BisectCmd) -> Result<()> {
    let repo = Repo::find()?;
    let current = |rev: Option<&str>| -> Result<Id> {
        match rev {
            Some(r) => repo.resolve(r),
            None => repo.head_commit()?.context("there are no commits yet"),
        }
    };
    match cmd {
        BisectCmd::Start { bad, good } => {
            if Bisect::load(&repo)?.is_some() {
                bail!("a bisect is already in progress; `copse bisect reset` ends it");
            }
            if let Some(r) = RebaseState::load(&repo)? {
                bail!("{}", r.busy());
            }
            if MergeState::load(&repo)?.is_some() {
                bail!("a merge is in progress; finish or abort it first");
            }
            require_clean(&repo, "bisecting")?;
            let back_to = match repo.head()? {
                Head::Branch(b) => b,
                Head::Detached(id) => id.to_hex().to_string(),
            };
            let mut b = Bisect { back_to, bad: None, good: vec![], skipped: vec![] };
            if let Some(r) = bad {
                b.bad = Some(repo.resolve(r)?);
            }
            for g in good {
                b.good.push(repo.resolve(g)?);
            }
            if let Some(bad) = b.bad {
                for g in &b.good {
                    if ancestors(&repo, *g)?.contains(&bad) {
                        bail!("{} is marked good but comes after the bad commit; check the order: copse bisect start BAD GOOD", short(g));
                    }
                }
            }
            b.save(&repo)?;
            advance(&repo, &b)?;
        }
        BisectCmd::Good(rev) | BisectCmd::Bad(rev) | BisectCmd::Skip(rev) => {
            let is_good = matches!(cmd, BisectCmd::Good(_));
            let is_bad = matches!(cmd, BisectCmd::Bad(_));
            let mut b = Bisect::load(&repo)?.context("no bisect is in progress; start one with `copse bisect start BAD GOOD`")?;
            let id = current(rev)?;
            if is_good {
                b.good.push(id);
            } else if is_bad {
                b.bad = Some(id);
            } else {
                b.skipped.push(id);
            }
            b.save(&repo)?;
            advance(&repo, &b)?;
        }
        BisectCmd::Status => match Bisect::load(&repo)? {
            None => println!("No bisect in progress. Start one with `copse bisect start BAD GOOD`."),
            Some(b) => {
                println!("Bisecting: {} good, {} skipped, bad: {}", b.good.len(), b.skipped.len(), b.bad.map_or("not yet marked".into(), |i| short(&i)));
                if b.bad.is_some() && !b.good.is_empty() {
                    if let Next::Test(_, left) = next_step(&repo, &b)? {
                        println!("{} commit(s) left to test.", left - 1);
                    }
                }
            }
        },
        BisectCmd::Reset => {
            let b = Bisect::load(&repo)?.context("no bisect is in progress")?;
            require_clean(&repo, "ending the bisect")?;
            match repo.branch(&b.back_to)? {
                Some(tip) => {
                    move_to(&repo, &tip)?;
                    repo.set_head(&Head::Branch(b.back_to.clone()))?;
                }
                None => move_to(&repo, &parse_hex(&b.back_to)?)?,
            }
            fs::remove_file(Bisect::path(&repo))?;
            println!("Bisect ended; back on {}.", b.back_to);
        }
        BisectCmd::Run(command) => {
            if command.is_empty() {
                bail!("say what to run: copse bisect run ./test.sh (exit 0 = good, 125 = skip, other = bad)");
            }
            let mut b = Bisect::load(&repo)?.context("start the bisect first: copse bisect start BAD GOOD")?;
            if b.bad.is_none() || b.good.is_empty() {
                bail!("mark a bad and a good commit first: copse bisect start BAD GOOD");
            }
            let line = command.join(" ");
            loop {
                let id = repo.head_commit()?.context("no commit checked out")?;
                // One argument is a shell line ("make && ./t"); several are a program and its arguments.
                let mut run = if command.len() == 1 {
                    let mut c = std::process::Command::new("sh");
                    c.arg("-c").arg(&command[0]);
                    c
                } else {
                    let mut c = std::process::Command::new(&command[0]);
                    c.args(&command[1..]);
                    c
                };
                let status = run.current_dir(&repo.root).status().with_context(|| format!("can't run `{line}`"))?;
                let code = status.code().unwrap_or(128);
                let verdict = match code {
                    0 => "good",
                    125 => "skip",
                    c if c < 128 => "bad",
                    c => bail!("`{line}` failed with status {c} (128 or more stops the bisect)"),
                };
                println!("{} {verdict} (exit {code})", short(&id));
                match verdict {
                    "good" => b.good.push(id),
                    "skip" => b.skipped.push(id),
                    _ => b.bad = Some(id),
                }
                b.save(&repo)?;
                if advance(&repo, &b)? {
                    break;
                }
            }
        }
    }
    Ok(())
}
