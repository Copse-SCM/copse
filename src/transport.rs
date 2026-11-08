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
/// Either way, an interrupted transfer never leaves a commit whose files are missing.
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

/// After receiving a push: boundary commits a shallow client announced, whose
/// parents we don't have, become our boundary too.
fn take_boundary(repo: &Repo, announced: &[Id]) -> Result<()> {
    let mut shallow = repo.shallow();
    let before = shallow.len();
    for id in announced {
        if let Ok(c) = repo.read_commit_raw(id) {
            if c.parents.iter().any(|p| !repo.store.contains(p)) {
                shallow.insert(*id);
            }
        }
    }
    if shallow.len() != before {
        repo.set_shallow(&shallow)?;
    }
    Ok(())
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
                take_boundary(repo, announced)?;
                repo.store.get(&new).context("the tagged object didn't arrive")?;
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
            take_boundary(repo, announced)?;
            repo.read_commit(&new).context("the pushed commit didn't arrive")?;
            repo.cas_branch(branch, old, &new)?;
            writeln!(w, "ok")?;
        }
        other => bail!("unknown request {other:?}"),
    }
    w.flush()?;
    Ok(true)
}
