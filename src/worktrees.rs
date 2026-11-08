//! Worktrees: more than one working directory for the same repository, each on its
//! own branch, sharing history, branches, tags, stashes and settings.
//!
//!   copse worktree add PATH [BRANCH]      check BRANCH out in a new folder
//!   copse worktree add -b NEW PATH [REV]  ... on a new branch starting at REV
//!   copse worktree list
//!   copse worktree remove PATH [--force]
//!   copse worktree prune                  forget worktrees whose folders are gone
//!
//! A worktree's folder has a `.copse` file pointing at `.copse/worktrees/<name>/`
//! in the main repository, which holds its own HEAD, index, reflog for HEAD, and
//! any merge, rebase or bisect state. A branch can be checked out in only one
//! worktree at a time, so two working directories never fight over it.

use crate::cmd::{apply_tree, paint, print_changes};
use crate::objects::{flatten, short};
use crate::repo::{validate_branch, write_atomic, Head, Repo, WORKTREE_MARK};
use crate::worktree::{changes, snapshot, Index};
use anyhow::{bail, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

/// Every working directory of the repository: (root, its state folder, its HEAD).
pub fn all(repo: &Repo) -> Result<Vec<(PathBuf, PathBuf, Option<Head>)>> {
    let read_head = |wt: &Path| -> Option<Head> {
        let s = fs::read_to_string(wt.join("HEAD")).ok()?;
        let s = s.trim();
        Some(match s.strip_prefix("ref: ") {
            Some(b) => Head::Branch(b.to_string()),
            None => Head::Detached(crate::objects::parse_hex(s).ok()?),
        })
    };
    let main_root = if repo.dir.file_name().is_some_and(|n| n == crate::repo::DIR) {
        repo.dir.parent().map(Path::to_path_buf).unwrap_or_else(|| repo.dir.clone())
    } else {
        repo.dir.clone() // bare
    };
    let mut out = vec![(main_root, repo.dir.clone(), read_head(&repo.dir))];
    if let Ok(entries) = fs::read_dir(repo.dir.join("worktrees")) {
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let wt = e.path();
            let root = fs::read_to_string(wt.join("path")).map(|p| PathBuf::from(p.trim())).unwrap_or_default();
            out.push((root, wt.clone(), read_head(&wt)));
        }
    }
    Ok(out)
}

/// The other working directory that has `branch` checked out, if any.
pub fn checked_out_elsewhere(repo: &Repo, branch: &str) -> Result<Option<PathBuf>> {
    for (root, wt, head) in all(repo)? {
        if wt != repo.wt && matches!(&head, Some(Head::Branch(b)) if b == branch) {
            return Ok(Some(root));
        }
    }
    Ok(None)
}

fn add(repo: &Repo, path: &str, rev: Option<&str>, new_branch: Option<&str>) -> Result<()> {
    let root = std::env::current_dir()?.join(path);
    if root.exists() && fs::read_dir(&root)?.next().is_some() {
        bail!("{} already exists and isn't empty", root.display());
    }
    // What to check out: a new branch at REV, an existing branch, or a commit (detached).
    let (head, commit) = match new_branch {
        Some(b) => {
            validate_branch(b)?;
            if repo.branch(b)?.is_some() {
                bail!("branch {b} already exists; leave out -b to check it out");
            }
            let start = match rev {
                Some(r) => repo.resolve(r)?,
                None => repo.head_commit()?.context("there are no commits yet")?,
            };
            repo.set_branch(b, &start)?;
            (Head::Branch(b.to_string()), start)
        }
        None => {
            let name = match rev {
                Some(r) => r.to_string(),
                None => root.file_name().map(|n| n.to_string_lossy().to_string()).context("give the worktree a folder name")?,
            };
            match repo.branch(&name)? {
                Some(id) => {
                    let in_use = all(repo)?.into_iter().find(|(_, _, h)| matches!(h, Some(Head::Branch(b)) if *b == name));
                    if let Some((other, _, _)) = in_use {
                        bail!("branch {name} is already checked out in {}; a branch can be in only one worktree at a time (use -b NEW to start another branch)", other.display());
                    }
                    (Head::Branch(name), id)
                }
                None if rev.is_some() => {
                    let id = repo.resolve(&name)?;
                    (Head::Detached(id), id)
                }
                None => bail!("there's no branch called {name}; use -b {name} to create it"),
            }
        }
    };

    // Its state folder in the main repository, named after the folder (made unique).
    let base = root.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "worktree".into());
    let mut name = base.clone();
    let mut n = 1;
    while repo.dir.join("worktrees").join(&name).exists() {
        n += 1;
        name = format!("{base}{n}");
    }
    let wt = repo.dir.join("worktrees").join(&name);
    fs::create_dir_all(wt.join("logs"))?;
    fs::create_dir_all(&root)?;
    let common = repo.dir.canonicalize()?;
    let root = root.canonicalize()?;
    let wt = wt.canonicalize()?;
    fs::write(wt.join("commondir"), format!("{}\n", common.display()))?;
    fs::write(wt.join("path"), format!("{}\n", root.display()))?;
    fs::write(wt.join("HEAD"), "ref: main\n")?; // replaced below
    write_atomic(&crate::objects::disk_path(&root, crate::repo::DIR), format!("{WORKTREE_MARK}{}\n", wt.display()).as_bytes())?;

    let result = (|| -> Result<()> {
        let new = Repo::open_path(&root)?;
        let tree = flatten(&new.store, &new.read_commit(&commit)?.tree)?;
        let mut index = Index::default();
        apply_tree(&new, &mut index, &[], &tree, &[])?;
        index.save(&new.wt)?;
        new.set_head(&head)?;
        Ok(())
    })();
    if let Err(e) = result {
        let _ = fs::remove_dir_all(&wt);
        let _ = fs::remove_dir_all(&root);
        return Err(e);
    }
    let what = match &head {
        Head::Branch(b) => format!("branch {b}"),
        Head::Detached(id) => format!("{} (detached)", short(id)),
    };
    println!("Made a worktree at {} on {what}.", root.display());
    println!("  It shares this repository's history, branches and settings. `copse worktree remove {path}` when you're done.");
    Ok(())
}

