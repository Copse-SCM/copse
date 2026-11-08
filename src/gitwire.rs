//! Speaking git: push copse history to GitHub, GitLab or any git server.
//!
//! copse keeps its own storage. To push, each copse object is translated into git's
//! exact format (blob / tree / commit, identified by SHA-1), and git's own push
//! protocol is spoken to `git-receive-pack` on the server, over SSH. The server
//! sees an ordinary git client.
//!
//! `.copse/gitmap` remembers every translation (copse id <-> git id), so each object
//! is translated once and later pushes only handle new work. Commits keep their
//! full git identity (see `objects::Commit`), so translation is deterministic.

use crate::merge::is_ancestor;
use crate::objects::{decode_tree, short, Commit, Id, Kind, Mode, Signature, Store, Tag};
use crate::repo::Repo;
use crate::transport::objects_to_send;
use anyhow::{bail, Context, Result};
use flate2::{write::ZlibEncoder, Compression};
use rayon::prelude::*;
use sha1::{Digest, Sha1};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

pub type GitId = [u8; 20];
const NO_ID: GitId = [0; 20];

pub fn hex(id: &GitId) -> String {
    id.iter().map(|b| format!("{b:02x}")).collect()
}

fn parse_git_id(s: &str) -> Result<GitId> {
    if s.len() != 40 {
        bail!("bad git object id {s:?}");
    }
    let mut out = [0u8; 20];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).with_context(|| format!("bad git object id {s:?}"))?;
    }
    Ok(out)
}

// ---------------------------------------------------------------- the id map

/// copse id <-> git id for every object translated so far. Stored in `.copse/gitmap`
/// as fixed 52-byte records, appended to as new objects are translated.
pub struct GitMap {
    path: PathBuf,
    to_git: HashMap<Id, GitId>,
    from_git: HashMap<GitId, Id>,
    pending: Vec<(Id, GitId)>,
}

impl GitMap {
    pub fn load(repo: &Repo) -> Result<GitMap> {
        let path = repo.dir.join("gitmap");
        let bytes = match fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e.into()),
        };
        let mut map = GitMap { path, to_git: HashMap::new(), from_git: HashMap::new(), pending: Vec::new() };
        // A trailing partial record (an interrupted append) is ignored; it's simply recomputed.
        for rec in bytes.chunks_exact(52) {
            let copse = Id::from_bytes(rec[..32].try_into().unwrap());
            let git: GitId = rec[32..].try_into().unwrap();
            map.to_git.insert(copse, git);
            map.from_git.entry(git).or_insert(copse);
        }
        Ok(map)
    }

    pub fn git(&self, id: &Id) -> Option<GitId> {
        self.to_git.get(id).copied()
    }

    pub fn copse(&self, id: &GitId) -> Option<Id> {
        self.from_git.get(id).copied()
    }

    pub fn insert(&mut self, copse: Id, git: GitId) {
        if self.to_git.insert(copse, git).is_none() {
            self.from_git.entry(git).or_insert(copse);
            self.pending.push((copse, git));
        }
    }

    pub fn save(&mut self) -> Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let mut buf = Vec::with_capacity(self.pending.len() * 52);
        for (copse, git) in &self.pending {
            buf.extend_from_slice(copse.as_bytes());
            buf.extend_from_slice(git);
        }
        let mut f = fs::OpenOptions::new().create(true).append(true).open(&self.path)?;
        f.write_all(&buf)?;
        f.sync_all()?;
        self.pending.clear();
        Ok(())
    }
}

// ---------------------------------------------------------------- translation

pub fn git_hash(kind: &str, body: &[u8]) -> GitId {
    let mut h = Sha1::new();
    h.update(format!("{kind} {}\0", body.len()).as_bytes());
    h.update(body);
    h.finalize().into()
}

fn git_mode(mode: Mode) -> &'static str {
    match mode {
        Mode::File => "100644",
        Mode::Exec => "100755",
        Mode::Link => "120000",
        Mode::Dir => "40000",
        Mode::Submodule => "160000",
    }
}

