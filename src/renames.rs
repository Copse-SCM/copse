//! Rename detection: which files that disappeared between two snapshots reappeared
//! under another name, as git works it out.
//!
//! A file deleted from `old` and a file added in `new` are a rename if they have
//! the same content, or (for text files) if at least half of the larger one's
//! bytes are lines the two share. Each file pairs at most once, best matches
//! first; a same-named file in another folder wins ties. Like git's
//! `diff.renameLimit`, very large sets of changes only look for exact renames.

use crate::merge::Files;
use crate::objects::{Id, Kind, Mode};
use crate::repo::Repo;
use std::collections::HashMap;

/// Minimum similarity, in percent (git's default, -M50%).
pub const THRESHOLD: u32 = 50;
/// Inexact matching compares every deleted file with every added one; past this
/// many comparisons, only exact renames are found (git's default limit is 1000²).
const MAX_PAIRS: usize = 1_000_000;
/// Files bigger than this are only matched exactly.
const MAX_SIZE: usize = 4 << 20;

/// A detected rename: from `from` in the old snapshot to `to` in the new one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rename {
    pub from: String,
    pub to: String,
    /// Similarity in percent (100 = identical content).
    pub score: u32,
}

fn is_file(mode: Mode) -> bool {
    matches!(mode, Mode::File | Mode::Exec | Mode::Link)
}

/// Each line's hash and the bytes it accounts for.
struct Profile {
    lines: HashMap<u64, (u32, usize)>,
    bytes: usize,
}

fn profile(data: &[u8]) -> Option<Profile> {
    if data.len() > MAX_SIZE || data.contains(&0) {
        return None; // too big to compare, or binary: exact matches only
    }
    let mut lines: HashMap<u64, (u32, usize)> = HashMap::new();
    for line in data.split_inclusive(|b| *b == b'\n') {
        // Lines that differ only in trailing whitespace count as the same, as git's hashing does.
        let trimmed = line.trim_ascii_end();
        let h = fxhash(trimmed);
        let e = lines.entry(h).or_insert((0, 0));
        e.0 += 1;
        e.1 += line.len();
    }
    Some(Profile { lines, bytes: data.len() })
}

fn fxhash(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in data {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// How much of the larger file the two share, in percent.
fn similarity(a: &Profile, b: &Profile) -> u32 {
    let max = a.bytes.max(b.bytes);
    if max == 0 {
        return 100;
    }
    // A big size difference rules it out before comparing lines (as git does).
    if a.bytes.min(b.bytes) * 100 < max * THRESHOLD as usize {
        return 0;
    }
    let (small, large) = if a.lines.len() <= b.lines.len() { (a, b) } else { (b, a) };
    let mut common = 0usize;
    for (h, (count_s, bytes_s)) in &small.lines {
        if let Some((count_l, bytes_l)) = large.lines.get(h) {
            // Bytes of the lines both have (per-line average, times how many both have).
            let shared = (*count_s).min(*count_l) as usize;
            let per_line = (*bytes_s / *count_s as usize).min(*bytes_l / *count_l as usize);
            common += shared * per_line;
        }
    }
    ((common * 100) / max) as u32
}

fn basename(p: &str) -> &str {
    p.rsplit('/').next().unwrap_or(p)
}

/// Renames from `old` to `new`.
pub fn detect(repo: &Repo, old: &Files, new: &Files) -> Vec<Rename> {
    let deleted: Vec<(&String, &(Mode, Id))> = old.iter().filter(|(p, (m, _))| is_file(*m) && !new.contains_key(*p)).collect();
    let added: Vec<(&String, &(Mode, Id))> = new.iter().filter(|(p, (m, _))| is_file(*m) && !old.contains_key(*p)).collect();
    if deleted.is_empty() || added.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut taken_from = std::collections::HashSet::new();
    let mut taken_to = std::collections::HashSet::new();

    // Exact renames first: same content.
    let mut by_id: HashMap<Id, Vec<&String>> = HashMap::new();
    for (p, (_, id)) in &deleted {
        by_id.entry(*id).or_default().push(p);
    }
    for (to, (_, id)) in &added {
        if let Some(froms) = by_id.get_mut(id) {
            // Prefer a source with the same file name.
            let pick = froms.iter().position(|f| basename(f) == basename(to)).unwrap_or(0);
            if !froms.is_empty() {
                let from = froms.remove(pick);
                taken_from.insert(from.clone());
                taken_to.insert((*to).clone());
                out.push(Rename { from: from.clone(), to: (*to).clone(), score: 100 });
            }
        }
    }

    // Then similar content, for text files.
    let deleted: Vec<_> = deleted.into_iter().filter(|(p, (m, _))| !taken_from.contains(*p) && *m != Mode::Link).collect();
    let added: Vec<_> = added.into_iter().filter(|(p, (m, _))| !taken_to.contains(*p) && *m != Mode::Link).collect();
    if deleted.is_empty() || added.is_empty() || deleted.len() * added.len() > MAX_PAIRS {
        return out;
    }
    let load = |id: &Id| -> Option<Profile> {
        if !repo.store.contains(id) {
            return None; // a partial clone's undownloaded file: don't fetch just to compare
        }
        repo.store.get_kind(id, Kind::Blob).ok().and_then(|d| profile(&d))
    };
    let from_profiles: Vec<Option<Profile>> = deleted.iter().map(|(_, (_, id))| load(id)).collect();
    let to_profiles: Vec<Option<Profile>> = added.iter().map(|(_, (_, id))| load(id)).collect();
    let mut candidates: Vec<(u32, bool, usize, usize)> = Vec::new();
    for (i, fp) in from_profiles.iter().enumerate() {
        let Some(fp) = fp else { continue };
        for (j, tp) in to_profiles.iter().enumerate() {
            let Some(tp) = tp else { continue };
            let score = similarity(fp, tp);
            if score >= THRESHOLD {
                let same_name = basename(deleted[i].0) == basename(added[j].0);
                candidates.push((score, same_name, i, j));
            }
        }
    }
    // Best first: higher similarity, then same file name, then path order (stable).
    candidates.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)).then(a.2.cmp(&b.2)).then(a.3.cmp(&b.3)));
    let (mut used_from, mut used_to) = (vec![false; deleted.len()], vec![false; added.len()]);
    for (score, _, i, j) in candidates {
        if used_from[i] || used_to[j] {
            continue;
        }
        used_from[i] = true;
        used_to[j] = true;
        out.push(Rename { from: deleted[i].0.clone(), to: added[j].0.clone(), score });
    }
    out.sort_by(|a, b| a.to.cmp(&b.to));
    out
}

