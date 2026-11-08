//! remote, fetch, pull, push and clone.

use crate::cmd::{apply_tree, paint};
use crate::merge::is_ancestor;
use crate::objects::{flatten, short, Id};
use crate::repo::{validate_remote, Head, Repo};
use crate::transport::{normalize_url, objects_to_send, Conn, RemoteRefs};
use crate::worktree::{snapshot, Index};
use anyhow::{bail, Context, Result};
use std::fs;
use std::path::PathBuf;

fn human(bytes: u64) -> String {
    match bytes {
        b if b >= 1 << 30 => format!("{:.1} GB", b as f64 / (1u64 << 30) as f64),
        b if b >= 1 << 20 => format!("{:.1} MB", b as f64 / (1u64 << 20) as f64),
        b if b >= 1 << 10 => format!("{:.1} KB", b as f64 / (1u64 << 10) as f64),
        b => format!("{b} bytes"),
    }
}

fn current_branch(repo: &Repo) -> Result<String> {
    match repo.head()? {
        Head::Branch(b) => Ok(b),
        Head::Detached(_) => bail!("HEAD is detached; say which branch, e.g. `copse push origin main`"),
    }
}

// ---------------------------------------------------------------- remote

pub fn list() -> Result<()> {
    let repo = Repo::find()?;
    let remotes = repo.remotes()?;
    if remotes.is_empty() {
        println!("No remotes. Add one with `copse remote add origin <path or user@host:path>`.");
    }
    for (name, url) in remotes {
        match git_url(&url) {
            Some(g) => println!("{name}  {}  (git)", crate::githttp::redact(g)),
            None => println!("{name}  {url}"),
        }
    }
    Ok(())
}

/// Git remotes are stored with this prefix in `.copse/remotes`.
const GIT_PREFIX: &str = "git::";

/// The address of a git (not copse) remote, if this is one.
fn git_url(stored: &str) -> Option<&str> {
    stored.strip_prefix(GIT_PREFIX)
}

/// Whether an address is a git server rather than another copse repository: anything
/// on the web, known git hosts, a local folder that is a git repository, or (for
/// addresses that can't be looked at, such as SSH) anything ending in `.git`.
fn looks_like_git(url: &str) -> bool {
    let hosts = ["github.com", "gitlab.com", "bitbucket.org", "codeberg.org"];
    if url.starts_with("https://") || url.starts_with("http://") {
        return true; // copse's own remotes are folders or SSH; anything on the web is git
    }
    if hosts.iter().any(|h| url.contains(h)) {
        return true;
    }
    if let crate::transport::Url::Local(p) = crate::transport::parse_url(url) {
        if p.exists() {
            // A folder says what it is. A git repository has a `.git` folder, or (bare)
            // a HEAD like "ref: refs/heads/main" or a SHA-1; copse's says "ref: main".
            let git_head = std::fs::read_to_string(p.join("HEAD"))
                .map(|h| h.starts_with("ref: refs/") || (h.trim().len() == 40 && h.trim().chars().all(|c| c.is_ascii_hexdigit())))
                .unwrap_or(false);
            return p.join(".git").is_dir() || git_head;
        }
    }
    url.trim_end_matches('/').ends_with(".git")
}

pub fn add(name: &str, url: &str, git: bool) -> Result<()> {
    validate_remote(name)?;
    let repo = Repo::find()?;
    let mut remotes = repo.remotes()?;
    if remotes.iter().any(|(n, _)| n == name) {
        bail!("remote {name} already exists");
    }
    let is_git = git || looks_like_git(url);
    let url = if is_git {
        // Parsing also rejects addresses copse can't use yet (HTTPS), before anything is saved.
        match crate::gitwire::parse_git_url(url)? {
            crate::gitwire::GitUrl::Local(_) => normalize_url(url)?,
            _ => url.to_string(),
        }
    } else {
        normalize_url(url)?
    };
    let stored = if is_git { format!("{GIT_PREFIX}{url}") } else { url.clone() };
    remotes.push((name.to_string(), stored));
    repo.save_remotes(&remotes)?;
    if is_git {
        println!("Added git remote {name} → {}", crate::githttp::redact(&url));
        println!("  `copse push {name}` and `copse pull {name}` work with it like any remote.");
    } else {
        println!("Added remote {name} → {url}");
    }
    Ok(())
}

