//! `copse restore` and `copse reset`.
//!
//!   copse restore FILE|FOLDER... [--from REV]   put files back as they were in REV
//!                                               (default: the last commit)
//!   copse reset [REV]                           move the current branch to REV,
//!                                               keeping your files as they are
//!   copse reset --hard [REV]                    move it and make your files match
//!
//! Whenever either would overwrite uncommitted work, that work is first saved as a
//! stash, so nothing is ever lost. Branch moves are in the reflog, so `copse undo`
//! reverses a reset.

use crate::cmd::{apply_tree, paint, write_file};
use crate::merge::MergeState;
use crate::objects::{flatten, short, Id, Mode};
use crate::rebase::RebaseState;
use crate::repo::{Head, Repo};
use crate::worktree::{changes, snapshot, Entry, File, Index};
use anyhow::{bail, Context, Result};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Component, Path, PathBuf};

/// A path the user typed (relative to where they are) as a path within the repository.
pub(crate) fn repo_path(repo: &Repo, typed: &str) -> Result<String> {
    let mut abs = PathBuf::new();
    for part in std::env::current_dir()?.join(typed).components() {
        match part {
            Component::ParentDir => {
                abs.pop();
            }
            Component::CurDir => {}
            other => abs.push(other),
        }
    }
    let root = repo.root.canonicalize().unwrap_or_else(|_| repo.root.clone());
    let abs = match abs.parent().and_then(|p| p.canonicalize().ok()) {
        Some(parent) => parent.join(abs.file_name().unwrap_or_default()),
        None => abs,
    };
    let rel = abs.strip_prefix(&root).map_err(|_| anyhow::anyhow!("{typed} is outside this repository"))?;
    if rel == Path::new(".copse") || rel.starts_with(".copse") {
        bail!("{typed} is copse's own data, not a project file");
    }
    Ok(rel.to_string_lossy().replace('\\', "/").trim_end_matches('/').to_string())
}

fn not_mid_operation(repo: &Repo, what: &str) -> Result<()> {
    if MergeState::load(repo)?.is_some() {
        bail!("a merge is in progress; finish it with `copse commit` or undo it with `copse merge --abort` before {what}");
    }
    if let Some(r) = RebaseState::load(repo)? {
        bail!("{}", r.busy());
    }
    Ok(())
}

fn title(repo: &Repo, id: &Id) -> Result<String> {
    Ok(repo.read_commit(id)?.message.lines().next().unwrap_or_default().to_string())
}

/// `copse restore PATHS... [--from REV]`
pub fn restore(paths: &[String], from: Option<&str>) -> Result<()> {
    let repo = Repo::find()?;
    let rev = from.unwrap_or("HEAD");
    let source = repo.resolve(rev)?;
    let tree = flatten(&repo.store, &repo.read_commit(&source)?.tree)?;
    let index = Index::load(&repo.wt)?;
    let files = snapshot(&repo.root, &index, None)?;
    let ch = changes(&index, &files);
    let current: HashMap<&str, (Mode, Id)> = files.iter().map(|f| (f.path.as_str(), (f.mode, f.id))).collect();

    // Which files to write: everything in REV under each path that differs from what's here now.
    let mut to_write: BTreeMap<String, (Mode, Id)> = BTreeMap::new();
    for typed in paths {
        let spec = repo_path(&repo, typed)?;
        let under = |p: &str| spec.is_empty() || p == spec || p.starts_with(&format!("{spec}/"));
        let matched: Vec<(&String, &(Mode, Id))> = tree.iter().filter(|(p, _)| under(p)).collect();
        if matched.is_empty() {
            if current.keys().any(|p| under(p)) {
                bail!("{typed} isn't in {rev}: it's new since then. If you don't want it, delete it yourself");
            }
            bail!("{typed} doesn't exist in {rev}");
        }
        for (path, &(mode, id)) in matched {
            if current.get(path.as_str()) != Some(&(mode, id)) {
                to_write.insert(path.clone(), (mode, id));
            }
        }
    }
    if to_write.is_empty() {
        println!("Nothing to restore: already as in {rev}.");
        return Ok(());
    }

    // A backup only contains files included in the snapshot. Never overwrite an
    // existing path it missed (for example, an ignored file), even if another
    // requested path would have triggered a backup.
    for path in to_write.keys() {
        match std::fs::symlink_metadata(crate::objects::disk_path(&repo.root, path)) {
            Ok(_) if !current.contains_key(path.as_str()) => {
                bail!("refusing to overwrite {path}: it was not included in the backup scan (it may be ignored); move it aside first");
            }
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                return Err(e).with_context(|| format!("checking {path} before restoring"));
            }
            _ => {}
        }
    }

    // Both edits and new files can collide with paths from an older revision.
    // Save their current contents before writing any of the requested files.
    let edited: BTreeSet<&str> = ch.modified.iter().chain(&ch.added).map(String::as_str).collect();
    if to_write.keys().any(|p| edited.contains(p.as_str())) {
        crate::stash::backup(&repo, &format!("before restoring {} from {rev}", paths.join(" ")))?;
    }

    for (path, (mode, id)) in &to_write {
        write_file(&repo, path, *mode, id).with_context(|| format!("writing {path}"))?;
    }
    let where_from = if from.is_some() { format!("{rev} ({})", short(&source)) } else { "the last commit".to_string() };
    println!("Restored {} file(s) from {where_from}:", to_write.len());
    for path in to_write.keys().take(20) {
        println!("  {path}");
    }
    if to_write.len() > 20 {
        println!("  ... and {} more", to_write.len() - 20);
    }
    Ok(())
}

