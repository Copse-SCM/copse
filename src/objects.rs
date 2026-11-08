//! Content-addressed object store.
//!
//! Every object is identified by the BLAKE3 hash of `kind byte || content`.
//! An object lives either loose, at `.copse/objects/<first 2 hex>/<remaining hex>`
//! (one kind byte followed by the zstd-compressed content), or in a pack file
//! under `.copse/objects/pack/` (see pack.rs), possibly stored as a delta.

use crate::pack::{load_packs, Pack, PackWriter};
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{RwLock, RwLockReadGuard};

pub type Id = blake3::Hash;

pub const ZSTD_LEVEL: i32 = 3;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Blob = 1,
    Tree = 2,
    Commit = 3,
    /// An annotated tag: a named, signed-off pointer to another object, with a message.
    Tag = 4,
}

impl Kind {
    pub fn from_u8(b: u8) -> Result<Kind> {
        Ok(match b {
            1 => Kind::Blob,
            2 => Kind::Tree,
            3 => Kind::Commit,
            4 => Kind::Tag,
            _ => bail!("unknown object kind {b}"),
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Mode {
    File,
    Exec,
    Link,
    Dir,
    /// A git submodule: another repository, pinned at one of its commits. The entry's
    /// id isn't an object here; it holds that commit's git id (see [`gitlink_id`]).
    Submodule,
}

/// A submodule entry's id: the pinned commit's 20-byte git id, padded with zeros.
pub fn gitlink_id(sha: &[u8; 20]) -> Id {
    let mut b = [0u8; 32];
    b[..20].copy_from_slice(sha);
    Id::from_bytes(b)
}

/// The git commit id a submodule entry pins.
pub fn gitlink_sha(id: &Id) -> [u8; 20] {
    id.as_bytes()[..20].try_into().unwrap()
}

/// The git commit id a submodule entry pins, as hex.
pub fn gitlink_hex(id: &Id) -> String {
    gitlink_sha(id).iter().map(|b| format!("{b:02x}")).collect()
}

impl Mode {
    fn to_u8(self) -> u8 {
        match self {
            Mode::File => 0,
            Mode::Exec => 1,
            Mode::Link => 2,
            Mode::Dir => 3,
            Mode::Submodule => 4,
        }
    }

    fn from_u8(b: u8) -> Result<Mode> {
        Ok(match b {
            0 => Mode::File,
            1 => Mode::Exec,
            2 => Mode::Link,
            3 => Mode::Dir,
            4 => Mode::Submodule,
            _ => bail!("unknown tree entry mode {b}"),
        })
    }
}

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// High bit of a pack entry's first byte: the entry is a delta against another object.
const DELTA_FLAG: u8 = 0x80;
const MAX_DELTA_CHAIN: usize = 64;

/// Fetches missing objects from elsewhere (a partial clone's remote).
pub type Promisor = std::sync::Arc<dyn Fn(&Store, &[Id]) -> Result<()> + Send + Sync>;

pub struct Store {
    dir: PathBuf,
    /// Loaded on first use; reset after a pack is added or removed.
    packs: RwLock<Option<Vec<Pack>>>,
    /// Partial clones: where to get objects that weren't downloaded.
    promisor: RwLock<Option<Promisor>>,
}

impl Store {
    pub fn new(dir: PathBuf) -> Store {
        Store { dir, packs: RwLock::new(None), promisor: RwLock::new(None) }
    }

    /// Partial clones: fetch missing objects with `p` when they're needed.
    pub fn set_promisor(&self, p: Promisor) {
        *self.promisor.write().unwrap() = Some(p);
    }

    pub fn is_partial(&self) -> bool {
        self.promisor.read().unwrap().is_some()
    }

    /// Make sure these objects are here, fetching any that aren't in one go
    /// (partial clones). A no-op otherwise.
    pub fn prefetch(&self, ids: &[Id]) -> Result<()> {
        let Some(p) = self.promisor.read().unwrap().clone() else { return Ok(()) };
        let missing: Vec<Id> = ids.iter().copied().filter(|id| !self.contains(id)).collect();
        if missing.is_empty() {
            return Ok(());
        }
        p(self, &missing)?;
        self.reload_packs();
        Ok(())
    }

    fn ensure(&self, id: &Id) -> Result<()> {
        if self.is_partial() && !self.contains(id) {
            self.prefetch(&[*id])?;
        }
        Ok(())
    }

    pub fn hash(kind: Kind, data: &[u8]) -> Id {
        let mut h = blake3::Hasher::new();
        h.update(&[kind as u8]);
        h.update(data);
        h.finalize()
    }

    pub fn loose_path(&self, id: &Id) -> PathBuf {
        let hex = id.to_hex();
        self.dir.join(&hex[..2]).join(&hex[2..])
    }

    fn pack_dir(&self) -> PathBuf {
        self.dir.join("pack")
    }

    pub fn packs(&self) -> RwLockReadGuard<'_, Option<Vec<Pack>>> {
        {
            let guard = self.packs.read().unwrap();
            if guard.is_some() {
                return guard;
            }
        }
        {
            let mut guard = self.packs.write().unwrap();
            if guard.is_none() {
                *guard = Some(load_packs(&self.pack_dir()));
            }
        }
        self.packs.read().unwrap()
    }

    /// Forget the loaded pack list, so the next lookup sees packs added or removed since.
    pub fn reload_packs(&self) {
        *self.packs.write().unwrap() = None;
    }

    pub fn new_pack_writer(&self, flags: u32) -> Result<PackWriter> {
        PackWriter::new(&self.pack_dir(), flags)
    }

    fn packed_entry(&self, id: &Id) -> Result<Option<Vec<u8>>> {
        let packs = self.packs();
        for pack in packs.as_ref().unwrap() {
            if let Some((offset, len)) = pack.find(id) {
                return Ok(Some(pack.read(offset, len)?));
            }
        }
        Ok(None)
    }

    fn is_packed(&self, id: &Id) -> bool {
        self.packs().as_ref().unwrap().iter().any(|p| p.find(id).is_some())
    }

    pub fn contains(&self, id: &Id) -> bool {
        self.is_packed(id) || self.loose_path(id).exists()
    }

    /// The stored entry for `id`: from a pack if it's there, otherwise the loose file.
    fn entry(&self, id: &Id) -> Result<Vec<u8>> {
        self.ensure(id)?;
        match self.packed_entry(id)? {
            Some(e) => Ok(e),
            None => fs::read(self.loose_path(id)).with_context(|| format!("missing object {}", id.to_hex())),
        }
    }

    pub fn put(&self, kind: Kind, data: &[u8]) -> Result<Id> {
        let id = Self::hash(kind, data);
        self.put_with_id(&id, kind, data)?;
        Ok(id)
    }

    /// Store `data` under an id the caller already computed with [`Store::hash`].
    pub fn put_with_id(&self, id: &Id, kind: Kind, data: &[u8]) -> Result<()> {
        if self.contains(id) {
            return Ok(());
        }
        write_new(&self.loose_path(id), &full_entry(kind, data)?).with_context(|| format!("writing object {}", id.to_hex()))
    }

    /// The object as a full (non-delta) entry: kind byte + compressed content, for transfer.
    pub fn read_raw(&self, id: &Id) -> Result<Vec<u8>> {
        let entry = self.entry(id)?;
        if entry.first().map_or(false, |b| b & DELTA_FLAG != 0) {
            let (kind, data) = self.decode_entry(&entry, 0)?;
            return full_entry(kind, &data);
        }
        Ok(entry)
    }

    /// Check that a full entry received from elsewhere really is object `id`.
    pub fn verify_raw(id: &Id, raw: &[u8]) -> Result<()> {
        let (&kind, rest) = raw.split_first().context("received an empty object")?;
        if kind & DELTA_FLAG != 0 {
            bail!("received a delta where a full object was expected");
        }
        let data = zstd::stream::decode_all(rest).context("received a corrupt object")?;
        let kind = Kind::from_u8(kind)?;
        if Self::hash(kind, &data) != *id {
            bail!("received object {} does not match its id", short(id));
        }
        // A folder listing is checked before it's kept: names like ".." or ".copse",
        // or a name listed twice, are refused here, so a hostile push can't plant one
        // in a shared repository (checkouts refuse them too, but it shouldn't get in).
        if kind == Kind::Tree {
            decode_tree(&data).with_context(|| format!("received folder listing {} refused", short(id)))?;
        }
        Ok(())
    }

    /// Store a received full entry as a loose object, after verifying it.
    pub fn write_raw(&self, id: &Id, raw: &[u8]) -> Result<()> {
        if self.contains(id) {
            return Ok(());
        }
        Self::verify_raw(id, raw)?;
        write_new(&self.loose_path(id), raw)
    }

    /// Write a full entry as a loose object even if a pack holds it too (gc uses this
    /// before deleting that pack).
    pub fn write_loose_raw(&self, id: &Id, raw: &[u8]) -> Result<()> {
        if self.loose_path(id).exists() {
            return Ok(());
        }
        Self::verify_raw(id, raw)?;
        write_new(&self.loose_path(id), raw)
    }

    pub fn get(&self, id: &Id) -> Result<(Kind, Vec<u8>)> {
        self.decode_entry(&self.entry(id)?, 0)
    }

    fn decode_entry(&self, entry: &[u8], depth: usize) -> Result<(Kind, Vec<u8>)> {
        let (&tag, rest) = entry.split_first().context("empty object")?;
        if tag & DELTA_FLAG == 0 {
            return Ok((Kind::from_u8(tag)?, zstd::stream::decode_all(rest)?));
        }
        if depth > MAX_DELTA_CHAIN {
            bail!("delta chain is too long (corrupt pack?)");
        }
        let kind = Kind::from_u8(tag & !DELTA_FLAG)?;
        let base_id = Id::from_bytes(rest.get(..32).context("corrupt delta")?.try_into().unwrap());
        let (_, base) = self.decode_entry(&self.entry(&base_id)?, depth + 1)?;
        let mut decoder = zstd::stream::Decoder::with_ref_prefix(&rest[32..], &base)?;
        decoder.window_log_max(27)?;
        let mut data = Vec::new();
        decoder.read_to_end(&mut data).context("corrupt delta")?;
        Ok((kind, data))
    }

    pub fn get_kind(&self, id: &Id, want: Kind) -> Result<Vec<u8>> {
        let (kind, data) = self.get(id)?;
        if kind != want {
            bail!("object {} is a {:?}, expected a {:?}", short(id), kind, want);
        }
        Ok(data)
    }

    /// Ids of all loose objects.
    pub fn loose_ids(&self) -> Result<Vec<Id>> {
        let mut out = Vec::new();
        for i in 0..256u32 {
            let prefix = format!("{i:02x}");
            let Ok(entries) = fs::read_dir(self.dir.join(&prefix)) else { continue };
            for entry in entries {
                let name = entry?.file_name();
                let name = name.to_string_lossy();
                if name.len() == 62 && !name.starts_with('.') {
                    if let Ok(id) = parse_hex(&format!("{prefix}{name}")) {
                        out.push(id);
                    }
                }
            }
        }
        Ok(out)
    }

    /// Find the unique object whose id starts with `prefix` (at least 4 hex digits).
    pub fn find_prefix(&self, prefix: &str) -> Result<Id> {
        if prefix.len() < 4 || !prefix.bytes().all(|b| b.is_ascii_hexdigit()) {
            bail!("unknown revision: {prefix}");
        }
        let prefix = prefix.to_ascii_lowercase();
        let mut found = BTreeSet::new();
        if let Ok(entries) = fs::read_dir(self.dir.join(&prefix[..2])) {
            for entry in entries {
                let name = entry?.file_name();
                let name = name.to_string_lossy();
                if name.starts_with(&prefix[2..]) && !name.starts_with('.') {
                    found.insert(format!("{}{}", &prefix[..2], name));
                }
            }
        }
        let first = u8::from_str_radix(&prefix[..2], 16)?;
        for pack in self.packs().as_ref().unwrap() {
            for i in pack.bucket(first) {
                let hex = pack.id_at(i).to_hex().to_string();
                if hex.starts_with(&prefix) {
                    found.insert(hex);
                }
            }
        }
        match found.len() {
            0 => Err(anyhow!("unknown revision: {prefix}")),
            1 => parse_hex(found.first().unwrap()),
            _ => bail!("ambiguous revision: {prefix}"),
        }
    }
}

/// A full entry: kind byte + zstd-compressed content.
pub fn full_entry(kind: Kind, data: &[u8]) -> Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(data.len() / 2 + 64);
    buf.push(kind as u8);
    zstd::stream::copy_encode(data, &mut buf, ZSTD_LEVEL)?;
    Ok(buf)
}

/// A delta entry: `data` compressed with `base` (the content of `base_id`) as a
/// reference, so whatever it shares with the base costs almost nothing.
/// None when the base isn't suitable.
pub fn delta_entry(kind: Kind, base_id: &Id, data: &[u8], base: &[u8], level: i32) -> Result<Option<Vec<u8>>> {
    let span = base.len() + data.len();
    if base.len() < 64 || span > 1 << 27 {
        return Ok(None);
    }
    // The compression window must reach back across the whole base.
    let window_log = span.next_power_of_two().trailing_zeros().clamp(17, 27);
    let mut out = Vec::with_capacity(64);
    out.push(DELTA_FLAG | kind as u8);
    out.extend_from_slice(base_id.as_bytes());
    // Same approach as `zstd --patch-from`: the base is a reference prefix, and
    // long-distance matching finds shared runs anywhere in it, even for large files.
    let mut encoder = zstd::stream::Encoder::with_ref_prefix(out, level, base)?;
    encoder.window_log(window_log)?;
    encoder.long_distance_matching(true)?;
    encoder.write_all(data)?;
    Ok(Some(encoder.finish()?))
}

/// A unique temp file name next to `path` (`.tmp-<pid>-<n>`; gc clears old leftovers).
fn tmp_beside(path: &Path) -> PathBuf {
    let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    path.with_file_name(format!(".tmp-{}-{}", std::process::id(), n))
}

/// Write to a unique temp file, flush it, then rename, so readers never see a
/// partial object and a crash can't leave a damaged one under its id.
fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = tmp_beside(path);
    let result = (|| -> Result<()> {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        crate::durable::sync_file(&f)?;
        fs::rename(&tmp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result?;
    crate::durable::note_dir(path.parent().unwrap_or(Path::new(".")));
    Ok(())
}

/// Files at least this big are hashed, stored and checked out in pieces, so memory
/// use stays flat however large they are.
pub const STREAM_THRESHOLD: u64 = 32 << 20;
const CHUNK: usize = 1 << 20;

impl Store {
    /// Hash a large file without loading it, and with `store` set, also store it
    /// (compressing as it goes). Returns its blob id.
    pub fn put_file_streaming(store: Option<&Store>, path: &Path) -> Result<Id> {
        let mut input = fs::File::open(path)?;
        let mut hasher = blake3::Hasher::new();
        hasher.update(&[Kind::Blob as u8]);
        let mut buf = vec![0u8; CHUNK];
        let Some(store) = store else {
            loop {
                let n = input.read(&mut buf)?;
                if n == 0 {
                    return Ok(hasher.finalize());
                }
                hasher.update(&buf[..n]);
            }
        };
        // Compress into a temp file in the objects folder while hashing; once the id
        // is known, rename it into place (or throw it away if it's already stored).
        let tmp = tmp_beside(&store.dir.join("00").join("x"));
        let result = (|| -> Result<Id> {
            let mut out = fs::File::create(&tmp)?;
            out.write_all(&[Kind::Blob as u8])?;
            let mut enc = zstd::stream::Encoder::new(out, ZSTD_LEVEL)?;
            loop {
                let n = input.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
                enc.write_all(&buf[..n])?;
            }
            let out = enc.finish()?;
            let id = hasher.finalize();
            if store.contains(&id) {
                fs::remove_file(&tmp)?;
            } else {
                crate::durable::sync_file(&out)?;
                let dest = store.loose_path(&id);
                fs::rename(&tmp, &dest)?;
                crate::durable::note_dir(dest.parent().unwrap());
            }
            Ok(id)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        result
    }

    /// A reader over the stored entry (kind byte first), and its length, without
    /// reading it into memory.
    pub fn entry_reader(&self, id: &Id) -> Result<(Box<dyn Read + Send>, u64)> {
        self.ensure(id)?;
        let packs = self.packs();
        for pack in packs.as_ref().unwrap() {
            if let Some((offset, len)) = pack.find(id) {
                let r = std::io::BufReader::with_capacity(1 << 16, pack.reader(offset, len)?);
                return Ok((Box::new(r), len as u64));
            }
        }
        let f = fs::File::open(self.loose_path(id)).with_context(|| format!("missing object {}", id.to_hex()))?;
        let len = f.metadata()?.len();
        Ok((Box::new(std::io::BufReader::with_capacity(1 << 16, f)), len))
    }

    /// Whether the stored entry is a delta (needs its base to be read).
    pub fn is_delta(&self, id: &Id) -> Result<bool> {
        let (mut r, _) = self.entry_reader(id)?;
        let mut tag = [0u8; 1];
        r.read_exact(&mut tag)?;
        Ok(tag[0] & DELTA_FLAG != 0)
    }

    /// Write blob `id`'s content to `out`, decompressing as it goes rather than
    /// holding the whole file in memory (deltas, which gc only makes for files
    /// small enough to hold, are the exception).
    pub fn copy_blob_to(&self, id: &Id, out: &mut impl Write) -> Result<()> {
        let (mut entry, _) = self.entry_reader(id)?;
        let mut tag = [0u8; 1];
        entry.read_exact(&mut tag)?;
        if tag[0] & DELTA_FLAG != 0 {
            out.write_all(&self.get_kind(id, Kind::Blob)?)?;
            return Ok(());
        }
        if Kind::from_u8(tag[0])? != Kind::Blob {
            bail!("object {} isn't a file", short(id));
        }
        let mut dec = zstd::stream::Decoder::new(entry)?;
        std::io::copy(&mut dec, out).with_context(|| format!("object {} is damaged", short(id)))?;
        Ok(())
    }

    /// Re-hash an object's content and check it matches its id (for fsck). Full
    /// objects are checked in pieces, so big files don't need to fit in memory.
    pub fn verify(&self, id: &Id) -> Result<Kind> {
        let (mut reader, _) = self.entry_reader(id)?;
        let mut tag = [0u8; 1];
        reader.read_exact(&mut tag).context("empty object")?;
        let tag = tag[0];
        let (kind, actual) = if tag & DELTA_FLAG == 0 {
            let kind = Kind::from_u8(tag)?;
            let mut hasher = blake3::Hasher::new();
            hasher.update(&[tag]);
            let mut dec = zstd::stream::Decoder::new(reader)?;
            std::io::copy(&mut dec, &mut hasher).context("can't be decompressed")?;
            (kind, hasher.finalize())
        } else {
            drop(reader);
            let (kind, data) = self.decode_entry(&self.entry(id)?, 0)?;
            (kind, Self::hash(kind, &data))
        };
        if actual != *id {
            bail!("its content doesn't match its id");
        }
        Ok(kind)
    }

    /// Size of the stored (compressed) entry, without decoding it.
    pub fn stored_len(&self, id: &Id) -> Result<u64> {
        let packs = self.packs();
        for pack in packs.as_ref().unwrap() {
            if let Some((_, len)) = pack.find(id) {
                return Ok(len as u64);
            }
        }
        Ok(fs::metadata(self.loose_path(id))?.len())
    }

    /// The folder loose objects live in.
    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

pub fn parse_hex(s: &str) -> Result<Id> {
    Id::from_hex(s).map_err(|_| anyhow!("invalid object id: {s}"))
}

pub fn short(id: &Id) -> String {
    id.to_hex()[..10].to_string()
}

// ---------------------------------------------------------------- trees

pub struct TreeEntry {
    pub name: String,
    pub mode: Mode,
    pub id: Id,
}

/// Binary layout per entry: mode (u8) | id (32 bytes) | name length (u32 LE) | name.
pub fn encode_tree(entries: &[TreeEntry]) -> Vec<u8> {
    let mut out = Vec::with_capacity(entries.len() * 48);
    for e in entries {
        out.push(e.mode.to_u8());
        out.extend_from_slice(e.id.as_bytes());
        out.extend_from_slice(&(e.name.len() as u32).to_le_bytes());
        out.extend_from_slice(e.name.as_bytes());
    }
    out
}

/// Characters macOS (HFS+/APFS) ignores when comparing file names, so ".g\u{200c}it"
/// would be the same folder as ".git" there (git strips the same set).
fn hfs_ignorable(c: char) -> bool {
    matches!(c as u32, 0x200c..=0x200f | 0x202a..=0x202e | 0x206a..=0x206f | 0xfeff)
}

/// Is `name` one of the folders copse or git keep their own data in, in any
/// disguise a filesystem would accept: other capitals (macOS and Windows ignore
/// case), trailing dots or spaces (Windows drops them), Windows short names
/// (GIT~1), or characters macOS ignores?
fn is_repository_folder(name: &str) -> bool {
    let plain: String = name.chars().filter(|c| !hfs_ignorable(*c)).collect::<String>().to_ascii_lowercase();
    let trimmed = plain.trim_end_matches(['.', ' ']);
    matches!(trimmed, ".git" | ".copse") || matches!(plain.as_str(), "git~1" | "copse~1")
}

/// Check one file or folder name from a tree before copse stores or uses it. A
/// repository from someone else could otherwise name an entry ".." (writing
/// outside your project on checkout) or ".copse" (overwriting copse's own data).
/// The rules are git's.
pub fn check_name(name: &str, mode: Mode) -> Result<()> {
    let bad = if name.is_empty() {
        Some("an empty name")
    } else if name == "." || name == ".." {
        Some("a name that means \"this folder\" or \"the folder above\"")
    } else if name.contains('/') || name.contains('\0') {
        Some("a name containing / or a null byte")
    } else if is_repository_folder(name) {
        Some("a name that is (or a filesystem would treat as) .copse or .git")
    } else if mode == Mode::Link && name.trim_end_matches(['.', ' ']).eq_ignore_ascii_case(".gitmodules") {
        Some("a .gitmodules that is a symlink")
    } else {
        None
    };
    match bad {
        Some(why) => bail!("unsafe file name {name:?}: {why}. copse refuses it, so it can't write outside your project or over its own data"),
        None => Ok(()),
    }
}

pub fn decode_tree(mut data: &[u8]) -> Result<Vec<TreeEntry>> {
    let mut out = Vec::new();
    while !data.is_empty() {
        if data.len() < 37 {
            bail!("corrupt tree object");
        }
        let mode = Mode::from_u8(data[0])?;
        let id = Id::from_bytes(data[1..33].try_into().unwrap());
        let len = u32::from_le_bytes(data[33..37].try_into().unwrap()) as usize;
        let name = data.get(37..37 + len).context("corrupt tree object")?;
        let name = std::str::from_utf8(name)?.to_string();
        check_name(&name, mode)?;
        // Entries are stored sorted; a repeated name (a link and a folder both called
        // "x", say) could make a checkout write through the link, so it's refused.
        if out.last().is_some_and(|prev: &TreeEntry| prev.name >= name) {
            bail!("a folder lists {name:?} twice (or out of order); copse refuses it");
        }
        out.push(TreeEntry { name, mode, id });
        data = &data[37 + len..];
    }
    Ok(out)
}

enum Node {
    File(Mode, Id),
    Dir(BTreeMap<String, Node>),
}

/// Build nested tree objects from a flat list of `path -> (mode, id)` and return the root tree id.
/// Unchanged directories hash to the same id, so they are stored only once across commits.
pub fn write_tree<'a>(store: &Store, files: impl Iterator<Item = (&'a str, Mode, Id)>) -> Result<Id> {
    let mut root = BTreeMap::new();
    for (path, mode, id) in files {
        let mut dir = &mut root;
        let mut parts = path.split('/').peekable();
        while let Some(part) = parts.next() {
            check_name(part, if parts.peek().is_none() { mode } else { Mode::Dir }).with_context(|| format!("can't record {path}"))?;
            if parts.peek().is_none() {
                dir.insert(part.to_string(), Node::File(mode, id));
            } else {
                dir = match dir.entry(part.to_string()).or_insert_with(|| Node::Dir(BTreeMap::new())) {
                    Node::Dir(sub) => sub,
                    Node::File(..) => bail!("path conflict at {path}"),
                };
            }
        }
    }
    write_dir(store, &root)
}

fn write_dir(store: &Store, dir: &BTreeMap<String, Node>) -> Result<Id> {
    let mut entries = Vec::with_capacity(dir.len());
    for (name, node) in dir {
        let (mode, id) = match node {
            Node::File(mode, id) => (*mode, *id),
            Node::Dir(sub) => (Mode::Dir, write_dir(store, sub)?),
        };
        entries.push(TreeEntry { name: name.clone(), mode, id });
    }
    store.put(Kind::Tree, &encode_tree(&entries))
}

/// Expand a tree into a flat, sorted map of `path -> (mode, blob id)`.
/// The entry at `path` inside `tree` (a file or a whole folder), if any.
pub fn entry_at(store: &Store, tree: &Id, path: &str) -> Result<Option<(Mode, Id)>> {
    if path.is_empty() {
        return Ok(Some((Mode::Dir, *tree)));
    }
    let mut current = *tree;
    let mut parts = path.split('/').peekable();
    while let Some(part) = parts.next() {
        let entries = decode_tree(&store.get_kind(&current, Kind::Tree)?)?;
        let Some(e) = entries.into_iter().find(|e| e.name == part) else { return Ok(None) };
        if parts.peek().is_none() {
            return Ok(Some((e.mode, e.id)));
        }
        if e.mode != Mode::Dir {
            return Ok(None);
        }
        current = e.id;
    }
    Ok(None)
}

pub fn flatten(store: &Store, tree: &Id) -> Result<BTreeMap<String, (Mode, Id)>> {
    let mut out = BTreeMap::new();
    flatten_into(store, tree, "", &mut out)?;
    Ok(out)
}

fn flatten_into(store: &Store, tree: &Id, prefix: &str, out: &mut BTreeMap<String, (Mode, Id)>) -> Result<()> {
    for e in decode_tree(&store.get_kind(tree, Kind::Tree)?)? {
        let path = if prefix.is_empty() { e.name } else { format!("{prefix}/{}", e.name) };
        if e.mode == Mode::Dir {
            flatten_into(store, &e.id, &path, out)?;
        } else {
            out.insert(path, (e.mode, e.id));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- commits

/// Who and when: "Name <email>", seconds since the epoch, and a git-style UTC offset.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Signature {
    pub who: String,
    pub time: i64,
    pub tz: String,
}

impl Signature {
    pub fn now() -> Signature {
        let (time, tz) = crate::identity::now();
        Signature { who: crate::identity::signature(), time, tz }
    }

    /// Just the name, without the email.
    pub fn name(&self) -> &str {
        self.who.split(" <").next().unwrap_or(&self.who)
    }
}

/// A commit. Everything git records is kept (author and committer with email and
/// timezone, plus any extra git header lines verbatim), so a commit can be
/// translated to git's format and back without changing its git id.
#[derive(Clone)]
pub struct Commit {
    pub tree: Id,
    pub parents: Vec<Id>,
    pub author: Signature,
    pub committer: Signature,
    /// Extra git header lines (such as `gpgsig`), exactly as git had them.
    pub extra: Vec<u8>,
    pub message: String,
}

impl Commit {
    /// A new commit by the current user, now.
    pub fn new(tree: Id, parents: Vec<Id>, message: String) -> Commit {
        let me = Signature::now();
        Commit { tree, parents, author: me.clone(), committer: me, extra: Vec::new(), message: with_newline(message) }
    }

    /// A rewritten copy of `original` (rebase, squash, amend): same author and
    /// authored time, but committed by the current user now, like git. Extra
    /// headers such as signatures are dropped, since they'd no longer be valid.
    pub fn rewritten(original: &Commit, tree: Id, parents: Vec<Id>, message: String) -> Commit {
        Commit { tree, parents, author: original.author.clone(), committer: Signature::now(), extra: Vec::new(), message: with_newline(message) }
    }

    /// The authored time, for display.
    pub fn time(&self) -> i64 {
        self.author.time
    }

    pub fn encode(&self) -> Vec<u8> {
        let one_line = |s: &str| s.replace('\n', " ");
        let mut s = format!("tree {}\n", self.tree.to_hex());
        for p in &self.parents {
            s += &format!("parent {}\n", p.to_hex());
        }
        s += &format!("author {}\ntime {}\ntz {}\n", one_line(&self.author.who), self.author.time, self.author.tz);
        if self.committer != self.author {
            s += &format!("committer {} {} {}\n", self.committer.time, self.committer.tz, one_line(&self.committer.who));
        }
        if !self.extra.is_empty() {
            s += &format!("extra {}\n", hex(&self.extra));
        }
        s += "\n";
        s += &self.message;
        s.into_bytes()
    }

    pub fn decode(data: &[u8]) -> Result<Commit> {
        let s = std::str::from_utf8(data)?;
        let (header, message) = s.split_once("\n\n").context("corrupt commit object")?;
        let (mut tree, mut parents) = (None, Vec::new());
        let mut author = Signature { who: String::new(), time: 0, tz: "+0000".into() };
        let mut committer = None;
        let mut extra = Vec::new();
        for line in header.lines() {
            let (key, value) = line.split_once(' ').context("corrupt commit object")?;
            match key {
                "tree" => tree = Some(parse_hex(value)?),
                "parent" => parents.push(parse_hex(value)?),
                "author" => author.who = value.to_string(),
                "time" => author.time = value.parse()?,
                "tz" => author.tz = value.to_string(),
                "committer" => {
                    let mut parts = value.splitn(3, ' ');
                    let (Some(time), Some(tz), Some(who)) = (parts.next(), parts.next(), parts.next()) else {
                        bail!("corrupt commit object");
                    };
                    committer = Some(Signature { who: who.to_string(), time: time.parse()?, tz: tz.to_string() });
                }
                "extra" => extra = unhex(value)?,
                _ => {}
            }
        }
        // Older commits have no committer line: the committer is the author.
        let committer = committer.unwrap_or_else(|| author.clone());
        Ok(Commit { tree: tree.context("commit has no tree")?, parents, author, committer, extra, message: message.to_string() })
    }
}

/// An annotated tag. Like commits, it keeps everything git records (tagger with
/// email and timezone, extra header lines verbatim), so it round-trips exactly.
#[derive(Clone)]
pub struct Tag {
    /// What it points at (almost always a commit).
    pub object: Id,
    pub target_kind: Kind,
    pub name: String,
    /// Some very old git tags have no tagger.
    pub tagger: Option<Signature>,
    pub extra: Vec<u8>,
    pub message: String,
}

impl Tag {
    pub fn new(object: Id, target_kind: Kind, name: &str, message: String) -> Tag {
        Tag { object, target_kind, name: name.to_string(), tagger: Some(Signature::now()), extra: Vec::new(), message: with_newline(message) }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut s = format!("object {}
type {}
tag {}
", self.object.to_hex(), self.target_kind as u8, self.name.replace('\n', " "));
        if let Some(t) = &self.tagger {
            s += &format!("tagger {} {} {}\n", t.time, t.tz, t.who.replace('\n', " "));
        }
        if !self.extra.is_empty() {
            s += &format!("extra {}\n", hex(&self.extra));
        }
        s += "\n";
        s += &self.message;
        s.into_bytes()
    }

    pub fn decode(data: &[u8]) -> Result<Tag> {
        let s = std::str::from_utf8(data)?;
        let (header, message) = s.split_once("\n\n").context("corrupt tag object")?;
        let (mut object, mut target_kind, mut name, mut tagger, mut extra) = (None, Kind::Commit, String::new(), None, Vec::new());
        for line in header.lines() {
            let (key, value) = line.split_once(' ').context("corrupt tag object")?;
            match key {
                "object" => object = Some(parse_hex(value)?),
                "type" => target_kind = Kind::from_u8(value.parse()?)?,
                "tag" => name = value.to_string(),
                "tagger" => {
                    let mut parts = value.splitn(3, ' ');
                    let (Some(time), Some(tz), Some(who)) = (parts.next(), parts.next(), parts.next()) else {
                        bail!("corrupt tag object");
                    };
                    tagger = Some(Signature { who: who.to_string(), time: time.parse()?, tz: tz.to_string() });
                }
                "extra" => extra = unhex(value)?,
                _ => {}
            }
        }
        Ok(Tag { object: object.context("tag has no object")?, target_kind, name, tagger, extra, message: message.to_string() })
    }
}

/// git's convention: a commit message ends with a newline. copse follows it for the
/// commits it makes; commits from git are kept exactly as they are (messages made
/// on GitHub's website, for example, often have no final newline).
fn with_newline(message: String) -> String {
    if message.ends_with('\n') {
        message
    } else {
        message + "\n"
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Result<Vec<u8>> {
    if s.len() % 2 != 0 {
        bail!("corrupt commit object");
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|_| anyhow!("corrupt commit object"))).collect()
}

#[cfg(test)]
mod name_tests {
    use super::*;

    #[test]
    fn unsafe_names_are_refused() {
        for bad in ["", ".", "..", "a/b", "a\0b", ".copse", ".COPSE", ".Copse.", ".copse ", "copse~1", ".git", ".GIT", ".git...", "GIT~1",
                    ".g\u{200c}it", ".\u{feff}copse"] {
            assert!(check_name(bad, Mode::File).is_err(), "{bad:?} should be refused");
            assert!(check_name(bad, Mode::Dir).is_err(), "{bad:?} should be refused as a folder");
        }
        assert!(check_name(".gitmodules", Mode::Link).is_err());
        for good in ["a", ".gitignore", ".copseignore", ".github", ".gitmodules", "git", "copse", "...", "a.b", "-rf", "x\\y", "notes~1", "café"] {
            assert!(check_name(good, Mode::File).is_ok(), "{good:?} should be allowed");
        }
    }

    #[test]
    fn decoding_refuses_unsafe_and_repeated_names() {
        let id = Id::from_bytes([7; 32]);
        let ok = encode_tree(&[TreeEntry { name: "a".into(), mode: Mode::File, id }, TreeEntry { name: "b".into(), mode: Mode::File, id }]);
        assert_eq!(decode_tree(&ok).unwrap().len(), 2);
        let dotdot = encode_tree(&[TreeEntry { name: "..".into(), mode: Mode::Dir, id }]);
        assert!(decode_tree(&dotdot).is_err());
        let twice = encode_tree(&[TreeEntry { name: "x".into(), mode: Mode::Link, id }, TreeEntry { name: "x".into(), mode: Mode::Dir, id }]);
        assert!(decode_tree(&twice).err().map(|e| e.to_string()).unwrap_or_default().contains("twice"));
    }
}
