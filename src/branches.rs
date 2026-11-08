//! `copse branch`: list, create, delete and rename branches.
//!
//!   copse branch                   list branches
//!   copse branch NAME [REV]        create NAME at REV (default: HEAD)
//!   copse branch -d NAME           delete NAME, if its work is merged into HEAD
//!   copse branch -D NAME           delete NAME even if that strands commits
//!   copse branch -m [OLD] NEW      rename OLD (default: the current branch) to NEW
//!
//! Deleting only removes the name: the commits stay in the repository, and copse
//! prints the command that brings the branch back.

use crate::cmd::paint;
use crate::merge::ancestors;
use crate::objects::short;
use crate::rebase::RebaseState;
use crate::repo::{validate_branch, Head, Repo};
use anyhow::{bail, Context, Result};

pub struct BranchArgs<'a> {
    pub name: Option<&'a str>,
    pub second: Option<&'a str>,
    pub delete: bool,
    pub rename: bool,
    pub force: bool,
}

pub fn branch(args: BranchArgs) -> Result<()> {
    let repo = Repo::find()?;
    match (args.name, args.delete, args.rename) {
        (None, false, false) => list(&repo),
        (Some(name), true, false) => delete(&repo, name, args.force),
        (Some(first), false, true) => match args.second {
            Some(new) => rename(&repo, first, new, args.force),
            None => match repo.head()? {
                Head::Branch(current) => rename(&repo, &current, first, args.force),
                Head::Detached(_) => bail!("HEAD is detached; say which branch to rename: copse branch -m OLD NEW"),
            },
        },
        (Some(name), false, false) => create(&repo, name, args.second),
        (None, true, _) => bail!("say which branch to delete: copse branch -d NAME"),
        (None, _, true) => bail!("say the new name: copse branch -m NEW (or -m OLD NEW)"),
        (Some(_), true, true) => bail!("use either -d (delete) or -m (rename), not both"),
    }
}

fn current_branch(repo: &Repo) -> Result<Option<String>> {
    Ok(match repo.head()? {
        Head::Branch(b) => Some(b),
        Head::Detached(_) => None,
    })
}

fn list(repo: &Repo) -> Result<()> {
    let current = current_branch(repo)?;
    for (name, id) in repo.branches()? {
        let marker = if current.as_deref() == Some(name.as_str()) { "*" } else { " " };
        println!("{marker} {name}  {}", paint("33", &short(&id)));
    }
    Ok(())
}

fn create(repo: &Repo, name: &str, start: Option<&str>) -> Result<()> {
    validate_branch(name)?;
    if repo.branch(name)?.is_some() {
        bail!("branch {name} already exists");
    }
    let at = match start {
        Some(rev) => repo.resolve(rev)?,
        None => repo.head_commit()?.context("commit something before creating a branch")?,
    };
    repo.set_branch(name, &at)?;
    println!("Created branch {name} at {}. Switch to it with `copse checkout {name}`.", short(&at));
    Ok(())
}

/// Refuse to touch the branch an unfinished rebase will write its result to.
fn not_mid_rebase(repo: &Repo, name: &str) -> Result<()> {
    if let Some(r) = RebaseState::load(repo)? {
        if r.branch == name {
            bail!("{name} is being rebased; finish with `copse rebase --continue` or `--abort` first");
        }
    }
    Ok(())
}

fn delete(repo: &Repo, name: &str, force: bool) -> Result<()> {
    let tip = repo.branch(name)?.with_context(|| format!("there is no branch called {name}"))?;
    if current_branch(repo)?.as_deref() == Some(name) {
        bail!("you're on {name}; switch to another branch before deleting it");
    }
    if let Some(other) = crate::worktrees::checked_out_elsewhere(repo, name)? {
        bail!("{name} is checked out in the worktree at {}; switch it to another branch first", other.display());
    }
    not_mid_rebase(repo, name)?;
    if !force {
        // Commits only this branch reaches would be stranded without its name.
        let stranded = match repo.head_commit()? {
            Some(head) => {
                let from_head = ancestors(repo, head)?;
                ancestors(repo, tip)?.iter().filter(|c| !from_head.contains(c)).count()
            }
            None => 1,
        };
        if stranded > 0 {
            bail!(
                "{name} has {stranded} commit(s) that aren't merged into your current branch. \
                 Merge it first, or use `copse branch -D {name}` to delete it anyway"
            );
        }
    }
    repo.delete_branch(name)?;
    println!("Deleted branch {name} (was {}).", short(&tip));
    println!("  To undo: copse branch {name} {}", short(&tip));
    Ok(())
}

fn rename(repo: &Repo, old: &str, new: &str, force: bool) -> Result<()> {
    validate_branch(new)?;
    repo.branch(old)?.with_context(|| format!("there is no branch called {old}"))?;
    if old == new {
        bail!("{old} already has that name");
    }
    if repo.branch(new)?.is_some() && !force {
        bail!("branch {new} already exists; use `copse branch -M {old} {new}` to replace it");
    }
    not_mid_rebase(repo, old)?;
    if let Some(other) = crate::worktrees::checked_out_elsewhere(repo, old)? {
        bail!("{old} is checked out in the worktree at {}; rename it from there", other.display());
    }
    if repo.branch(new)?.is_some() {
        repo.delete_branch(new)?; // -M: the replaced branch's history stays findable
    }
    repo.rename_branch(old, new)?;
    println!("Renamed branch {old} to {new}.");
    Ok(())
}