fn list(repo: &Repo) -> Result<()> {
    for (i, (root, wt, head)) in all(repo)?.into_iter().enumerate() {
        let id = match &head {
            Some(Head::Branch(b)) => repo.branch(b)?,
            Some(Head::Detached(id)) => Some(*id),
            None => None,
        };
        let label = match &head {
            Some(Head::Branch(b)) => paint("1", b),
            Some(Head::Detached(_)) => "(detached)".into(),
            None => "(unreadable)".into(),
        };
        let gone = if i > 0 && !root.exists() { paint("31", "  missing; `copse worktree prune` forgets it") } else { String::new() };
        let here = if wt == repo.wt { paint("32", "  ← you are here") } else { String::new() };
        println!("{}  {}  {label}{here}{gone}", root.display(), id.map_or("          ".into(), |i| paint("33", &short(&i))));
    }
    Ok(())
}

fn remove(repo: &Repo, path: &str, force: bool) -> Result<()> {
    let target = std::env::current_dir()?.join(path);
    let target = target.canonicalize().unwrap_or(target);
    let found = all(repo)?.into_iter().skip(1).find(|(root, _, _)| *root == target);
    let Some((root, wt, _)) = found else {
        bail!("{} isn't a worktree of this repository (`copse worktree list` shows them)", target.display());
    };
    if wt == repo.wt {
        bail!("you're inside that worktree; run this from another one");
    }
    if root.exists() && !force {
        let other = Repo::open_path(&root)?;
        let index = Index::load(&other.wt)?;
        let ch = changes(&index, &snapshot(&other.root, &index, None)?);
        if !ch.is_empty() {
            print_changes(&ch);
            bail!("that worktree has {} uncommitted change(s); commit them, or use --force to throw them away", ch.len());
        }
    }
    if root.exists() {
        fs::remove_dir_all(&root).with_context(|| format!("removing {}", root.display()))?;
    }
    fs::remove_dir_all(&wt)?;
    println!("Removed the worktree at {}. Its branch and commits are still here.", root.display());
    Ok(())
}

fn prune(repo: &Repo) -> Result<()> {
    let mut n = 0;
    for (root, wt, _) in all(repo)?.into_iter().skip(1) {
        if !crate::objects::disk_path(&root, crate::repo::DIR).exists() {
            fs::remove_dir_all(&wt)?;
            println!("Forgot {} (its folder is gone).", root.display());
            n += 1;
        }
    }
    if n == 0 {
        println!("Nothing to prune.");
    }
    Ok(())
}

pub enum WorktreeCmd<'a> {
    List,
    Add { path: &'a str, rev: Option<&'a str>, new_branch: Option<&'a str> },
    Remove { path: &'a str, force: bool },
    Prune,
}

pub fn worktree(cmd: WorktreeCmd) -> Result<()> {
    let repo = Repo::find_any()?;
    match cmd {
        WorktreeCmd::List => list(&repo),
        WorktreeCmd::Add { path, rev, new_branch } => add(&repo, path, rev, new_branch),
        WorktreeCmd::Remove { path, force } => remove(&repo, path, force),
        WorktreeCmd::Prune => prune(&repo),
    }
}