/// git orders tree entries by name, but compares a folder as if its name ended in '/'.
/// (So "foo.txt" sorts before the folder "foo", because '.' comes before '/'.)
fn git_sort_key(name: &str, mode: Mode) -> Vec<u8> {
    let mut key = name.as_bytes().to_vec();
    if mode == Mode::Dir {
        key.push(b'/');
    }
    key
}

pub struct Translator<'a> {
    repo: &'a Repo,
    pub map: GitMap,
}

impl<'a> Translator<'a> {
    pub fn new(repo: &'a Repo) -> Result<Translator<'a>> {
        Ok(Translator { repo, map: GitMap::load(repo)? })
    }

    fn store(&self) -> &Store {
        &self.repo.store
    }

    /// Give `tip` and everything it reaches a git id. Returns the tip's git id.
    pub fn translate(&mut self, tip: Id) -> Result<GitId> {
        let commits = self.untranslated_commits(tip)?;
        let (mut trees, mut blobs, mut seen) = (Vec::new(), Vec::new(), HashSet::new());
        for (_, c) in &commits {
            self.collect(c.tree, &mut seen, &mut trees, &mut blobs)?;
        }
        // File contents are the bulk of the work: hash them in parallel.
        let hashed: Vec<(Id, GitId)> = blobs
            .par_iter()
            .map(|id| Ok((*id, git_hash("blob", &self.store().get_kind(id, Kind::Blob)?))))
            .collect::<Result<_>>()?;
        for (copse, git) in hashed {
            self.map.insert(copse, git);
        }
        for tree in trees {
            let body = self.tree_body(&tree)?; // children first, so theirs are known
            self.map.insert(tree, git_hash("tree", &body));
        }
        for (id, c) in &commits {
            let body = self.commit_body(c)?; // parents first
            self.map.insert(*id, git_hash("commit", &body));
        }
        self.map.git(&tip).context("internal error: tip wasn't translated")
    }

    /// The git form of a commit that may not be stored yet (for signing): its tree
    /// and parents are translated first.
    pub fn unsigned_body(&mut self, c: &Commit) -> Result<Vec<u8>> {
        for p in &c.parents {
            self.translate(*p)?;
        }
        let (mut trees, mut blobs, mut seen) = (Vec::new(), Vec::new(), HashSet::new());
        self.collect(c.tree, &mut seen, &mut trees, &mut blobs)?;
        let hashed: Vec<(Id, GitId)> = blobs
            .par_iter()
            .map(|id| Ok((*id, git_hash("blob", &self.store().get_kind(id, Kind::Blob)?))))
            .collect::<Result<_>>()?;
        for (copse, git) in hashed {
            self.map.insert(copse, git);
        }
        for tree in trees {
            let body = self.tree_body(&tree)?;
            self.map.insert(tree, git_hash("tree", &body));
        }
        self.commit_body(c)
    }

    /// Give a commit or an annotated tag (and everything it reaches) a git id.
    pub fn translate_any(&mut self, id: Id) -> Result<GitId> {
        if let Some(g) = self.map.git(&id) {
            return Ok(g);
        }
        let (kind, data) = self.store().get(&id)?;
        match kind {
            Kind::Commit => self.translate(id),
            Kind::Tag => {
                let tag = Tag::decode(&data)?;
                self.translate_any(tag.object)?; // its target first
                let body = self.tag_body(&tag)?;
                let g = git_hash("tag", &body);
                self.map.insert(id, g);
                Ok(g)
            }
            _ => bail!("only commits and tags can be pushed"),
        }
    }

    /// The git form of an annotated tag (which may not be stored yet), for signing.
    pub fn tag_payload(&mut self, t: &Tag) -> Result<Vec<u8>> {
        self.translate_any(t.object)?;
        self.tag_body(t)
    }

    fn tag_body(&self, t: &Tag) -> Result<Vec<u8>> {
        let target = self.map.git(&t.object).context("internal error: tag target wasn't translated")?;
        let kind = match t.target_kind {
            Kind::Commit => "commit",
            Kind::Tag => "tag",
            Kind::Tree => "tree",
            Kind::Blob => "blob",
        };
        let mut head = format!("object {}\ntype {kind}\ntag {}\n", hex(&target), t.name);
        if let Some(s) = &t.tagger {
            head += &format!("tagger {} {} {}\n", s.who, s.time, s.tz);
        }
        let mut out = head.into_bytes();
        out.extend_from_slice(&t.extra);
        out.push(b'\n');
        out.extend_from_slice(t.message.as_bytes());
        Ok(out)
    }

    /// Commits reachable from `tip` that have no git id yet, parents first.
    fn untranslated_commits(&self, tip: Id) -> Result<Vec<(Id, Commit)>> {
        let mut out = Vec::new();
        let mut visited = HashSet::new();
        let mut stack = vec![(tip, false)];
        while let Some((id, expanded)) = stack.pop() {
            if expanded {
                out.push((id, self.repo.read_commit(&id)?));
                continue;
            }
            if self.map.git(&id).is_some() || !visited.insert(id) {
                continue;
            }
            stack.push((id, true));
            for p in self.repo.read_commit(&id)?.parents.iter().rev() {
                stack.push((*p, false));
            }
        }
        Ok(out)
    }

    /// Trees (children before parents) and blobs under `tree` with no git id yet.
    fn collect(&self, tree: Id, seen: &mut HashSet<Id>, trees: &mut Vec<Id>, blobs: &mut Vec<Id>) -> Result<()> {
        if self.map.git(&tree).is_some() || !seen.insert(tree) {
            return Ok(());
        }
        for e in decode_tree(&self.store().get_kind(&tree, Kind::Tree)?)? {
            if e.mode == Mode::Dir {
                self.collect(e.id, seen, trees, blobs)?;
            } else if e.mode == Mode::Submodule {
                // Its git id is in the entry itself.
            } else if self.map.git(&e.id).is_none() && seen.insert(e.id) {
                blobs.push(e.id);
            }
        }
        trees.push(tree);
        Ok(())
    }

    fn tree_body(&self, tree: &Id) -> Result<Vec<u8>> {
        let mut entries = decode_tree(&self.store().get_kind(tree, Kind::Tree)?)?;
        entries.sort_by_cached_key(|e| git_sort_key(&e.name, e.mode));
        let mut out = Vec::with_capacity(entries.len() * 40);
        for e in entries {
            let child = if e.mode == Mode::Submodule {
                crate::objects::gitlink_sha(&e.id)
            } else {
                self.map.git(&e.id).context("internal error: tree entry wasn't translated")?
            };
            out.extend_from_slice(git_mode(e.mode).as_bytes());
            out.push(b' ');
            out.extend_from_slice(e.name.as_bytes());
            out.push(0);
            out.extend_from_slice(&child);
        }
        Ok(out)
    }

    fn commit_body(&self, c: &Commit) -> Result<Vec<u8>> {
        let sig = |s: &Signature| {
            // Commits made before copse recorded emails have a bare name.
            let who = if s.who.contains('<') && s.who.ends_with('>') {
                s.who.clone()
            } else {
                format!("{} <{}>", s.who, crate::identity::email())
            };
            format!("{who} {} {}", s.time, s.tz)
        };
        let tree = self.map.git(&c.tree).context("internal error: commit tree wasn't translated")?;
        let mut head = format!("tree {}\n", hex(&tree));
        for p in &c.parents {
            let parent = self.map.git(p).context("internal error: parent commit wasn't translated")?;
            head += &format!("parent {}\n", hex(&parent));
        }
        head += &format!("author {}\ncommitter {}\n", sig(&c.author), sig(&c.committer));
        let mut out = head.into_bytes();
        out.extend_from_slice(&c.extra);
        out.push(b'\n');
        out.extend_from_slice(c.message.as_bytes()); // exactly as stored, final newline or not
        Ok(out)
    }

    /// An already-translated object in git form: (pack type number, body).
    pub fn git_object(&self, id: &Id) -> Result<(u8, Vec<u8>)> {
        let (kind, data) = self.store().get(id)?;
        Ok(match kind {
            Kind::Commit => (1, self.commit_body(&Commit::decode(&data)?)?),
            Kind::Tree => (2, self.tree_body(id)?),
            Kind::Blob => (3, data),
            Kind::Tag => (4, self.tag_body(&Tag::decode(&data)?)?),
        })
    }
}

// ---------------------------------------------------------------- git's pack format

struct HashingWriter<W: Write> {
    inner: W,
    hash: Sha1,
    bytes: u64,
}

impl<W: Write> Write for HashingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hash.update(&buf[..n]);
        self.bytes += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// Type and size, packed the way git's pack format wants: 3 type bits, then the
/// size 4 bits at a time, then 7 bits at a time, with a "more follows" high bit.
fn object_header(kind: u8, mut size: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(4);
    let mut byte = (kind << 4) | (size & 0x0f) as u8;
    size >>= 4;
    while size > 0 {
        out.push(byte | 0x80);
        byte = (size & 0x7f) as u8;
        size >>= 7;
    }
    out.push(byte);
    out
}

/// Write a version-2 git pack of `ids` (all already translated): "PACK", version,
/// count, then each object zlib-compressed, then a SHA-1 of everything before it.
/// Returns (objects, bytes).
fn write_git_pack(w: &mut impl Write, tr: &Translator, ids: &[Id]) -> Result<(usize, u64)> {
    let mut seen = HashSet::new();
    let mut unique = Vec::with_capacity(ids.len());
    for id in ids {
        let git = tr.map.git(id).context("internal error: object wasn't translated")?;
        if seen.insert(git) {
            unique.push(*id);
        }
    }
    let mut out = HashingWriter { inner: w, hash: Sha1::new(), bytes: 0 };
    out.write_all(b"PACK")?;
    out.write_all(&2u32.to_be_bytes())?;
    out.write_all(&(unique.len() as u32).to_be_bytes())?;
    for chunk in unique.chunks(512) {
        let encoded: Vec<Vec<u8>> = chunk
            .par_iter()
            .map(|id| {
                let (kind, body) = tr.git_object(id)?;
                let mut z = ZlibEncoder::new(object_header(kind, body.len()), Compression::fast());
                z.write_all(&body)?;
                Ok(z.finish()?)
            })
            .collect::<Result<_>>()?;
        for e in encoded {
            out.write_all(&e)?;
        }
    }
    let digest: [u8; 20] = out.hash.clone().finalize().into();
    let bytes = out.bytes + 20;
    out.inner.write_all(&digest)?;
    out.inner.flush()?;
    Ok((unique.len(), bytes))
}

// ---------------------------------------------------------------- talking to the server

pub enum GitUrl {
    Local(PathBuf),
    Ssh { host: String, port: Option<String>, path: String },
    Http(String),
}

/// `https://github.com/you/repo.git`, `git@github.com:you/repo.git`,
/// `ssh://[user@]host[:port]/path`, or a local path.
pub fn parse_git_url(url: &str) -> Result<GitUrl> {
    if url.starts_with("https://") || url.starts_with("http://") {
        return Ok(GitUrl::Http(url.to_string()));
    }
    if let Some(rest) = url.strip_prefix("ssh://") {
        let (authority, path) = rest.split_once('/').context("an ssh:// address needs a path")?;
        let (host, port) = match authority.rsplit_once(':') {
            Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) => (h.to_string(), Some(p.to_string())),
            _ => (authority.to_string(), None),
        };
        let path = if path.starts_with('~') { path.to_string() } else { format!("/{path}") };
        return Ok(GitUrl::Ssh { host, port, path });
    }
    if let Some(path) = url.strip_prefix("file://") {
        return Ok(GitUrl::Local(PathBuf::from(path)));
    }
    if let Some((host, path)) = url.split_once(':') {
        let looks_local = url.starts_with('/') || url.starts_with('.') || url.starts_with('~') || host.contains('/');
        if !looks_local && !host.is_empty() {
            return Ok(GitUrl::Ssh { host: host.to_string(), port: None, path: path.to_string() });
        }
    }
    Ok(GitUrl::Local(PathBuf::from(url)))
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// A running `git-receive-pack` (for pushing) or `git-upload-pack` (for fetching),
/// local or over SSH.
pub struct GitConn {
    child: Child,
    r: Option<BufReader<ChildStdout>>,
    w: Option<BufWriter<ChildStdin>>,
    /// Whether we've sent our update commands. If not, we say "nothing to do" on
    /// the way out, as git does, rather than just hanging up on the server.
    pub sent_commands: bool,
}

impl GitConn {
    /// Start git's server program `service` ("receive-pack" or "upload-pack").
    pub fn open(url: &str, service: &str) -> Result<GitConn> {
        let mut cmd = match parse_git_url(url)? {
            GitUrl::Local(path) => {
                let mut c = Command::new("git");
                c.arg(service).arg(path);
                c
            }
            GitUrl::Ssh { host, port, path } => {
                // A "host" starting with - would be read by ssh as an option
                // (-oProxyCommand=... runs a command): never pass one on.
                if host.starts_with('-') {
                    bail!("refusing the address {url:?}: its host starts with '-', which ssh would read as an option");
                }
                let ssh = std::env::var("COPSE_SSH").unwrap_or_else(|_| "ssh".into());
                let mut c = Command::new(ssh);
                if let Some(p) = port {
                    c.arg("-p").arg(p);
                }
                c.arg(host).arg(format!("git-{service} {}", shell_quote(&path)));
                c
            }
            GitUrl::Http(_) => bail!("internal error: HTTP addresses don't use a pipe"),
        };
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("connecting to {url}"))?;
        let r = BufReader::new(child.stdout.take().unwrap());
        let w = BufWriter::new(child.stdin.take().unwrap());
        Ok(GitConn { child, r: Some(r), w: Some(w), sent_commands: false })
    }

