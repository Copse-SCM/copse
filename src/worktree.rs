//! Scanning the working directory and the stat-cache index.
//!
//! The index records, for every file in the current commit, its size, mtime,
//! mode and blob id. A file whose size, mtime and mode still match is assumed
//! unchanged and is never read, which is what makes `status` fast.

use crate::objects::{Id, Kind, Mode, Store};
use crate::repo::write_atomic;
use anyhow::{Context, Result};
use ignore::{WalkBuilder, WalkState};
use rayon::prelude::*;
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// Files modified this close to the index's write time are re-hashed, because a
/// write in the same clock tick as the index save would not change the mtime.
pub const RACY_WINDOW_NS: i64 = 2_000_000_000;

#[derive(Clone, Copy)]
pub struct Entry {
    pub size: u64,
    pub mtime_ns: i64,
    pub mode: Mode,
    pub id: [u8; 32],
}

#[derive(Default)]
pub struct Index {
    pub written_ns: i64,
    pub entries: BTreeMap<String, Entry>,
}

impl Index {
    pub fn load(dir: &Path) -> Result<Index> {
        match fs::read(dir.join("index")) {
            Ok(bytes) => Index::decode(&bytes).context("the index file is corrupt"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Index::default()),
            Err(e) => Err(e.into()),
        }
    }

    pub fn save(&mut self, dir: &Path) -> Result<()> {
        self.written_ns = now_ns();
        write_atomic(&dir.join("index"), &self.encode())
    }

    /// The index file's bytes. The layout is the one bincode 1 wrote (copse used it
    /// before; it's now unmaintained), so every existing index still reads:
    /// written_ns i64, the entry count u64, then per entry its path (length u64 +
    /// UTF-8), size u64, mtime_ns i64, mode u32, id 32 bytes. All little-endian.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(16 + self.entries.len() * 96);
        out.extend_from_slice(&self.written_ns.to_le_bytes());
        out.extend_from_slice(&(self.entries.len() as u64).to_le_bytes());
        for (path, e) in &self.entries {
            out.extend_from_slice(&(path.len() as u64).to_le_bytes());
            out.extend_from_slice(path.as_bytes());
            out.extend_from_slice(&e.size.to_le_bytes());
            out.extend_from_slice(&e.mtime_ns.to_le_bytes());
            out.extend_from_slice(&(mode_number(e.mode)).to_le_bytes());
            out.extend_from_slice(&e.id);
        }
        out
    }

    pub fn decode(mut b: &[u8]) -> Result<Index> {
        fn take<'a>(b: &mut &'a [u8], n: usize) -> Result<&'a [u8]> {
            anyhow::ensure!(b.len() >= n, "it ends too soon");
            let (head, rest) = b.split_at(n);
            *b = rest;
            Ok(head)
        }
        let u64_ = |b: &mut &[u8]| -> Result<u64> { Ok(u64::from_le_bytes(take(b, 8)?.try_into().unwrap())) };
        let written_ns = u64_(&mut b)? as i64;
        let count = u64_(&mut b)?;
        anyhow::ensure!(count <= (b.len() / 60) as u64, "it lists more entries than it holds");
        let mut entries = BTreeMap::new();
        for _ in 0..count {
            let len = u64_(&mut b)? as usize;
            let path = String::from_utf8(take(&mut b, len)?.to_vec()).context("a path isn't UTF-8")?;
            let size = u64_(&mut b)?;
            let mtime_ns = u64_(&mut b)? as i64;
            let mode = mode_from_number(u32::from_le_bytes(take(&mut b, 4)?.try_into().unwrap()))?;
            let id: [u8; 32] = take(&mut b, 32)?.try_into().unwrap();
            entries.insert(path, Entry { size, mtime_ns, mode, id });
        }
        anyhow::ensure!(b.is_empty(), "it has extra bytes at the end");
        Ok(Index { written_ns, entries })
    }
}

/// A mode's number in the index file (its place in `Mode`, as bincode numbered it).
fn mode_number(m: Mode) -> u32 {
    match m {
        Mode::File => 0,
        Mode::Exec => 1,
        Mode::Link => 2,
        Mode::Dir => 3,
        Mode::Submodule => 4,
    }
}

fn mode_from_number(n: u32) -> Result<Mode> {
    Ok(match n {
        0 => Mode::File,
        1 => Mode::Exec,
        2 => Mode::Link,
        3 => Mode::Dir,
        4 => Mode::Submodule,
        _ => anyhow::bail!("unknown file mode {n}"),
    })
}

#[cfg(test)]
mod index_format_tests {
    use super::*;

