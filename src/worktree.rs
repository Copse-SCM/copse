//! Scanning the working directory and the stat-cache index.
//!
//! The index records, for every file in the current commit, its size, mtime,
//! ctime, inode number, mode and blob id. A file whose stat still matches is
//! assumed unchanged and is never read, which is what makes `status` fast. ctime
//! and the inode catch what size and mtime alone miss: different contents of the
//! same size put in place with their mtime kept (`cp -p`, `rsync -a`, `tar x`).

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
    /// When the file's inode last changed (written, chmod-ed, renamed into place).
    /// 0 when unknown (an index from before copse recorded it): never matches.
    pub ctime_ns: i64,
    pub ino: u64,
    pub mode: Mode,
    pub id: [u8; 32],
}

impl Entry {
    /// Does a file with this stat still hold this entry's content, as far as the
    /// stat can tell? (Mode is compared by the caller.)
    pub fn stat_matches(&self, m: &Meta) -> bool {
        let (ctime, ino) = stat_checks();
        self.size == m.size && self.mtime_ns == m.mtime_ns && (!ctime || self.ctime_ns == m.ctime_ns) && (!ino || self.ino == m.ino)
    }

    /// Can the stat cache vouch for a file with this stat, so it needn't be read?
    /// Its stat must match, and it must not have changed (mtime, or ctime when that's
    /// trusted) within `RACY_WINDOW_NS` of `written_ns`, the index's write time.
    pub fn vouches_for(&self, m: &Meta, written_ns: i64) -> bool {
        let cutoff = written_ns - RACY_WINDOW_NS;
        self.mode == m.mode && self.stat_matches(m) && m.mtime_ns < cutoff && (!stat_checks().0 || m.ctime_ns < cutoff)
    }
}

/// Which extra stat fields to trust, from git's settings: core.trustctime = false
/// skips ctime (for filesystems or tools that change it on their own), and
/// core.checkStat = minimal skips ctime and the inode number too.
fn stat_checks() -> (bool, bool) {
    static CHECKS: std::sync::OnceLock<(bool, bool)> = std::sync::OnceLock::new();
    *CHECKS.get_or_init(|| {
        let off = |v: Option<String>| v.is_some_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "false" | "no" | "off" | "0"));
        let minimal = crate::config::get("core.checkstat").is_some_and(|v| v.trim().eq_ignore_ascii_case("minimal"));
        (!minimal && !off(crate::config::get("core.trustctime")), !minimal)
    })
}

/// The first 8 bytes of `index.stat`, the file beside the index holding each
/// entry's ctime and inode number.
const STAT_MAGIC: &[u8; 8] = b"CPSSTAT1";

#[derive(Default)]
pub struct Index {
    pub written_ns: i64,
    pub entries: BTreeMap<String, Entry>,
}

