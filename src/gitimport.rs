//! Fetching from git servers: git's pack format in, copse objects out.
//!
//! 1. Ask `git-upload-pack` for the branches we don't have, telling it which
//!    commits we already have so it sends only what's new.
//! 2. Read the pack it sends: inflate each object and apply git's binary deltas.
//! 3. Translate each git object into a copse object, recording the pair in the id
//!    map with the object's *original* git id. Pushing it back later therefore
//!    always uses the same git id, so history stays identical on the server.

use crate::gitwire::{git_hash, hex, human, pkt, GitId, GitMap, Session, Translator, FLUSH};
use crate::objects::{encode_tree, full_entry, Commit, Id, Kind, Mode, Signature, Store, Tag, TreeEntry};
use crate::repo::Repo;
use crate::transport::RemoteRefs;
use anyhow::{anyhow, bail, Context, Result};
use flate2::{Decompress, FlushDecompress, Status};
use rayon::prelude::*;
use sha1::{Digest, Sha1};
use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::sync::Arc;

/// Fetches with fewer objects than this are stored loose; larger ones as one pack.
const LOOSE_LIMIT: usize = 500;

/// Fetch every branch of a git server. Returns its branches as copse commit ids.
pub fn fetch(repo: &Repo, url: &str) -> Result<RemoteRefs> {
    let mut session = Session::open(url, "upload-pack")?;
    let (refs, caps) = session.advertisement()?;
    let head = caps.iter().find_map(|c| c.strip_prefix("symref=HEAD:refs/heads/")).map(str::to_string);
    let branches: Vec<(String, GitId)> =
        refs.iter().filter_map(|(name, id)| name.strip_prefix("refs/heads/").map(|b| (b.to_string(), *id))).collect();
    // Tags, skipping the "peeled" lines (`v1.0^{}`) that just say what an annotated tag points at.
    let tags: Vec<(String, GitId)> = refs
        .iter()
        .filter(|(name, _)| !name.ends_with("^{}"))
        .filter_map(|(name, id)| name.strip_prefix("refs/tags/").map(|t| (t.to_string(), *id)))
        .collect();

    let mut map = GitMap::load(repo)?;
    let have = |g: &GitId, map: &GitMap| map.copse(g).map_or(false, |t| repo.store.contains(&t));
    let mut wants = Vec::new();
    let mut asked = HashSet::new();
    for (_, g) in branches.iter().chain(&tags) {
        if !have(g, &map) && asked.insert(*g) {
            wants.push(*g);
        }
    }

    if !wants.is_empty() {
        let mut ask = Vec::new();
        if caps.contains("ofs-delta") && std::env::var_os("COPSE_NO_OFS_DELTA").is_none() {
            ask.push("ofs-delta");
        }
        if caps.contains("no-progress") && !std::io::IsTerminal::is_terminal(&std::io::stderr()) {
            ask.push("no-progress"); // nobody is watching: don't print the server's progress
        }
        ask.push("agent=copse/0.1");
        let mut request = pkt(format!("want {} {}\n", hex(&wants[0]), ask.join(" ")).as_bytes());
        for w in &wants[1..] {
            request.extend(pkt(format!("want {}\n", hex(w)).as_bytes()));
        }
        request.extend_from_slice(FLUSH);
        // What we already have, so the server leaves it out.
        let mut ours: Vec<Id> = repo.branches()?.into_iter().map(|(_, id)| id).collect();
        ours.extend(repo.tracking_refs()?.into_iter().map(|(_, id)| id));
        let haves: HashSet<GitId> = ours.iter().filter_map(|t| map.git(t)).collect();
        for g in &haves {
            request.extend(pkt(format!("have {}\n", hex(g)).as_bytes()));
        }
        request.extend(pkt(b"done\n"));
        let mut reply = session.exchange(&request)?;

        // The server acknowledges what we have (NAK, or one ACK per common commit; how
        // many depends on the git version), then sends the pack, which starts "PACK".
        let mut pack = Vec::new();
        loop {
            let mut head = [0u8; 4];
            reply.read_exact(&mut head).context("the git server hung up before sending anything")?;
            if &head == b"PACK" {
                pack.extend_from_slice(&head);
                reply.read_to_end(&mut pack)?;
                break;
            }
            let len = std::str::from_utf8(&head).ok().and_then(|h| usize::from_str_radix(h, 16).ok());
            let len = len.with_context(|| format!("unexpected reply from the git server: {:?}", String::from_utf8_lossy(&head)))?;
            if len < 4 {
                continue; // a flush packet
            }
            let mut line = vec![0u8; len - 4];
            reply.read_exact(&mut line)?;
            if !(line.starts_with(b"ACK") || line.starts_with(b"NAK")) {
                bail!("unexpected reply from the git server: {}", String::from_utf8_lossy(&line).trim_end());
            }
        }
        drop(reply);
        let count = import_pack(repo, &mut map, &pack)?;
        println!("Received {count} objects ({})", human(pack.len() as u64));
    }
    drop(session);

    // Tags become local tags, as with git. Yours are never overwritten.
    for (name, g) in tags {
        let Some(copse) = map.copse(&g) else { continue }; // e.g. a tag of a single file: not supported
        if crate::repo::validate_branch(&name).is_err() {
            println!("  skipped tag {name} (not a valid name)");
            continue;
        }
        match repo.tag(&name)? {
            None => {
                repo.set_tag(&name, &copse)?;
                println!("  {}  new tag", crate::cmd::paint("32", &format!("tag {name}")));
            }
            Some(mine) if mine == copse => {}
            Some(_) => println!("  tag {name} on the server differs from yours; kept yours"),
        }
    }

    let mut out = Vec::new();
    for (branch, g) in branches {
        let copse = map.copse(&g).with_context(|| format!("the server didn't send branch {branch}"))?;
        out.push((branch, copse));
    }
    Ok(RemoteRefs { head, branches: out, tags: Vec::new(), tag_targets: Vec::new() }) // tags were already made local above
}

