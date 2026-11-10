//! Talking to another repository.
//!
//! Every remote, local folder or SSH, is reached by running `copse serve <path>`
//! (locally, or via `ssh host`) and speaking a small protocol over its stdin/stdout:
//!
//!   server: "copse 1"                          greeting (or "error <msg>")
//!   client: "refs" or "refs tags"             server: "head <b>", "ref <b> <id>"...,
//!                                             ("tag <t> <id> <commit>"... if asked), "end"
//!   client: "fetch", "want <id>"..., "have <id>"...,
//!           ["depth <n>"], ["shallow <id>"]..., "end"
//!                                             server: "shallow <id>"..., "pack", then a pack
//!   client: "push <branch> <old|none> <new>"  server: "ready" (or "error ...")
//!   client: <pack>                            server: "ok"    (or "error ...")
//!   client: "pushtag <tag> <old|none> <new>"  same as push, for a tag
//!   client: "delete <branch> <old>"           server: "ok"    (or "error ...")
//!   client: "deletetag <tag> <old>"           same, for a tag
//!   client: "filter blob:none" (in a fetch)   leave out file contents (partial clones)
//!   client: "get", "want <id>"..., "end"      server: "pack", then exactly those objects
//!   client: "shallow <id>"                    (before a push from a shallow clone; no reply)
//!                                             the receiver records it as a boundary if
//!                                             it doesn't have that commit's parents
//!
//! `depth n` asks for only the newest n commits of history; the server answers with
//! the commits whose parents it left out ("shallow"). A shallow client sends its own
//! boundary commits as "shallow" lines, so the server knows it lacks what's below.
//!   client: "quit"
//!
//! A pack is a run of frames `id (32 bytes) | length (u32 LE) | raw object`,
//! ended by 32 zero bytes. Raw objects are sent exactly as stored (already
//! compressed) and the receiver re-hashes every one before keeping it.

use crate::objects::{decode_tree, parse_hex, Id, Kind, Mode, Store};
use crate::pack::PackWriter;
use crate::repo::{validate_branch, Head, Repo};
use anyhow::{bail, Context, Result};
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

const GREETING: &str = "copse 1";
const BATCH_BYTES: usize = 64 << 20;
/// Transfers with fewer objects than this are stored loose; larger ones as one pack.
const LOOSE_LIMIT: usize = 500;

// ---------------------------------------------------------------- what to send

/// Every object the receiver needs to have `tips`, given that it already has the
/// commits in `haves` (and therefore everything they reach).
pub fn objects_to_send(repo: &Repo, tips: &[Id], haves: &[Id]) -> Result<Vec<Id>> {
    Ok(objects_to_send_shallow(repo, tips, haves, &HashSet::new(), None, false)?.0)
}

/// Follow annotated tags down to what they name. Returns the tag objects passed
/// through (which need sending too) and the final object.
fn peel_tags(repo: &Repo, mut id: Id) -> Result<(Vec<Id>, Id, Kind)> {
    let mut tags = Vec::new();
    for _ in 0..16 {
        let (kind, data) = repo.store.get(&id)?;
        if kind != Kind::Tag {
            return Ok((tags, id, kind));
        }
        tags.push(id);
        id = crate::objects::Tag::decode(&data)?.object;
    }
    bail!("tags nested too deeply at {}", crate::objects::short(&id))
}

/// Like [`objects_to_send`], for clients that are (or want to be) shallow: walking
/// stops at the client's own boundary commits (`client_shallow`), and with `depth`
/// only the newest `depth` commits from each tip are sent. Returns the objects and
/// the commits sent without their parents (the client's new boundary).
pub fn objects_to_send_shallow(
    repo: &Repo,
    tips: &[Id],
    haves: &[Id],
    client_shallow: &HashSet<Id>,
    depth: Option<usize>,
    skip_blobs: bool,
) -> Result<(Vec<Id>, Vec<Id>)> {
    let mut walk = Walk { sent: HashSet::new(), skip_blobs, compared: HashSet::new(), out: Vec::new() };
    // Annotated tags among the tips: send the tag objects, then walk from what they name.
    let mut commit_tips = Vec::new();
    for tip in tips {
        let (tag_objects, target, kind) = peel_tags(repo, *tip)?;
        for t in tag_objects {
            if walk.sent.insert(t) {
                walk.out.push(t);
            }
        }
        match kind {
            Kind::Commit => commit_tips.push(target),
            Kind::Tree => tree_delta(&repo.store, target, None, &mut walk)?,
            _ => {
                if walk.sent.insert(target) {
                    walk.out.push(target);
                }
            }
        }
    }

    let mut known = HashSet::new();
    let mut stack: Vec<Id> = Vec::new();
    for h in haves.iter().filter(|h| repo.store.contains(h)) {
        if let Ok((_, target, Kind::Commit)) = peel_tags(repo, *h) {
            stack.push(target);
        }
    }
    while let Some(id) = stack.pop() {
        if known.insert(id) && !client_shallow.contains(&id) {
            stack.extend(repo.read_commit(&id)?.parents);
        }
    }

    // New commits, breadth first so each is reached by its shortest path (for depth).
    let ours_shallow = repo.shallow();
    let mut commits = Vec::new();
    let mut boundary = Vec::new();
    let mut seen = HashSet::new();
    let mut queue: std::collections::VecDeque<(Id, usize)> = commit_tips.iter().map(|t| (*t, 1)).collect();
    while let Some((id, level)) = queue.pop_front() {
        if known.contains(&id) || !seen.insert(id) {
            continue;
        }
        let commit = repo.read_commit(&id)?;
        let cut = depth.is_some_and(|d| level >= d) && !commit.parents.is_empty();
        // Our own boundary (if we're shallow too) passes on to the client.
        if cut || ours_shallow.contains(&id) {
            boundary.push(id);
        }
        if !cut {
            queue.extend(commit.parents.iter().map(|p| (*p, level + 1)));
        }
        commits.push((id, commit, cut));
    }

    // For each new commit, send only the trees and blobs that differ from its first
    // parent. The parent is either already on the other side, or a new commit whose
    // own differences are sent too, so by induction every commit arrives complete.
    // A commit whose parents are cut off is sent with its whole tree.
    for (id, commit, cut) in &commits {
        if walk.sent.insert(*id) {
            walk.out.push(*id);
        }
        let parent_tree = match commit.parents.first() {
            Some(p) if !cut => Some(repo.read_commit(p)?.tree),
            _ => None,
        };
        tree_delta(&repo.store, commit.tree, parent_tree, &mut walk)?;
    }
    Ok((walk.out, boundary))
}