/// The folders a rename moved a file between, ignoring the part of the path that
/// stayed the same: src/foo/bar.c -> lib/foo/bar.c is a move from src to lib (git's
/// rule). The new folder may be "" (the top level). None when the file came from
/// the top level, or stayed in its folder.
fn renamed_dirs(from: &str, to: &str) -> Option<(String, String)> {
    let (a, b): (Vec<&str>, Vec<&str>) = (from.split('/').collect(), to.split('/').collect());
    let (mut i, mut j) = (a.len(), b.len());
    while i > 0 && j > 0 && a[i - 1] == b[j - 1] {
        i -= 1;
        j -= 1;
    }
    if i == a.len() {
        // Different file names: the folders are the ones holding the files.
        i -= 1;
        j -= 1;
    }
    let (old, new) = (a[..i].join("/"), b[..j].join("/"));
    (!old.is_empty() && old != new).then_some((old, new))
}

/// Folders a side renamed, as git works it out: every file rename votes for where
/// its folder went, and a folder counts as renamed only if that side has nothing
/// left in it and one destination wins outright.
pub fn dir_renames(side: &Files, renames: &[Rename]) -> Vec<(String, String)> {
    let mut votes: HashMap<String, HashMap<String, usize>> = HashMap::new();
    for r in renames {
        if let Some((old, new)) = renamed_dirs(&r.from, &r.to) {
            *votes.entry(old).or_default().entry(new).or_default() += 1;
        }
    }
    let mut out = Vec::new();
    for (old, targets) in votes {
        let prefix = format!("{old}/");
        if side.keys().any(|p| p.starts_with(&prefix)) {
            continue; // the folder is still there on this side: not renamed
        }
        let best = targets.values().copied().max().unwrap_or(0);
        let winners: Vec<&String> = targets.iter().filter(|(_, n)| **n == best).map(|(d, _)| d).collect();
        // A tie is ambiguous: no folder move. The winner may be "" (the top level).
        if let [new] = winners.as_slice() {
            out.push((old, (*new).clone()));
        }
    }
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn similarity_scores() {
        let a = profile(b"one\ntwo\nthree\nfour\n").unwrap();
        let b = profile(b"one\ntwo\nthree\nfour\n").unwrap();
        assert_eq!(similarity(&a, &b), 100);
        let c = profile(b"one\ntwo\nthree\nFIVE\n").unwrap();
        assert!(similarity(&a, &c) >= 70);
        let d = profile(b"nothing\nin\ncommon\nhere\n").unwrap();
        assert_eq!(similarity(&a, &d), 0);
        assert!(profile(b"bin\0ary").is_none());
    }

    #[test]
    fn folder_moves_from_file_renames() {
        assert_eq!(renamed_dirs("src/foo/bar.c", "lib/foo/bar.c"), Some(("src".into(), "lib".into())));
        assert_eq!(renamed_dirs("a/b/x.txt", "a/c/y.txt"), Some(("a/b".into(), "a/c".into())));
        assert_eq!(renamed_dirs("x.txt", "lib/x.txt"), None);
        assert_eq!(renamed_dirs("src/x.txt", "x.txt"), Some(("src".into(), "".into())));
        assert_eq!(renamed_dirs("lib/x.txt", "lib/y.txt"), None);
    }
}