// ---------------------------------------------------------------- git's pack format

enum Base {
    None,
    /// OFS_DELTA: the base is the entry at this byte offset in the pack.
    Offset(usize),
    /// REF_DELTA: the base is the object with this id.
    Id(GitId),
}

struct Entry {
    offset: usize,
    kind: u8,
    data: Vec<u8>,
    base: Base,
}

fn type_name(kind: u8) -> Result<&'static str> {
    Ok(match kind {
        1 => "commit",
        2 => "tree",
        3 => "blob",
        4 => "tag",
        _ => bail!("unknown git object type {kind}"),
    })
}

/// Every object in a pack, deltas resolved: git id -> (type, content). Returns the
/// objects and how many entries the pack had.
fn read_pack(pack: &[u8]) -> Result<(HashMap<GitId, (u8, Arc<Vec<u8>>)>, usize)> {
    if pack.len() < 32 || &pack[..4] != b"PACK" {
        bail!("the git server didn't send a pack");
    }
    let version = u32::from_be_bytes(pack[4..8].try_into().unwrap());
    if version != 2 && version != 3 {
        bail!("unsupported git pack version {version}");
    }
    let count = u32::from_be_bytes(pack[8..12].try_into().unwrap()) as usize;
    let (body, trailer) = pack.split_at(pack.len() - 20);
    let digest: [u8; 20] = Sha1::digest(body).into();
    if digest != trailer {
        bail!("the pack from the git server is damaged (checksum mismatch)");
    }

    let byte = |pos: usize| -> Result<u8> { body.get(pos).copied().ok_or_else(|| anyhow!("the pack is truncated")) };
    let mut entries = Vec::with_capacity(count);
    let mut pos = 12;
    for _ in 0..count {
        let offset = pos;
        let mut c = byte(pos)?;
        pos += 1;
        let kind = (c >> 4) & 7;
        let mut size = (c & 0x0f) as usize;
        let mut shift = 4;
        while c & 0x80 != 0 {
            c = byte(pos)?;
            pos += 1;
            size |= ((c & 0x7f) as usize) << shift;
            shift += 7;
        }
        let base = match kind {
            6 => {
                // Big-endian base-128 with an offset of 1 added per continuation byte.
                let mut c = byte(pos)?;
                pos += 1;
                let mut back = (c & 0x7f) as usize;
                while c & 0x80 != 0 {
                    c = byte(pos)?;
                    pos += 1;
                    back = ((back + 1) << 7) | (c & 0x7f) as usize;
                }
                Base::Offset(offset.checked_sub(back).context("bad delta offset in pack")?)
            }
            7 => {
                let id: GitId = body.get(pos..pos + 20).context("the pack is truncated")?.try_into().unwrap();
                pos += 20;
                Base::Id(id)
            }
            1..=4 => Base::None,
            _ => bail!("unknown object type {kind} in pack"),
        };
        let mut z = Decompress::new(true);
        let mut data = Vec::with_capacity(size + 1);
        loop {
            let (in_before, out_before) = (z.total_in(), z.total_out());
            let input = body.get(pos + z.total_in() as usize..).context("the pack is truncated")?;
            if z.decompress_vec(input, &mut data, FlushDecompress::Finish)? == Status::StreamEnd {
                break;
            }
            if z.total_in() == in_before && z.total_out() == out_before {
                // No progress: either the output needs room, or the data is bad.
                if data.len() == data.capacity() {
                    data.reserve(64 * 1024);
                } else {
                    bail!("the pack is corrupt (bad compressed data)");
                }
            }
        }
        pos += z.total_in() as usize;
        if data.len() != size {
            bail!("the pack is corrupt (object size mismatch)");
        }
        entries.push(Entry { offset, kind, data, base });
    }
    if pos != body.len() {
        bail!("the pack has unexpected trailing data");
    }

    // Resolve deltas. Offset bases always come earlier in the pack; id bases can be
    // anywhere, so keep going until nothing more can be resolved.
    let index_of: HashMap<usize, usize> = entries.iter().enumerate().map(|(i, e)| (e.offset, i)).collect();
    let mut resolved: Vec<Option<(u8, Arc<Vec<u8>>)>> = vec![None; entries.len()];
    let mut by_id: HashMap<GitId, usize> = HashMap::new();
    let mut pending: Vec<usize> = (0..entries.len()).collect();
    while !pending.is_empty() {
        let mut still = Vec::new();
        for i in pending.iter().copied() {
            let base = match &entries[i].base {
                Base::None => None,
                Base::Offset(off) => Some(*index_of.get(off).context("delta base missing from pack")?),
                Base::Id(id) => match by_id.get(id) {
                    Some(&j) => Some(j),
                    None => {
                        still.push(i);
                        continue;
                    }
                },
            };
            let (kind, content) = match base {
                None => (entries[i].kind, Arc::new(std::mem::take(&mut entries[i].data))),
                Some(j) => match &resolved[j] {
                    Some((k, b)) => (*k, Arc::new(apply_delta(b, &entries[i].data)?)),
                    None => {
                        still.push(i);
                        continue;
                    }
                },
            };
            entries[i].data = Vec::new();
            by_id.insert(git_hash(type_name(kind)?, &content), i);
            resolved[i] = Some((kind, content));
        }
        if still.len() == pending.len() {
            bail!("the pack refers to objects it doesn't contain");
        }
        pending = still;
    }
    let objects = by_id.into_iter().map(|(id, i)| (id, resolved[i].take().unwrap())).collect();
    Ok((objects, count))
}