    pub fn r(&mut self) -> &mut BufReader<ChildStdout> {
        self.r.as_mut().expect("connection closed")
    }

    pub fn w(&mut self) -> &mut BufWriter<ChildStdin> {
        self.w.as_mut().expect("connection closed")
    }

}

/// One pkt-line from `r`: 4 hex digits of length (including themselves), then data.
/// None is a flush packet ("0000").
pub fn read_pkt(r: &mut dyn Read) -> Result<Option<Vec<u8>>> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len).context("the git server hung up (check the address and your access to it)")?;
    let len = usize::from_str_radix(std::str::from_utf8(&len)?, 16).context("garbled reply from the git server")?;
    if len == 0 {
        return Ok(None);
    }
    if len < 4 {
        bail!("garbled reply from the git server");
    }
    let mut data = vec![0u8; len - 4];
    r.read_exact(&mut data)?;
    Ok(Some(data))
}

/// One pkt-line, encoded.
pub fn pkt(data: &[u8]) -> Vec<u8> {
    let mut out = format!("{:04x}", data.len() + 4).into_bytes();
    out.extend_from_slice(data);
    out
}

pub const FLUSH: &[u8] = b"0000";

/// The server's branches and capabilities, up to the flush packet.
pub fn read_advertisement(r: &mut dyn Read) -> Result<(Vec<(String, GitId)>, HashSet<String>)> {
    let mut refs = Vec::new();
    let mut caps = HashSet::new();
    while let Some(line) = read_pkt(r)? {
        let line = line.strip_suffix(b"\n").unwrap_or(&line);
        let (main, cap_part) = match line.iter().position(|&b| b == 0) {
            Some(i) => (&line[..i], Some(&line[i + 1..])),
            None => (line, None),
        };
        if let Some(c) = cap_part {
            caps.extend(String::from_utf8_lossy(c).split(' ').map(str::to_string));
        }
        let main = String::from_utf8_lossy(main);
        if let Some(err) = main.strip_prefix("ERR ") {
            bail!("the git server said: {err}");
        }
        let (id, name) = main.split_once(' ').context("garbled branch list from the git server")?;
        if name != "capabilities^{}" {
            refs.push((name.to_string(), parse_git_id(id)?));
        }
    }
    Ok((refs, caps))
}

