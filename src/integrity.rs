//! What's reachable, and `copse fsck`.
//!
//! Reachable objects are everything a ref can lead to: branches, HEAD, tags,
//! remote-tracking branches, stashes, a merge/rebase/cherry-pick/revert/bisect in
//! progress, and every commit the reflog mentions (so `copse undo` and `name@{n}`
//! keep working). `gc` keeps exactly these; `fsck` checks they're all there.

use crate::merge::MergeState;
use crate::objects::{decode_tree, short, Commit, Id, Kind, Mode, Tag};
use crate::rebase::RebaseState;
use crate::repo::Repo;
use anyhow::Result;
use rayon::prelude::*;
use std::collections::HashSet;
use std::fs;

/// A starting point for the walk: an object, what refers to it, and whether it's
/// only in the reflog (missing reflog entries are a warning, not damage).
pub struct Root {
    pub id: Id,
    pub from: String,
    pub soft: bool,
}

pub fn roots(repo: &Repo) -> Result<Vec<Root>> {
    let mut out = Vec::new();
    let mut add = |id: Id, from: String, soft: bool| out.push(Root { id, from, soft });
    for (name, id) in repo.branches()? {
        add(id, format!("branch {name}"), false);
    }
    if let Some(id) = repo.head_commit()? {
        add(id, "HEAD".into(), false);
    }
    // Every worktree's HEAD (a detached one may be the only thing keeping its commits).
    if let Ok(entries) = fs::read_dir(repo.dir.join("worktrees")) {
        for e in entries.flatten() {
            let head = fs::read_to_string(e.path().join("HEAD")).unwrap_or_default();
            let head = head.trim();
            let id = match head.strip_prefix("ref: ") {
                Some(b) => repo.branch(b)?,
                None => crate::objects::parse_hex(head).ok(),
            };
            if let Some(id) = id {
                add(id, format!("HEAD of worktree {}", e.file_name().to_string_lossy()), false);
            }
        }
    }
    for (name, id) in repo.tags()? {
        add(id, format!("tag {name}"), false);
    }
    for (name, id) in repo.tracking_refs()? {
        add(id, format!("remote branch {name}"), false);
    }
    for (n, e) in crate::stash::load(repo)?.into_iter().enumerate() {
        add(e.id, format!("stash {n}"), false);
    }
    if let Some(m) = MergeState::load(repo)? {
        add(m.other, "the merge in progress".into(), false);
    }
    if let Some(r) = RebaseState::load(repo)? {
        for id in r.ids() {
            add(id, format!("the {} in progress", r.op.name()), false);
        }
    }
    if !repo.is_bare() {
        for id in crate::investigate::bisect_ids(repo)? {
            add(id, "the bisect in progress".into(), false);
        }
    }
    for (id, from) in crate::reflog::all_ids(&repo.dir)? {
        add(id, from, true);
    }
    Ok(out)
}

/// The result of walking from the roots.
pub struct Walk {
    pub reachable: HashSet<Id>,
    /// Objects that are referred to but missing or unreadable: (id, why, referred to by, soft).
    pub problems: Vec<(Id, String, String, bool)>,
    /// Partial clones: file contents not downloaded yet (fetched when needed).
    pub promised: usize,
}

/// Walk from `roots` to every object they lead to. Blobs are only checked for
/// existence here (fsck re-hashes every object separately).
pub fn walk(repo: &Repo, roots: &[Root]) -> Walk {
    let store = &repo.store;
    let shallow = repo.shallow();
    let partial = repo.is_partial();
    let mut promised = 0usize;
    let mut reachable = HashSet::new();
    let mut problems = Vec::new();
    // (id, the kind it should be, who refers to it, soft)
    // Reflog-only roots go at the bottom of the stack, so everything real refs lead to
    // is walked (and any damage there reported as such) before them.
    let mut stack: Vec<(Id, Option<Kind>, String, bool)> = roots.iter().filter(|r| r.soft).chain(roots.iter().filter(|r| !r.soft)).map(|r| (r.id, None, r.from.clone(), r.soft)).collect();
    while let Some((id, want, from, soft)) = stack.pop() {
        if reachable.contains(&id) {
            continue;
        }
        if want == Some(Kind::Blob) {
            if store.contains(&id) {
                reachable.insert(id);
            } else if partial {
                promised += 1; // a partial clone fetches it when it's needed
            } else {
                problems.push((id, "missing".into(), from, soft));
            }
            continue;
        }
        let (kind, data) = match store.get(&id) {
            Ok(x) => x,
            Err(e) => {
                let why = if store.contains(&id) { format!("unreadable ({e:#})") } else { "missing".into() };
                problems.push((id, why, from, soft));
                continue;
            }
        };
        if let Some(w) = want {
            if w != kind {
                problems.push((id, format!("is a {kind:?}, but should be a {w:?}"), from, soft));
                continue;
            }
        }
        reachable.insert(id);
        let here = format!("{:?} {}", kind, short(&id)).to_lowercase();
        let parsed = match kind {
            Kind::Commit => Commit::decode(&data).map(|mut c| {
                if shallow.contains(&id) {
                    c.parents.clear(); // a shallow clone's boundary: its parents were never fetched
                }
                stack.push((c.tree, Some(Kind::Tree), here.clone(), soft));
                for p in c.parents {
                    stack.push((p, Some(Kind::Commit), here.clone(), soft));
                }
            }),
            Kind::Tree => decode_tree(&data).map(|entries| {
                for e in entries.into_iter().filter(|e| e.mode != Mode::Submodule) {
                    let k = if e.mode == Mode::Dir { Kind::Tree } else { Kind::Blob };
                    stack.push((e.id, Some(k), format!("{here} ({})", e.name), soft));
                }
            }),
            Kind::Tag => Tag::decode(&data).map(|t| stack.push((t.object, Some(t.target_kind), here.clone(), soft))),
            Kind::Blob => Ok(()),
        };
        if let Err(e) = parsed {
            reachable.remove(&id);
            problems.push((id, format!("can't be read as a {kind:?} ({e:#})"), from, soft));
        }
    }
    Walk { reachable, problems, promised }
}

