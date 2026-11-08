//! Pack files: many objects in one file, found through a sorted index.
//!
//! objects/pack/pack-<name>.pack
//!   "CPPK" | version u32 | entries...
//!   Each entry is either a full object (kind byte + zstd content, exactly like a
//!   loose object) or a delta (0x80|kind, base id, zstd content compressed with
//!   the base object's content as the dictionary).
//!
//! objects/pack/pack-<name>.idx
//!   "CPIX" | version u32 | flags u32 | count u32
//!   count x (id 32 bytes | offset u64 | length u32), sorted by id
//!   BLAKE3 of everything above (also gives the pack its name)
//!
//! A pack only becomes visible once its .idx exists, and the .idx is written
//! last, so readers never see a half-written pack.

use crate::objects::Id;
use anyhow::{bail, Context, Result};
use std::fs::{self, File};
use std::io::{BufWriter, Read, Write};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const PACK_MAGIC: &[u8; 4] = b"CPPK";
const IDX_MAGIC: &[u8; 4] = b"CPIX";
const VERSION: u32 = 1;
const HEADER: usize = 16;
const RECORD: usize = 44;

/// Set on packs written by `copse gc`, which already contain deltas.
pub const FLAG_COMPACTED: u32 = 1;

pub struct Pack {
    pub name: String,
    pub flags: u32,
    pub idx_path: PathBuf,
    pub pack_path: PathBuf,
    file: File,
    idx: Vec<u8>,
    /// fanout[b]..fanout[b+1] is the range of records whose id starts with byte b.
    fanout: [u32; 257],
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

impl Pack {
    pub fn open(idx_path: &Path) -> Result<Pack> {
        let idx = fs::read(idx_path)?;
        if idx.len() < HEADER + 32 || &idx[..4] != IDX_MAGIC {
            bail!("not a copse pack index");
        }
        if u32_at(&idx, 4) != VERSION {
            bail!("unsupported pack version {}", u32_at(&idx, 4));
        }
        let flags = u32_at(&idx, 8);
        let count = u32_at(&idx, 12) as usize;
        if idx.len() != HEADER + count * RECORD + 32 {
            bail!("pack index is truncated");
        }
        let (body, trailer) = idx.split_at(idx.len() - 32);
        if blake3::hash(body).as_bytes() != trailer {
            bail!("pack index is corrupt");
        }
        let mut fanout = [0u32; 257];
        for i in 0..count {
            fanout[idx[HEADER + i * RECORD] as usize + 1] += 1;
        }
        for i in 1..257 {
            fanout[i] += fanout[i - 1];
        }
        let pack_path = idx_path.with_extension("pack");
        let file = File::open(&pack_path).with_context(|| format!("opening {}", pack_path.display()))?;
        let name = idx_path.file_stem().unwrap_or_default().to_string_lossy().trim_start_matches("pack-").to_string();
        Ok(Pack { name, flags, idx_path: idx_path.to_path_buf(), pack_path, file, idx, fanout })
    }

    fn record(&self, i: usize) -> &[u8] {
        &self.idx[HEADER + i * RECORD..HEADER + (i + 1) * RECORD]
    }

    pub fn len(&self) -> usize {
        self.fanout[256] as usize
    }

    pub fn id_at(&self, i: usize) -> Id {
        Id::from_bytes(self.record(i)[..32].try_into().unwrap())
    }

    /// Indexes of the records whose id starts with `first_byte`.
    pub fn bucket(&self, first_byte: u8) -> std::ops::Range<usize> {
        self.fanout[first_byte as usize] as usize..self.fanout[first_byte as usize + 1] as usize
    }

    /// Where the object's entry lives in the pack: (offset, length).
    pub fn find(&self, id: &Id) -> Option<(u64, u32)> {
        let key = id.as_bytes();
        let range = self.bucket(key[0]);
        let (mut lo, mut hi) = (range.start, range.end);
        while lo < hi {
            let mid = (lo + hi) / 2;
            let r = self.record(mid);
            match r[..32].cmp(&key[..]) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => {
                    let offset = u64::from_le_bytes(r[32..40].try_into().unwrap());
                    return Some((offset, u32_at(r, 40)));
                }
            }
        }
        None
    }

    /// A reader over one entry, for objects too big to read in one go.
    pub fn reader(&self, offset: u64, len: u32) -> Result<RangeReader> {
        Ok(RangeReader { file: self.file.try_clone()?, pos: offset, end: offset + len as u64 })
    }

    pub fn read(&self, offset: u64, len: u32) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; len as usize];
        self.file
            .read_exact_at(&mut buf, offset)
            .with_context(|| format!("reading {}", self.pack_path.display()))?;
        Ok(buf)
    }
}