pub fn remove(name: &str) -> Result<()> {
    let repo = Repo::find()?;
    let mut remotes = repo.remotes()?;
    let before = remotes.len();
    remotes.retain(|(n, _)| n != name);
    if remotes.len() == before {
        bail!("there is no remote called {name}");
    }
    repo.save_remotes(&remotes)?;
    if validate_remote(name).is_ok() {
        let _ = fs::remove_dir_all(repo.dir.join("tracking").join(name));
    }
    println!("Removed remote {name}");
    Ok(())
}

// ---------------------------------------------------------------- fetch

/// Download every branch of `remote` that we don't have, and update remote/branch refs.
/// How much history to fetch.
#[derive(Clone, Copy, Default, PartialEq)]
pub enum Depth {
    /// Everything new (a shallow repository stays as shallow as it is).
    #[default]
    Full,
    /// Only the newest n commits (`--depth n`).
    Last(usize),
    /// Fetch everything a shallow clone is missing (`--unshallow`).
    Unshallow,
}

fn fetch_all(repo: &Repo, remote: &str, depth: Depth) -> Result<RemoteRefs> {
    let url = repo.remote_url(remote)?;
    let refs = match git_url(&url) {
        Some(git) => {
            if depth != Depth::Full {
                bail!("--depth and --unshallow work with copse remotes; shallow clones from git servers aren't supported yet");
            }
            crate::gitimport::fetch(repo, git)?
        }
        None => {
            let mut conn = Conn::open(&url)?;
            let refs = conn.refs()?;
            let shallow = repo.shallow();
            // A shallow fetch takes only branches; tags follow afterwards if they name
            // history that arrived (as git does), rather than pulling in their own.
            let shallow_mode = matches!(depth, Depth::Last(_)) || (!shallow.is_empty() && depth != Depth::Unshallow);
            let wanted_refs: Vec<&(String, Id)> =
                if shallow_mode { refs.branches.iter().collect() } else { refs.branches.iter().chain(&refs.tags).collect() };
            let mut wants: Vec<Id> = wanted_refs.iter().map(|(_, id)| *id).filter(|id| !repo.store.contains(id)).collect();
            wants.sort_unstable_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
            wants.dedup();
            if depth == Depth::Unshallow {
                if shallow.is_empty() {
                    bail!("this repository isn't shallow; it already has all of history");
                }
                // The parents left out at the boundary, with everything below them.
                for id in &shallow {
                    wants.extend(repo.read_commit_raw(id)?.parents);
                }
            }
            if !wants.is_empty() {
                let mut haves: Vec<Id> = repo.branches()?.into_iter().map(|(_, id)| id).collect();
                haves.extend(repo.tracking_refs()?.into_iter().map(|(_, id)| id));
                let mine: Vec<Id> = shallow.iter().copied().collect();
                let d = if let Depth::Last(n) = depth { Some(n) } else { None };
                let (n, bytes, boundary) = conn.fetch_filtered(&repo.store, &wants, &haves, &mine, d, repo.is_partial())?;
                println!("Received {n} objects ({})", human(bytes));
                let mut now = if depth == Depth::Unshallow { Default::default() } else { shallow };
                now.extend(boundary);
                if now != repo.shallow() {
                    repo.set_shallow(&now)?;
                }
                if depth == Depth::Unshallow {
                    println!("All of history is here now; this is no longer a shallow clone.");
                }
            }
            if shallow_mode {
                // Annotated tags naming commits we now have: just the tag objects.
                let follow: Vec<Id> = refs
                    .tags
                    .iter()
                    .zip(&refs.tag_targets)
                    .filter(|((_, id), target)| !repo.store.contains(id) && target.is_some_and(|t| repo.store.contains(&t)))
                    .map(|((_, id), _)| *id)
                    .collect();
                if !follow.is_empty() {
                    let haves: Vec<Id> = refs.branches.iter().map(|(_, id)| *id).filter(|id| repo.store.contains(id)).collect();
                    let mine: Vec<Id> = repo.shallow().into_iter().collect();
                    conn.fetch(&repo.store, &follow, &haves, &mine, Some(1))?;
                }
            }
            // Tags become local tags, as with git servers. Yours are never overwritten.
            for (name, id) in &refs.tags {
                if !repo.store.contains(id) {
                    continue; // names something outside the history we fetched (shallow)
                }
                match repo.tag(name)? {
                    None => {
                        repo.set_tag(name, id)?;
                        println!("  {}  new tag", paint("32", &format!("tag {name}")));
                    }
                    Some(mine) if mine == *id => {}
                    Some(_) => println!("  tag {name} on the server differs from yours; kept yours"),
                }
            }
            refs
        }
    };
    for (branch, id) in &refs.branches {
        let old = repo.tracking(remote, branch)?;
        if old != Some(*id) {
            repo.set_tracking(remote, branch, id)?;
            let change = match old {
                Some(o) => format!("{}..{}", short(&o), short(id)),
                None => format!("new, at {}", short(id)),
            };
            println!("  {}  {change}", paint("32", &format!("{remote}/{branch}")));
        }
    }
    // Branches deleted on the server: drop their remote/branch names (the commits stay).
    for (full, _) in repo.tracking_refs()? {
        if let Some((r, b)) = full.split_once('/') {
            if r == remote && !refs.branches.iter().any(|(name, _)| name == b) {
                repo.delete_tracking(r, b)?;
                println!("  {full}  deleted on the server");
            }
        }
    }
    Ok(refs)
}

