//! Three-way merge.
//!
//! `copse merge <rev>` finds the newest common ancestor (the base) of HEAD and
//! <rev>, then for every file compares base, ours and theirs:
//!   - changed on one side only        -> take that side
//!   - changed identically on both     -> take it
//!   - changed differently on both     -> merge line by line (diff3), marking conflicts
//! A clean merge commits immediately with two parents. A conflicted one writes
//! conflict markers, records .copse/MERGE_STATE, and waits for `copse commit`.

use crate::cmd::{apply_tree, head_label, paint, print_changes};
use crate::objects::{flatten, parse_hex, short, write_tree, Commit, Id, Kind, Mode};
use crate::repo::Repo;
use crate::worktree::{changes, snapshot, Entry, Index};
use anyhow::{bail, Context, Result};
use similar::{capture_diff_slices, Algorithm, DiffTag};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::io::ErrorKind;
use std::path::PathBuf;

// ---------------------------------------------------------------- merge state

/// An in-progress merge that stopped on conflicts.
pub struct MergeState {
    /// The commit being merged in; it becomes the second parent of the merge commit.
    pub other: Id,
    /// What the user called it, for the default commit message.
    pub label: String,
    /// Files that had conflicts when the merge stopped.
    pub conflicts: Vec<String>,
    /// Files the merge created that HEAD doesn't have (removed again on abort).
    pub added: Vec<String>,
}

impl MergeState {
    fn path(repo: &Repo) -> PathBuf {
        repo.wt.join("MERGE_STATE")
    }