fn varint(data: &[u8], pos: &mut usize) -> Result<usize> {
    let (mut value, mut shift) = (0usize, 0);
    loop {
        let c = *data.get(*pos).context("corrupt delta")?;
        *pos += 1;
        value |= ((c & 0x7f) as usize) << shift;
        shift += 7;
        if c & 0x80 == 0 {
            return Ok(value);
        }
    }
}

/// Apply one of git's binary deltas: a list of "copy this range of the base" and
/// "insert these literal bytes" instructions.
fn apply_delta(base: &[u8], delta: &[u8]) -> Result<Vec<u8>> {
    let mut p = 0;
    if varint(delta, &mut p)? != base.len() {
        bail!("corrupt delta (base size mismatch)");
    }
    let size = varint(delta, &mut p)?;
    let mut out = Vec::with_capacity(size);
    while p < delta.len() {
        let op = delta[p];
        p += 1;
        if op & 0x80 != 0 {
            let (mut off, mut len) = (0usize, 0usize);
            for i in 0..4 {
                if op & (1 << i) != 0 {
                    off |= (*delta.get(p).context("corrupt delta")? as usize) << (8 * i);
                    p += 1;
                }
            }
            for i in 0..3 {
                if op & (0x10 << i) != 0 {
                    len |= (*delta.get(p).context("corrupt delta")? as usize) << (8 * i);
                    p += 1;
                }
            }
            if len == 0 {
                len = 0x10000;
            }
            out.extend_from_slice(base.get(off..off + len).context("corrupt delta (copy out of range)")?);
        } else if op != 0 {
            let n = op as usize;
            out.extend_from_slice(delta.get(p..p + n).context("corrupt delta (truncated insert)")?);
            p += n;
        } else {
            bail!("corrupt delta (reserved instruction)");
        }
    }
    if out.len() != size {
        bail!("corrupt delta (result size mismatch)");
    }
    Ok(out)
}