/// Every object id in the store, loose and packed.
pub fn all_objects(repo: &Repo) -> Result<Vec<Id>> {
    let mut all = repo.store.loose_ids()?;
    for p in repo.store.packs().as_ref().unwrap() {
        all.extend((0..p.len()).map(|i| p.id_at(i)));
    }
    all.sort_unstable_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
    all.dedup();
    Ok(all)
}

/// Temp files left in the objects folders by interrupted writes: (path, age in seconds).
pub fn leftover_temp_files(repo: &Repo) -> Vec<(std::path::PathBuf, u64)> {
    let mut out = Vec::new();
    let now = std::time::SystemTime::now();
    let objects = repo.store.dir();
    let mut dirs: Vec<std::path::PathBuf> = (0..256u32).map(|i| objects.join(format!("{i:02x}"))).collect();
    dirs.push(objects.join("pack"));
    for d in dirs {
        let Ok(entries) = fs::read_dir(&d) else { continue };
        for e in entries.flatten() {
            let name = e.file_name();
            let name = name.to_string_lossy();
            if name.starts_with(".tmp-") || name.starts_with("tmp-") || name.ends_with(".tmp") {
                let age = e.metadata().and_then(|m| m.modified()).ok().and_then(|m| now.duration_since(m).ok()).map_or(0, |d| d.as_secs());
                out.push((e.path(), age));
            }
        }
    }
    out
}

/// `copse fsck`: check that every object is intact and that everything refs lead to
/// is there. Exits with status 1 if anything is damaged or missing.
pub fn fsck() -> Result<()> {
    let repo = Repo::find_any()?;
    let mut damaged = 0usize;
    let mut warnings = 0usize;

    // Pack indexes that don't open (load_packs skips them, so check each one here).
    let pack_dir = repo.store.dir().join("pack");
    if let Ok(entries) = fs::read_dir(&pack_dir) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with("pack-") && name.ends_with(".idx") {
                if let Err(err) = crate::pack::Pack::open(&e.path()) {
                    println!("damaged pack: {name}: {err:#}");
                    damaged += 1;
                }
            } else if name.starts_with("pack-") && name.ends_with(".pack") && !pack_dir.join(name.replace(".pack", ".idx")).exists() {
                println!("warning: {name} has no index, so copse can't use it");
                warnings += 1;
            }
        }
    }

    // Every object: decompress it and check its content matches its id.
    let all = all_objects(&repo)?;
    let bad: Vec<(Id, String)> = all
        .par_iter()
        .filter_map(|id| repo.store.verify(id).err().map(|e| (*id, format!("{e:#}"))))
        .collect();
    for (id, why) in &bad {
        println!("damaged object {}: {why}", id.to_hex());
    }
    damaged += bad.len();

    // Everything the refs lead to must be there.
    let roots = roots(&repo)?;
    let w = walk(&repo, &roots);
    for (id, why, from, soft) in &w.problems {
        if *soft {
            println!("warning: {} ({from}) is {why}; that undo step can't be used", short(id));
            warnings += 1;
        } else {
            println!("{} object {} (needed by {from})", if why == "missing" { "missing" } else { "broken" }, id.to_hex());
            if why != "missing" {
                println!("  {why}");
            }
            damaged += 1;
        }
    }

    let temps = leftover_temp_files(&repo);
    if !temps.is_empty() {
        println!("note: {} temporary file(s) left by interrupted writes; `copse gc` clears them", temps.len());
    }
    let unreachable = all.iter().filter(|id| !w.reachable.contains(*id)).count();
    let refs = roots.iter().filter(|r| !r.soft).count();
    if damaged == 0 {
        println!("Checked {} objects from {refs} ref(s): everything is intact.", all.len());
        if warnings > 0 {
            println!("({warnings} warning(s) above.)");
        }
        if unreachable > 0 {
            println!("{unreachable} object(s) aren't used by anything; `copse gc` removes them once they're two weeks old.");
        }
        if w.promised > 0 {
            println!("{} file version(s) aren't downloaded yet; this partial clone fetches them when they're needed.", w.promised);
        }
        Ok(())
    } else {
        println!("Checked {} objects: {damaged} problem(s) found.", all.len());
        println!("Nothing was changed. If another copy of this repository exists (a remote or a clone), clone it again to recover.");
        std::process::exit(1);
    }
}