    fn sample() -> Index {
        let mut entries = BTreeMap::new();
        entries.insert("a.txt".to_string(), Entry { size: 5, mtime_ns: 1_700_000_000_123_456_789, mode: Mode::File, id: [7; 32] });
        entries.insert("bin/run".to_string(), Entry { size: 99, mtime_ns: -3, mode: Mode::Exec, id: [0xab; 32] });
        entries.insert("ünï/link".to_string(), Entry { size: 0, mtime_ns: 0, mode: Mode::Link, id: [1; 32] });
        entries.insert("sub".to_string(), Entry { size: 0, mtime_ns: 0, mode: Mode::Submodule, id: [2; 32] });
        Index { written_ns: 1_794_000_000_000_000_000, entries }
    }

    #[test]
    fn same_bytes_as_bincode_wrote() {
        // Written by bincode 1.3 for sample(), before copse stopped using it.
        const BINCODE: &str = include_str!("index_format_vector.hex");
        let want: Vec<u8> = (0..BINCODE.trim().len()).step_by(2).map(|i| u8::from_str_radix(&BINCODE.trim()[i..i + 2], 16).unwrap()).collect();
        assert_eq!(sample().encode(), want);
        let back = Index::decode(&want).unwrap();
        assert_eq!(back.written_ns, sample().written_ns);
        assert_eq!(back.encode(), want);
    }

    #[test]
    fn damaged_files_are_refused() {
        let good = sample().encode();
        assert!(Index::decode(&good[..good.len() - 1]).is_err());
        let mut extra = good.clone();
        extra.push(0);
        assert!(Index::decode(&extra).is_err());
        let mut huge = good.clone();
        huge[8..16].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(Index::decode(&huge).is_err());
    }
}

pub fn now_ns() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as i64).unwrap_or(0)
}

/// A file in the working directory, with its content id.
pub struct File {
    pub path: String,
    pub size: u64,
    pub mtime_ns: i64,
    pub mode: Mode,
    pub id: Id,
    /// True if the content was read and hashed (stat cache missed).
    pub hashed: bool,
    /// Not on disk: a committed file outside the sparse checkout, taken as unchanged.
    pub absent: bool,
}

impl File {
    pub fn entry(&self) -> Entry {
        Entry { size: self.size, mtime_ns: self.mtime_ns, mode: self.mode, id: *self.id.as_bytes() }
    }
}

struct Stat {
    path: String,
    size: u64,
    mtime_ns: i64,
    mode: Mode,
}

pub fn stat_of(md: &fs::Metadata) -> (u64, i64, Mode) {
    let mode = if md.file_type().is_symlink() {
        Mode::Link
    } else if md.permissions().mode() & 0o111 != 0 {
        Mode::Exec
    } else {
        Mode::File
    };
    (md.size(), md.mtime() * 1_000_000_000 + md.mtime_nsec(), mode)
}

/// Walk the working directory in parallel, honoring .gitignore and .copseignore, the
/// global ignore file and .copse/exclude.
fn scan(root: &Path) -> Vec<Stat> {
    let found = Mutex::new(Vec::new());
    let mut builder = WalkBuilder::new(root);
    for file in crate::config::ignore_files(root) {
        if let Some(err) = builder.add_ignore(&file) {
            eprintln!("copse: warning: {}: {err}", file.display());
        }
    }
    // Another repository inside this one (a submodule) isn't walked into: it's
    // reported once, as a Submodule entry, and snapshot() decides what it is.
    let nested = std::sync::Arc::new(Mutex::new(Vec::new()));
    let nested_in = nested.clone();
    let root_owned = root.to_path_buf();
    builder
        .hidden(false)
        .ignore(false)
        .parents(false)
        .git_global(false)
        .git_exclude(false)
        .git_ignore(true)
        .require_git(false)
        .add_custom_ignore_filename(".copseignore")
        .filter_entry(move |e| {
            if e.depth() == 0 {
                return true;
            }
            if e.file_name() == ".copse" || e.file_name() == ".git" {
                return false;
            }
            if is_nested_repo(e) {
                if let Some(path) = e.path().strip_prefix(&root_owned).ok().and_then(|p| p.to_str()) {
                    nested_in.lock().unwrap().push(path.to_string());
                }
                return false;
            }
            true
        })
        .build_parallel()
        .run(|| {
            let found = &found;
            Box::new(move |res| {
                let entry = match res {
                    Ok(e) => e,
                    Err(err) => {
                        eprintln!("copse: warning: {err}");
                        return WalkState::Continue;
                    }
                };
                match entry.file_type() {
                    Some(t) if t.is_file() || t.is_symlink() => {}
                    _ => return WalkState::Continue,
                }
                let md = match fs::symlink_metadata(entry.path()) {
                    Ok(md) => md,
                    Err(err) => {
                        eprintln!("copse: warning: {}: {err}", entry.path().display());
                        return WalkState::Continue;
                    }
                };
                let rel = entry.path().strip_prefix(root).expect("walked outside the repository");
                // A name that isn't UTF-8 (made by another program on Linux) is kept,
                // with stand-ins for its odd bytes, rather than skipped.
                let path = crate::objects::path_from_disk(rel);
                let (size, mtime_ns, mode) = stat_of(&md);
                found.lock().unwrap().push(Stat { path: path.to_string(), size, mtime_ns, mode });
                WalkState::Continue
            })
        });
    let mut found = found.into_inner().unwrap();
    found.extend(nested.lock().unwrap().drain(..).map(|path| Stat { path, size: 0, mtime_ns: 0, mode: Mode::Submodule }));
    found.par_sort_unstable_by(|a, b| a.path.cmp(&b.path));
    found
}

