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
/// `depth` makes or deepens a shallow clone (git's `deepen`).
pub fn fetch(repo: &Repo, url: &str, depth: crate::remote::Depth) -> Result<RemoteRefs> {
    use crate::remote::Depth;
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

    // Shallow clones: the commits whose parents we don't have, as git ids.
    let shallow_now = repo.shallow();
    if depth == Depth::Unshallow && shallow_now.is_empty() {
        bail!("this repository isn't shallow; it already has all of history");
    }
    let shallow_git: Vec<GitId> = shallow_now.iter().filter_map(|id| map.git(id)).collect();
    let deepen = match depth {
        Depth::Last(n) => Some(n.max(1)),
        Depth::Unshallow => Some(2_147_483_647), // what git sends for --unshallow
        Depth::Full => None,
    };
    // A shallow fetch takes branches only; tags come along if they name history that
    // arrived (include-tag), as git does.
    let shallow_mode = matches!(depth, Depth::Last(_)) || (!shallow_now.is_empty() && depth != Depth::Unshallow);
    if (deepen.is_some() || !shallow_git.is_empty()) && !caps.contains("shallow") {
        bail!("this git server doesn't support shallow clones; clone without --depth");
    }
    let mut wants = Vec::new();
    let mut asked = HashSet::new();
    let wanted: Vec<&(String, GitId)> = if shallow_mode { branches.iter().collect() } else { branches.iter().chain(&tags).collect() };
    for (_, g) in wanted {
        // Deepening asks again for tips we already have, so the server walks from them.
        if (deepen.is_some() || !have(g, &map)) && asked.insert(*g) {
            wants.push(*g);
        }
    }
    let (mut new_shallow, mut unshallowed) = (HashSet::new(), HashSet::new());
    let dumb = caps.contains("dumb");
    if dumb && (deepen.is_some() || !shallow_git.is_empty() || repo.is_partial()) {
        bail!("this git server only serves plain files (the \"dumb\" protocol), which has no shallow or partial clones; clone without --depth or --filter");
    }
    // A partial clone asks for commits and folders only; file contents come later.
    let partial = repo.is_partial();
    if partial && !caps.contains("filter") {
        bail!("this git server doesn't support partial clones (filters); clone without --filter");
    }

    if dumb && !wants.is_empty() {
        let h = session.http().context("internal error: a dumb server over something other than HTTP")?;
        let objects = dumb_objects(h, &wants, |g| have(g, &map))?;
        let count = import_objects(repo, &mut map, &objects, &HashSet::new(), false)?;
        println!("Received {count} objects");
    } else if !wants.is_empty() {
        let mut ask = Vec::new();
        if caps.contains("ofs-delta") && std::env::var_os("COPSE_NO_OFS_DELTA").is_none() {
            ask.push("ofs-delta");
        }
        if caps.contains("no-progress") && !std::io::IsTerminal::is_terminal(&std::io::stderr()) {
            ask.push("no-progress"); // nobody is watching: don't print the server's progress
        }
        if deepen.is_some() || !shallow_git.is_empty() {
            ask.push("shallow");
        }
        if partial {
            ask.push("filter");
        }
        if shallow_mode && caps.contains("include-tag") {
            ask.push("include-tag");
        }
        let agent = format!("agent=copse/{}", crate::VERSION);
        ask.push(&agent);
        let mut request = pkt(format!("want {} {}\n", hex(&wants[0]), ask.join(" ")).as_bytes());
        for w in &wants[1..] {
            request.extend(pkt(format!("want {}\n", hex(w)).as_bytes()));
        }
        for g in &shallow_git {
            request.extend(pkt(format!("shallow {}\n", hex(g)).as_bytes()));
        }
        if let Some(d) = deepen {
            request.extend(pkt(format!("deepen {d}\n").as_bytes()));
        }
        if partial {
            request.extend(pkt(b"filter blob:none\n"));
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

        let (pack, sh, unsh) = read_reply(&mut reply)?;
        new_shallow.extend(sh);
        unshallowed.extend(unsh);
        drop(reply);
        let count = import_pack(repo, &mut map, &pack, &new_shallow, partial)?;
        println!("Received {count} objects ({})", human(pack.len() as u64));
    }
    drop(session);

    // Where history now stops: commits that got their parents leave the list, the
    // server's new boundary joins it, and commits above the old boundary are
    // renumbered (their copse ids depend on their parents').
    if deepen.is_some() || !new_shallow.is_empty() || !unshallowed.is_empty() {
        let mut set: HashSet<Id> = if depth == Depth::Unshallow { HashSet::new() } else { shallow_now.clone() };
        set.retain(|id| map.git(id).map_or(true, |g| !unshallowed.contains(&g)));
        set.extend(new_shallow.iter().filter_map(|g| map.copse(g)));
        let renumbered = if unshallowed.is_empty() && depth != Depth::Unshallow { HashMap::new() } else { renumber(repo, &mut map)? };
        let set: HashSet<Id> = set.into_iter().map(|id| *renumbered.get(&id).unwrap_or(&id)).collect();
        if set != repo.shallow() {
            repo.set_shallow(&set)?;
        }
        if depth == Depth::Unshallow {
            println!("All of history is here now; this is no longer a shallow clone.");
        }
    }

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

/// Deeper history arrived for a shallow clone: commits that held their missing
/// parents as git ids get the real ones, and every commit after them is rebuilt
/// with its parents' new ids (copse ids depend on parents). Branches, remote
/// branches, tags and a detached HEAD move to the new ids; git ids don't change.
/// Returns old id -> new id.
fn renumber(repo: &Repo, map: &mut GitMap) -> Result<HashMap<Id, Id>> {
    use crate::objects::{gitlink_sha, is_gitlink};
    let mut tips: Vec<Id> = repo.branches()?.into_iter().map(|(_, id)| id).collect();
    tips.extend(repo.tracking_refs()?.into_iter().map(|(_, id)| id));
    for (_, id) in repo.tags()? {
        tips.extend(repo.peel(id).ok());
    }
    tips.extend(repo.head_commit()?);
    // Every commit they reach, parents first (as stored: shallow ones keep their parents).
    let mut order = Vec::new();
    let mut seen = HashSet::new();
    let mut stack: Vec<(Id, bool)> = tips.into_iter().map(|t| (t, false)).collect();
    while let Some((id, expanded)) = stack.pop() {
        if expanded {
            order.push(id);
            continue;
        }
        if is_gitlink(&id) || !repo.store.contains(&id) || !seen.insert(id) {
            continue;
        }
        stack.push((id, true));
        for p in repo.read_commit_raw(&id)?.parents.iter().rev() {
            stack.push((*p, false));
        }
    }
    let mut renumbered: HashMap<Id, Id> = HashMap::new();
    for id in order {
        let c = repo.read_commit_raw(&id)?;
        let parents: Vec<Id> = c
            .parents
            .iter()
            .map(|p| if is_gitlink(p) { map.copse(&gitlink_sha(p)).filter(|c| repo.store.contains(c)).unwrap_or(*p) } else { *renumbered.get(p).unwrap_or(p) })
            .collect();
        if parents != c.parents {
            let mut n = c.clone();
            n.parents = parents;
            let new_id = repo.store.put(Kind::Commit, &n.encode())?;
            if let Some(g) = map.git(&id) {
                map.replace(new_id, g);
            }
            renumbered.insert(id, new_id);
        }
    }
    if renumbered.is_empty() {
        return Ok(renumbered);
    }
    map.save()?;
    for (name, id) in repo.branches()? {
        if let Some(n) = renumbered.get(&id) {
            repo.set_branch(&name, n)?;
        }
    }
    for (full, id) in repo.tracking_refs()? {
        if let (Some(n), Some((remote, branch))) = (renumbered.get(&id), full.split_once('/')) {
            repo.set_tracking(remote, branch, n)?;
        }
    }
    for (name, id) in repo.tags()? {
        if let Some(n) = renumbered.get(&id) {
            repo.set_tag(&name, n)?;
        } else if let Ok((Kind::Tag, data)) = repo.store.get(&id) {
            let mut t = Tag::decode(&data)?;
            if let Some(n) = renumbered.get(&t.object) {
                t.object = *n;
                let new_tag = repo.store.put(Kind::Tag, &t.encode())?;
                if let Some(g) = map.git(&id) {
                    map.replace(new_tag, g);
                }
                repo.set_tag(&name, &new_tag)?;
            }
        }
    }
    if let crate::repo::Head::Detached(id) = repo.head()? {
        if let Some(n) = renumbered.get(&id) {
            repo.set_head(&crate::repo::Head::Detached(*n))?;
        }
    }
    map.save()?;
    Ok(renumbered)
}

/// The server's reply to a fetch: shallow lines (where history now stops), its
/// acknowledgements, then the pack, which starts "PACK".
fn read_reply(reply: &mut dyn Read) -> Result<(Vec<u8>, HashSet<GitId>, HashSet<GitId>)> {
    let (mut shallow, mut unshallow) = (HashSet::new(), HashSet::new());
    let mut pack = Vec::new();
    loop {
        let mut head = [0u8; 4];
        reply.read_exact(&mut head).context("the git server hung up before sending anything")?;
        if &head == b"PACK" {
            pack.extend_from_slice(&head);
            reply.read_to_end(&mut pack)?;
            return Ok((pack, shallow, unshallow));
        }
        let len = std::str::from_utf8(&head).ok().and_then(|h| usize::from_str_radix(h, 16).ok());
        let len = len.with_context(|| format!("unexpected reply from the git server: {:?}", String::from_utf8_lossy(&head)))?;
        if len < 4 {
            continue; // a flush packet
        }
        let mut line = vec![0u8; len - 4];
        reply.read_exact(&mut line)?;
        if let Some(g) = line.strip_prefix(b"shallow ") {
            shallow.insert(parse_git_id(String::from_utf8_lossy(g).trim())?);
        } else if let Some(g) = line.strip_prefix(b"unshallow ") {
            unshallow.insert(parse_git_id(String::from_utf8_lossy(g).trim())?);
        } else if let Some(err) = line.strip_prefix(b"ERR ") {
            bail!("the git server refused: {}", String::from_utf8_lossy(err).trim_end());
        } else if !(line.starts_with(b"ACK") || line.starts_with(b"NAK")) {
            bail!("unexpected reply from the git server: {}", String::from_utf8_lossy(&line).trim_end());
        }
    }
}

/// A partial clone of a git server: fetch these files' contents (named by their git
/// ids) and store them under those names.
pub fn fetch_blobs(store: &Store, url: &str, ids: &[Id]) -> Result<()> {
    use crate::objects::{gitlink_sha, is_gitlink};
    let mut wants = Vec::new();
    for id in ids {
        if !is_gitlink(id) {
            bail!("{} isn't a file from the git server, so it can't be fetched from there", crate::objects::short(id));
        }
        wants.push(gitlink_sha(id));
    }
    if wants.is_empty() {
        return Ok(());
    }
    let mut session = Session::open(url, "upload-pack")?;
    let (_, caps) = session.advertisement()?;
    let agent = format!("agent=copse/{}", crate::VERSION);
    let mut ask = vec!["no-progress", agent.as_str()];
    if caps.contains("ofs-delta") {
        ask.push("ofs-delta");
    }
    let mut request = pkt(format!("want {} {}\n", hex(&wants[0]), ask.join(" ")).as_bytes());
    for w in &wants[1..] {
        request.extend(pkt(format!("want {}\n", hex(w)).as_bytes()));
    }
    request.extend_from_slice(FLUSH);
    request.extend(pkt(b"done\n"));
    let mut reply = session.exchange(&request)?;
    let (pack, _, _) = read_reply(&mut reply)?;
    drop(reply);
    let (objects, _) = read_pack(&pack)?;
    for w in &wants {
        let Some((3, data)) = objects.get(w).map(|(k, d)| (*k, d)) else {
            bail!("the git server didn't send the file {}", hex(w));
        };
        store.put_with_id(&crate::objects::gitlink_id(w), Kind::Blob, data)?;
    }
    Ok(())
}

/// A "dumb" server (plain files over HTTP, as cgit serves): walk from `wants`,
/// downloading each object loose, or the pack holding it, until reaching what we
/// already have.
fn dumb_objects(h: &mut crate::githttp::Http, wants: &[GitId], have: impl Fn(&GitId) -> bool) -> Result<HashMap<GitId, (u8, Arc<Vec<u8>>)>> {
    let mut objects: HashMap<GitId, (u8, Arc<Vec<u8>>)> = HashMap::new();
    let mut packs: Option<Vec<(String, HashSet<GitId>)>> = None; // not downloaded yet: name, ids
    let mut queue: Vec<GitId> = wants.to_vec();
    let mut walked = HashSet::new();
    while let Some(g) = queue.pop() {
        if have(&g) || !walked.insert(g) {
            continue;
        }
        if !objects.contains_key(&g) {
            let x = hex(&g);
            if let Some(z) = h.get_maybe(&format!("objects/{}/{}", &x[..2], &x[2..]))? {
                let (kind, data) = loose_object(&z).with_context(|| format!("object {x} from the server"))?;
                if git_hash(type_name(kind)?, &data) != g {
                    bail!("object {x} from the server doesn't match its id");
                }
                objects.insert(g, (kind, Arc::new(data)));
            } else {
                // Not loose: find the pack that has it.
                if packs.is_none() {
                    let list = h.get_maybe("objects/info/packs")?.unwrap_or_default();
                    let mut found = Vec::new();
                    for line in String::from_utf8_lossy(&list).lines() {
                        if let Some(name) = line.strip_prefix("P ").map(str::trim).and_then(|n| n.strip_suffix(".pack")) {
                            let idx = h.get_maybe(&format!("objects/pack/{name}.idx"))?.with_context(|| format!("the server lists {name}.pack but has no index for it"))?;
                            found.push((name.to_string(), pack_index_ids(&idx)?));
                        }
                    }
                    packs = Some(found);
                }
                let list = packs.as_mut().unwrap();
                let at = list.iter().position(|(_, ids)| ids.contains(&g)).with_context(|| format!("the server is missing object {x}"))?;
                let (name, _) = list.remove(at);
                let pack = h.get_maybe(&format!("objects/pack/{name}.pack"))?.with_context(|| format!("the server is missing {name}.pack"))?;
                let (from_pack, _) = read_pack(&pack)?;
                objects.extend(from_pack);
            }
        }
        // What it leads to.
        let (kind, data) = objects.get(&g).map(|(k, d)| (*k, d.clone())).context("internal error: object vanished")?;
        match kind {
            1 => {
                let c = parse_commit(&data, &g)?;
                queue.push(c.tree);
                queue.extend(c.parents);
            }
            2 => queue.extend(parse_tree(&data)?.into_iter().filter(|(mode, _, _)| mode.as_slice() != b"160000").map(|(_, _, id)| id)),
            4 => queue.push(parse_tag(&data, &g)?.object),
            _ => {}
        }
    }
    // Only what the walk reached (a pack may hold more).
    objects.retain(|g, _| walked.contains(g));
    Ok(objects)
}

/// A loose git object: zlib("TYPE SIZE\0" + content).
fn loose_object(z: &[u8]) -> Result<(u8, Vec<u8>)> {
    let mut raw = Vec::new();
    flate2::read::ZlibDecoder::new(z).read_to_end(&mut raw).context("can't be decompressed")?;
    let nul = raw.iter().position(|b| *b == 0).context("malformed")?;
    let header = std::str::from_utf8(&raw[..nul]).context("malformed")?;
    let (ty, size) = header.split_once(' ').context("malformed")?;
    let kind = match ty {
        "commit" => 1,
        "tree" => 2,
        "blob" => 3,
        "tag" => 4,
        _ => bail!("unknown object type {ty}"),
    };
    let data = raw[nul + 1..].to_vec();
    if size.parse::<usize>().ok() != Some(data.len()) {
        bail!("its size doesn't match");
    }
    Ok((kind, data))
}

/// The ids in a git pack index (version 1 or 2).
fn pack_index_ids(idx: &[u8]) -> Result<HashSet<GitId>> {
    let v2 = idx.starts_with(b"\xfftOc");
    let fan = if v2 { 8 } else { 0 };
    let n = idx.get(fan + 255 * 4..fan + 256 * 4).map(|b| u32::from_be_bytes(b.try_into().unwrap()) as usize).context("damaged pack index")?;
    let start = fan + 256 * 4;
    let mut ids = HashSet::with_capacity(n);
    for i in 0..n {
        let at = if v2 { start + i * 20 } else { start + i * 24 + 4 };
        let id: GitId = idx.get(at..at + 20).context("damaged pack index")?.try_into().unwrap();
        ids.insert(id);
    }
    Ok(ids)
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
    // Bytes that aren't UTF-8 (old encodings) are kept as stand-ins, so they round-trip.
    let header = crate::objects::from_git_bytes(header);
    let message = crate::objects::from_git_bytes(message);
    let (mut tree, mut parents, mut author, mut committer) = (None, Vec::new(), None, None);
    let mut extra = Vec::new();
    let mut last_was_extra = false;
    for line in header.split('\n') {
        if line.starts_with(' ') {
            // A continuation line (e.g. inside a signature) belongs to the header above it.
            if !last_was_extra {
                bail!("commit {} has an unexpected continuation line", hex(id));
            }
            extra.extend_from_slice(&crate::objects::to_git_bytes(line));
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
                extra.extend_from_slice(&crate::objects::to_git_bytes(line));
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
    let header = crate::objects::from_git_bytes(&data[..split]);
    let message = crate::objects::from_git_bytes(data.get(split + 2..).unwrap_or_default());
    let (mut object, mut kind, mut name, mut tagger, mut extra) = (None, String::new(), String::new(), None, Vec::new());
    let mut last_was_extra = false;
    for line in header.split('\n') {
        if line.starts_with(' ') && last_was_extra {
            extra.extend_from_slice(&crate::objects::to_git_bytes(line));
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
                extra.extend_from_slice(&crate::objects::to_git_bytes(line));
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
        let name = crate::objects::from_git_bytes(&data[space + 1..nul]);
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
    /// A shallow fetch's boundary: these commits' parents weren't sent.
    shallow: &'a HashSet<GitId>,
    /// A partial clone: file contents weren't sent.
    partial: bool,
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
                match self.map.copse(&child) {
                    Some(id) => id,
                    None if self.partial => crate::objects::gitlink_id(&child), // fetched when needed
                    None => bail!("the server didn't send the contents of {name}"),
                }
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
            .map(|p| match self.map.copse(p) {
                Some(id) => Ok(id),
                // At a shallow boundary the parent stays a git id until it's fetched.
                None if self.shallow.contains(&g) => Ok(crate::objects::gitlink_id(p)),
                None => Err(anyhow!("missing parent commit {}", hex(p))),
            })
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
/// Commits in `shallow` (a shallow fetch's boundary) may name parents that weren't sent.
/// With `partial`, files whose contents weren't sent are named by their git ids.
pub fn import_pack(repo: &Repo, map: &mut GitMap, pack: &[u8], shallow: &HashSet<GitId>, partial: bool) -> Result<usize> {
    let (objects, count) = read_pack(pack)?;
    import_objects(repo, map, &objects, shallow, partial)?;
    Ok(count)
}

/// Translate git objects into copse and store them. Returns how many there were.
fn import_objects(repo: &Repo, map: &mut GitMap, objects: &HashMap<GitId, (u8, Arc<Vec<u8>>)>, shallow: &HashSet<GitId>, partial: bool) -> Result<usize> {
    let objects = objects.clone();
    let count = objects.len();
    let mut im = Importer { map, objects: &objects, out: Vec::new(), shallow, partial };

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
