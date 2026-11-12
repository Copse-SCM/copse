//! Fast-forwards that only touch what changed (`copse pull`, `copse merge` when
//! the other branch is simply ahead).
//!
//! Instead of scanning every file for uncommitted changes and rebuilding the whole
//! index, this works out which files differ between the two commits (descending
//! only into folders that differ), checks just those for local edits or untracked
//! files in the way, writes them, and updates only their index entries. That's
//! how git does it, and like git it means uncommitted edits to *other* files no
//! longer get in the way of a pull.

use crate::cmd::write_file;
use crate::objects::{decode_tree, Id, Kind, Mode, Store};
use crate::repo::Repo;
use crate::worktree::{read_content, stat_of, Entry, Index};
use anyhow::{bail, Context, Result};
use rayon::prelude::*;
use std::collections::BTreeMap;
use std::fs;

type Side = Option<(Mode, Id)>;

/// Paths whose entry differs between two trees: (path, before, after).
pub fn tree_changes(store: &Store, old: Option<Id>, new: Option<Id>) -> Result<Vec<(String, Side, Side)>> {
    let mut out = Vec::new();
    walk(store, old, new, "", &mut out)?;
    Ok(out)
}

fn entries(store: &Store, tree: Option<Id>) -> Result<BTreeMap<String, (Mode, Id)>> {
    Ok(match tree {
        Some(t) => decode_tree(&store.get_kind(&t, Kind::Tree)?)?.into_iter().map(|e| (e.name, (e.mode, e.id))).collect(),
        None => BTreeMap::new(),
    })
}

fn walk(store: &Store, old: Option<Id>, new: Option<Id>, prefix: &str, out: &mut Vec<(String, Side, Side)>) -> Result<()> {
    if old == new {
        return Ok(()); // the whole folder is unchanged
    }
    let (a, b) = (entries(store, old)?, entries(store, new)?);
    let names: std::collections::BTreeSet<&String> = a.keys().chain(b.keys()).collect();
    for name in names {
        let (x, y) = (a.get(name).copied(), b.get(name).copied());
        if x == y {
            continue;
        }
        let path = format!("{prefix}{name}");
        let dir = |s: Side| s.filter(|(m, _)| *m == Mode::Dir).map(|(_, id)| id);
        let file = |s: Side| s.filter(|(m, _)| *m != Mode::Dir);
        // Folders on either side are walked into; files are reported.
        if dir(x).is_some() || dir(y).is_some() {
            walk(store, dir(x), dir(y), &format!("{path}/"), out)?;
        }
        if file(x) != file(y) {
            out.push((path, file(x), file(y)));
        }
    }
    Ok(())
}

/// Is the file at `path` exactly what the index says (no uncommitted edit)?
fn unchanged_on_disk(repo: &Repo, index: &Index, path: &str, e: &Entry) -> Result<bool> {
    let full = crate::objects::disk_path(&repo.root, path);
    let Ok(md) = fs::symlink_metadata(&full) else { return Ok(false) };
    if e.mode == Mode::Submodule {
        return Ok(md.is_dir());
    }
    let meta = stat_of(&md);
    let mode = meta.mode;
    if mode != e.mode {
        return Ok(false);
    }
    if e.vouches_for(&meta, index.written_ns) {
        return Ok(true); // the stat cache vouches for it
    }
    // Look at the content.
    let id = if crate::lfs::Attrs::load(&repo.root).tracked(path) && matches!(mode, Mode::File | Mode::Exec) {
        crate::lfs::snapshot_blob(&repo.dir, &full, None)?
    } else {
        crate::objects::blob_id_like(&Id::from_bytes(e.id), &read_content(&full, mode)?)
    };
    Ok(id.as_bytes() == &e.id)
}