/// A folder that's a repository of its own (has a `.copse` or `.git` inside).
fn is_nested_repo(e: &ignore::DirEntry) -> bool {
    e.file_type().is_some_and(|t| t.is_dir()) && (e.path().join(".copse").exists() || e.path().join(".git").exists())
}

/// Paths of every file copse tracks (everything not ignored), sorted.
pub fn tracked_paths(root: &Path) -> Vec<String> {
    scan(root).into_iter().filter(|s| s.mode != Mode::Submodule).map(|s| s.path).collect()
}

/// Paths of files that .gitignore / .copseignore rules hide, sorted.
pub fn ignored_paths(root: &Path) -> Vec<String> {
    let tracked: std::collections::HashSet<String> = tracked_paths(root).into_iter().collect();
    let mut out = Vec::new();
    let walker = WalkBuilder::new(root)
        .standard_filters(false)
        .hidden(false)
        // Never into another repository: its files aren't ours to list (or clean).
        .filter_entry(|e| !(e.depth() > 0 && (e.file_name() == ".copse" || e.file_name() == ".git" || is_nested_repo(e))))
        .build();
    for entry in walker.flatten() {
        if !entry.file_type().map_or(false, |t| t.is_file() || t.is_symlink()) {
            continue;
        }
        if let Some(rel) = entry.path().strip_prefix(root).ok().and_then(|r| r.to_str()) {
            if !tracked.contains(rel) {
                out.push(rel.to_string());
            }
        }
    }
    out.sort();
    out
}

pub fn read_content(path: &Path, mode: Mode) -> std::io::Result<Vec<u8>> {
    if mode == Mode::Link {
        Ok(fs::read_link(path)?.into_os_string().into_vec())
    } else {
        fs::read(path)
    }
}

/// Scan the working directory and compute every file's content id, sorted by path.
/// Files the stat cache can't vouch for are read and hashed in parallel; with
/// `store` set, their content is also written to the object store.
pub fn snapshot(root: &Path, index: &Index, store: Option<&Store>) -> Result<Vec<File>> {
    let racy_cutoff = index.written_ns - RACY_WINDOW_NS;
    let submodules = crate::submodules::declared_paths(root);
    // LFS: files it handles are stored as pointers (their contents go to .copse/lfs).
    let lfs = crate::lfs::Attrs::load(root);
    let lfs_dir = if lfs.active() { crate::repo::Repo::open_path(root).ok().map(|r| r.dir) } else { None };
    scan(root)
        .into_par_iter()
        .filter(|s| {
            // A nested repository counts only if it's a submodule (committed, or in .gitmodules).
            s.mode != Mode::Submodule
                || index.entries.get(&s.path).is_some_and(|e| e.mode == Mode::Submodule)
                || submodules.contains(&s.path)
        })
        .map(|s| {
            if s.mode == Mode::Submodule {
                // Its entry is the commit it has checked out (or, if that can't be told, what's committed).
                let id = crate::submodules::head_gitlink(&crate::objects::disk_path(&root, &s.path))
                    .or_else(|| index.entries.get(&s.path).map(|e| Id::from_bytes(e.id)))
                    .with_context(|| format!("can't tell which commit the submodule {} is on", s.path))?;
                return Ok(File { size: 0, mtime_ns: 0, mode: Mode::Submodule, id, hashed: false, absent: false, path: s.path });
            }
            if let Some(e) = index.entries.get(&s.path) {
                if e.size == s.size && e.mtime_ns == s.mtime_ns && e.mode == s.mode && s.mtime_ns < racy_cutoff {
                    let id = Id::from_bytes(e.id);
                    return Ok(File { size: s.size, mtime_ns: s.mtime_ns, mode: s.mode, id, hashed: false, absent: false, path: s.path });
                }
            }
            if let (Some(dir), Mode::File | Mode::Exec) = (&lfs_dir, s.mode) {
                if lfs.tracked(&s.path) {
                    let id = crate::lfs::snapshot_blob(dir, &crate::objects::disk_path(&root, &s.path), store).with_context(|| format!("reading {}", s.path))?;
                    return Ok(File { size: s.size, mtime_ns: s.mtime_ns, mode: s.mode, id, hashed: true, absent: false, path: s.path });
                }
            }
            // Named by its git id (a partial clone of a git server): compared by that.
            let git_named = index.entries.get(&s.path).map(|e| Id::from_bytes(e.id)).filter(crate::objects::is_gitlink);
            if s.mode != Mode::Link && s.size >= crate::objects::STREAM_THRESHOLD && git_named.is_none() {
                let id = Store::put_file_streaming(store, &crate::objects::disk_path(&root, &s.path)).with_context(|| format!("reading {}", s.path))?;
                return Ok(File { size: s.size, mtime_ns: s.mtime_ns, mode: s.mode, id, hashed: true, absent: false, path: s.path });
            }
            let data = read_content(&crate::objects::disk_path(&root, &s.path), s.mode).with_context(|| format!("reading {}", s.path))?;
            let id = match &git_named {
                Some(g) => crate::objects::blob_id_like(g, &data),
                None => Store::hash(Kind::Blob, &data),
            };
            if let Some(store) = store {
                store.put_with_id(&id, Kind::Blob, &data)?;
            }
            Ok(File { size: s.size, mtime_ns: s.mtime_ns, mode: s.mode, id, hashed: true, absent: false, path: s.path })
        })
        .collect::<Result<Vec<File>>>()
        .map(|files| with_sparse_files(root, index, files))
}

