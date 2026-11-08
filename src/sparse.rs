//! Sparse checkout: only some folders of the project on disk.
//!
//!   copse sparse                    show which folders are checked out
//!   copse sparse set FOLDER...      check out only these folders (and top-level files)
//!   copse sparse add FOLDER...      add more folders
//!   copse sparse disable            check everything out again
//!
//! The folder list lives in `.copse/sparse` (per worktree). History is untouched:
//! commits still contain every file. The rule that makes this work everywhere:
//! outside the chosen folders, a committed file that isn't on disk is taken as
//! unchanged (worktree.rs adds it back to every snapshot), and checkouts don't
//! write files there (cmd.rs). Files that *are* on disk outside them, such as a
//! conflict written out for you to fix, behave normally.

use crate::cmd::{paint, print_changes, write_file};
use crate::merge::Files;
use crate::objects::flatten;
use crate::repo::{write_atomic, Repo};
use crate::worktree::{changes, snapshot, Index};
use anyhow::{bail, Context, Result};
use std::fs;
use std::path::Path;

#[derive(Default, Clone, PartialEq)]
pub struct Sparse {
    /// Folders checked out (repository paths, no trailing slash). Empty: not sparse.
    pub folders: Vec<String>,
}

impl Sparse {
    pub fn load(wt: &Path) -> Sparse {
        let folders = fs::read_to_string(wt.join("sparse"))
            .unwrap_or_default()
            .lines()
            .map(|l| l.trim().trim_matches('/').to_string())
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .collect();
        Sparse { folders }
    }

    /// The sparse settings for the working directory at `root`.
    pub fn for_root(root: &Path) -> Sparse {
        Repo::open_path(root).map(|r| Sparse::load(&r.wt)).unwrap_or_default()
    }

    pub fn active(&self) -> bool {
        !self.folders.is_empty()
    }

    /// Whether `path` is outside the checked-out folders. Top-level files are
    /// always checked out (as in git's cone mode).
    pub fn outside(&self, path: &str) -> bool {
        self.active() && path.contains('/') && !self.folders.iter().any(|f| path.starts_with(&format!("{f}/")))
    }

    fn save(&self, repo: &Repo) -> Result<()> {
        let path = repo.wt.join("sparse");
        if self.folders.is_empty() {
            match fs::remove_file(&path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
                _ => {}
            }
            return Ok(());
        }
        write_atomic(&path, format!("{}\n", self.folders.join("\n")).as_bytes())
    }
}

/// Write conflicted files out even where sparse checkout would leave them off disk,
/// so they can be fixed.
pub fn write_conflicts<'a>(repo: &Repo, merged: &Files, conflicts: impl Iterator<Item = &'a String>) -> Result<()> {
    let sparse = Sparse::load(&repo.wt);
    if !sparse.active() {
        return Ok(());
    }
    for path in conflicts {
        if sparse.outside(path) && !repo.root.join(path).exists() {
            if let Some((mode, id)) = merged.get(path) {
                write_file(repo, path, *mode, id)?;
            }
        }
    }
    Ok(())
}

fn remove_and_tidy(root: &Path, path: &str) -> Result<()> {
    let full = root.join(path);
    fs::remove_file(&full).with_context(|| format!("removing {path}"))?;
    let mut dir = full.parent();
    while let Some(d) = dir.filter(|d| *d != root) {
        if fs::remove_dir(d).is_err() {
            break;
        }
        dir = d.parent();
    }
    Ok(())
}

/// Make the files on disk match `new`: remove committed files now outside it,
/// write the ones now inside that aren't on disk.
fn apply(repo: &Repo, old: &Sparse, new: &Sparse) -> Result<(usize, usize)> {
    let index = Index::load(&repo.wt)?;
    let files = snapshot(&repo.root, &index, None)?;
    let ch = changes(&index, &files);
    // Uncommitted work in files about to disappear would be lost.
    let at_risk: Vec<&String> =
        ch.added.iter().chain(&ch.modified).filter(|p| new.outside(p) && repo.root.join(p.as_str()).exists()).collect();
    if !at_risk.is_empty() {
        for p in &at_risk {
            println!("  {p}");
        }
        bail!("commit or stash your changes to {} file(s) outside those folders first", at_risk.len());
    }
    let head = repo.head_commit()?.context("commit something first")?;
    let tree = flatten(&repo.store, &repo.read_commit(&head)?.tree)?;
    let (mut removed, mut written) = (0, 0);
    for (path, (mode, id)) in &tree {
        let on_disk = repo.root.join(path).symlink_metadata().is_ok();
        if new.outside(path) && on_disk && index.entries.contains_key(path) {
            remove_and_tidy(&repo.root, path)?;
            removed += 1;
        } else if !new.outside(path) && !on_disk && (old.outside(path) || !ch.deleted.contains(path)) {
            write_file(repo, path, *mode, id)?;
            written += 1;
        }
    }
    new.save(repo)?;
    Ok((removed, written))
}

pub enum SparseCmd<'a> {
    Show,
    Set(&'a [String]),
    Add(&'a [String]),
    Disable,
}

pub fn sparse(cmd: SparseCmd) -> Result<()> {
    let repo = Repo::find()?;
    if crate::rebase::RebaseState::load(&repo)?.is_some() || crate::merge::MergeState::load(&repo)?.is_some() {
        bail!("finish the merge or rebase in progress first");
    }
    let old = Sparse::load(&repo.wt);
    let clean = |folders: &[String]| -> Result<Vec<String>> {
        let mut out = Vec::new();
        for f in folders {
            let p = crate::restore::repo_path(&repo, f)?;
            if p.is_empty() {
                bail!("{f} is the whole project; use `copse sparse disable` to check everything out");
            }
            out.push(p);
        }
        Ok(out)
    };
    let new = match cmd {
        SparseCmd::Show => {
            if !old.active() {
                println!("Not sparse: every folder is checked out. `copse sparse set FOLDER...` checks out only some.");
            } else {
                println!("Checked out (plus top-level files):");
                for f in &old.folders {
                    println!("  {f}/");
                }
            }
            return Ok(());
        }
        SparseCmd::Set(folders) => Sparse { folders: clean(folders)? },
        SparseCmd::Add(folders) => {
            if !old.active() {
                bail!("not sparse yet; use `copse sparse set FOLDER...` first");
            }
            let mut s = old.clone();
            for f in clean(folders)? {
                if !s.folders.contains(&f) {
                    s.folders.push(f);
                }
            }
            s
        }
        SparseCmd::Disable => Sparse::default(),
    };
    if let Some(head) = repo.head_commit()? {
        let tree = flatten(&repo.store, &repo.read_commit(&head)?.tree)?;
        for f in &new.folders {
            if !tree.keys().any(|p| p.starts_with(&format!("{f}/"))) {
                eprintln!("copse: note: {f}/ has no committed files yet");
            }
        }
    }
    let (removed, written) = apply(&repo, &old, &new)?;
    if new.active() {
        println!("Checked out {} folder(s) (plus top-level files): {}", new.folders.len(), new.folders.join(", "));
    } else {
        println!("Sparse checkout off: every folder is checked out.");
    }
    println!("  {written} file(s) written, {removed} removed {}", paint("2", "(history is unchanged; commits still have every file)"));
    let index = Index::load(&repo.wt)?;
    let ch = changes(&index, &snapshot(&repo.root, &index, None)?);
    if !ch.is_empty() {
        print_changes(&ch);
    }
    Ok(())
}