/// Move the working directory and index from tree `from` to tree `to`, touching
/// only what differs. Refuses (changing nothing) if a local edit or an untracked
/// file would be overwritten. Returns (written, removed).
pub fn fast_forward(repo: &Repo, index: &mut Index, from: Id, to: Id) -> Result<(usize, usize)> {
    let changes = tree_changes(&repo.store, Some(from), Some(to))?;
    let sparse = crate::sparse::Sparse::load(&repo.wt);

    // First, check everything: nothing is written unless all of it is safe.
    let mut in_the_way = Vec::new();
    for (path, before, _after) in &changes {
        let on_disk = fs::symlink_metadata(crate::objects::disk_path(&repo.root, path)).is_ok();
        match (index.entries.get(path), before) {
            (Some(e), Some(_)) => {
                let absent_by_design = !on_disk && (sparse.outside(path) || e.mode == Mode::Submodule);
                if !absent_by_design && !unchanged_on_disk(repo, index, path, e)? {
                    in_the_way.push(format!("{path} (changed here)"));
                }
            }
            // A file the pull adds, where an untracked file already is.
            (None, None) if on_disk && !fs::metadata(crate::objects::disk_path(&repo.root, path)).is_ok_and(|m| m.is_dir()) => {
                in_the_way.push(format!("{path} (untracked here)"));
            }
            _ => {}
        }
    }
    if !in_the_way.is_empty() {
        for p in &in_the_way {
            println!("  {p}");
        }
        bail!("{} file(s) would be overwritten; commit or stash them first (other uncommitted changes are fine)", in_the_way.len());
    }

    // Removals first (so a file can become a folder), then writes.
    let mut removed = 0;
    for (path, before, after) in &changes {
        if before.is_some() && after.is_none() {
            index.entries.remove(path);
            let full = crate::objects::disk_path(&repo.root, path);
            match fs::symlink_metadata(&full) {
                Ok(md) if md.is_dir() => {
                    let _ = fs::remove_dir(&full); // an emptied submodule folder; kept if it has files
                }
                Ok(_) => {
                    fs::remove_file(&full).with_context(|| format!("removing {path}"))?;
                    removed += 1;
                }
                Err(_) => {}
            }
            let mut dir = full.parent();
            while let Some(d) = dir.filter(|d| *d != repo.root.as_path()) {
                if fs::remove_dir(d).is_err() {
                    break;
                }
                dir = d.parent();
            }
        }
    }
    let writes: Vec<(&str, Mode, Id)> = changes
        .iter()
        .filter_map(|(p, _, after)| after.map(|(m, id)| (p.as_str(), m, id)))
        .filter(|(p, m, _)| !(sparse.outside(p) && !crate::objects::disk_path(&repo.root, p).exists() && *m != Mode::Submodule))
        .collect();
    let skipped: Vec<(&str, Mode, Id)> = changes
        .iter()
        .filter_map(|(p, _, after)| after.map(|(m, id)| (p.as_str(), m, id)))
        .filter(|(p, _, id)| !writes.iter().any(|(wp, _, wid)| wp == p && wid == id))
        .collect();
    if repo.is_partial() {
        let ids: Vec<Id> = writes.iter().filter(|(_, m, _)| *m != Mode::Submodule).map(|(_, _, id)| *id).collect();
        repo.store.prefetch(&ids)?;
    }
    // LFS rules come from the commit being checked out (its .gitattributes may be new).
    let mut top = crate::merge::Files::new();
    if let Some(e) = decode_tree(&repo.store.get_kind(&to, Kind::Tree)?)?.into_iter().find(|e| e.name == ".gitattributes") {
        top.insert(e.name, (e.mode, e.id));
    }
    let lfs = crate::lfs::Attrs::for_files(repo, &top);
    crate::lfs::before_checkout(repo, &lfs, &writes);
    writes.par_iter().try_for_each(|(path, mode, id)| write_file(repo, path, *mode, id).with_context(|| format!("writing {path}")))?;
    crate::lfs::after_checkout(repo, &lfs, &writes)?;
    for (path, mode, id) in &writes {
        let entry = match fs::symlink_metadata(crate::objects::disk_path(&repo.root, path)) {
            Ok(md) if *mode != Mode::Submodule => Entry { mode: *mode, ..stat_of(&md).entry(id) },
            _ => Entry { size: 0, mtime_ns: 0, ctime_ns: 0, ino: 0, mode: *mode, id: *id.as_bytes() },
        };
        index.entries.insert(path.to_string(), entry);
    }
    for (path, mode, id) in skipped {
        // Outside the sparse checkout: recorded, not written.
        index.entries.insert(path.to_string(), Entry { size: 0, mtime_ns: 0, ctime_ns: 0, ino: 0, mode, id: *id.as_bytes() });
    }
    Ok((writes.len(), removed))
}
