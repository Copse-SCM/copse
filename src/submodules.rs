//! Git submodules: another repository, pinned at one of its commits, inside this one.
//!
//!   copse submodule                     each submodule, and whether it's on its pinned commit
//!   copse submodule update [--init]     check each one out at its pinned commit
//!                                       (--init clones the ones not there yet)
//!   copse submodule add URL PATH        add one (then commit)
//!   copse clone --recurse-submodules    clone, then `submodule update --init`
//!
//! In a commit, a submodule is a tree entry (mode Submodule, git's 160000) holding
//! the pinned commit's git id; `.gitmodules` says where to clone it from, exactly as
//! in git, so repositories with submodules round-trip with git servers unchanged.
//! To move a submodule, check out another commit inside it and commit here: the
//! scan records the commit each submodule folder is on.

use crate::cmd::paint;
use crate::merge::Files;
use crate::objects::{flatten, gitlink_hex, gitlink_id, Id, Mode};
use crate::repo::Repo;
use anyhow::{bail, Context, Result};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// (name, path, url) for each `[submodule "name"]` in a .gitmodules file.
pub fn parse_gitmodules(text: &str) -> Vec<(String, String, String)> {
    let mut out: Vec<(String, String, String)> = Vec::new();
    for line in text.lines() {
        let t = line.trim();
        if let Some(name) = t.strip_prefix("[submodule").and_then(|r| r.strip_suffix(']')) {
            out.push((name.trim().trim_matches('"').to_string(), String::new(), String::new()));
        } else if let (Some((k, v)), Some(last)) = (t.split_once('='), out.last_mut()) {
            match k.trim().to_ascii_lowercase().as_str() {
                "path" => last.1 = v.trim().trim_end_matches('/').to_string(),
                "url" => last.2 = v.trim().to_string(),
                _ => {}
            }
        }
    }
    out.retain(|(_, p, _)| !p.is_empty());
    out
}

/// Paths .gitmodules (in the working directory) declares as submodules.
pub fn declared_paths(root: &Path) -> HashSet<String> {
    match fs::read_to_string(root.join(".gitmodules")) {
        Ok(t) => parse_gitmodules(&t).into_iter().map(|(_, p, _)| p).collect(),
        Err(_) => HashSet::new(),
    }
}

/// The git id of the commit the repository at `dir` has checked out, as a
/// submodule entry id. None if it has no commit (or can't be read).
pub fn head_gitlink(dir: &Path) -> Option<Id> {
    if dir.join(".copse").exists() {
        let sub = Repo::open_path(dir).ok()?;
        let head = sub.head_commit().ok()??;
        let mut tr = crate::gitwire::Translator::new(&sub).ok()?;
        let sha = match tr.map.git(&head) {
            Some(g) => g,
            None => {
                let g = tr.translate(head).ok()?;
                let _ = tr.map.save();
                g
            }
        };
        return Some(gitlink_id(&sha));
    }
    if dir.join(".git").exists() {
        // A git checkout (made by git itself): ask git.
        let out = Command::new("git").arg("-C").arg(dir).args(["rev-parse", "HEAD"]).output().ok()?;
        let hex = String::from_utf8(out.stdout).ok()?;
        let bytes: Vec<u8> = (0..40).step_by(2).map(|i| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok()).collect::<Option<_>>()?;
        return Some(gitlink_id(&bytes.try_into().ok()?));
    }
    None
}

/// Submodules in a snapshot: (path, pinned id).
fn pinned(files: &Files) -> Vec<(String, Id)> {
    files.iter().filter(|(_, (m, _))| *m == Mode::Submodule).map(|(p, (_, id))| (p.clone(), *id)).collect()
}

/// Where to clone a submodule from: its .gitmodules URL, with a relative one
/// (`../lib.git`) taken relative to this repository's origin, as git does.
fn resolve_url(repo: &Repo, url: &str) -> Result<String> {
    // A URL starting with - would be read as an option by the commands it's passed to.
    if url.starts_with('-') {
        bail!("refusing the submodule URL {url:?}: it starts with '-'");
    }
    if !(url.starts_with("./") || url.starts_with("../")) {
        return Ok(url.to_string());
    }
    let origin = repo.remote_url("origin").context("the submodule's URL is relative, so it needs this repository's origin to resolve it")?;
    let mut base = origin.strip_prefix("git::").unwrap_or(&origin).trim_end_matches('/').to_string();
    let mut rest = url;
    loop {
        if let Some(r) = rest.strip_prefix("./") {
            rest = r;
        } else if let Some(r) = rest.strip_prefix("../") {
            rest = r;
            match base.rfind(['/', ':']) {
                Some(i) => base.truncate(i),
                None => bail!("can't resolve the submodule URL {url} against {origin}"),
            }
        } else {
            break;
        }
    }
    let sep = if base.ends_with(':') { "" } else { "/" };
    Ok(format!("{base}{sep}{rest}"))
}