impl Index {
    pub fn load(dir: &Path) -> Result<Index> {
        let mut index = match fs::read(dir.join("index")) {
            Ok(bytes) => Index::decode(&bytes).context("the index file is corrupt")?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Index::default()),
            Err(e) => return Err(e.into()),
        };
        if let Ok(bytes) = fs::read(dir.join("index.stat")) {
            index.apply_stat(&bytes);
        }
        Ok(index)
    }

    /// Ctime and inode numbers live in `index.stat`, not the index itself, so the
    /// index keeps the layout every copse reads: switching to an older copse (or
    /// between a release and a nightly) never makes a repository unreadable. The
    /// stat file is written first and stamped with the index's write time; one that
    /// doesn't match (an older copse rewrote the index, or a crash came between the
    /// two) is ignored, and each file is re-checked by its content once. It's only a
    /// cache, so it isn't flushed to disk.
    pub fn save(&mut self, dir: &Path) -> Result<()> {
        self.written_ns = now_ns();
        let stat = dir.join("index.stat");
        let tmp = dir.join(format!("index.stat.{}.tmp", std::process::id()));
        if fs::write(&tmp, self.encode_stat()).and_then(|_| fs::rename(&tmp, &stat)).is_err() {
            let _ = fs::remove_file(&tmp);
            let _ = fs::remove_file(&stat);
        }
        write_atomic(&dir.join("index"), &self.encode())
    }

    /// `index.stat`'s bytes: STAT_MAGIC, written_ns i64, the entry count u64, then
    /// ctime_ns i64 and ino u64 per entry, in the index's (path) order.
    pub fn encode_stat(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(24 + self.entries.len() * 16);
        out.extend_from_slice(STAT_MAGIC);
        out.extend_from_slice(&self.written_ns.to_le_bytes());
        out.extend_from_slice(&(self.entries.len() as u64).to_le_bytes());
        for e in self.entries.values() {
            out.extend_from_slice(&e.ctime_ns.to_le_bytes());
            out.extend_from_slice(&e.ino.to_le_bytes());
        }
        out
    }

    /// Fill in ctimes and inode numbers from `index.stat`, if it was written with
    /// this index. Otherwise they stay unknown (0), which never matches a file.
    pub fn apply_stat(&mut self, b: &[u8]) {
        let n = self.entries.len();
        let word = |i: usize| u64::from_le_bytes(b[i..i + 8].try_into().unwrap());
        if b.len() != 24 + n * 16 || !b.starts_with(STAT_MAGIC) || word(8) as i64 != self.written_ns || word(16) != n as u64 {
            return;
        }
        for (i, e) in self.entries.values_mut().enumerate() {
            e.ctime_ns = word(24 + i * 16) as i64;
            e.ino = word(32 + i * 16);
        }
    }

    /// The index file's bytes. The layout is the one bincode 1 wrote (copse used it
    /// before; it's now unmaintained), so every existing index still reads, and every
    /// copse reads this one: written_ns i64, the entry count u64, then per entry its
    /// path (length u64 + UTF-8), size u64, mtime_ns i64, mode u32, id 32 bytes. All
    /// little-endian. (Ctimes and inode numbers go in `index.stat`; see `save`.)
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
            let (ctime_ns, ino) = (0, 0); // from index.stat, if it matches
            let mode = mode_from_number(u32::from_le_bytes(take(&mut b, 4)?.try_into().unwrap()))?;
            let id: [u8; 32] = take(&mut b, 32)?.try_into().unwrap();
            entries.insert(path, Entry { size, mtime_ns, ctime_ns, ino, mode, id });
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
        let e = |size, mtime_ns, ctime_ns, ino, mode, id| Entry { size, mtime_ns, ctime_ns, ino, mode, id };
        entries.insert("a.txt".to_string(), e(5, 1_700_000_000_123_456_789, 1_700_000_001_000_000_007, 123_456, Mode::File, [7; 32]));
        entries.insert("bin/run".to_string(), e(99, -3, -4, u64::MAX, Mode::Exec, [0xab; 32]));
        entries.insert("ünï/link".to_string(), e(0, 0, 0, 0, Mode::Link, [1; 32]));
        entries.insert("sub".to_string(), e(0, 0, 0, 0, Mode::Submodule, [2; 32]));
        Index { written_ns: 1_794_000_000_000_000_000, entries }
    }

    #[test]
    fn same_bytes_as_bincode_wrote() {
        // Written by bincode 1.3 for sample() (less its ctimes and inode numbers,
        // which go in index.stat), before copse stopped using it.
        const BINCODE: &str = include_str!("index_format_vector.hex");
        let want: Vec<u8> = (0..BINCODE.trim().len()).step_by(2).map(|i| u8::from_str_radix(&BINCODE.trim()[i..i + 2], 16).unwrap()).collect();
        assert_eq!(sample().encode(), want);
        let back = Index::decode(&want).unwrap();
        assert_eq!(back.written_ns, sample().written_ns);
        assert_eq!(back.encode(), want);
        // Without index.stat, ctimes and inode numbers are unknown: re-checked once.
        assert!(back.entries.values().all(|e| e.ctime_ns == 0 && e.ino == 0));
    }

    #[test]
    fn stat_file_round_trips_only_with_its_own_index() {
        let full = sample();
        let mut back = Index::decode(&full.encode()).unwrap();
        back.apply_stat(&full.encode_stat());
        for (path, want) in &full.entries {
            assert_eq!((back.entries[path].ctime_ns, back.entries[path].ino), (want.ctime_ns, want.ino));
        }
        // A stat file from another save of the index (an older copse rewrote it) is ignored.
        let mut other = Index::decode(&full.encode()).unwrap();
        other.written_ns += 1;
        other.apply_stat(&full.encode_stat());
        assert!(other.entries.values().all(|e| e.ctime_ns == 0 && e.ino == 0));
        // So is a damaged one.
        let mut cut = Index::decode(&full.encode()).unwrap();
        let stat = full.encode_stat();
        cut.apply_stat(&stat[..stat.len() - 1]);
        assert!(cut.entries.values().all(|e| e.ctime_ns == 0 && e.ino == 0));
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
    pub ctime_ns: i64,
    pub ino: u64,
    pub mode: Mode,
    pub id: Id,
    /// True if the content was read and hashed (stat cache missed).
    pub hashed: bool,
    /// Not on disk: a committed file outside the sparse checkout, taken as unchanged.
    pub absent: bool,
}

impl File {
    pub fn entry(&self) -> Entry {
        Entry { size: self.size, mtime_ns: self.mtime_ns, ctime_ns: self.ctime_ns, ino: self.ino, mode: self.mode, id: *self.id.as_bytes() }
    }
}

