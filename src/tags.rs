//! `copse tag`: name a commit, usually a release.
//!
//!   copse tag                          list tags
//!   copse tag v1.0 [rev]               lightweight tag: just a name for a commit
//!   copse tag -a v1.0 -m "..." [rev]   annotated tag: a tag object with its own
//!                                     message, author and date (what GitHub
//!                                     releases usually use)
//!   copse tag -d v1.0                  delete
//!
//! Tags live in `.copse/tags/<name>`, holding a commit id (lightweight) or a tag
//! object id (annotated).

use crate::cmd::paint;
use crate::objects::{short, Id, Kind, Tag};
use crate::repo::{validate_branch, Repo};
use anyhow::{bail, Context, Result};

pub struct TagArgs<'a> {
    pub name: Option<&'a str>,
    pub rev: Option<&'a str>,
    pub message: Option<&'a str>,
    pub annotate: bool,
    /// Sign it (Some(true)), don't (Some(false)), or follow tag.gpgSign (None).
    pub sign: Option<bool>,
    pub delete: bool,
    pub force: bool,
}

pub fn tag(args: TagArgs) -> Result<()> {
    let repo = Repo::find_any()?;
    let Some(name) = args.name else {
        return list(&repo);
    };
    if args.delete {
        let old = repo.tag(name)?.with_context(|| format!("there is no tag called {name}"))?;
        repo.delete_tag(name)?;
        println!("Deleted tag {name} (was {})", short(&old));
        return Ok(());
    }
    validate_branch(name)?;
    if let (Some(old), false) = (repo.tag(name)?, args.force) {
        bail!("tag {name} already exists (at {}); use --force to move it", short(&repo.peel(old)?));
    }
    let commit = repo.resolve(args.rev.unwrap_or("HEAD"))?;
    let title = repo.read_commit(&commit)?.message.lines().next().unwrap_or_default().to_string();

    let sign = args.sign.unwrap_or_else(crate::signing::sign_tags_by_default);
    let annotated = args.annotate || args.message.is_some() || sign;
    if annotated {
        let message = args.message.map(str::trim).filter(|m| !m.is_empty());
        let message = message.context("an annotated tag needs a message: copse tag -a NAME -m \"...\"")?;
        let mut tag = Tag::new(commit, Kind::Commit, name, message.to_string());
        if sign {
            crate::signing::sign_tag(&repo, &mut tag)?;
        }
        let id = repo.store.put(Kind::Tag, &tag.encode())?;
        repo.set_tag(name, &id)?;
        println!("Tagged {} {title} as {name} ({})", short(&commit), if sign { "annotated, signed" } else { "annotated" });
    } else {
        repo.set_tag(name, &commit)?;
        println!("Tagged {} {title} as {name}", short(&commit));
    }
    Ok(())
}

fn list(repo: &Repo) -> Result<()> {
    let tags = repo.tags()?;
    if tags.is_empty() {
        println!("No tags. Create one with `copse tag v1.0` or `copse tag -a v1.0 -m \"First release\"`.");
    }
    for (name, id) in tags {
        let (commit, note) = describe(repo, id)?;
        println!("{}  {}  {}", paint("33", &name), short(&commit), paint("2", &note));
    }
    Ok(())
}

/// The commit a tag points at, and a one-line description: an annotated tag's
/// own message, or the commit's title for a lightweight one.
fn describe(repo: &Repo, id: Id) -> Result<(Id, String)> {
    let (kind, data) = repo.store.get(&id)?;
    let commit = repo.peel(id)?;
    let note = if kind == Kind::Tag {
        Tag::decode(&data)?.message.lines().next().unwrap_or_default().to_string()
    } else {
        repo.read_commit(&commit)?.message.lines().next().unwrap_or_default().to_string()
    };
    Ok((commit, note))
}