pub fn fetch(remote: &str, depth: Depth) -> Result<()> {
    let repo = Repo::find()?;
    fetch_all(&repo, remote, depth)?;
    Ok(())
}

// ---------------------------------------------------------------- pull

pub fn pull(remote: &str, branch: Option<&str>, rebase: bool) -> Result<()> {
    let repo = Repo::find()?;
    let branch = match branch {
        Some(b) => b.to_string(),
        None => current_branch(&repo)?,
    };
    fetch_all(&repo, remote, Depth::Full)?;
    let theirs = repo.tracking(remote, &branch)?.with_context(|| format!("{remote} has no branch called {branch}"))?;

    if repo.head_commit()?.is_none() {
        // Nothing here yet: just take the remote's branch.
        let mut index = Index::load(&repo.wt)?;
        if !snapshot(&repo.root, &index, None)?.is_empty() {
            bail!("this repository has no commits but has files; commit them first so pulling can't overwrite them");
        }
        let tree = flatten(&repo.store, &repo.read_commit(&theirs)?.tree)?;
        apply_tree(&repo, &mut index, &[], &tree, &[])?;
        index.save(&repo.wt)?;
        repo.advance_head(&theirs)?;
        println!("Checked out {remote}/{branch} ({} files)", tree.len());
        return Ok(());
    }
    let target = format!("{remote}/{branch}");
    if rebase {
        crate::rebase::rebase(crate::rebase::RebaseArgs { upstream: Some(&target), ..Default::default() })
    } else {
        crate::merge::merge(Some(&target), false, crate::merge::FastForward::Allow, None)
    }
}

// ---------------------------------------------------------------- push

