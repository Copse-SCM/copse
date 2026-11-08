//! `copse gc`: repack every object into one pack file, storing older versions of
//! files and folders as deltas against their newer versions.
//!
//! Delta bases come from history: wherever a commit changed a path, the version
//! before the change is stored as a delta against the version after it. The
//! newest version of everything stays whole, so reading recent work stays fast,
//! and chains are capped at MAX_DEPTH so reading old history stays bounded.
//!
//! Objects nothing can reach (not from a branch, tag, stash, operation in progress
//! or the reflog) are removed once they're older than the grace period (`--prune`,
//! default two weeks). Nothing is removed from a repository missing objects.

use crate::merge::MergeState;
use crate::objects::{decode_tree, delta_entry, short, Id, Kind, Mode, Store, ZSTD_LEVEL};
use crate::pack::FLAG_COMPACTED;
use crate::repo::Repo;
use anyhow::{anyhow, bail, Result};
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::time::SystemTime;

const MAX_DEPTH: usize = 10;

fn human(bytes: u64) -> String {
    match bytes {
        b if b >= 1 << 30 => format!("{:.1} GB", b as f64 / (1u64 << 30) as f64),
        b if b >= 1 << 20 => format!("{:.1} MB", b as f64 / (1u64 << 20) as f64),
        b if b >= 1 << 10 => format!("{:.1} KB", b as f64 / (1u64 << 10) as f64),
        b => format!("{b} bytes"),
    }
}

fn file_size(p: &std::path::Path) -> u64 {
    fs::metadata(p).map(|m| m.len()).unwrap_or(0)
}

/// How long unreachable objects are kept before gc removes them: `--prune` (or
/// gc.pruneExpire), as "now", "never", or a number of days or weeks ("14d", "2w",
/// "2.weeks"). The wait protects objects another command has just written but not
/// yet pointed a ref at. Default: two weeks, as in git.
fn grace(prune: Option<&str>) -> Result<Option<u64>> {
    let text = prune.map(str::to_string).or_else(|| crate::config::get("gc.pruneexpire")).unwrap_or_else(|| "2w".into());
    let t = text.trim().to_ascii_lowercase().replace('.', "").replace(' ', "");
    if t == "now" {
        return Ok(Some(0));
    }
    if t == "never" {
        return Ok(None);
    }
    let digits: String = t.chars().take_while(|c| c.is_ascii_digit()).collect();
    let unit = &t[digits.len()..];
    let n: u64 = digits.parse().map_err(|_| anyhow!("--prune takes now, never, or a time like 14d or 2w (got {text:?})"))?;
    let day = 86_400;
    Ok(Some(match unit {
        "d" | "day" | "days" => n * day,
        "w" | "week" | "weeks" => n * 7 * day,
        "h" | "hour" | "hours" => n * 3600,
        _ => bail!("--prune takes now, never, or a time like 14d or 2w (got {text:?})"),
    }))
}

fn age_of(p: &std::path::Path) -> (u64, SystemTime) {
    let modified = fs::metadata(p).and_then(|m| m.modified()).unwrap_or_else(|_| SystemTime::now());
    (SystemTime::now().duration_since(modified).map_or(0, |d| d.as_secs()), modified)
}