// ---------------------------------------------------------------- git -> copse

struct ParsedCommit {
    tree: GitId,
    parents: Vec<GitId>,
    author: Signature,
    committer: Signature,
    extra: Vec<u8>,
    message: String,
}

fn parse_git_id(s: &str) -> Result<GitId> {
    let mut out = [0u8; 20];
    if s.len() != 40 {
        bail!("bad object id {s:?} in commit");
    }
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16)?;
    }
    Ok(out)
}

/// "Name <email> 1700000000 +0100"
fn parse_signature(s: &str) -> Result<Signature> {
    let mut parts = s.rsplitn(3, ' ');
    let (Some(tz), Some(time), Some(who)) = (parts.next(), parts.next(), parts.next()) else {
        bail!("can't read the signature {s:?}");
    };
    Ok(Signature { who: who.to_string(), time: time.parse().with_context(|| format!("bad time in {s:?}"))?, tz: tz.to_string() })
}

fn parse_commit(data: &[u8], id: &GitId) -> Result<ParsedCommit> {
    let split = data.windows(2).position(|w| w == b"\n\n").with_context(|| format!("commit {} is malformed", hex(id)))?;
    let (header, message) = (&data[..split], &data[split + 2..]);
    let header = std::str::from_utf8(header).map_err(|_| anyhow!("commit {} has a header that isn't UTF-8 (not supported yet)", hex(id)))?;
    let message = String::from_utf8(message.to_vec()).map_err(|_| anyhow!("commit {} has a message that isn't UTF-8 (not supported yet)", hex(id)))?;
    let (mut tree, mut parents, mut author, mut committer) = (None, Vec::new(), None, None);
    let mut extra = Vec::new();
    let mut last_was_extra = false;
    for line in header.split('\n') {
        if line.starts_with(' ') {
            // A continuation line (e.g. inside a signature) belongs to the header above it.
            if !last_was_extra {
                bail!("commit {} has an unexpected continuation line", hex(id));
            }
            extra.extend_from_slice(line.as_bytes());
            extra.push(b'\n');
            continue;
        }
        let (key, value) = line.split_once(' ').unwrap_or((line, ""));
        last_was_extra = false;
        match key {
            "tree" => tree = Some(parse_git_id(value)?),
            "parent" => parents.push(parse_git_id(value)?),
            "author" => author = Some(parse_signature(value)?),
            "committer" => committer = Some(parse_signature(value)?),
            _ => {
                // encoding, gpgsig, mergetag...: kept verbatim so the commit round-trips.
                extra.extend_from_slice(line.as_bytes());
                extra.push(b'\n');
                last_was_extra = true;
            }
        }
    }
    let author = author.with_context(|| format!("commit {} has no author", hex(id)))?;
    Ok(ParsedCommit {
        tree: tree.with_context(|| format!("commit {} has no tree", hex(id)))?,
        parents,
        committer: committer.unwrap_or_else(|| author.clone()),
        author,
        extra,
        message,
    })
}