/// A conversation with a git server: over a pipe (SSH or a local folder) or HTTP(S).
/// Both carry the same messages; HTTP just sends each half as one request.
pub enum Session {
    Pipe(GitConn),
    Http(crate::githttp::Http),
}

impl Session {
    /// `service` is "receive-pack" (pushing) or "upload-pack" (fetching).
    pub fn open(url: &str, service: &'static str) -> Result<Session> {
        Ok(match parse_git_url(url)? {
            GitUrl::Http(u) => Session::Http(crate::githttp::Http::new(&u, service)),
            _ => Session::Pipe(GitConn::open(url, service)?),
        })
    }

    /// The server's branches and capabilities.
    pub fn advertisement(&mut self) -> Result<(Vec<(String, GitId)>, HashSet<String>)> {
        match self {
            Session::Pipe(c) => read_advertisement(c.r()),
            Session::Http(h) => h.discover(),
        }
    }

    /// Send our whole request and return the server's reply.
    pub fn exchange(&mut self, request: &[u8]) -> Result<Box<dyn Read + '_>> {
        match self {
            Session::Pipe(c) => {
                c.w().write_all(request)?;
                c.w().flush()?;
                c.sent_commands = true;
                Ok(Box::new(c.r()))
            }
            Session::Http(h) => Ok(h.post(request)?),
        }
    }
}

