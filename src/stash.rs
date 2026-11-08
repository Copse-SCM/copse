//! `copse stash`: put uncommitted work aside and bring it back later.
//!
//! A stash is an ordinary commit whose tree is the working directory and whose
//! parent is the commit you were on. The list lives in `.copse/stash`, newest
//! first, one "<id> <message>" per line. Bringing a stash back is a three-way
//! merge (base = its parent, ours = HEAD, theirs = the stash), so it still
//! applies after you've committed or switched branches.

use crate::cmd::{apply_tree, head_label, paint, print_changes};
use crate::merge::{merge_trees, stage_as_changes, MergeState};
use crate::objects::{flatten, parse_hex, short, write_tree, Commit, Id, Kind};
use crate::rebase::RebaseState;
use crate::repo::{write_atomic, Repo};
use crate::worktree::{changes, snapshot, Index};
use anyhow::{bail, Context, Result};
use std::fs;
use std::io::ErrorKind;

pub struct Entry {
    pub id: Id,
    pub message: String,
}

pub fn load(repo: &Repo) -> Result<Vec<Entry>> {
    let text = match fs::read_to_string(repo.dir.join("stash")) {
        Ok(t) => t,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    text.lines()
        .filter(|l| !l.is_empty())
        .map(|l| {
            let (id, msg) = l.split_once(' ').unwrap_or((l, ""));
            Ok(Entry { id: parse_hex(id)?, message: msg.to_string() })
        })
        .collect()
}

fn save(repo: &Repo, entries: &[Entry]) -> Result<()> {
    let text: String = entries.iter().map(|e| format!("{} {}\n", e.id.to_hex(), e.message.replace('\n', " "))).collect();
    write_atomic(&repo.dir.join("stash"), text.as_bytes())
}

fn no_operation_in_progress(repo: &Repo) -> Result<()> {
    if MergeState::load(repo)?.is_some() {
        bail!("a merge is in progress; finish it with `copse commit` or undo it with `copse merge --abort` first");
    }
    if let Some(r) = RebaseState::load(repo)? {
        bail!("{}", r.busy());
    }
    Ok(())
}

fn pick(entries: &[Entry], n: usize) -> Result<&Entry> {
    match entries.len() {
        0 => bail!("there are no stashes"),
        len if n >= len => bail!("there is no stash {n} (there are {len}; see `copse stash list`)"),
        _ => Ok(&entries[n]),
    }
}

/// `copse stash [-m message]`
pub fn push(message: Option<&str>, paths: &[String]) -> Result<()> {
    let repo = Repo::find()?;
    no_operation_in_progress(&repo)?;
    let head = repo.head_commit()?.context("commit something first; there's nothing to stash against yet")?;
    let mut index = Index::load(&repo.wt)?;
    let files = snapshot(&repo.root, &index, Some(&repo.store))?;
    let ch = changes(&index, &files);
    if ch.is_empty() {
        println!("No changes to stash.");
        return Ok(());
    }
    if !paths.is_empty() {
        return push_some(&repo, head, &files, &ch, message, paths);
    }

    let head_commit = repo.read_commit(&head)?;
    let title = head_commit.message.lines().next().unwrap_or_default();
    let message = match message.map(str::trim).filter(|m| !m.is_empty()) {
        Some(m) => m.to_string(),
        None => format!("on {}: {} {title}", head_label(&repo)?, short(&head)),
    };
    // Record it before touching any files, so the work is safe even if the reset fails.
    record(&repo, &files, head, &message)?;

    let head_tree = flatten(&repo.store, &head_commit.tree)?;
    apply_tree(&repo, &mut index, &files, &head_tree, &ch.added)?;
    index.save(&repo.wt)?;
    println!(
        "Stashed {} change(s) as stash 0: {message}",
        ch.len()
    );
    println!("Your files are back to {}. Bring the work back with `copse stash pop`.", short(&head));
    Ok(())
}

/// `copse stash -- PATHS`: stash only the changes under PATHS; everything else stays.
fn push_some(repo: &Repo, head: Id, files: &[crate::worktree::File], ch: &crate::worktree::Changes, message: Option<&str>, paths: &[String]) -> Result<()> {
    let specs = paths.iter().map(|p| crate::restore::repo_path(repo, p)).collect::<Result<Vec<_>>>()?;
    let under = |p: &str, s: &str| s.is_empty() || p == s || p.starts_with(&format!("{s}/"));
    let changed: Vec<&String> = ch.added.iter().chain(&ch.modified).chain(&ch.deleted).collect();
    for (typed, spec) in paths.iter().zip(&specs) {
        if !changed.iter().any(|p| under(p, spec)) {
            bail!("{typed} has no changes to stash");
        }
    }
    let picked = |p: &str| specs.iter().any(|s| under(p, s));

    // The stash: the last commit with just the picked changes on top.
    let head_commit = repo.read_commit(&head)?;
    let head_tree = flatten(&repo.store, &head_commit.tree)?;
    let mut stashed = head_tree.clone();
    for p in ch.deleted.iter().filter(|p| picked(p)) {
        stashed.remove(p);
    }
    let mut count = ch.deleted.iter().filter(|p| picked(p)).count();
    for f in files.iter().filter(|f| picked(&f.path) && head_tree.get(&f.path) != Some(&(f.mode, f.id))) {
        stashed.insert(f.path.clone(), (f.mode, f.id));
        count += 1;
    }
    let title = head_commit.message.lines().next().unwrap_or_default();
    let message = match message.map(str::trim).filter(|m| !m.is_empty()) {
        Some(m) => m.to_string(),
        None => format!("on {}: {} {title} ({})", head_label(repo)?, short(&head), paths.join(" ")),
    };
    let tree = write_tree(&repo.store, stashed.iter().map(|(p, (m, id))| (p.as_str(), *m, *id)))?;
    let id = repo.store.put(Kind::Commit, &Commit::new(tree, vec![head], message.clone()).encode())?;
    let mut entries = load(repo)?;
    entries.insert(0, Entry { id, message: message.clone() });
    save(repo, &entries)?;

    // Put the picked files back as they were in the last commit.
    for p in changed.iter().filter(|p| picked(p)) {
        match head_tree.get(p.as_str()) {
            Some((mode, blob)) => crate::cmd::write_file(repo, p, *mode, blob)?,
            None => {
                let full = crate::objects::disk_path(&repo.root, p.as_str());
                std::fs::remove_file(&full)?;
                // Tidy folders the removal left empty.
                let mut dir = full.parent();
                while let Some(d) = dir.filter(|d| *d != repo.root.as_path()) {
                    if std::fs::remove_dir(d).is_err() {
                        break;
                    }
                    dir = d.parent();
                }
            }
        }
    }
    let left = changed.len() - changed.iter().filter(|p| picked(p)).count();
    println!("Stashed {count} change(s) as stash 0: {message}");
    println!("{left} other change(s) left as they are. Bring the stashed work back with `copse stash pop`.");
    Ok(())
}

/// Save the working directory as a new stash 0 (newest), without touching any files.
fn record(repo: &Repo, files: &[crate::worktree::File], head: Id, message: &str) -> Result<Id> {
    let tree = write_tree(&repo.store, files.iter().map(|f| (f.path.as_str(), f.mode, f.id)))?;
    let commit = Commit::new(tree, vec![head], message.to_string());
    let id = repo.store.put(Kind::Commit, &commit.encode())?;
    let mut entries = load(repo)?;
    entries.insert(0, Entry { id, message: message.to_string() });
    save(repo, &entries)?;
    Ok(id)
}

/// Before a command overwrites uncommitted work (restore, reset --hard), keep a copy
/// of the whole working directory as stash 0, so `copse stash apply` brings it back.
pub fn backup(repo: &Repo, message: &str) -> Result<()> {
    let head = repo.head_commit()?.context("there are no commits yet")?;
    let index = Index::load(&repo.wt)?;
    let files = snapshot(&repo.root, &index, Some(&repo.store))?;
    record(repo, &files, head, message)?;
    println!("Saved your uncommitted work as stash 0 first; `copse stash apply 0` brings it back.");
    Ok(())
}

pub fn list() -> Result<()> {
    let repo = Repo::find()?;
    let entries = load(&repo)?;
    if entries.is_empty() {
        println!("No stashes.");
    }
    for (n, e) in entries.iter().enumerate() {
        let when = repo.read_commit(&e.id).map(|c| crate::cmd::format_time(c.time())).unwrap_or_default();
        println!("{}  {}  {}", paint("33", &n.to_string()), paint("2", &when), e.message);
    }
    Ok(())
}

pub fn show(n: usize) -> Result<()> {
    let repo = Repo::find()?;
    let entries = load(&repo)?;
    let entry = pick(&entries, n)?;
    let commit = repo.read_commit(&entry.id)?;
    let before = match commit.parents.first() {
        Some(p) => flatten(&repo.store, &repo.read_commit(p)?.tree)?,
        None => Default::default(),
    };
    let after = flatten(&repo.store, &commit.tree)?;
    println!("stash {n}: {}", entry.message);
    let mut ch = crate::worktree::Changes::default();
    for (path, v) in &after {
        match before.get(path) {
            None => ch.added.push(path.clone()),
            Some(old) if old != v => ch.modified.push(path.clone()),
            _ => {}
        }
    }
    ch.deleted.extend(before.keys().filter(|p| !after.contains_key(*p)).cloned());
    print_changes(&ch);
    Ok(())
}

pub fn drop(n: usize) -> Result<()> {
    let repo = Repo::find()?;
    let mut entries = load(&repo)?;
    pick(&entries, n)?;
    let removed = entries.remove(n);
    save(&repo, &entries)?;
    println!("Dropped stash {n}: {} ({})", removed.message, short(&removed.id));
    Ok(())
}

/// `copse stash apply [n]` (keep_on_success = true) or `copse stash pop [n]` (false).
pub fn apply(n: usize, keep: bool) -> Result<()> {
    let repo = Repo::find()?;
    no_operation_in_progress(&repo)?;
    let mut entries = load(&repo)?;
    let entry = pick(&entries, n)?;
    let (stash_id, message) = (entry.id, entry.message.clone());

    let mut index = Index::load(&repo.wt)?;
    let files = snapshot(&repo.root, &index, None)?;
    let ch = changes(&index, &files);

    let stash = repo.read_commit(&stash_id)?;
    let base = match stash.parents.first() {
        Some(p) => flatten(&repo.store, &repo.read_commit(p)?.tree)?,
        None => Default::default(),
    };
    let ours = match repo.head_commit()? {
        Some(h) => flatten(&repo.store, &repo.read_commit(&h)?.tree)?,
        None => Default::default(),
    };
    let theirs = flatten(&repo.store, &stash.tree)?;
    // Your other uncommitted changes can stay, as long as the stash doesn't touch them.
    let touched: std::collections::BTreeSet<&String> =
        base.keys().chain(theirs.keys()).filter(|p| base.get(*p) != theirs.get(*p)).collect();
    let in_the_way: Vec<&String> = ch.added.iter().chain(&ch.modified).chain(&ch.deleted).filter(|p| touched.contains(p)).collect();
    if !in_the_way.is_empty() {
        for p in &in_the_way {
            println!("  {p}");
        }
        bail!("stash {n} changes {} file(s) you've also changed; commit or stash those first", in_the_way.len());
    }
    let (merged, conflicts) = merge_trees(&repo, &base, &ours, &theirs, "current", &format!("stash {n}"))?;

    // Your files as they are, with the stashed files brought back over them.
    let mut target: crate::merge::Files = files.iter().map(|f| (f.path.clone(), (f.mode, f.id))).collect();
    for p in &touched {
        match merged.get(*p) {
            Some(v) => target.insert((*p).clone(), *v),
            None => target.remove(*p),
        };
    }
    apply_tree(&repo, &mut index, &files, &target, &[])?;
    // The stashed work (and your other changes) show as uncommitted changes, as before.
    stage_as_changes(&mut index, &target, &ours);
    index.save(&repo.wt)?;

    crate::sparse::write_conflicts(&repo, &merged, conflicts.iter().map(|(p, _)| p))?;
    if !conflicts.is_empty() {
        println!("Brought back stash {n} with {} conflict(s):", conflicts.len());
        for (path, reason) in &conflicts {
            println!("  {} {path}  {}", paint("31", "C"), paint("2", reason));
        }
        println!("Fix the conflict markers in those files. The stash was kept, so nothing is lost;");
        println!("drop it with `copse stash drop {n}` once you're happy.");
        return Ok(());
    }

    if keep {
        println!("Applied stash {n}: {message} (still saved; `copse stash drop {n}` removes it)");
    } else {
        entries.remove(n);
        save(&repo, &entries)?;
        println!("Restored stash {n}: {message}");
    }
    let now = changes(&index, &snapshot(&repo.root, &index, None)?);
    print_changes(&now);
    Ok(())
}