struct ParsedTag {
    object: GitId,
    kind: String,
    name: String,
    tagger: Option<Signature>,
    extra: Vec<u8>,
    message: String,
}

fn parse_tag(data: &[u8], id: &GitId) -> Result<ParsedTag> {
    let split = data.windows(2).position(|w| w == b"\n\n").unwrap_or(data.len());
    let header = std::str::from_utf8(&data[..split]).map_err(|_| anyhow!("tag {} isn't UTF-8 (not supported yet)", hex(id)))?;
    let message = data.get(split + 2..).unwrap_or_default();
    let message = String::from_utf8(message.to_vec()).map_err(|_| anyhow!("tag {} has a message that isn't UTF-8", hex(id)))?;
    let (mut object, mut kind, mut name, mut tagger, mut extra) = (None, String::new(), String::new(), None, Vec::new());
    let mut last_was_extra = false;
    for line in header.split('\n') {
        if line.starts_with(' ') && last_was_extra {
            extra.extend_from_slice(line.as_bytes());
            extra.push(b'\n');
            continue;
        }
        let (key, value) = line.split_once(' ').unwrap_or((line, ""));
        last_was_extra = false;
        match key {
            "object" => object = Some(parse_git_id(value)?),
            "type" => kind = value.to_string(),
            "tag" => name = value.to_string(),
            "tagger" => tagger = Some(parse_signature(value)?),
            _ => {
                extra.extend_from_slice(line.as_bytes());
                extra.push(b'\n');
                last_was_extra = true;
            }
        }
    }
    Ok(ParsedTag { object: object.with_context(|| format!("tag {} has no object", hex(id)))?, kind, name, tagger, extra, message })
}

fn parse_tree(data: &[u8]) -> Result<Vec<(Vec<u8>, String, GitId)>> {
    let mut out = Vec::new();
    let mut p = 0;
    while p < data.len() {
        let space = data[p..].iter().position(|&b| b == b' ').context("malformed git tree")? + p;
        let nul = data[space..].iter().position(|&b| b == 0).context("malformed git tree")? + space;
        let id: GitId = data.get(nul + 1..nul + 21).context("malformed git tree")?.try_into().unwrap();
        let name = String::from_utf8(data[space + 1..nul].to_vec()).map_err(|_| anyhow!("a file name isn't UTF-8 (not supported yet)"))?;
        out.push((data[p..space].to_vec(), name, id));
        p = nul + 21;
    }
    Ok(out)
}

struct Importer<'a> {
    map: &'a mut GitMap,
    objects: &'a HashMap<GitId, (u8, Arc<Vec<u8>>)>,
    /// New copse objects to store: (copse id, kind, content, original git id).
    out: Vec<(Id, Kind, Arc<Vec<u8>>, GitId)>,
}