fn copse(dir: &Path, args: &[&str]) -> Result<()> {
    let status = Command::new(std::env::current_exe()?).args(args).current_dir(dir).status()?;
    if !status.success() {
        bail!("`copse {}` failed in {}", args.join(" "), dir.display());
    }
    Ok(())
}

/// Check one submodule out at `pin`. Returns what happened.
fn update_one(repo: &Repo, path: &str, pin: &Id, url: Option<&str>, init: bool) -> Result<String> {
    let dir = repo.root.join(path);
    let present = dir.join(".copse").exists();
    if !present {
        if dir.join(".git").exists() {
            bail!("{path} is a git checkout; copse leaves it to git");
        }
        if !init {
            return Ok("not checked out (use --init)".into());
        }
        let url = url.with_context(|| format!("{path} isn't in .gitmodules, so there's no address to clone it from"))?;
        let url = resolve_url(repo, url)?;
        fs::create_dir_all(&dir)?;
        copse(&repo.root, &["clone", "--", &url, path])?; // -- : nothing after it is read as an option
    }
    if head_gitlink(&dir) == Some(*pin) {
        return Ok("on its pinned commit".into());
    }
    let sub = Repo::open_path(&dir)?;
    let sha = crate::objects::gitlink_sha(pin);
    let mut target = crate::gitwire::GitMap::load(&sub)?.copse(&sha);
    if target.is_none() {
        copse(&dir, &["fetch"])?;
        target = crate::gitwire::GitMap::load(&sub)?.copse(&sha);
    }
    let target = target.with_context(|| format!("{path}'s remote doesn't have the pinned commit {}", gitlink_hex(pin)))?;
    copse(&dir, &["checkout", &target.to_hex()])?;
    Ok(format!("checked out {}", &gitlink_hex(pin)[..10]))
}

pub enum SubmoduleCmd<'a> {
    Status,
    Update { init: bool },
    Add { url: &'a str, path: &'a str },
}

pub fn submodule(cmd: SubmoduleCmd) -> Result<()> {
    let repo = Repo::find()?;
    let head = repo.head_commit()?;
    let tree: Files = match head {
        Some(h) => flatten(&repo.store, &repo.read_commit(&h)?.tree)?,
        None => Files::new(),
    };
    let modules = fs::read_to_string(repo.root.join(".gitmodules")).map(|t| parse_gitmodules(&t)).unwrap_or_default();
    let url_of = |path: &str| modules.iter().find(|(_, p, _)| p == path).map(|(_, _, u)| u.clone());
    match cmd {
        SubmoduleCmd::Status => {
            let subs = pinned(&tree);
            if subs.is_empty() {
                println!("No submodules.");
            }
            for (path, pin) in subs {
                let hex = gitlink_hex(&pin);
                let dir = repo.root.join(&path);
                match head_gitlink(&dir) {
                    None => println!("-{}  {path}  {}", &hex[..10], paint("2", "(not checked out; `copse submodule update --init`)")),
                    Some(now) if now == pin => println!(" {}  {path}", &hex[..10]),
                    Some(now) => println!(
                        "+{}  {path}  {}",
                        &gitlink_hex(&now)[..10],
                        paint("33", &format!("(on another commit; pinned at {}. Commit to pin this one, or `copse submodule update`)", &hex[..10]))
                    ),
                }
            }
        }
        SubmoduleCmd::Update { init } => {
            let subs = pinned(&tree);
            if subs.is_empty() {
                println!("No submodules.");
            }
            for (path, pin) in subs {
                let what = update_one(&repo, &path, &pin, url_of(&path).as_deref(), init)?;
                println!("  {path}: {what}");
            }
        }
        SubmoduleCmd::Add { url, path } => {
            let spec = crate::restore::repo_path(&repo, path)?;
            let dir = repo.root.join(&spec);
            if dir.exists() && fs::read_dir(&dir)?.next().is_some() {
                bail!("{path} already exists and isn't empty");
            }
            if modules.iter().any(|(_, p, _)| *p == spec) {
                bail!("{spec} is already a submodule");
            }
            let clone_from = resolve_url(&repo, url)?;
            copse(&repo.root, &["clone", "--", &clone_from, &spec])?;
            let mut text = fs::read_to_string(repo.root.join(".gitmodules")).unwrap_or_default();
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            text += &format!("[submodule \"{spec}\"]\n\tpath = {spec}\n\turl = {url}\n");
            fs::write(repo.root.join(".gitmodules"), text)?;
            println!("Added submodule {spec} (from {url}). Commit to record it: copse commit -m \"Add {spec}\"");
        }
    }
    Ok(())
}

/// After `clone --recurse-submodules`.
pub fn init_all(root: &PathBuf) -> Result<()> {
    copse(root, &["submodule", "update", "--init"])
}