pub fn gc(prune: Option<&str>) -> Result<()> {
    let repo = Repo::find_any()?;
    let store = &repo.store;
    let grace = grace(prune)?;
    let expired = |age: u64| grace.is_some_and(|g| age >= g);

    // Never remove anything from a repository that's already missing objects.
    let roots = crate::integrity::roots(&repo)?;
    let walk = crate::integrity::walk(&repo, &roots);
    if let Some((id, why, from, _)) = walk.problems.iter().find(|p| !p.3) {
        bail!("object {} (needed by {from}) is {why}; nothing was changed. `copse fsck` shows everything that's wrong", short(id));
    }
    let reachable = &walk.reachable;

    // Temp files from interrupted writes, once nothing could still be writing them.
    let mut temps_removed = 0;
    for (path, age) in crate::integrity::leftover_temp_files(&repo) {
        if age > 3600 && fs::remove_file(&path).is_ok() {
            temps_removed += 1;
        }
    }

    let loose = store.loose_ids()?;
    let old_packs: Vec<(String, std::path::PathBuf, std::path::PathBuf, u32, Vec<Id>)> = store
        .packs()
        .as_ref()
        .unwrap()
        .iter()
        .map(|p| (p.name.clone(), p.idx_path.clone(), p.pack_path.clone(), p.flags, (0..p.len()).map(|i| p.id_at(i)).collect()))
        .collect();

    // What becomes of each unreachable object: removed once it's old enough, kept otherwise.
    // Loose ones that are kept stay loose; packed ones that are kept go back to being loose
    // files carrying their pack's age, so they still expire on time.
    let mut remove_loose = Vec::new();
    let mut keep_loose = HashSet::new();
    for id in &loose {
        if !reachable.contains(id) {
            if expired(age_of(&store.loose_path(id)).0) {
                remove_loose.push(*id);
            } else {
                keep_loose.insert(*id);
            }
        }
    }
    let mut unpack: Vec<(Id, SystemTime)> = Vec::new(); // unreachable, packed, not yet expired
    let mut dropped_packed = 0usize;
    let mut packed_unreachable_expired = false;
    for (_, _, pack_path, _, ids) in &old_packs {
        let (age, modified) = age_of(pack_path);
        for id in ids.iter().filter(|id| !reachable.contains(*id) && !keep_loose.contains(*id)) {
            if expired(age) {
                dropped_packed += 1;
                packed_unreachable_expired = true;
            } else {
                unpack.push((*id, modified));
            }
        }
    }
    unpack.sort_unstable_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    unpack.dedup_by(|a, b| a.0 == b.0);

    let to_pack_loose = loose.iter().filter(|id| reachable.contains(*id)).count();
    let already_packed = to_pack_loose == 0 && old_packs.len() == 1 && old_packs[0].3 & FLAG_COMPACTED != 0 && !packed_unreachable_expired;
    if (already_packed || (old_packs.is_empty() && to_pack_loose == 0)) && remove_loose.is_empty() {
        let waiting = keep_loose.len() + unpack.len();
        println!("{}", if old_packs.is_empty() { "Nothing to pack." } else { "Already packed." });
        if waiting > 0 {
            println!("{waiting} unused object(s) are kept for now; they're removed once they're old enough (see --prune).");
        }
        if temps_removed > 0 {
            println!("Removed {temps_removed} leftover temporary file(s).");
        }
        return Ok(());
    }

    let size_before: u64 = loose.iter().map(|id| file_size(&store.loose_path(id))).sum::<u64>()
        + old_packs.iter().map(|p| file_size(&p.1) + file_size(&p.2)).sum::<u64>();

    // Keep-for-now packed objects go back out as loose files before their pack is deleted.
    for (id, modified) in &unpack {
        if store.loose_path(id).exists() {
            continue;
        }
        let raw = store.read_raw(id)?;
        store.write_loose_raw(id, &raw)?;
        if let Ok(f) = fs::File::options().write(true).open(store.loose_path(id)) {
            let _ = f.set_modified(*modified);
        }
    }

    // The new pack: every reachable object.
    let mut all: Vec<Id> = reachable.iter().copied().filter(|id| store.contains(id)).collect();
    all.sort_unstable_by(|a, b| a.as_bytes().cmp(b.as_bytes()));

    let base_of = plan_deltas(&repo)?;

    let mut writer = store.new_pack_writer(FLAG_COMPACTED)?;
    let mut deltas = 0usize;
    // Big objects stored whole are copied across as they are, without holding them.
    let mut normal = Vec::with_capacity(all.len());
    for id in &all {
        if store.stored_len(id)? > crate::objects::STREAM_THRESHOLD && !store.is_delta(id)? {
            let (mut r, len) = store.entry_reader(id)?;
            writer.add_from(id, &mut r, len)?;
        } else {
            normal.push(*id);
        }
    }
    let all_count = all.len();
    let all = normal;
    for chunk in all.chunks(1024) {
        let entries: Vec<(Vec<u8>, bool)> =
            chunk.par_iter().map(|id| encode(store, id, base_of.get(id))).collect::<Result<_>>()?;
        for (id, (entry, is_delta)) in chunk.iter().zip(entries) {
            writer.add(id, &entry)?;
            deltas += is_delta as usize;
        }
    }
    let name = writer.finish()?;
    store.reload_packs();

    // The new pack is visible (and on disk), so everything it replaces can go.
    let mut size_after = 0;
    for (old_name, idx, pack, _, _) in &old_packs {
        if *old_name == name {
            size_after = file_size(idx) + file_size(pack);
            continue;
        }
        fs::remove_file(idx)?;
        fs::remove_file(pack)?;
    }
    let unpacked: HashSet<Id> = unpack.iter().map(|(id, _)| *id).collect();
    loose.par_iter().filter(|id| !keep_loose.contains(*id) && !unpacked.contains(*id)).for_each(|id| {
        let _ = fs::remove_file(store.loose_path(id));
    });
    store.reload_packs();
    if size_after == 0 {
        if let Some(p) = store.packs().as_ref().unwrap().iter().find(|p| p.name == name) {
            size_after = file_size(&p.idx_path) + file_size(&p.pack_path);
        }
    }
    let size_after = size_after + keep_loose.iter().chain(&unpacked).map(|id| file_size(&store.loose_path(id))).sum::<u64>();

    println!(
        "Packed {} objects into one file ({} stored as deltas): {} → {}",
        all_count,
        deltas,
        human(size_before),
        human(size_after)
    );
    let removed = remove_loose.len() + dropped_packed;
    if removed > 0 {
        println!("Removed {removed} unused object(s) (not reachable from any branch, tag, stash or the reflog).");
    }
    let waiting = keep_loose.len() + unpacked.len();
    if waiting > 0 {
        println!("{waiting} unused object(s) are kept for now; they're removed once they're old enough (see --prune).");
    }
    if temps_removed > 0 {
        println!("Removed {temps_removed} leftover temporary file(s).");
    }
    Ok(())
}