pub fn push(remote: &str, name: Option<&str>, force: bool, tags: bool, delete: bool, no_verify: bool) -> Result<()> {
    use crate::gitwire::RefUpdate;
    let repo = Repo::find()?;
    let url = repo.remote_url(remote)?;
    if !no_verify {
        // Tell the hook what's about to go: one "<name> <commit>" line per ref.
        let mut lines = String::new();
        if delete {
            lines += &format!("{} delete\n", name.unwrap_or_default());
        } else {
            let mut add = |n: &str, id: Option<crate::objects::Id>| {
                if let Some(id) = id {
                    lines += &format!("{n} {}\n", id.to_hex());
                }
            };
            match name {
                Some(n) => add(n, repo.branch(n)?.or(repo.tag(n)?)),
                None if !tags => {
                    let b = current_branch(&repo)?;
                    add(&b, repo.branch(&b)?);
                }
                None => {}
            }
            if tags {
                for (n, id) in repo.tags()? {
                    add(&n, Some(id));
                }
            }
        }
        crate::hooks::check(&repo, "pre-push", &[remote, &url], Some(&lines), "push")?;
    }
    if delete {
        let name = name.context("say what to delete: copse push origin --delete BRANCH")?;
        if let Some(git) = git_url(&url) {
            return crate::gitwire::delete_remote(&repo, remote, git, name);
        }
        let mut conn = Conn::open(&url)?;
        let refs = conn.refs()?;
        let old = refs.branches.iter().find(|(b, _)| b == name).map(|(_, id)| *id);
        let old = old.with_context(|| format!("{remote} has no branch called {name}"))?;
        conn.delete(name, old)?;
        repo.delete_tracking(remote, name)?;
        println!("Deleted branch {name} on {remote} (was {}).", short(&old));
        return Ok(());
    }
    if let Some(git) = git_url(&url) {
        let mut updates = Vec::new();
        match name {
            // A name can be a branch or a tag; a branch wins if both exist.
            Some(n) => match (repo.branch(n)?, repo.tag(n)?) {
                (Some(id), _) => updates.push(RefUpdate { name: n.to_string(), is_tag: false, local: id }),
                (None, Some(id)) => updates.push(RefUpdate { name: n.to_string(), is_tag: true, local: id }),
                (None, None) => bail!("there's no branch or tag called {n}"),
            },
            None if !tags => {
                let b = current_branch(&repo)?;
                let id = repo.branch(&b)?.with_context(|| format!("branch {b} has no commits to push"))?;
                updates.push(RefUpdate { name: b, is_tag: false, local: id });
            }
            None => {}
        }
        if tags {
            for (n, id) in repo.tags()? {
                if !updates.iter().any(|u| u.is_tag && u.name == n) {
                    updates.push(RefUpdate { name: n, is_tag: true, local: id });
                }
            }
            if updates.is_empty() {
                println!("No tags to push.");
                return Ok(());
            }
        }
        return crate::gitwire::push(&repo, remote, git, &updates, force);
    }
    let names_a_tag = match name {
        Some(n) => repo.branch(n)?.is_none() && repo.tag(n)?.is_some(),
        None => false,
    };
    if tags || names_a_tag {
        return push_tags(&repo, remote, &url, if tags { None } else { name }, force);
    }
    let branch = match name {
        Some(b) => b.to_string(),
        None => current_branch(&repo)?,
    };
    let local = repo.branch(&branch)?.with_context(|| format!("branch {branch} has no commits to push"))?;
    let mut conn = Conn::open(&url)?;
    let refs = conn.refs()?;
    let remote_old = refs.branches.iter().find(|(b, _)| *b == branch).map(|(_, id)| *id);

    if remote_old == Some(local) {
        repo.set_tracking(remote, &branch, &local)?;
        println!("Everything up to date.");
        return Ok(());
    }
    if let Some(old) = remote_old {
        let fast_forward = repo.store.contains(&old) && is_ancestor(&repo, old, local)?;
        if !fast_forward && !force {
            bail!(
                "rejected: {remote}/{branch} has commits you don't have. \
                 Run `copse pull` to merge them first, or `copse push --force` to throw them away."
            );
        }
    }

    let mut haves: Vec<Id> = refs.branches.iter().map(|(_, id)| *id).collect();
    haves.extend(refs.tags.iter().map(|(_, id)| *id));
    let objects = objects_to_send(&repo, &[local], &haves)?;
    if repo.is_shallow() {
        let boundary: Vec<Id> = repo.shallow().into_iter().filter(|id| objects.contains(id)).collect();
        conn.announce_shallow(&boundary)?;
    }
    let (n, bytes) = conn.push(&repo.store, &branch, remote_old, local, &objects)?;
    repo.set_tracking(remote, &branch, &local)?;

    let from = remote_old.map_or_else(|| "new branch".to_string(), |o| short(&o));
    println!("Pushed {branch} to {remote}: {from} → {} ({n} objects, {})", short(&local), human(bytes));
    Ok(())
}