/// `copse reset [--hard] [REV]`
pub fn reset(rev: Option<&str>, hard: bool) -> Result<()> {
    let repo = Repo::find()?;
    not_mid_operation(&repo, "resetting")?;
    let current = repo.head_commit()?.context("there are no commits to reset yet")?;
    let target = repo.resolve(rev.unwrap_or("HEAD"))?;
    let head = repo.head()?;
    let label = match &head {
        Head::Branch(b) => b.clone(),
        Head::Detached(_) => "HEAD".to_string(),
    };
    let mut index = Index::load(&repo.wt)?;
    let files = snapshot(&repo.root, &index, None)?;
    let ch = changes(&index, &files);
    let tree = flatten(&repo.store, &repo.read_commit(&target)?.tree)?;

    if target == current && (!hard || ch.is_empty()) {
        println!("Nothing to do: {label} is already at {} and your files are as they were.", short(&target));
        return Ok(());
    }

    if hard {
        if !ch.is_empty() {
            crate::stash::backup(&repo, &format!("before reset --hard to {}", rev.unwrap_or("HEAD")))?;
        }
        apply_tree(&repo, &mut index, &files, &tree, &[])?;
    } else {
        // Your files stay as they are; the index now describes the target commit, so
        // whatever differs from it shows up as uncommitted changes.
        let scanned: HashMap<&str, &File> = files.iter().map(|f| (f.path.as_str(), f)).collect();
        index.entries = tree
            .iter()
            .map(|(path, &(mode, id))| {
                let entry = match scanned.get(path.as_str()) {
                    Some(f) if f.id == id && f.mode == mode => f.entry(),
                    _ => Entry { size: 0, mtime_ns: 0, mode, id: *id.as_bytes() },
                };
                (path.clone(), entry)
            })
            .collect();
    }
    index.save(&repo.wt)?;
    match head {
        Head::Branch(b) => repo.set_branch(&b, &target)?,
        Head::Detached(_) => repo.set_head(&Head::Detached(target))?,
    }

    let to = format!("{} {}", paint("33", &short(&target)), title(&repo, &target)?);
    if hard {
        println!("{label} is now at {to}, and your files match it.");
    } else {
        let now = changes(&index, &snapshot(&repo.root, &index, None)?);
        println!("{label} moved from {} to {to}.", short(&current));
        if now.is_empty() {
            println!("Your files already match it.");
        } else {
            println!("Your files are unchanged, so {} difference(s) now show as uncommitted changes.", now.len());
        }
    }
    println!("  To put it back: copse undo");
    Ok(())
}