impl Importer<'_> {
    fn get(&self, g: &GitId, want: u8) -> Result<Arc<Vec<u8>>> {
        let (kind, data) = self.objects.get(g).with_context(|| format!("the server didn't send object {}", hex(g)))?;
        if *kind != want {
            bail!("object {} has the wrong type", hex(g));
        }
        Ok(data.clone())
    }

    fn tree(&mut self, g: GitId) -> Result<Id> {
        if let Some(t) = self.map.copse(&g) {
            return Ok(t);
        }
        let data = self.get(&g, 2)?;
        let mut entries = Vec::new();
        for (mode, name, child) in parse_tree(&data)? {
            let mode = match mode.as_slice() {
                b"100644" | b"100664" | b"100640" => Mode::File,
                b"100755" => Mode::Exec,
                b"120000" => Mode::Link,
                b"40000" | b"040000" => Mode::Dir,
                b"160000" => Mode::Submodule,
                other => bail!("unknown file mode {} for {name}", String::from_utf8_lossy(other)),
            };
            crate::objects::check_name(&name, mode).context("the repository can't be used safely")?;
            let id = if mode == Mode::Dir {
                self.tree(child)?
            } else if mode == Mode::Submodule {
                crate::objects::gitlink_id(&child) // another repository's commit: kept as its git id
            } else {
                self.map.copse(&child).with_context(|| format!("the server didn't send the contents of {name}"))?
            };
            entries.push(TreeEntry { name, mode, id });
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name)); // copse's own order
        if let Some(w) = entries.windows(2).find(|w| w[0].name == w[1].name) {
            bail!("the repository can't be used safely: a folder lists {:?} twice", w[0].name);
        }
        let bytes = encode_tree(&entries);
        let id = Store::hash(Kind::Tree, &bytes);
        self.map.insert(id, g);
        self.out.push((id, Kind::Tree, Arc::new(bytes), g));
        Ok(id)
    }

    /// Translate an annotated tag. None if it points at something copse can't tag
    /// (a single file or folder).
    fn tag(&mut self, g: GitId) -> Result<Option<Id>> {
        if let Some(t) = self.map.copse(&g) {
            return Ok(Some(t));
        }
        let t = parse_tag(&self.get(&g, 4)?, &g)?;
        let (target, target_kind) = match t.kind.as_str() {
            "commit" => (self.map.copse(&t.object), Kind::Commit),
            "tag" => (self.tag(t.object)?, Kind::Tag),
            _ => (None, Kind::Blob),
        };
        let Some(target) = target else { return Ok(None) };
        let tag = Tag { object: target, target_kind, name: t.name, tagger: t.tagger, extra: t.extra, message: t.message };
        let bytes = tag.encode();
        let id = Store::hash(Kind::Tag, &bytes);
        self.map.insert(id, g);
        self.out.push((id, Kind::Tag, Arc::new(bytes), g));
        Ok(Some(id))
    }

    fn commit(&mut self, g: GitId) -> Result<()> {
        if self.map.copse(&g).is_some() {
            return Ok(()); // already have it (or just imported it)
        }
        let c = parse_commit(&self.get(&g, 1)?, &g)?;
        let tree = self.tree(c.tree)?;
        let parents = c
            .parents
            .iter()
            .map(|p| self.map.copse(p).with_context(|| format!("missing parent commit {}", hex(p))))
            .collect::<Result<Vec<_>>>()?;
        let commit = Commit { tree, parents, author: c.author, committer: c.committer, extra: c.extra, message: c.message };
        let bytes = commit.encode();
        let id = Store::hash(Kind::Commit, &bytes);
        self.map.insert(id, g);
        self.out.push((id, Kind::Commit, Arc::new(bytes), g));
        Ok(())
    }
}

/// Commits in the pack that we don't have yet, parents before children.
fn commit_order(objects: &HashMap<GitId, (u8, Arc<Vec<u8>>)>, map: &GitMap) -> Result<Vec<GitId>> {
    let mut order = Vec::new();
    let mut visited = HashSet::new();
    let mut roots: Vec<&GitId> = objects.iter().filter(|(_, (k, _))| *k == 1).map(|(g, _)| g).collect();
    roots.sort(); // deterministic
    for root in roots {
        let mut stack = vec![(*root, false)];
        while let Some((g, expanded)) = stack.pop() {
            if expanded {
                order.push(g);
                continue;
            }
            if map.copse(&g).is_some() || !visited.insert(g) {
                continue;
            }
            stack.push((g, true));
            let Some((1, data)) = objects.get(&g).map(|(k, d)| (*k, d)) else { continue };
            for p in parse_commit(data, &g)?.parents.iter().rev() {
                if objects.contains_key(p) {
                    stack.push((*p, false));
                }
            }
        }
    }
    Ok(order)
}