/// Every readable pack in `dir`. Damaged packs are skipped with a warning.
pub fn load_packs(dir: &Path) -> Vec<Pack> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut packs = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("pack-") && name.ends_with(".idx") {
            match Pack::open(&path) {
                Ok(p) => packs.push(p),
                Err(e) => eprintln!("copse: warning: skipping {}: {e:#}", path.display()),
            }
        }
    }
    // Newest-written packs are usually the hottest; order doesn't affect correctness.
    packs.sort_by(|a, b| b.len().cmp(&a.len()));
    packs
}

/// Reads a byte range of a pack with positioned reads (so it never disturbs, or is
/// disturbed by, other readers of the same file).
pub struct RangeReader {
    file: File,
    pos: u64,
    end: u64,
}

impl Read for RangeReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let want = buf.len().min((self.end - self.pos) as usize);
        if want == 0 {
            return Ok(0);
        }
        let n = self.file.read_at(&mut buf[..want], self.pos)?;
        if n == 0 {
            return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "pack is shorter than its index says"));
        }
        self.pos += n as u64;
        Ok(n)
    }
}

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Streams entries into a new pack. Dropping it without `finish` deletes the temp file.
pub struct PackWriter {
    dir: PathBuf,
    tmp: PathBuf,
    out: Option<BufWriter<File>>,
    offset: u64,
    entries: Vec<([u8; 32], u64, u32)>,
    flags: u32,
}

impl PackWriter {
    pub fn new(dir: &Path, flags: u32) -> Result<PackWriter> {
        fs::create_dir_all(dir)?;
        let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let tmp = dir.join(format!("tmp-{}-{n}.pack", std::process::id()));
        let mut out = BufWriter::with_capacity(1 << 20, File::create(&tmp)?);
        out.write_all(PACK_MAGIC)?;
        out.write_all(&VERSION.to_le_bytes())?;
        Ok(PackWriter { dir: dir.to_path_buf(), tmp, out: Some(out), offset: 8, entries: Vec::new(), flags })
    }

    pub fn add(&mut self, id: &Id, entry: &[u8]) -> Result<()> {
        let len = u32::try_from(entry.len()).context("object too large for a pack (over 4 GB)")?;
        self.out.as_mut().unwrap().write_all(entry)?;
        self.entries.push((*id.as_bytes(), self.offset, len));
        self.offset += entry.len() as u64;
        Ok(())
    }

    /// Add an entry by copying `len` bytes from `reader`, for entries too big to hold.
    pub fn add_from(&mut self, id: &Id, reader: &mut impl Read, len: u64) -> Result<()> {
        let len32 = u32::try_from(len).context("object too large for a pack (over 4 GB)")?;
        let copied = std::io::copy(reader, self.out.as_mut().unwrap())?;
        if copied != len {
            bail!("object {} was cut short while packing", id.to_hex());
        }
        self.entries.push((*id.as_bytes(), self.offset, len32));
        self.offset += len;
        Ok(())
    }

    /// Flush, write the index, and make the pack visible. Returns the pack's name.
    pub fn finish(mut self) -> Result<String> {
        let out = self.out.take().unwrap();
        let file = out.into_inner().map_err(|e| e.into_error())?;
        file.sync_all()?;

        let mut entries = std::mem::take(&mut self.entries);
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        entries.dedup_by(|a, b| a.0 == b.0);
        let mut idx = Vec::with_capacity(HEADER + entries.len() * RECORD + 32);
        idx.extend_from_slice(IDX_MAGIC);
        idx.extend_from_slice(&VERSION.to_le_bytes());
        idx.extend_from_slice(&self.flags.to_le_bytes());
        idx.extend_from_slice(&(entries.len() as u32).to_le_bytes());
        for (id, offset, len) in &entries {
            idx.extend_from_slice(id);
            idx.extend_from_slice(&offset.to_le_bytes());
            idx.extend_from_slice(&len.to_le_bytes());
        }
        let digest = blake3::hash(&idx);
        idx.extend_from_slice(digest.as_bytes());
        let name = digest.to_hex()[..40].to_string();

        fs::rename(&self.tmp, self.dir.join(format!("pack-{name}.pack")))?;
        let idx_tmp = self.dir.join(format!("tmp-{name}.idx"));
        let mut f = File::create(&idx_tmp)?;
        f.write_all(&idx)?;
        f.sync_all()?;
        fs::rename(&idx_tmp, self.dir.join(format!("pack-{name}.idx")))?;
        Ok(name)
    }
}

impl Drop for PackWriter {
    fn drop(&mut self) {
        if self.out.is_some() {
            self.out = None;
            let _ = fs::remove_file(&self.tmp);
        }
    }
}