struct Walk {
    /// Objects already queued, so nothing is sent twice.
    sent: HashSet<Id>,
    /// Partial clones: send folders but not file contents.
    skip_blobs: bool,
    /// (tree, compared-against) pairs already done. This has to be the pair, not
    /// just the tree: a tree first compared against a newer version (say, after a
    /// file was reverted) still needs comparing against its own parent's version.
    compared: HashSet<(Id, Option<Id>)>,
    out: Vec<Id>,
}

fn tree_delta(store: &Store, tree: Id, old: Option<Id>, walk: &mut Walk) -> Result<()> {
    if old == Some(tree) || !walk.compared.insert((tree, old)) {
        return Ok(());
    }
    if walk.sent.insert(tree) {
        walk.out.push(tree);
    }
    let old_entries: HashMap<String, (Mode, Id)> = match old {
        Some(o) => decode_tree(&store.get_kind(&o, Kind::Tree)?)?.into_iter().map(|e| (e.name, (e.mode, e.id))).collect(),
        None => HashMap::new(),
    };
    for e in decode_tree(&store.get_kind(&tree, Kind::Tree)?)? {
        let prev = old_entries.get(&e.name).copied();
        if prev.map(|(_, id)| id) == Some(e.id) {
            continue; // unchanged file or whole unchanged folder
        }
        if e.mode == Mode::Dir {
            let old_sub = prev.filter(|(m, _)| *m == Mode::Dir).map(|(_, id)| id);
            tree_delta(store, e.id, old_sub, walk)?;
        } else if e.mode == Mode::Submodule || walk.skip_blobs {
            // Another repository's commit, or a file a partial clone fetches later.
        } else if walk.sent.insert(e.id) {
            walk.out.push(e.id);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- packs

/// Write a pack. Returns (objects, bytes).
pub fn write_pack(w: &mut impl Write, store: &Store, ids: &[Id]) -> Result<(usize, u64)> {
    let mut bytes = 0u64;
    for chunk in ids.chunks(512) {
        let raws: Vec<Vec<u8>> = chunk.par_iter().map(|id| store.read_raw(id)).collect::<Result<_>>()?;
        for (id, raw) in chunk.iter().zip(raws) {
            w.write_all(id.as_bytes())?;
            w.write_all(&(raw.len() as u32).to_le_bytes())?;
            w.write_all(&raw)?;
            bytes += raw.len() as u64;
        }
    }
    w.write_all(&[0u8; 32])?;
    w.flush()?;
    Ok((ids.len(), bytes))
}

/// Read a pack into `store`, verifying every object.
///
/// Small transfers become loose objects, with commits written last. Larger ones are
/// streamed into a single pack file, which becomes visible only once it's complete.
/// Refs are updated separately, only after their reachable objects are validated.
pub fn read_pack(r: &mut impl Read, store: &Store) -> Result<(usize, u64)> {
    let (mut count, mut bytes) = (0usize, 0u64);
    let mut batch: Vec<(Id, Vec<u8>)> = Vec::new();
    let mut batch_bytes = 0;
    let mut writer: Option<PackWriter> = None;
    loop {
        let mut id = [0u8; 32];
        r.read_exact(&mut id).context("the transfer was cut off")?;
        if id == [0u8; 32] {
            break;
        }
        let mut len = [0u8; 4];
        r.read_exact(&mut len).context("the transfer was cut off")?;
        let mut raw = vec![0u8; u32::from_le_bytes(len) as usize];
        r.read_exact(&mut raw).context("the transfer was cut off")?;
        count += 1;
        bytes += raw.len() as u64;
        batch_bytes += raw.len();
        batch.push((Id::from_bytes(id), raw));
        if writer.is_none() && count >= LOOSE_LIMIT {
            writer = Some(store.new_pack_writer(0)?);
        }
        if let Some(w) = writer.as_mut() {
            if batch_bytes > BATCH_BYTES {
                add_verified(w, &mut batch)?;
                batch_bytes = 0;
            }
        }
    }
    match writer {
        Some(mut w) => {
            add_verified(&mut w, &mut batch)?;
            w.finish()?;
            store.reload_packs();
        }
        None => {
            let (commits, rest): (Vec<_>, Vec<_>) =
                batch.into_iter().partition(|(_, raw)| raw.first() == Some(&(Kind::Commit as u8)));
            rest.par_iter().try_for_each(|(id, raw)| store.write_raw(id, raw))?;
            commits.par_iter().try_for_each(|(id, raw)| store.write_raw(id, raw))?;
        }
    }
    Ok((count, bytes))
}

/// Verify a batch in parallel, then append it to the pack being written.
fn add_verified(w: &mut PackWriter, batch: &mut Vec<(Id, Vec<u8>)>) -> Result<()> {
    batch.par_iter().try_for_each(|(id, raw)| Store::verify_raw(id, raw))?;
    for (id, raw) in batch.drain(..) {
        w.add(&id, &raw)?;
    }
    Ok(())
}

// ---------------------------------------------------------------- urls

pub enum Url {
    Local(PathBuf),
    Ssh { host: String, path: String },
}

/// `ssh://host/path`, `user@host:path` (scp style), or a local path.
pub fn parse_url(url: &str) -> Url {
    if let Some(rest) = url.strip_prefix("ssh://") {
        let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
        let path = if path.starts_with('~') { path.to_string() } else { format!("/{path}") };
        return Url::Ssh { host: host.to_string(), path };
    }
    if let Some((host, path)) = url.split_once(':') {
        let looks_local = url.starts_with('/') || url.starts_with('.') || url.starts_with('~') || host.contains('/');
        if !looks_local && !host.is_empty() {
            return Url::Ssh { host: host.to_string(), path: path.to_string() };
        }
    }
    let path = match url.strip_prefix("~/") {
        Some(rest) => PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(rest),
        None => PathBuf::from(url),
    };
    Url::Local(path)
}

/// Make local paths absolute so the remote keeps working from any directory.
pub fn normalize_url(url: &str) -> Result<String> {
    Ok(match parse_url(url) {
        Url::Ssh { .. } => url.to_string(),
        Url::Local(p) => {
            let abs = if p.is_absolute() { p } else { std::env::current_dir()?.join(p) };
            // Tidy `a/../b` even when the folder doesn't exist yet.
            let mut clean = PathBuf::new();
            for part in abs.components() {
                match part {
                    std::path::Component::ParentDir => {
                        clean.pop();
                    }
                    std::path::Component::CurDir => {}
                    other => clean.push(other),
                }
            }
            clean.canonicalize().unwrap_or(clean).to_string_lossy().into_owned()
        }
    })
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

fn remote_path_arg(path: &str) -> String {
    // Leave a leading ~/ unquoted so the remote shell expands it.
    match path.strip_prefix("~/") {
        Some(rest) => format!("~/{}", shell_quote(rest)),
        None if path == "~" => "~".into(),
        None => shell_quote(path),
    }
}

// ---------------------------------------------------------------- client

pub struct RemoteRefs {
    pub head: Option<String>,
    pub branches: Vec<(String, Id)>,
    pub tags: Vec<(String, Id)>,
    /// For each tag (same order), the commit it finally names.
    pub tag_targets: Vec<Option<Id>>,
}

pub struct Conn {
    child: Child,
    // Options only so Drop can close them before waiting for the child.
    r: Option<BufReader<ChildStdout>>,
    w: Option<BufWriter<ChildStdin>>,
}

impl Conn {
    pub fn open(url: &str) -> Result<Conn> {
        let mut cmd = match parse_url(url) {
            Url::Ssh { host, path } => {
                // A "host" starting with - would be read by ssh as an option
                // (-oProxyCommand=... runs a command): never pass one on.
                if host.starts_with('-') {
                    bail!("refusing the address {url:?}: its host starts with '-', which ssh would read as an option");
                }
                let ssh = std::env::var("COPSE_SSH").unwrap_or_else(|_| "ssh".into());
                let copse = std::env::var("COPSE_REMOTE_CMD").unwrap_or_else(|_| "copse".into());
                let mut c = Command::new(ssh);
                c.arg(host).arg(format!("{copse} serve {}", remote_path_arg(&path)));
                c
            }
            Url::Local(path) => {
                let mut c = Command::new(std::env::current_exe()?);
                c.arg("serve").arg(path);
                c
            }
        };
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("connecting to {url}"))?;
        let r = BufReader::new(child.stdout.take().unwrap());
        let w = BufWriter::new(child.stdin.take().unwrap());
        let mut conn = Conn { child, r: Some(r), w: Some(w) };
        let hello = conn.line().with_context(|| format!("connecting to {url}"))?;
        if hello != GREETING {
            bail!("{url} didn't answer like a copse repository (got {hello:?})");
        }
        Ok(conn)
    }

    fn send(&mut self, line: &str) -> Result<()> {
        self.w().write_all(line.as_bytes())?;
        self.w().write_all(b"\n")?;
        Ok(())
    }

    fn line(&mut self) -> Result<String> {
        let mut s = String::new();
        if self.r().read_line(&mut s)? == 0 {
            bail!("the remote hung up");
        }
        let s = s.trim_end_matches('\n');
        if let Some(e) = s.strip_prefix("error ") {
            bail!("remote: {e}");
        }
        Ok(s.to_string())
    }

    fn expect(&mut self, want: &str) -> Result<()> {
        let got = self.line()?;
        if got != want {
            bail!("unexpected reply from the remote: {got:?}");
        }
        Ok(())
    }

    pub fn refs(&mut self) -> Result<RemoteRefs> {
        self.send("refs tags")?;
        self.w().flush()?;
        let mut refs = RemoteRefs { head: None, branches: Vec::new(), tags: Vec::new(), tag_targets: Vec::new() };
        loop {
            let line = self.line()?;
            let mut parts = line.splitn(3, ' ');
            match (parts.next(), parts.next(), parts.next()) {
                (Some("end"), None, None) => return Ok(refs),
                (Some("head"), Some(b), None) => refs.head = Some(b.to_string()),
                (Some("ref"), Some(b), Some(id)) => refs.branches.push((b.to_string(), parse_hex(id)?)),
                (Some("tag"), Some(t), Some(rest)) => {
                    let (id, target) = rest.split_once(' ').unwrap_or((rest, ""));
                    refs.tags.push((t.to_string(), parse_hex(id)?));
                    refs.tag_targets.push(parse_hex(target).ok());
                }
                _ => bail!("unexpected reply from the remote: {line:?}"),
            }
        }
    }

    /// Fetch `wants`. With `depth`, only that many commits of history; `shallow`
    /// lists our own boundary commits. Returns the transfer size and the commits
    /// that arrived without their parents.
    pub fn fetch(&mut self, store: &Store, wants: &[Id], haves: &[Id], shallow: &[Id], depth: Option<usize>) -> Result<(usize, u64, Vec<Id>)> {
        self.fetch_filtered(store, wants, haves, shallow, depth, false)
    }

    /// [`Conn::fetch`], optionally leaving out file contents (partial clones).
    pub fn fetch_filtered(
        &mut self,
        store: &Store,
        wants: &[Id],
        haves: &[Id],
        shallow: &[Id],
        depth: Option<usize>,
        no_blobs: bool,
    ) -> Result<(usize, u64, Vec<Id>)> {
        self.send("fetch")?;
        if no_blobs {
            self.send("filter blob:none")?;
        }
        for id in wants {
            self.send(&format!("want {}", id.to_hex()))?;
        }
        for id in haves {
            self.send(&format!("have {}", id.to_hex()))?;
        }
        for id in shallow {
            self.send(&format!("shallow {}", id.to_hex()))?;
        }
        if let Some(d) = depth {
            self.send(&format!("depth {d}"))?;
        }
        self.send("end")?;
        self.w().flush()?;
        let mut boundary = Vec::new();
        loop {
            let line = self.line()?;
            if line == "pack" {
                break;
            }
            match line.strip_prefix("shallow ") {
                Some(id) => boundary.push(parse_hex(id)?),
                None => bail!("unexpected reply from the remote: {line:?}"),
            }
        }
        let (n, bytes) = read_pack(self.r(), store)?;
        Ok((n, bytes, boundary))
    }

    /// Fetch exactly these objects (a partial clone filling in file contents).
    pub fn get_objects(&mut self, store: &Store, ids: &[Id]) -> Result<(usize, u64)> {
        self.send("get")?;
        for id in ids {
            self.send(&format!("want {}", id.to_hex()))?;
        }
        self.send("end")?;
        self.w().flush()?;
        self.expect("pack")?;
        read_pack(self.r(), store)
    }

    /// Before pushing from a shallow clone: tell the remote where our history stops.
    pub fn announce_shallow(&mut self, ids: &[Id]) -> Result<()> {
        for id in ids {
            self.send(&format!("shallow {}", id.to_hex()))?;
        }
        Ok(())
    }

    /// Set or delete a tag on the remote, if it still has the value we saw.
    pub fn push_tag(&mut self, store: &Store, tag: &str, old: Option<Id>, new: Option<Id>, objects: &[Id]) -> Result<(usize, u64)> {
        let old_s = old.map_or_else(|| "none".to_string(), |id| id.to_hex().to_string());
        match new {
            None => {
                self.send(&format!("deletetag {tag} {old_s}"))?;
                self.w().flush()?;
                self.expect("ok")?;
                Ok((0, 0))
            }
            Some(new) => {
                self.send(&format!("pushtag {tag} {old_s} {}", new.to_hex()))?;
                self.w().flush()?;
                self.expect("ready")?;
                let stats = write_pack(self.w(), store, objects)?;
                self.expect("ok")?;
                Ok(stats)
            }
        }
    }

    /// Delete `branch` on the remote, if it still points at `old`.
    pub fn delete(&mut self, branch: &str, old: Id) -> Result<()> {
        self.send(&format!("delete {branch} {}", old.to_hex()))?;
        self.w().flush()?;
        self.expect("ok")
    }

    pub fn push(&mut self, store: &Store, branch: &str, old: Option<Id>, new: Id, objects: &[Id]) -> Result<(usize, u64)> {
        let old = old.map_or_else(|| "none".to_string(), |id| id.to_hex().to_string());
        self.send(&format!("push {branch} {old} {}", new.to_hex()))?;
        self.w().flush()?;
        self.expect("ready")?;
        let stats = write_pack(self.w(), store, objects)?;
        self.expect("ok")?;
        Ok(stats)
    }
}

impl Conn {
    fn r(&mut self) -> &mut BufReader<ChildStdout> {
        self.r.as_mut().expect("connection already closed")
    }

    fn w(&mut self) -> &mut BufWriter<ChildStdin> {
        self.w.as_mut().expect("connection already closed")
    }
}

impl Drop for Conn {
    fn drop(&mut self) {
        // Say goodbye, then close both pipes *before* waiting. If we're bailing out
        // mid-transfer (say, a corrupt object), the other side may still be writing to
        // us; closing our end makes its writes fail so it exits, instead of both sides
        // waiting on each other forever.
        if let Some(mut w) = self.w.take() {
            let _ = w.write_all(b"quit\n");
            let _ = w.flush();
        }
        self.r = None;
        let _ = self.child.wait();
    }
}

// ---------------------------------------------------------------- server

/// `copse serve <path>`: answer one client on stdin/stdout.
pub fn serve(path: &Path) -> Result<()> {
    crate::reflog::set_reason("push received");
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut r = BufReader::new(stdin.lock());
    let mut w = BufWriter::new(stdout.lock());
    let repo = match Repo::open_path(path) {
        Ok(repo) => repo,
        Err(e) => {
            writeln!(w, "error {e:#}")?;
            w.flush()?;
            return Ok(());
        }
    };
    writeln!(w, "{GREETING}")?;
    w.flush()?;
    let mut announced = Vec::new();
    loop {
        let mut line = String::new();
        if r.read_line(&mut line)? == 0 {
            return Ok(());
        }
        if let Some(id) = line.trim_end().strip_prefix("shallow ") {
            announced.push(parse_hex(id)?);
            continue;
        }
        match serve_one(&repo, line.trim_end(), &mut r, &mut w, &announced) {
            Ok(true) => {}
            Ok(false) => return Ok(()),
            Err(e) => {
                // Also say it on stderr, which reaches the user's terminal: if we fail
                // mid-transfer, the client is reading raw data and can't see this line.
                eprintln!("copse (remote): {e:#}");
                let _ = writeln!(w, "error {}", format!("{e:#}").replace('\n', " "));
                let _ = w.flush();
                return Ok(());
            }
        }
    }
}

/// Validate the complete graph before publishing a pushed ref. Only explicitly
/// shallow commits may omit parents; their trees and blobs must still be present.
/// Submodule ids belong to another repository and are not local objects.
/// Return the proposed shallow set without changing repository metadata on error.
fn validate_push(repo: &Repo, tip: Id, want: Option<Kind>, announced: &[Id]) -> Result<HashSet<Id>> {
    let mut shallow = repo.shallow();
    let announced: HashSet<Id> = announced.iter().copied().collect();
    let mut checked: HashMap<Id, Kind> = HashMap::new();
    let mut stack = vec![(tip, want)];
    while let Some((id, want)) = stack.pop() {
        let kind = if let Some(kind) = checked.get(&id) {
            *kind
        } else {
            if !repo.store.contains(&id) {
                bail!("pushed history is missing object {}", id.to_hex());
            }
            repo.store.verify(&id).with_context(|| format!("checking pushed object {}", id.to_hex()))?
        };
        // Check the kind on every edge, even if this object was already visited
        // under a different role (for example, both a tree and a file).
        if want.is_some_and(|want| want != kind) {
            bail!("pushed object {} is a {kind:?}, expected a {:?}", id.to_hex(), want.unwrap());
        }
        if checked.insert(id, kind).is_some() {
            continue;
        }
        match kind {
            Kind::Commit => {
                let c = repo.read_commit_raw(&id).with_context(|| format!("reading pushed commit {}", id.to_hex()))?;
                if announced.contains(&id) && c.parents.iter().any(|p| !repo.store.contains(p)) {
                    shallow.insert(id);
                }
                stack.push((c.tree, Some(Kind::Tree)));
                if !shallow.contains(&id) {
                    stack.extend(c.parents.into_iter().map(|p| (p, Some(Kind::Commit))));
                }
            }
            Kind::Tree => {
                for entry in decode_tree(&repo.store.get_kind(&id, Kind::Tree)?)? {
                    match entry.mode {
                        Mode::Submodule => {}
                        Mode::Dir => stack.push((entry.id, Some(Kind::Tree))),
                        _ => stack.push((entry.id, Some(Kind::Blob))),
                    }
                }
            }
            Kind::Tag => {
                let tag = crate::objects::Tag::decode(&repo.store.get_kind(&id, Kind::Tag)?)?;
                stack.push((tag.object, Some(tag.target_kind)));
            }
            Kind::Blob => {}
        }
    }
    Ok(shallow)
}

fn serve_one(repo: &Repo, line: &str, r: &mut impl BufRead, w: &mut impl Write, announced: &[Id]) -> Result<bool> {
    let mut parts = line.split(' ');
    match parts.next().unwrap_or("") {
        "quit" => return Ok(false),
        "refs" => {
            if let Head::Branch(b) = repo.head()? {
                writeln!(w, "head {b}")?;
            }
            for (name, id) in repo.branches()? {
                writeln!(w, "ref {name} {}", id.to_hex())?;
            }
            // Only clients that ask know what a tag line is.
            if parts.next() == Some("tags") {
                for (name, id) in repo.tags()? {
                    let target = repo.peel(id).map_or_else(|_| "-".to_string(), |c| c.to_hex().to_string());
                    writeln!(w, "tag {name} {} {target}", id.to_hex())?;
                }
            }
            writeln!(w, "end")?;
        }
        "fetch" => {
            let (mut wants, mut haves, mut shallow, mut depth, mut no_blobs) = (Vec::new(), Vec::new(), HashSet::new(), None, false);
            loop {
                let mut l = String::new();
                if r.read_line(&mut l)? == 0 {
                    bail!("the client hung up");
                }
                match l.trim_end().split_once(' ') {
                    None if l.trim_end() == "end" => break,
                    Some(("filter", "blob:none")) => no_blobs = true,
                    Some(("filter", f)) => bail!("unsupported filter {f:?} (only blob:none)"),
                    Some(("want", id)) => wants.push(parse_hex(id)?),
                    Some(("have", id)) => haves.push(parse_hex(id)?),
                    Some(("shallow", id)) => {
                        shallow.insert(parse_hex(id)?);
                    }
                    Some(("depth", n)) => depth = Some(n.parse::<usize>().ok().filter(|n| *n > 0).context("depth must be a positive number")?),
                    _ => bail!("bad fetch request: {l:?}"),
                }
            }
            let (objects, boundary) = objects_to_send_shallow(repo, &wants, &haves, &shallow, depth, no_blobs)?;
            for id in boundary {
                writeln!(w, "shallow {}", id.to_hex())?;
            }
            writeln!(w, "pack")?;
            write_pack(w, &repo.store, &objects)?;
        }
        "get" => {
            let mut ids = Vec::new();
            loop {
                let mut l = String::new();
                if r.read_line(&mut l)? == 0 {
                    bail!("the client hung up");
                }
                match l.trim_end().split_once(' ') {
                    None if l.trim_end() == "end" => break,
                    Some(("want", id)) => ids.push(parse_hex(id)?),
                    _ => bail!("bad get request: {l:?}"),
                }
            }
            for id in &ids {
                if !repo.store.contains(id) {
                    bail!("this repository doesn't have object {}", crate::objects::short(id));
                }
            }
            writeln!(w, "pack")?;
            write_pack(w, &repo.store, &ids)?;
        }
        "pushtag" | "deletetag" => {
            let deleting = line.starts_with("deletetag");
            let (Some(tag), Some(old)) = (parts.next(), parts.next()) else {
                bail!("bad tag request");
            };
            validate_branch(tag).with_context(|| format!("invalid tag name {tag:?}"))?;
            let old = if old == "none" { None } else { Some(parse_hex(old)?) };
            if repo.tag(tag)? != old {
                bail!("tag {tag} has changed there since you looked");
            }
            if deleting {
                repo.delete_tag(tag)?;
            } else {
                let new = parse_hex(parts.next().context("bad tag request")?)?;
                writeln!(w, "ready")?;
                w.flush()?;
                read_pack(r, &repo.store)?;
                let shallow = validate_push(repo, new, None, announced)?;
                if shallow != repo.shallow() {
                    repo.set_shallow(&shallow)?;
                }
                repo.set_tag(tag, &new)?;
            }
            writeln!(w, "ok")?;
        }
        "delete" => {
            let (Some(branch), Some(old)) = (parts.next(), parts.next()) else {
                bail!("bad delete request");
            };
            validate_branch(branch)?;
            if !repo.is_bare() {
                if let Head::Branch(current) = repo.head()? {
                    if current == branch {
                        bail!("{branch} is checked out in that repository's working directory, so it can't be deleted");
                    }
                }
            }
            repo.cas_delete_branch(branch, &parse_hex(old)?)?;
            writeln!(w, "ok")?;
        }
        "push" => {
            let (Some(branch), Some(old), Some(new)) = (parts.next(), parts.next(), parts.next()) else {
                bail!("bad push request");
            };
            validate_branch(branch)?;
            let old = if old == "none" { None } else { Some(parse_hex(old)?) };
            let new = parse_hex(new)?;
            if !repo.is_bare() {
                if let Head::Branch(current) = repo.head()? {
                    if current == branch {
                        bail!(
                            "{branch} is checked out in that repository's working directory, so it can't be pushed to. \
                             Push to a bare repository (`copse init --bare`), or switch that repository to another branch."
                        );
                    }
                }
            }
            if repo.branch(branch)? != old {
                bail!("{branch} has changed there since you looked; pull first");
            }
            writeln!(w, "ready")?;
            w.flush()?;
            read_pack(r, &repo.store)?;
            let shallow = validate_push(repo, new, Some(Kind::Commit), announced)?;
            if shallow != repo.shallow() {
                repo.set_shallow(&shallow)?;
            }
            repo.cas_branch(branch, old, &new)?;
            writeln!(w, "ok")?;
        }
        other => bail!("unknown request {other:?}"),
    }
    w.flush()?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::{read_pack, serve_one};
    use crate::objects::{encode_tree, full_entry, Commit, Id, Kind, Mode, Store, Tag, TreeEntry};
    use crate::repo::Repo;

    fn push_repo(name: &str) -> (std::path::PathBuf, Repo, Id) {
        let dir = std::env::temp_dir().join(format!("copse-push-{name}-{}", std::process::id()));
        let repo = Repo::init_bare(&dir).unwrap();
        let tree = repo.store.put(Kind::Tree, &encode_tree(&[])).unwrap();
        let old = repo.store.put(Kind::Commit, &Commit::new(tree, vec![], "existing".into()).encode()).unwrap();
        repo.cas_branch("main", None, &old).unwrap();
        (dir, repo, old)
    }

    #[test]
    fn incomplete_native_pushes_leave_refs_and_boundaries_unchanged() {
        let blob = b"contents".to_vec();
        let blob_id = Store::hash(Kind::Blob, &blob);
        let tree = encode_tree(&[TreeEntry { name: "file".into(), mode: Mode::File, id: blob_id }]);
        let tree_id = Store::hash(Kind::Tree, &tree);
        let missing = Store::hash(Kind::Blob, b"not sent");
        let commit = |tree, parents| Commit::new(tree, parents, "incoming".into()).encode();
        let parent = commit(missing, vec![]);
        let parent_id = Store::hash(Kind::Commit, &parent);
        let subtree = encode_tree(&[TreeEntry { name: "folder".into(), mode: Mode::Dir, id: tree_id }]);
        let subtree_id = Store::hash(Kind::Tree, &subtree);
        let wrong_type = encode_tree(&[
            TreeEntry { name: "a-file".into(), mode: Mode::File, id: tree_id },
            TreeEntry { name: "z-folder".into(), mode: Mode::Dir, id: tree_id },
        ]);
        let wrong_type_id = Store::hash(Kind::Tree, &wrong_type);
        let cases = [
            ("missing-tree", vec![(Kind::Commit, commit(tree_id, vec![]))]),
            ("missing-blob", vec![(Kind::Tree, tree.clone()), (Kind::Commit, commit(tree_id, vec![]))]),
            ("missing-parent", vec![(Kind::Blob, blob.clone()), (Kind::Tree, tree.clone()), (Kind::Commit, commit(tree_id, vec![missing]))]),
            ("parent-tree", vec![(Kind::Blob, blob.clone()), (Kind::Tree, tree.clone()), (Kind::Commit, parent), (Kind::Commit, commit(tree_id, vec![parent_id]))]),
            ("nested-tree", vec![(Kind::Tree, subtree), (Kind::Commit, commit(subtree_id, vec![]))]),
            ("wrong-kind", vec![(Kind::Blob, blob.clone()), (Kind::Tree, tree.clone()), (Kind::Tree, wrong_type), (Kind::Commit, commit(wrong_type_id, vec![]))]),
            ("blob-as-tree", vec![(Kind::Blob, blob.clone()), (Kind::Commit, commit(blob_id, vec![]))]),
            ("blob-as-parent", vec![(Kind::Blob, blob.clone()), (Kind::Tree, tree.clone()), (Kind::Commit, commit(tree_id, vec![blob_id]))]),
            ("malformed-parent", vec![(Kind::Blob, blob.clone()), (Kind::Tree, tree.clone()), (Kind::Commit, b"invalid".to_vec()), (Kind::Commit, commit(tree_id, vec![Store::hash(Kind::Commit, b"invalid")]))]),
        ];
        for (name, objects) in cases {
            let (dir, repo, old) = push_repo(name);
            let (bytes, ids) = transfer(&objects);
            let tip = *ids.last().unwrap();
            let before_log = std::fs::read(repo.dir.join("logs/refs/main")).unwrap();
            let result = serve_one(&repo, &format!("push main {} {}", old.to_hex(), tip.to_hex()), &mut bytes.as_slice(), &mut Vec::new(), &[]);
            assert!(result.is_err(), "{name}: incomplete push was accepted");
            assert_eq!(repo.branch("main").unwrap(), Some(old), "{name}: ref moved");
            assert_eq!(std::fs::read(repo.dir.join("logs/refs/main")).unwrap(), before_log, "{name}: reflog changed");
            assert!(!repo.dir.join("shallow").exists(), "{name}: shallow boundary changed");
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn shallow_native_push_still_requires_a_complete_tree() {
        let (dir, repo, old) = push_repo("shallow-incomplete");
        let missing = Store::hash(Kind::Blob, b"missing");
        let tree = encode_tree(&[TreeEntry { name: "file".into(), mode: Mode::File, id: missing }]);
        let tree_id = Store::hash(Kind::Tree, &tree);
        let commit = Commit::new(tree_id, vec![missing], "boundary".into()).encode();
        let (bytes, ids) = transfer(&[(Kind::Tree, tree), (Kind::Commit, commit)]);
        let tip = ids[1];
        assert!(serve_one(&repo, &format!("push main {} {}", old.to_hex(), tip.to_hex()), &mut bytes.as_slice(), &mut Vec::new(), &[tip]).is_err());
        assert_eq!(repo.branch("main").unwrap(), Some(old));
        assert!(repo.shallow().is_empty());
        assert!(!repo.dir.join("shallow").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn native_tags_require_their_targets_complete() {
        let (dir, repo, _) = push_repo("tag-incomplete");
        let missing = Store::hash(Kind::Blob, b"missing");
        let tag = Tag::new(missing, Kind::Commit, "v1", "incomplete".into()).encode();
        let (bytes, ids) = transfer(&[(Kind::Tag, tag)]);
        assert!(serve_one(&repo, &format!("pushtag v1 none {}", ids[0].to_hex()), &mut bytes.as_slice(), &mut Vec::new(), &[]).is_err());
        assert_eq!(repo.tag("v1").unwrap(), None);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn complete_native_pushes_allow_existing_objects_submodules_and_nested_tags() {
        let (dir, repo, old) = push_repo("complete");
        let blob = repo.store.put(Kind::Blob, b"already here").unwrap();
        let tree = encode_tree(&[
            TreeEntry { name: "file".into(), mode: Mode::File, id: blob },
            TreeEntry { name: "module".into(), mode: Mode::Submodule, id: crate::objects::gitlink_id(&[7; 20]) },
        ]);
        let tree_id = Store::hash(Kind::Tree, &tree);
        let commit = Commit::new(tree_id, vec![old], "complete".into()).encode();
        let (bytes, ids) = transfer(&[(Kind::Tree, tree), (Kind::Commit, commit)]);
        let tip = ids[1];
        let mut reply = Vec::new();
        serve_one(&repo, &format!("push main {} {}", old.to_hex(), tip.to_hex()), &mut bytes.as_slice(), &mut reply, &[]).unwrap();
        assert_eq!(reply, b"ready\nok\n");
        assert_eq!(repo.branch("main").unwrap(), Some(tip));

        let inner = Tag::new(tip, Kind::Commit, "inner", "inner".into()).encode();
        let inner_id = Store::hash(Kind::Tag, &inner);
        let outer = Tag::new(inner_id, Kind::Tag, "outer", "outer".into()).encode();
        let (bytes, ids) = transfer(&[(Kind::Tag, inner), (Kind::Tag, outer)]);
        serve_one(&repo, &format!("pushtag outer none {}", ids[1].to_hex()), &mut bytes.as_slice(), &mut Vec::new(), &[]).unwrap();
        assert_eq!(repo.tag("outer").unwrap(), Some(ids[1]));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn complete_shallow_native_push_records_only_reachable_boundaries() {
        let (dir, repo, old) = push_repo("shallow-complete");
        let tree = repo.read_commit_raw(&old).unwrap().tree;
        let missing = Store::hash(Kind::Commit, b"omitted parent");
        let commit = Commit::new(tree, vec![missing], "shallow".into()).encode();
        let unrelated = Commit::new(tree, vec![missing], "unrelated".into()).encode();
        let (bytes, ids) = transfer(&[(Kind::Commit, commit), (Kind::Commit, unrelated)]);
        serve_one(&repo, &format!("push main {} {}", old.to_hex(), ids[0].to_hex()), &mut bytes.as_slice(), &mut Vec::new(), &ids).unwrap();
        assert_eq!(repo.branch("main").unwrap(), Some(ids[0]));
        assert_eq!(repo.shallow(), std::collections::HashSet::from([ids[0]]));
        assert_eq!(Repo::open_path(&dir).unwrap().shallow(), repo.shallow());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn native_push_rejects_corrupt_objects_already_in_the_receiver() {
        let (dir, repo, old) = push_repo("corrupt-existing");
        let blob = repo.store.put(Kind::Blob, b"original").unwrap();
        std::fs::write(repo.store.loose_path(&blob), full_entry(Kind::Blob, b"corrupt").unwrap()).unwrap();
        let tree = encode_tree(&[TreeEntry { name: "file".into(), mode: Mode::File, id: blob }]);
        let commit = Commit::new(Store::hash(Kind::Tree, &tree), vec![old], "bad existing blob".into()).encode();
        let (bytes, ids) = transfer(&[(Kind::Tree, tree), (Kind::Commit, commit)]);
        let err = serve_one(&repo, &format!("push main {} {}", old.to_hex(), ids[1].to_hex()), &mut bytes.as_slice(), &mut Vec::new(), &[]).unwrap_err();
        assert!(format!("{err:#}").contains("doesn't match its id"));
        assert_eq!(repo.branch("main").unwrap(), Some(old));
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A transfer in copse's format: frames of id | length | raw object, then 32 zero bytes.
    fn transfer(objects: &[(Kind, Vec<u8>)]) -> (Vec<u8>, Vec<Id>) {
        let mut out = Vec::new();
        let mut ids = Vec::new();
        for (kind, data) in objects {
            let id = Store::hash(*kind, data);
            let raw = full_entry(*kind, data).unwrap();
            out.extend_from_slice(id.as_bytes());
            out.extend_from_slice(&(raw.len() as u32).to_le_bytes());
            out.extend_from_slice(&raw);
            ids.push(id);
        }
        out.extend_from_slice(&[0u8; 32]);
        (out, ids)
    }

    fn temp_store(tag: &str) -> (std::path::PathBuf, Store) {
        let dir = std::env::temp_dir().join(format!("copse-recv-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for i in 0..256u32 {
            std::fs::create_dir_all(dir.join(format!("{i:02x}"))).unwrap(); // as `copse init` makes them
        }
        (dir.clone(), Store::new(dir))
    }

    #[test]
    fn hostile_folder_listings_are_refused_on_arrival() {
        let blob = b"pwned\n".to_vec();
        let blob_id = Store::hash(Kind::Blob, &blob);
        let hostile = [
            vec![TreeEntry { name: "..".into(), mode: Mode::File, id: blob_id }],
            vec![TreeEntry { name: ".COPSE".into(), mode: Mode::Dir, id: blob_id }],
            vec![TreeEntry { name: "x".into(), mode: Mode::Link, id: blob_id }, TreeEntry { name: "x".into(), mode: Mode::Dir, id: blob_id }],
        ];
        for (n, entries) in hostile.iter().enumerate() {
            let tree = encode_tree(entries);
            let (bytes, ids) = transfer(&[(Kind::Blob, blob.clone()), (Kind::Tree, tree)]);
            let (dir, store) = temp_store(&format!("bad{n}"));
            let err = read_pack(&mut bytes.as_slice(), &store).expect_err("a hostile folder listing must be refused");
            assert!(format!("{err:#}").contains("refused"), "wrong error: {err:#}");
            assert!(!store.contains(&ids[1]), "the hostile listing must not be stored");
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    #[test]
    fn ordinary_folder_listings_still_arrive() {
        let blob = b"fine\n".to_vec();
        let blob_id = Store::hash(Kind::Blob, &blob);
        let tree = encode_tree(&[TreeEntry { name: ".gitignore".into(), mode: Mode::File, id: blob_id }, TreeEntry { name: "src".into(), mode: Mode::File, id: blob_id }]);
        let (bytes, ids) = transfer(&[(Kind::Blob, blob), (Kind::Tree, tree)]);
        let (dir, store) = temp_store("good");
        read_pack(&mut bytes.as_slice(), &store).unwrap();
        assert!(ids.iter().all(|id| store.contains(id)));
        let _ = std::fs::remove_dir_all(dir);
    }
}