/// With sparse checkout, committed files outside the checked-out folders that
/// aren't on disk are counted as present and unchanged, so nothing mistakes them
/// for deletions.
fn with_sparse_files(root: &Path, index: &Index, mut files: Vec<File>) -> Vec<File> {
    let sparse = crate::sparse::Sparse::for_root(root);
    let has_submodules = index.entries.values().any(|e| e.mode == Mode::Submodule);
    if !sparse.active() && !has_submodules {
        return files;
    }
    let present: std::collections::HashSet<&str> = files.iter().map(|f| f.path.as_str()).collect();
    // Also submodules not checked out yet (an empty folder): unchanged, not deleted.
    let missing: Vec<File> = index
        .entries
        .iter()
        .filter(|(p, e)| (sparse.outside(p) || e.mode == Mode::Submodule) && !present.contains(p.as_str()))
        .map(|(p, e)| File { path: p.clone(), size: e.size, mtime_ns: e.mtime_ns, mode: e.mode, id: Id::from_bytes(e.id), hashed: false, absent: true })
        .collect();
    if !missing.is_empty() {
        files.extend(missing);
        files.sort_unstable_by(|a, b| a.path.cmp(&b.path));
    }
    files
}

#[derive(Default)]
pub struct Changes {
    pub added: Vec<String>,
    pub modified: Vec<String>,
    pub deleted: Vec<String>,
}

impl Changes {
    pub fn len(&self) -> usize {
        self.added.len() + self.modified.len() + self.deleted.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Compare the working directory (sorted `files`) with the committed state in the index.
pub fn changes(index: &Index, files: &[File]) -> Changes {
    let mut ch = Changes::default();
    let mut committed = index.entries.iter().peekable();
    for f in files {
        while let Some((path, _)) = committed.peek() {
            if path.as_str() >= f.path.as_str() {
                break;
            }
            ch.deleted.push(path.to_string());
            committed.next();
        }
        match committed.peek() {
            Some((path, e)) if **path == f.path => {
                if e.id != *f.id.as_bytes() || e.mode != f.mode {
                    ch.modified.push(f.path.clone());
                }
                committed.next();
            }
            _ => ch.added.push(f.path.clone()),
        }
    }
    ch.deleted.extend(committed.map(|(p, _)| p.clone()));
    ch
}

/// Update cached stat info for files whose content turned out to be unchanged,
/// so the next scan can skip reading them. Returns true if anything was updated.
pub fn refresh(index: &mut Index, files: &[File]) -> bool {
    let mut updated = false;
    for f in files.iter().filter(|f| f.hashed) {
        if let Some(e) = index.entries.get_mut(&f.path) {
            if e.id == *f.id.as_bytes() && e.mode == f.mode {
                e.size = f.size;
                e.mtime_ns = f.mtime_ns;
                updated = true;
            }
        }
    }
    updated
}