    pub fn load(repo: &Repo) -> Result<Option<MergeState>> {
        let text = match fs::read_to_string(Self::path(repo)) {
            Ok(t) => t,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let mut state = MergeState { other: Id::from_bytes([0; 32]), label: String::new(), conflicts: vec![], added: vec![] };
        let mut have_other = false;
        for line in text.lines() {
            let (key, value) = line.split_once(' ').context("corrupt MERGE_STATE")?;
            match key {
                "other" => {
                    state.other = parse_hex(value)?;
                    have_other = true;
                }
                "label" => state.label = value.to_string(),
                "C" => state.conflicts.push(value.to_string()),
                "A" => state.added.push(value.to_string()),
                _ => {}
            }
        }
        if !have_other {
            bail!("corrupt MERGE_STATE");
        }
        Ok(Some(state))
    }

    fn save(&self, repo: &Repo) -> Result<()> {
        let mut s = format!("other {}\nlabel {}\n", self.other.to_hex(), self.label.replace('\n', " "));
        for p in &self.conflicts {
            s += &format!("C {p}\n");
        }
        for p in &self.added {
            s += &format!("A {p}\n");
        }
        crate::repo::write_atomic(&Self::path(repo), s.as_bytes())
    }

    pub fn clear(repo: &Repo) -> Result<()> {
        match fs::remove_file(Self::path(repo)) {
            Err(e) if e.kind() != ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        }
    }

    /// Conflicted files that still contain conflict markers.
    pub fn unresolved(&self, repo: &Repo) -> Vec<String> {
        with_markers(repo, &self.conflicts)
    }
}

/// The files among `paths` that still contain conflict markers.
pub fn with_markers(repo: &Repo, paths: &[String]) -> Vec<String> {
    paths
        .iter()
        .filter(|p| {
            fs::read_to_string(repo.root.join(p))
                .map(|text| text.lines().any(|l| l.starts_with("<<<<<<< ") || l.starts_with(">>>>>>> ")))
                .unwrap_or(false)
        })
        .cloned()
        .collect()
}

// ---------------------------------------------------------------- merge base

/// `id` and every commit before it.
pub fn ancestors(repo: &Repo, id: Id) -> Result<HashSet<Id>> {
    let mut seen = HashSet::new();
    let mut stack = vec![id];
    while let Some(id) = stack.pop() {
        if seen.insert(id) {
            stack.extend(repo.read_commit(&id)?.parents);
        }
    }
    Ok(seen)
}

/// True if `a` is `b` or one of its ancestors.
pub fn is_ancestor(repo: &Repo, a: Id, b: Id) -> Result<bool> {
    Ok(ancestors(repo, b)?.contains(&a))
}

/// The best common ancestor of `a` and `b`: a commit both reach that isn't itself an
/// ancestor of another such commit. Exact even when commits share a timestamp; if
/// several candidates remain (criss-cross history), the newest is chosen.
pub fn merge_base(repo: &Repo, a: Id, b: Id) -> Result<Option<Id>> {
    let of_a = ancestors(repo, a)?;
    let common: Vec<Id> = ancestors(repo, b)?.into_iter().filter(|id| of_a.contains(id)).collect();

    let mut redundant = HashSet::new();
    let mut stack = Vec::new();
    for c in &common {
        stack.extend(repo.read_commit(c)?.parents);
    }
    while let Some(id) = stack.pop() {
        if redundant.insert(id) {
            stack.extend(repo.read_commit(&id)?.parents);
        }
    }

    let mut best: Option<(i64, [u8; 32])> = None;
    for c in common.iter().filter(|c| !redundant.contains(*c)) {
        let key = (repo.read_commit(c)?.committer.time, *c.as_bytes());
        if best.map_or(true, |b| key > b) {
            best = Some(key);
        }
    }
    Ok(best.map(|(_, bytes)| Id::from_bytes(bytes)))
}

// ---------------------------------------------------------------- line merge

/// A run of changed lines: base[b0..b1] was replaced by side[s0..s1].
#[derive(Clone, Copy)]
struct Hunk {
    b0: usize,
    b1: usize,
    s0: usize,
    s1: usize,
}

fn hunks(base: &[&str], side: &[&str]) -> Vec<Hunk> {
    let mut out: Vec<Hunk> = Vec::new();
    for op in capture_diff_slices(Algorithm::Myers, base, side) {
        let (tag, old, new) = op.as_tag_tuple();
        if tag == DiffTag::Equal {
            continue;
        }
        match out.last_mut() {
            Some(h) if h.b1 == old.start && h.s1 == new.start => {
                h.b1 = old.end;
                h.s1 = new.end;
            }
            _ => out.push(Hunk { b0: old.start, b1: old.end, s0: new.start, s1: new.end }),
        }
    }
    out
}

/// Where base[start..end] landed on one side, given that side's hunks inside the range
/// and its line offset before the range.
fn side_range(hs: &[Hunk], start: usize, end: usize, offset: isize) -> (usize, usize) {
    match (hs.first(), hs.last()) {
        (Some(first), Some(last)) => (first.s0 - (first.b0 - start), last.s1 + (end - last.b1)),
        _ => ((start as isize + offset) as usize, (end as isize + offset) as usize),
    }
}

fn push_lines(out: &mut String, lines: &[&str]) {
    for l in lines {
        out.push_str(l);
    }
}

fn push_conflict_side(out: &mut String, lines: &[&str]) {
    push_lines(out, lines);
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
}

/// Merge text line by line. Returns the merged text and the number of conflict blocks.
pub fn merge_text(base: &str, ours: &str, theirs: &str, ours_label: &str, theirs_label: &str) -> (String, usize) {
    let b: Vec<&str> = base.split_inclusive('\n').collect();
    let o: Vec<&str> = ours.split_inclusive('\n').collect();
    let t: Vec<&str> = theirs.split_inclusive('\n').collect();
    let (ho, ht) = (hunks(&b, &o), hunks(&b, &t));

    let mut out = String::with_capacity(ours.len().max(theirs.len()));
    let mut conflicts = 0;
    let (mut i, mut j, mut pos) = (0, 0, 0);
    let (mut off_o, mut off_t) = (0isize, 0isize);

    loop {
        let start = match (ho.get(i), ht.get(j)) {
            (None, None) => break,
            (Some(x), None) => x.b0,
            (None, Some(y)) => y.b0,
            (Some(x), Some(y)) => x.b0.min(y.b0),
        };
        // Grow the chunk until no hunk from either side overlaps or touches it.
        let (i0, j0) = (i, j);
        let mut end = start;
        loop {
            let mut grew = false;
            while let Some(h) = ho.get(i).filter(|h| h.b0 <= end) {
                end = end.max(h.b1);
                i += 1;
                grew = true;
            }
            while let Some(h) = ht.get(j).filter(|h| h.b0 <= end) {
                end = end.max(h.b1);
                j += 1;
                grew = true;
            }
            if !grew {
                break;
            }
        }

        push_lines(&mut out, &b[pos..start]);
        let (o0, o1) = side_range(&ho[i0..i], start, end, off_o);
        let (t0, t1) = side_range(&ht[j0..j], start, end, off_t);
        let (os, ts) = (&o[o0..o1], &t[t0..t1]);

        if i == i0 {
            push_lines(&mut out, ts); // only theirs changed
        } else if j == j0 || os == ts {
            push_lines(&mut out, os); // only ours changed, or both made the same change
        } else {
            conflicts += 1;
            out.push_str(&format!("<<<<<<< {ours_label}\n"));
            push_conflict_side(&mut out, os);
            out.push_str("=======\n");
            push_conflict_side(&mut out, ts);
            out.push_str(&format!(">>>>>>> {theirs_label}\n"));
        }

        if let Some(h) = ho[i0..i].last() {
            off_o = h.s1 as isize - h.b1 as isize;
        }
        if let Some(h) = ht[j0..j].last() {
            off_t = h.s1 as isize - h.b1 as isize;
        }
        pos = end;
    }
    push_lines(&mut out, &b[pos..]);
    (out, conflicts)
}

// ---------------------------------------------------------------- command

/// How `copse merge` may finish when the current branch has nothing of its own.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FastForward {
    /// Fast-forward when possible, otherwise make a merge commit (the default).
    Allow,
    /// Always make a merge commit, so the branch's history stays visible as a group.
    Never,
    /// Only fast-forward; refuse if the branches have diverged.
    Only,
}