impl Drop for GitConn {
    fn drop(&mut self) {
        if !self.sent_commands {
            if let Some(w) = self.w.as_mut() {
                let _ = w.write_all(b"0000"); // an empty command list: "nothing to do"
                let _ = w.flush();
            }
        }
        // Close both pipes before waiting, so the server can't be left blocked on us.
        self.w = None;
        self.r = None;
        let _ = self.child.wait();
    }
}

pub fn human(bytes: u64) -> String {
    match bytes {
        b if b >= 1 << 20 => format!("{:.1} MB", b as f64 / (1u64 << 20) as f64),
        b if b >= 1 << 10 => format!("{:.1} KB", b as f64 / (1u64 << 10) as f64),
        b => format!("{b} bytes"),
    }
}

/// One branch or tag to update on a git server.
pub struct RefUpdate {
    pub name: String,
    pub is_tag: bool,
    /// A commit, or (for an annotated tag) a tag object.
    pub local: Id,
}

impl RefUpdate {
    fn refname(&self) -> String {
        if self.is_tag {
            format!("refs/tags/{}", self.name)
        } else {
            format!("refs/heads/{}", self.name)
        }
    }
}

/// `copse push` to a git server: update every branch and tag in `updates` in one push.
pub fn push(repo: &Repo, remote: &str, url: &str, updates: &[RefUpdate], force: bool) -> Result<()> {
    if repo.is_shallow() {
        bail!("this is a shallow clone, so it can't be translated for a git server (git ids depend on all of history). Run `copse fetch --unshallow` first");
    }
    let mut session = Session::open(url, "receive-pack")?;
    let (refs, caps) = session.advertisement()?;
    let on_server = |name: &str| refs.iter().find(|(n, _)| n == name).map(|(_, g)| *g);

    let mut tr = Translator::new(repo)?;
    let mut commands = Vec::new();
    for u in updates {
        let new = tr.translate_any(u.local)?;
        let old = on_server(&u.refname());
        if old == Some(new) {
            if !u.is_tag {
                repo.set_tracking(remote, &u.name, &u.local)?;
            }
            continue;
        }
        if let (Some(old), false) = (old, force) {
            if u.is_tag {
                bail!(
                    "rejected: tag {} already exists on {remote} and points somewhere else. Tags normally never \
                     move; use `copse push --force` only if you really mean to replace it",
                    u.name
                );
            }
            let fast_forward = match tr.map.copse(&old) {
                Some(t) => repo.store.contains(&t) && is_ancestor(repo, repo.peel(t)?, u.local)?,
                None => false,
            };
            if !fast_forward {
                bail!(
                    "rejected: {remote}/{} has commits you don't have. Run `copse pull` to bring them in first, \
                     or `copse push --force` to overwrite them",
                    u.name
                );
            }
        }
        commands.push((u, old, new));
    }
    tr.map.save()?;
    if commands.is_empty() {
        println!("Everything up to date.");
        return Ok(());
    }

    // Send only what the server can't already reach: history for each commit, plus
    // the annotated tag objects themselves.
    let haves: Vec<Id> = refs
        .iter()
        .filter_map(|(_, g)| tr.map.copse(g))
        .filter(|t| repo.store.contains(t))
        .filter_map(|t| repo.peel(t).ok())
        .collect();
    let tips: Vec<Id> = commands.iter().map(|(u, _, _)| repo.peel(u.local)).collect::<Result<_>>()?;
    let mut objects = objects_to_send(repo, &tips, &haves)?;
    // LFS files' contents go to the LFS server before the pointers reach git.
    crate::lfs::before_push(repo, remote, &objects)?;
    for (u, _, _) in &commands {
        let mut id = u.local;
        loop {
            let (kind, data) = repo.store.get(&id)?;
            if kind != Kind::Tag {
                break;
            }
            objects.push(id);
            id = Tag::decode(&data)?.object;
        }
    }

    let mut wanted = Vec::new();
    if caps.contains("report-status") {
        wanted.push("report-status");
    }
    wanted.push("agent=copse/0.1");
    let mut request = Vec::new();
    for (i, (u, old, new)) in commands.iter().enumerate() {
        let mut line = format!("{} {} {}", hex(&old.unwrap_or(NO_ID)), hex(new), u.refname());
        if i == 0 {
            line += &format!("\0{}", wanted.join(" "));
        }
        line.push('\n');
        request.extend(pkt(line.as_bytes()));
    }
    request.extend_from_slice(FLUSH);
    let (count, bytes) = write_git_pack(&mut request, &tr, &objects)?;
    let mut reply = session.exchange(&request)?;

    if caps.contains("report-status") {
        read_report(&mut *reply)?;
    }
    drop(reply);
    drop(session);

    for (u, old, new) in &commands {
        if u.is_tag {
            println!("Pushed tag {} to {remote} (git {})", u.name, &hex(new)[..10]);
        } else {
            repo.set_tracking(remote, &u.name, &u.local)?;
            let from = old.map_or_else(|| "new branch".to_string(), |o| hex(&o)[..10].to_string());
            println!("Pushed {} to {remote}: {from} → {}", u.name, &hex(new)[..10]);
            println!("  (copse {} = git {})", short(&u.local), &hex(new)[..10]);
        }
    }
    println!("  ({count} objects, {})", human(bytes));
    Ok(())
}