/// The pack entry for `id`: a delta against `base` if that's smaller, otherwise the full object.
/// (Higher zstd levels were measured: much slower, no smaller on real data.)
fn encode(store: &Store, id: &Id, base: Option<&Id>) -> Result<(Vec<u8>, bool)> {
    let Some(base_id) = base else {
        return Ok((store.read_raw(id)?, false)); // already compressed; reuse the bytes
    };
    // A partial clone may not have the base; don't download it just to compare.
    if !store.contains(base_id) {
        return Ok((store.read_raw(id)?, false));
    }
    // Very large files aren't worth holding in memory to try a delta.
    if store.stored_len(id)? > 64 << 20 || store.stored_len(base_id)? > 64 << 20 {
        return Ok((store.read_raw(id)?, false));
    }
    let (kind, data) = store.get(id)?;
    let full = crate::objects::full_entry(kind, &data)?;
    let (_, base) = store.get(base_id)?;
    match delta_entry(kind, base_id, &data, &base, ZSTD_LEVEL)? {
        Some(delta) if delta.len() < full.len() => Ok((delta, true)),
        _ => Ok((full, false)),
    }
}

/// Choose a delta base for older versions of each path: object -> newer version.
fn plan_deltas(repo: &Repo) -> Result<HashMap<Id, Id>> {
    // Every commit reachable from any ref, newest first.
    let mut tips: Vec<Id> = repo.branches()?.into_iter().map(|(_, id)| id).collect();
    tips.extend(repo.tracking_refs()?.into_iter().map(|(_, id)| id));
    tips.extend(repo.head_commit()?);
    if let Some(m) = MergeState::load(repo)? {
        tips.push(m.other);
    }
    tips.extend(crate::stash::load(repo)?.into_iter().map(|e| e.id));
    for (_, id) in repo.tags()? {
        tips.extend(repo.peel(id).ok());
    }
    let mut seen = HashSet::new();
    let mut commits = Vec::new();
    while let Some(id) = tips.pop() {
        if seen.insert(id) {
            let c = repo.read_commit(&id)?;
            tips.extend(c.parents.iter().copied());
            commits.push((c.committer.time, id, c));
        }
    }
    commits.sort_by(|a, b| (b.0, b.1.as_bytes()).cmp(&(a.0, a.1.as_bytes())));

    // (older version, newer version) for every path each commit changed.
    let mut pairs = Vec::new();
    let mut seen_pairs = HashSet::new();
    for (_, _, c) in &commits {
        if let Some(parent) = c.parents.first() {
            let old_tree = repo.read_commit(parent)?.tree;
            pair_trees(&repo.store, old_tree, c.tree, &mut seen_pairs, &mut pairs)?;
        }
    }

    let mut base_of: HashMap<Id, Id> = HashMap::new();
    for (old, new) in pairs {
        if old == new || base_of.contains_key(&old) {
            continue;
        }
        // Refuse links that would make a cycle (a file reverted to an old version)
        // or a chain that's already too long.
        let (mut cur, mut steps, mut ok) = (new, 0, true);
        while let Some(&next) = base_of.get(&cur) {
            if next == old || steps >= MAX_DEPTH {
                ok = false;
                break;
            }
            cur = next;
            steps += 1;
        }
        if ok {
            base_of.insert(old, new);
        }
    }

    // Later links can lengthen earlier chains; cut any chain that grew past the cap.
    let mut depth: HashMap<Id, usize> = HashMap::new();
    let keys: Vec<Id> = base_of.keys().copied().collect();
    for key in keys {
        let mut chain = Vec::new();
        let mut cur = key;
        while !depth.contains_key(&cur) {
            match base_of.get(&cur) {
                Some(&next) => {
                    chain.push(cur);
                    cur = next;
                }
                None => break,
            }
        }
        let mut d = depth.get(&cur).copied().unwrap_or(0);
        for id in chain.into_iter().rev() {
            d += 1;
            if d > MAX_DEPTH {
                base_of.remove(&id);
                d = 0;
            }
            depth.insert(id, d);
        }
    }
    Ok(base_of)
}

fn pair_trees(store: &Store, old: Id, new: Id, seen: &mut HashSet<(Id, Id)>, pairs: &mut Vec<(Id, Id)>) -> Result<()> {
    if old == new || !seen.insert((old, new)) {
        return Ok(());
    }
    pairs.push((old, new));
    let before: HashMap<String, (Mode, Id)> =
        decode_tree(&store.get_kind(&old, Kind::Tree)?)?.into_iter().map(|e| (e.name, (e.mode, e.id))).collect();
    for e in decode_tree(&store.get_kind(&new, Kind::Tree)?)? {
        let Some(&(old_mode, old_id)) = before.get(&e.name) else { continue };
        if old_id == e.id {
            continue;
        }
        match (old_mode, e.mode) {
            (Mode::Dir, Mode::Dir) => pair_trees(store, old_id, e.id, seen, pairs)?,
            (Mode::File | Mode::Exec, Mode::File | Mode::Exec) => pairs.push((old_id, e.id)),
            _ => {}
        }
    }
    Ok(())
}