pub fn merge(target: Option<&str>, abort: bool, ff: FastForward, message: Option<&str>) -> Result<()> {
    let repo = Repo::find()?;
    if abort {
        return abort_merge(&repo);
    }
    let target = target.context("name the branch or commit to merge, e.g. `copse merge feature`")?;
    if MergeState::load(&repo)?.is_some() {
        bail!("a merge is already in progress: fix the conflicts and run `copse commit`, or run `copse merge --abort`");
    }
    if let Some(r) = crate::rebase::RebaseState::load(&repo)? {
        bail!("{}", r.busy());
    }

    let mut index = Index::load(&repo.wt)?;
    let ours = repo.head_commit()?.context("there are no commits to merge into yet")?;
    let theirs = repo.resolve(target)?;
    let base = merge_base(&repo, ours, theirs)?;
    if base == Some(theirs) {
        println!("Already up to date.");
        return Ok(());
    }
    if ff == FastForward::Only && base != Some(ours) {
        bail!("can't fast-forward: {} and {target} have both moved on. Merge without --ff-only, or rebase first", head_label(&repo)?);
    }
    if base == Some(ours) && ff != FastForward::Never {
        // Only the files that differ are checked and written (uncommitted changes
        // to other files can stay, as with git).
        let (from, to) = (repo.read_commit(&ours)?.tree, repo.read_commit(&theirs)?.tree);
        let (written, removed) = crate::ffwd::fast_forward(&repo, &mut index, from, to)?;
        index.save(&repo.wt)?;
        repo.advance_head(&theirs)?;
        println!("Fast-forward to {}", paint("33", &short(&theirs)));
        println!("  {written} file(s) written, {removed} removed");
        crate::hooks::notify(&repo, "post-merge", &["0"]);
        return Ok(());
    }

    // A real merge writes its result over your files, so they must be committed first.
    let files = snapshot(&repo.root, &index, None)?;
    let ch = changes(&index, &files);
    if !ch.is_empty() {
        print_changes(&ch);
        bail!("commit or discard your {} change(s) before merging", ch.len());
    }
    let theirs_tree = flatten(&repo.store, &repo.read_commit(&theirs)?.tree)?;
    let ours_tree = flatten(&repo.store, &repo.read_commit(&ours)?.tree)?;
    let base_tree = match base {
        Some(b) => flatten(&repo.store, &repo.read_commit(&b)?.tree)?,
        None => BTreeMap::new(),
    };
    let ours_label = head_label(&repo)?;
    let (merged, conflicts) = merge_trees(&repo, &base_tree, &ours_tree, &theirs_tree, &ours_label, target)?;

    let added: Vec<String> = merged.keys().filter(|p| !ours_tree.contains_key(*p)).cloned().collect();
    let (written, removed) = apply_tree(&repo, &mut index, &files, &merged, &[])?;

    if conflicts.is_empty() {
        let tree = write_tree(&repo.store, merged.iter().map(|(p, (m, id))| (p.as_str(), *m, *id)))?;
        let text = message.map(str::trim).filter(|m| !m.is_empty()).map_or_else(|| format!("Merge {target} into {ours_label}"), str::to_string);
        let commit = Commit::new(tree, vec![ours, theirs], text);
        let id = crate::signing::store(&repo, commit)?;
        repo.advance_head(&id)?;
        index.save(&repo.wt)?;
        println!("Merged {target} into {ours_label} [{}]", paint("33", &short(&id)));
        println!("  {written} file(s) written, {removed} removed");
        crate::hooks::notify(&repo, "post-merge", &["0"]);
        return Ok(());
    }

    // Stop for the user. The merge result shows up as uncommitted changes, and the
    // next `copse commit` records it.
    crate::sparse::write_conflicts(&repo, &merged, conflicts.iter().map(|(p, _)| p))?;
    stage_as_changes(&mut index, &merged, &ours_tree);
    index.save(&repo.wt)?;
    MergeState { other: theirs, label: target.to_string(), conflicts: conflicts.iter().map(|(p, _)| p.clone()).collect(), added }
        .save(&repo)?;

    println!("Merging {target} into {ours_label}: {} conflict(s)", conflicts.len());
    for (path, reason) in &conflicts {
        println!("  {} {path}  {}", paint("31", "C"), paint("2", reason));
    }
    println!("Fix the conflicts, then run `copse commit`. To give up, run `copse merge --abort`.");
    Ok(())
}