/// The server's verdict on a push ("report-status"): fail if anything was refused.
fn read_report(reply: &mut dyn Read) -> Result<()> {
    let mut problems = Vec::new();
    while let Some(line) = read_pkt(reply)? {
        let line = String::from_utf8_lossy(&line).trim_end().to_string();
        if let Some(status) = line.strip_prefix("unpack ") {
            if status != "ok" {
                problems.push(format!("the server couldn't unpack the data: {status}"));
            }
        } else if let Some(rest) = line.strip_prefix("ng ") {
            problems.push(format!("the server refused {rest}"));
        }
    }
    if !problems.is_empty() {
        bail!("push failed: {}", problems.join("; "));
    }
    Ok(())
}

/// `copse push <remote> --delete NAME` on a git server: delete a branch (or, if there's
/// no such branch, a tag). Only the name goes; nothing else changes on the server.
pub fn delete_remote(repo: &Repo, remote: &str, url: &str, name: &str) -> Result<()> {
    let mut session = Session::open(url, "receive-pack")?;
    let (refs, caps) = session.advertisement()?;
    let find = |r: String| refs.iter().find(|(n, _)| *n == r).map(|(_, g)| (r.clone(), *g));
    let (refname, old) = find(format!("refs/heads/{name}"))
        .or_else(|| find(format!("refs/tags/{name}")))
        .with_context(|| format!("{remote} has no branch or tag called {name}"))?;
    if !caps.contains("delete-refs") {
        bail!("{remote} doesn't allow deleting branches");
    }
    let mut wanted = vec!["delete-refs"];
    if caps.contains("report-status") {
        wanted.push("report-status");
    }
    wanted.push("agent=copse/0.1");
    // Only deletions, so no pack follows the commands.
    let mut request = pkt(format!("{} {} {refname}\0{}\n", hex(&old), hex(&NO_ID), wanted.join(" ")).as_bytes());
    request.extend_from_slice(FLUSH);
    let mut reply = session.exchange(&request)?;
    if caps.contains("report-status") {
        read_report(&mut *reply)?;
    }
    drop(reply);
    drop(session);
    let is_branch = refname.starts_with("refs/heads/");
    if is_branch {
        repo.delete_tracking(remote, name)?;
    }
    println!("Deleted {} {name} on {remote} (was git {}).", if is_branch { "branch" } else { "tag" }, &hex(&old)[..10]);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_like_git() {
        // `printf 'hello\n' | git hash-object --stdin`
        assert_eq!(hex(&git_hash("blob", b"hello\n")), "ce013625030ba8dba906f756967f9e9ca394464a");
        // The empty tree every git user has seen.
        assert_eq!(hex(&git_hash("tree", b"")), "4b825dc642cb6eb9a060e54bf8d69288fbee4904");
    }

    #[test]
    fn pack_object_headers() {
        assert_eq!(object_header(3, 5), vec![0x35]);
        assert_eq!(object_header(1, 16), vec![0x90, 0x01]);
        assert_eq!(object_header(2, 1000), vec![0xa8, 0x3e]);
    }

    #[test]
    fn folders_sort_as_if_they_end_in_a_slash() {
        let mut names = vec![("foo", Mode::Dir), ("foo.txt", Mode::File), ("foo-bar", Mode::File), ("foo0", Mode::File)];
        names.sort_by_key(|(n, m)| git_sort_key(n, *m));
        assert_eq!(names.iter().map(|(n, _)| *n).collect::<Vec<_>>(), vec!["foo-bar", "foo.txt", "foo", "foo0"]);
    }

    #[test]
    fn understands_remote_addresses() {
        assert!(matches!(parse_git_url("git@github.com:me/repo.git").unwrap(),
            GitUrl::Ssh { ref host, ref path, port: None } if host == "git@github.com" && path == "me/repo.git"));
        assert!(matches!(parse_git_url("ssh://git@host.example:2222/srv/r.git").unwrap(),
            GitUrl::Ssh { ref host, port: Some(ref p), ref path } if host == "git@host.example" && p == "2222" && path == "/srv/r.git"));
        assert!(matches!(parse_git_url("/tmp/repo.git").unwrap(), GitUrl::Local(_)));
        assert!(matches!(parse_git_url("https://github.com/me/repo.git").unwrap(), GitUrl::Http(_)));
    }
}