/// Push one tag (or with `only` None, all of them) to a copse remote. A tag that's
/// already there with a different value is refused unless `force`.
fn push_tags(repo: &Repo, remote: &str, url: &str, only: Option<&str>, force: bool) -> Result<()> {
    let mine: Vec<(String, Id)> = match only {
        Some(n) => vec![(n.to_string(), repo.tag(n)?.with_context(|| format!("there's no tag called {n}"))?)],
        None => repo.tags()?,
    };
    if mine.is_empty() {
        println!("No tags to push.");
        return Ok(());
    }
    let mut conn = Conn::open(url)?;
    let refs = conn.refs()?;
    let mut haves: Vec<Id> = refs.branches.iter().map(|(_, id)| *id).collect();
    haves.extend(refs.tags.iter().map(|(_, id)| *id));
    let (mut pushed, mut same, mut refused) = (0, 0, Vec::new());
    for (name, id) in &mine {
        let theirs = refs.tags.iter().find(|(t, _)| t == name).map(|(_, id)| *id);
        if theirs == Some(*id) {
            same += 1;
            continue;
        }
        if theirs.is_some() && !force {
            refused.push(name.clone());
            continue;
        }
        let objects = objects_to_send(repo, &[*id], &haves)?;
        if repo.is_shallow() {
            let boundary: Vec<Id> = repo.shallow().into_iter().filter(|b| objects.contains(b)).collect();
            conn.announce_shallow(&boundary)?;
        }
        conn.push_tag(&repo.store, name, theirs, Some(*id), &objects)?;
        println!("  {}  {}", paint("32", &format!("tag {name}")), if theirs.is_some() { "replaced" } else { "new" });
        pushed += 1;
    }
    println!("Pushed {pushed} tag(s) to {remote}{}.", if same > 0 { format!(" ({same} already there)") } else { String::new() });
    if !refused.is_empty() {
        bail!("{} tag(s) already exist on {remote} with a different commit: {}. Use --force to replace them", refused.len(), refused.join(", "));
    }
    Ok(())
}

// ---------------------------------------------------------------- clone

pub fn clone(url: &str, dir: Option<PathBuf>, depth: Option<usize>, recurse: bool, filter: Option<&str>) -> Result<()> {
    match filter {
        None | Some("blob:none") => {}
        Some(f) => bail!("unsupported --filter {f:?}; use --filter=blob:none (fetch file contents only when needed)"),
    }
    let is_git = looks_like_git(url);
    let url = match (is_git, crate::gitwire::parse_git_url(url)) {
        (true, Ok(crate::gitwire::GitUrl::Local(_))) | (false, _) => normalize_url(url)?,
        (true, Ok(_)) => url.to_string(),
        (true, Err(e)) => return Err(e),
    };
    let dir = match dir {
        Some(d) => d,
        None => {
            let name = url.trim_end_matches('/').rsplit(['/', ':']).next().unwrap_or("repo");
            let name = name.strip_suffix(".copse").or_else(|| name.strip_suffix(".git")).unwrap_or(name);
            PathBuf::from(name)
        }
    };
    let dir = std::env::current_dir()?.join(dir);
    if dir.exists() && fs::read_dir(&dir)?.next().is_some() {
        bail!("{} already exists and isn't empty", dir.display());
    }
    fs::create_dir_all(&dir)?;
    println!("Cloning {} into {}", crate::githttp::redact(&url), dir.display());

    let result = (|| -> Result<()> {
        let repo = Repo::init(&dir)?;
        let stored = if is_git { format!("{GIT_PREFIX}{url}") } else { url.clone() };
        repo.save_remotes(&[("origin".to_string(), stored)])?;
        if filter.is_some() {
            if is_git {
                bail!("--filter works with copse remotes; partial clones from git servers aren't supported yet");
            }
            fs::write(repo.dir.join("promisor"), "origin\n")?;
        }
        let repo = Repo::open_path(&dir)?; // reopened, so it fetches missing file contents from origin
        let refs = fetch_all(&repo, "origin", depth.map_or(Depth::Full, Depth::Last))?;
        let has = |b: &str| refs.branches.iter().any(|(n, _)| n == b);
        let branch = refs
            .head
            .clone()
            .filter(|h| has(h))
            .or_else(|| has("main").then(|| "main".to_string()))
            .or_else(|| refs.branches.first().map(|(n, _)| n.clone()));
        let Some(branch) = branch else {
            println!("The remote has no commits yet.");
            return Ok(());
        };
        let id = repo.tracking("origin", &branch)?.context("fetched branch is missing")?;
        repo.set_branch(&branch, &id)?;
        repo.set_head(&Head::Branch(branch.clone()))?;
        let tree = flatten(&repo.store, &repo.read_commit(&id)?.tree)?;
        let mut index = Index::default();
        apply_tree(&repo, &mut index, &[], &tree, &[])?;
        index.save(&repo.wt)?;
        println!("Checked out {branch} ({} files)", tree.len());
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&dir);
        return result;
    }
    if recurse && dir.join(".gitmodules").exists() {
        crate::submodules::init_all(&dir)?;
    }
    Ok(())
}