/// A snapshot as a flat map of path -> (mode, blob id).
pub type Files = BTreeMap<String, (Mode, Id)>;

/// Line up renames before a three-way merge. Where one side renamed A to B and the
/// other kept A (changed or not), A is moved to B in the base and in that other
/// side, so the ordinary per-path merge combines both sides' changes at B. A file
/// renamed differently on each side is reported as a conflict and kept under both
/// names.
/// Files renamed differently on each side: (old name, our name, their name).
type DoubleRenames = Vec<(String, String, String)>;

fn follow_renames(repo: &Repo, base: &Files, ours: &Files, theirs: &Files) -> (Files, Files, Files, Vec<(String, &'static str)>, DoubleRenames) {
    let (mut base, mut ours, mut theirs) = (base.clone(), ours.clone(), theirs.clone());
    let mut conflicts = Vec::new();
    let mut doubles = Vec::new();
    if base.is_empty() {
        return (base, ours, theirs, conflicts, doubles);
    }
    let ours_renames = crate::renames::detect(repo, &base, &ours);
    let theirs_renames = crate::renames::detect(repo, &base, &theirs);
    let ours_from: BTreeMap<&str, &str> = ours_renames.iter().map(|r| (r.from.as_str(), r.to.as_str())).collect();
    let theirs_from: BTreeMap<&str, &str> = theirs_renames.iter().map(|r| (r.from.as_str(), r.to.as_str())).collect();
    let mut moves: Vec<(String, String, bool)> = Vec::new(); // (from, to, renamed by theirs)
    for r in &theirs_renames {
        match ours_from.get(r.from.as_str()) {
            Some(to) if *to == r.to => {
                // Both renamed it the same way: merge their changes against the original.
                if !base.contains_key(&r.to) {
                    if let Some(v) = base.remove(&r.from) {
                        base.insert(r.to.clone(), v);
                    }
                }
            }
            Some(ours_to) => {
                conflicts.push((r.to.clone(), "renamed differently on each side; kept both names"));
                doubles.push((r.from.clone(), ours_to.to_string(), r.to.clone()));
            }
            None => moves.push((r.from.clone(), r.to.clone(), true)),
        }
    }
    for r in &ours_renames {
        if !theirs_from.contains_key(r.from.as_str()) {
            moves.push((r.from.clone(), r.to.clone(), false));
        }
    }
    for (from, to, by_theirs) in moves {
        let other = if by_theirs { &mut ours } else { &mut theirs };
        // Only if the other side still has the old file and nothing at the new name.
        if other.contains_key(&to) || base.contains_key(&to) {
            continue;
        }
        if !other.contains_key(&from) {
            // Renamed on one side, deleted on the other: keep the renamed file, and say so.
            conflicts.push((to, "renamed on one side, deleted on the other; kept the renamed file"));
            continue;
        }
        if let Some(v) = other.remove(&from) {
            other.insert(to.clone(), v);
        }
        if let Some(v) = base.remove(&from) {
            base.insert(to, v);
        }
    }
    (base, ours, theirs, conflicts, doubles)
}

/// Three-way merge of whole snapshots. Returns the merged snapshot (conflicted files
/// contain markers) and the conflicts as (path, reason).
pub fn merge_trees(
    repo: &Repo,
    base_tree: &Files,
    ours_tree: &Files,
    theirs_tree: &Files,
    ours_label: &str,
    theirs_label: &str,
) -> Result<(Files, Vec<(String, &'static str)>)> {
    let mut conflicts = Vec::new();
    // Renames: a file one side renamed is merged under its new name, so changes
    // the other side made to it under the old name land there too (as git does).
    let (base_tree, ours_tree, theirs_tree, rename_conflicts, doubles) = follow_renames(repo, base_tree, ours_tree, theirs_tree);
    conflicts.extend(rename_conflicts);
    let (base_tree, ours_tree, theirs_tree) = (&base_tree, &ours_tree, &theirs_tree);
    let mut merged = Files::new();
    let paths: BTreeSet<&String> = base_tree.keys().chain(ours_tree.keys()).chain(theirs_tree.keys()).collect();
    for path in paths {
        let b = base_tree.get(path).copied();
        let o = ours_tree.get(path).copied();
        let t = theirs_tree.get(path).copied();
        let pick = if o == t || t == b {
            o
        } else if o == b {
            t
        } else {
            match (o, t) {
                (Some(ov), Some(tv)) => {
                    let (merged_file, conflict) = merge_file(repo, path, b, ov, tv, ours_label, theirs_label)?;
                    if let Some(reason) = conflict {
                        conflicts.push((path.clone(), reason));
                    }
                    Some(merged_file)
                }
                // Deleted on one side, changed on the other: keep the changed version.
                (Some(v), None) | (None, Some(v)) => {
                    conflicts.push((path.clone(), "deleted on one side, changed on the other; kept the changed file"));
                    Some(v)
                }
                (None, None) => None,
            }
        };
        if let Some(v) = pick {
            merged.insert(path.clone(), v);
        }
    }
    // Renamed differently on each side: both names get the combined changes (as git does).
    for (from, ours_to, theirs_to) in doubles {
        if let (Some(&o), Some(&t)) = (ours_tree.get(&ours_to), theirs_tree.get(&theirs_to)) {
            let (combined, conflict) = merge_file(repo, &ours_to, base_tree.get(&from).copied(), o, t, ours_label, theirs_label)?;
            if let Some(reason) = conflict {
                conflicts.push((ours_to.clone(), reason));
            }
            merged.insert(ours_to, combined);
            merged.insert(theirs_to, combined);
        }
    }
    Ok((merged, conflicts))
}

/// After writing `merged` to the working directory, point the index back at `ours`,
/// so the difference shows up in `status` as uncommitted changes.
pub fn stage_as_changes(index: &mut Index, merged: &Files, ours_tree: &Files) {
    for (path, (mode, id)) in merged {
        match ours_tree.get(path) {
            Some(&(om, oid)) if (om, oid) != (*mode, *id) => {
                index.entries.insert(path.clone(), Entry { size: 0, mtime_ns: 0, mode: om, id: *oid.as_bytes() });
            }
            None => {
                index.entries.remove(path);
            }
            _ => {}
        }
    }
    for (path, (om, oid)) in ours_tree {
        if !merged.contains_key(path) {
            index.entries.insert(path.clone(), Entry { size: 0, mtime_ns: 0, mode: *om, id: *oid.as_bytes() });
        }
    }
}

/// Merge one file changed differently on both sides. Returns the result and a conflict reason, if any.
fn merge_file(
    repo: &Repo,
    path: &str,
    base: Option<(Mode, Id)>,
    (o_mode, o_id): (Mode, Id),
    (t_mode, t_id): (Mode, Id),
    ours_label: &str,
    theirs_label: &str,
) -> Result<((Mode, Id), Option<&'static str>)> {
    // Mode: take theirs if only theirs changed it, otherwise keep ours.
    let mode = if base.map_or(false, |(bm, _)| bm == o_mode) { t_mode } else { o_mode };
    if o_mode == Mode::Link || t_mode == Mode::Link {
        return Ok(((o_mode, o_id), Some("symlink changed on both sides; kept ours")));
    }
    if o_mode == Mode::Submodule || t_mode == Mode::Submodule {
        return Ok(((o_mode, o_id), Some("submodule moved on both sides; kept ours (run `copse submodule update` after choosing)")));
    }
    let blob = |id: &Id| repo.store.get_kind(id, Kind::Blob).with_context(|| format!("reading {path}"));
    let base_data = match base {
        Some((_, id)) => blob(&id)?,
        None => Vec::new(),
    };
    let (ours_data, theirs_data) = (blob(&o_id)?, blob(&t_id)?);
    let as_text = |d: &[u8]| std::str::from_utf8(d).ok().filter(|s| !s.contains('\0')).map(str::to_owned);
    let (Some(bt), Some(ot), Some(tt)) = (as_text(&base_data), as_text(&ours_data), as_text(&theirs_data)) else {
        return Ok(((o_mode, o_id), Some("binary file changed on both sides; kept ours")));
    };
    let (text, n) = merge_text(&bt, &ot, &tt, ours_label, theirs_label);
    let id = repo.store.put(Kind::Blob, text.as_bytes())?;
    Ok(((mode, id), (n > 0).then_some("conflicting edits")))
}

fn abort_merge(repo: &Repo) -> Result<()> {
    let state = MergeState::load(repo)?.context("no merge is in progress")?;
    let mut index = Index::load(&repo.wt)?;
    let files = snapshot(&repo.root, &index, None)?;
    let head = repo.head_commit()?.context("HEAD has no commit")?;
    let tree = flatten(&repo.store, &repo.read_commit(&head)?.tree)?;
    apply_tree(repo, &mut index, &files, &tree, &state.added)?;
    index.save(&repo.wt)?;
    MergeState::clear(repo)?;
    println!("Merge aborted; back at {}.", short(&head));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::merge_text;

    fn m(base: &str, ours: &str, theirs: &str) -> (String, usize) {
        merge_text(base, ours, theirs, "ours", "theirs")
    }

    #[test]
    fn edits_in_different_places_merge_cleanly() {
        let base = "a\nb\nc\nd\ne\n";
        assert_eq!(m(base, "A\nb\nc\nd\ne\n", "a\nb\nc\nd\nE\n"), ("A\nb\nc\nd\nE\n".into(), 0));
    }

    #[test]
    fn insertions_and_deletions_on_both_sides() {
        let base = "1\n2\n3\n4\n5\n6\n";
        let ours = "0\n1\n2\n3\n4\n5\n6\n"; // insert at top
        let theirs = "1\n2\n3\n4\n6\n7\n"; // delete 5, append 7
        assert_eq!(m(base, ours, theirs), ("0\n1\n2\n3\n4\n6\n7\n".into(), 0));
    }

    #[test]
    fn identical_changes_are_not_conflicts() {
        assert_eq!(m("a\nb\nc\n", "a\nX\nc\n", "a\nX\nc\n"), ("a\nX\nc\n".into(), 0));
    }

    #[test]
    fn overlapping_changes_conflict() {
        let (out, n) = m("a\nb\nc\n", "a\nOURS\nc\n", "a\nTHEIRS\nc\n");
        assert_eq!(n, 1);
        assert_eq!(out, "a\n<<<<<<< ours\nOURS\n=======\nTHEIRS\n>>>>>>> theirs\nc\n");
    }

    #[test]
    fn missing_trailing_newline_still_gets_clean_markers() {
        let (out, n) = m("a", "b", "c");
        assert_eq!(n, 1);
        assert_eq!(out, "<<<<<<< ours\nb\n=======\nc\n>>>>>>> theirs\n");
    }

    #[test]
    fn no_base_both_added() {
        let (_, n) = m("", "x\n", "y\n");
        assert_eq!(n, 1);
        assert_eq!(m("", "same\n", "same\n"), ("same\n".into(), 0));
    }
}