/// Read a pack from a git server into copse. Returns the number of objects received.
pub fn import_pack(repo: &Repo, map: &mut GitMap, pack: &[u8]) -> Result<usize> {
    let (objects, count) = read_pack(pack)?;
    let mut im = Importer { map, objects: &objects, out: Vec::new() };

    // File contents first, hashed in parallel.
    let blobs: Vec<(GitId, Arc<Vec<u8>>)> = objects
        .iter()
        .filter(|(g, (k, _))| *k == 3 && im.map.copse(g).is_none())
        .map(|(g, (_, d))| (*g, d.clone()))
        .collect();
    let hashed: Vec<(GitId, Id, Arc<Vec<u8>>)> = blobs.into_par_iter().map(|(g, d)| (g, Store::hash(Kind::Blob, &d), d)).collect();
    for (g, id, data) in hashed {
        im.map.insert(id, g);
        im.out.push((id, Kind::Blob, data, g));
    }
    // Then commits (each bringing its folders), parents first, then annotated tags.
    for g in commit_order(&objects, im.map)? {
        im.commit(g)?;
    }
    let mut tag_ids: Vec<GitId> = objects.iter().filter(|(_, (k, _))| *k == 4).map(|(g, _)| *g).collect();
    tag_ids.sort();
    for g in tag_ids {
        im.tag(g)?;
    }
    let out = std::mem::take(&mut im.out);

    // Store everything, then record the id map (so it never points at missing objects).
    if out.len() >= LOOSE_LIMIT {
        let mut writer = repo.store.new_pack_writer(0)?;
        for chunk in out.chunks(512) {
            let entries: Vec<Vec<u8>> = chunk.par_iter().map(|(_, k, d, _)| full_entry(*k, d)).collect::<Result<_>>()?;
            for ((id, _, _, _), e) in chunk.iter().zip(entries) {
                writer.add(id, &e)?;
            }
        }
        writer.finish()?;
        repo.store.reload_packs();
    } else {
        for commits in [false, true] {
            out.par_iter()
                .filter(|(_, k, _, _)| (*k == Kind::Commit) == commits)
                .try_for_each(|(id, k, d, _)| repo.store.put_with_id(id, *k, d))?;
        }
    }
    map.save()?;

    // Check that each object translates back to exactly the id git gave it. Anything
    // that doesn't (unusual formatting) still round-trips, through the id map.
    let tr = Translator::new(repo)?;
    let mut differ = 0;
    for (id, kind, _, g) in &out {
        if *kind != Kind::Blob {
            let (ty, body) = tr.git_object(id)?;
            if git_hash(type_name(ty)?, &body) != *g {
                differ += 1;
            }
        }
    }
    if differ > 0 {
        println!("  note: {differ} object(s) have unusual git formatting; they keep their original git ids");
    }
    let _ = std::io::stdout().flush();
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applies_git_deltas() {
        let base = b"hello, world";
        // base size 12, result size 19: copy 7 bytes from offset 0, insert "there, ", copy 5 from offset 7
        let mut delta = vec![12, 19];
        delta.extend_from_slice(&[0x90, 7]); // copy: offset 0 (no offset bytes), size 7
        delta.push(7);
        delta.extend_from_slice(b"there, ");
        delta.extend_from_slice(&[0x91, 7, 5]); // copy: offset 7, size 5
        assert_eq!(apply_delta(base, &delta).unwrap(), b"hello, there, world");
        assert!(apply_delta(b"short", &delta).is_err(), "wrong base size is rejected");
    }

    #[test]
    fn reads_commits_with_extra_headers() {
        let raw = b"tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n\
author A U Thor <a@example.com> 1700000000 +0100\n\
committer C O Mitter <c@example.com> 1700000100 -0500\n\
gpgsig -----BEGIN PGP SIGNATURE-----\n \n abc\n -----END PGP SIGNATURE-----\n\
\nSubject line\n\nBody.\n";
        let c = parse_commit(raw, &[0; 20]).unwrap();
        assert_eq!(c.author.who, "A U Thor <a@example.com>");
        assert_eq!((c.committer.time, c.committer.tz.as_str()), (1700000100, "-0500"));
        assert!(c.parents.is_empty());
        assert_eq!(c.extra, b"gpgsig -----BEGIN PGP SIGNATURE-----\n \n abc\n -----END PGP SIGNATURE-----\n");
        assert_eq!(c.message, "Subject line\n\nBody.\n");
    }
}