struct Stat {
    path: String,
    size: u64,
    mtime_ns: i64,
    ctime_ns: i64,
    ino: u64,
    mode: Mode,
}

/// What the stat cache records about a file on disk.
pub struct Meta {
    pub size: u64,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
    pub ino: u64,
    pub mode: Mode,
}

impl Meta {
    /// An index entry for this file holding `id`.
    pub fn entry(&self, id: &Id) -> Entry {
        Entry { size: self.size, mtime_ns: self.mtime_ns, ctime_ns: self.ctime_ns, ino: self.ino, mode: self.mode, id: *id.as_bytes() }
    }
}

pub fn stat_of(md: &fs::Metadata) -> Meta {
    let mode = if md.file_type().is_symlink() {
        Mode::Link
    } else if md.permissions().mode() & 0o111 != 0 {
        Mode::Exec
    } else {
        Mode::File
    };
    Meta { size: md.size(), mtime_ns: md.mtime() * 1_000_000_000 + md.mtime_nsec(), ctime_ns: md.ctime() * 1_000_000_000 + md.ctime_nsec(), ino: md.ino(), mode }
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
                let m = stat_of(&md);
                found.lock().unwrap().push(Stat { path: path.to_string(), size: m.size, mtime_ns: m.mtime_ns, ctime_ns: m.ctime_ns, ino: m.ino, mode: m.mode });
                WalkState::Continue
            })
        });
    let mut found = found.into_inner().unwrap();
    found.extend(nested.lock().unwrap().drain(..).map(|path| Stat { path, size: 0, mtime_ns: 0, ctime_ns: 0, ino: 0, mode: Mode::Submodule }));
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
                return Ok(File { size: 0, mtime_ns: 0, ctime_ns: 0, ino: 0, mode: Mode::Submodule, id, hashed: false, absent: false, path: s.path });
            }
            if let Some(e) = index.entries.get(&s.path) {
                let meta = Meta { size: s.size, mtime_ns: s.mtime_ns, ctime_ns: s.ctime_ns, ino: s.ino, mode: s.mode };
                if e.vouches_for(&meta, index.written_ns) {
                    let id = Id::from_bytes(e.id);
                    return Ok(File { size: s.size, mtime_ns: s.mtime_ns, ctime_ns: s.ctime_ns, ino: s.ino, mode: s.mode, id, hashed: false, absent: false, path: s.path });
                }
            }
            if let (Some(dir), Mode::File | Mode::Exec) = (&lfs_dir, s.mode) {
                if lfs.tracked(&s.path) {
                    let id = crate::lfs::snapshot_blob(dir, &crate::objects::disk_path(&root, &s.path), store).with_context(|| format!("reading {}", s.path))?;
                    return Ok(File { size: s.size, mtime_ns: s.mtime_ns, ctime_ns: s.ctime_ns, ino: s.ino, mode: s.mode, id, hashed: true, absent: false, path: s.path });
                }
            }
            // Named by its git id (a partial clone of a git server): compared by that.
            let git_named = index.entries.get(&s.path).map(|e| Id::from_bytes(e.id)).filter(crate::objects::is_gitlink);
            if s.mode != Mode::Link && s.size >= crate::objects::STREAM_THRESHOLD && git_named.is_none() {
                let id = Store::put_file_streaming(store, &crate::objects::disk_path(&root, &s.path)).with_context(|| format!("reading {}", s.path))?;
                return Ok(File { size: s.size, mtime_ns: s.mtime_ns, ctime_ns: s.ctime_ns, ino: s.ino, mode: s.mode, id, hashed: true, absent: false, path: s.path });
            }
            let data = read_content(&crate::objects::disk_path(&root, &s.path), s.mode).with_context(|| format!("reading {}", s.path))?;
            let id = match &git_named {
                Some(g) => crate::objects::blob_id_like(g, &data),
                None => Store::hash(Kind::Blob, &data),
            };
            if let Some(store) = store {
                store.put_with_id(&id, Kind::Blob, &data)?;
            }
            Ok(File { size: s.size, mtime_ns: s.mtime_ns, ctime_ns: s.ctime_ns, ino: s.ino, mode: s.mode, id, hashed: true, absent: false, path: s.path })
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
        .map(|(p, e)| File { path: p.clone(), size: e.size, mtime_ns: e.mtime_ns, ctime_ns: e.ctime_ns, ino: e.ino, mode: e.mode, id: Id::from_bytes(e.id), hashed: false, absent: true })
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
                e.ctime_ns = f.ctime_ns;
                e.ino = f.ino;
                updated = true;
            }
        }
    }
    updated
}

