//! Repository layout, HEAD and branch refs.
//!
//! .copse/
//!   HEAD            "ref: <branch>" or a commit id (detached)
//!   refs/<branch>   commit id
//!   objects/xx/...  object store
//!   index           stat cache of the files in the current commit
//!   shallow         (shallow clones) commits whose parents weren't fetched
//!   worktrees/<n>/  each extra worktree's own HEAD, index and state files
//!
//! An extra worktree (`copse worktree add`) has a `.copse` *file* naming its folder
//! under `worktrees/`; everything else (objects, refs, tags, settings) is shared.

use crate::objects::{parse_hex, short, Commit, Id, Kind, Store, Tag};
use anyhow::{bail, Context, Result};
use std::ffi::OsString;
use std::fs;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

pub const DIR: &str = ".copse";

pub struct Repo {
    pub root: PathBuf,
    /// The shared repository folder: objects, refs, tags, settings, stashes.
    pub dir: PathBuf,
    /// This working directory's own state: HEAD, index, merge/rebase/bisect state.
    /// The same as `dir`, except in an extra worktree.
    pub wt: PathBuf,
    pub store: Store,
    /// Shallow clones: commits whose parents weren't fetched (read once, on first use).
    shallow: std::sync::RwLock<Option<std::collections::HashSet<Id>>>,
}

/// What a worktree's `.copse` file starts with.
pub const WORKTREE_MARK: &str = "copse-worktree: ";

pub enum Head {
    Branch(String),
    Detached(Id),
}

impl Repo {
    pub fn init(root: &Path) -> Result<Repo> {
        let dir = crate::objects::disk_path(&root, DIR);
        if dir.exists() {
            bail!("already a copse repository: {}", dir.display());
        }
        init_dir(&dir)?;
        Ok(Self::open(root.to_path_buf()))
    }

    /// A bare repository has no working directory: `path` itself holds HEAD, refs and objects.
    /// Use one as a shared remote that you push to.
    pub fn init_bare(path: &Path) -> Result<Repo> {
        if path.exists() && fs::read_dir(path)?.next().is_some() {
            bail!("{} already exists and isn't empty", path.display());
        }
        init_dir(path)?;
        Self::open_path(path)
    }

    fn build(root: PathBuf, dir: PathBuf, wt: PathBuf) -> Repo {
        let store = Store::new(dir.join("objects"));
        let repo = Repo { root, dir, wt, store, shallow: std::sync::RwLock::new(None) };
        repo.attach_promisor();
        repo
    }

    /// Partial clones (`.copse/promisor` names the remote): fetch file contents
    /// from there whenever they're needed and missing.
    fn attach_promisor(&self) {
        let Ok(remote) = fs::read_to_string(self.dir.join("promisor")) else { return };
        let Ok(url) = self.remote_url(remote.trim()) else { return };
        let label = remote.trim().to_string();
        if let Some(git) = crate::remote::git_url(&url).map(str::to_string) {
            // A git server: contents are fetched by their git ids.
            self.store.set_promisor(std::sync::Arc::new(move |store: &Store, ids: &[Id]| {
                if ids.len() > 1 && std::io::IsTerminal::is_terminal(&std::io::stderr()) {
                    eprintln!("Fetching {} file(s) from {label}...", ids.len());
                }
                crate::gitimport::fetch_blobs(store, &git, ids).with_context(|| format!("fetching file contents from {label} (this is a partial clone)"))
            }));
            return;
        }
        self.store.set_promisor(std::sync::Arc::new(move |store: &Store, ids: &[Id]| {
            if ids.len() > 1 && std::io::IsTerminal::is_terminal(&std::io::stderr()) {
                eprintln!("Fetching {} file(s) from {label}...", ids.len());
            }
            let mut conn = crate::transport::Conn::open(&url)?;
            conn.get_objects(store, ids).with_context(|| format!("fetching file contents from {label} (this is a partial clone)"))?;
            Ok(())
        }));
    }

    /// A partial clone: file contents are fetched from its remote when needed.
    pub fn is_partial(&self) -> bool {
        self.store.is_partial()
    }

    fn open(root: PathBuf) -> Repo {
        let marker = crate::objects::disk_path(&root, DIR);
        if marker.is_file() {
            // An extra worktree: its state folder, and from there the shared one.
            if let Some(wt) = fs::read_to_string(&marker).ok().and_then(|t| t.trim().strip_prefix(WORKTREE_MARK.trim()).map(|p| PathBuf::from(p.trim()))) {
                let dir = fs::read_to_string(wt.join("commondir")).map(|c| PathBuf::from(c.trim())).unwrap_or_else(|_| wt.join("..").join(".."));
                return Self::build(root, dir, wt);
            }
        }
        Self::build(root, marker.clone(), marker)
    }

    /// Open the repository at exactly `path`, which may be a working directory or a bare repository.
    pub fn open_path(path: &Path) -> Result<Repo> {
        if path.join(DIR).exists() {
            return Ok(Self::open(path.to_path_buf()));
        }
        if path.join("HEAD").is_file() && path.join("objects").is_dir() {
            return Ok(Self::build(path.to_path_buf(), path.to_path_buf(), path.to_path_buf()));
        }
        bail!("no copse repository at {}", path.display())
    }

    pub fn is_bare(&self) -> bool {
        self.root == self.dir
    }

    /// Commits whose parents weren't fetched (shallow clones); empty otherwise.
    pub fn shallow(&self) -> std::collections::HashSet<Id> {
        if let Some(s) = self.shallow.read().unwrap().as_ref() {
            return s.clone();
        }
        let set: std::collections::HashSet<Id> = fs::read_to_string(self.dir.join("shallow"))
            .unwrap_or_default()
            .lines()
            .filter_map(|l| parse_hex(l.trim()).ok())
            .collect();
        *self.shallow.write().unwrap() = Some(set.clone());
        set
    }

    pub fn is_shallow(&self) -> bool {
        !self.shallow().is_empty()
    }

    pub fn set_shallow(&self, set: &std::collections::HashSet<Id>) -> Result<()> {
        let path = self.dir.join("shallow");
        if set.is_empty() {
            let _ = fs::remove_file(&path);
        } else {
            let mut lines: Vec<String> = set.iter().map(|id| id.to_hex().to_string()).collect();
            lines.sort();
            write_atomic(&path, format!("{}\n", lines.join("\n")).as_bytes())?;
        }
        *self.shallow.write().unwrap() = Some(set.clone());
        Ok(())
    }

    /// A commit exactly as stored, parents and all, even at a shallow boundary.
    pub fn read_commit_raw(&self, id: &Id) -> Result<Commit> {
        Commit::decode(&self.store.get_kind(id, Kind::Commit)?)
    }

    /// Find the repository containing the current directory.
    pub fn find() -> Result<Repo> {
        let mut cur = std::env::current_dir()?;
        loop {
            if cur.join(DIR).exists() {
                return Ok(Self::open(cur));
            }
            if !cur.pop() {
                bail!("not inside a copse repository (run `copse init` first)");
            }
        }
    }

    /// Like `find`, but also accepts being inside a bare repository.
    pub fn find_any() -> Result<Repo> {
        let mut cur = std::env::current_dir()?;
        loop {
            if let Ok(repo) = Self::open_path(&cur) {
                return Ok(repo);
            }
            if !cur.pop() {
                bail!("not inside a copse repository (run `copse init` first)");
            }
        }
    }

    pub fn head(&self) -> Result<Head> {
        let s = fs::read_to_string(self.wt.join("HEAD"))?;
        let s = s.trim();
        Ok(match s.strip_prefix("ref: ") {
            Some(branch) => Head::Branch(branch.to_string()),
            None => Head::Detached(parse_hex(s)?),
        })
    }

    pub fn set_head(&self, head: &Head) -> Result<()> {
        let before = self.head_commit().ok().flatten();
        let s = match head {
            Head::Branch(b) => format!("ref: {b}\n"),
            Head::Detached(id) => format!("{}\n", id.to_hex()),
        };
        write_atomic(&self.wt.join("HEAD"), s.as_bytes())?;
        let after = self.head_commit()?;
        if before != after {
            crate::reflog::record(&self.wt, "HEAD", before, after)?;
        }
        Ok(())
    }

    pub fn branch(&self, name: &str) -> Result<Option<Id>> {
        if validate_branch(name).is_err() {
            return Ok(None);
        }
        match fs::read_to_string(self.dir.join("refs").join(ref_file(name))) {
            Ok(s) => Ok(Some(parse_hex(s.trim())?)),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn set_branch(&self, name: &str, id: &Id) -> Result<()> {
        validate_branch(name)?;
        let before = self.branch(name)?;
        if before.is_none() {
            check_clash(self.branches()?.into_iter().map(|(b, _)| b), name, "branch")?;
        }
        write_atomic(&self.dir.join("refs").join(ref_file(name)), format!("{}\n", id.to_hex()).as_bytes())?;
        crate::reflog::record(&self.dir, name, before, Some(*id))?;
        // Moving the checked-out branch moves HEAD too.
        if before != Some(*id) && matches!(self.head(), Ok(Head::Branch(b)) if b == name) {
            crate::reflog::record(&self.wt, "HEAD", before, Some(*id))?;
        }
        Ok(())
    }

    /// Rename a branch, carrying its history (reflog) over to the new name.
    pub fn rename_branch(&self, old: &str, new: &str) -> Result<()> {
        validate_branch(new)?;
        let tip = self.branch(old)?.with_context(|| format!("there is no branch called {old}"))?;
        check_clash(self.branches()?.into_iter().map(|(b, _)| b).filter(|b| b != old), new, "branch")?;
        crate::reflog::rename(&self.dir, old, new)?;
        write_atomic(&self.dir.join("refs").join(ref_file(new)), format!("{}\n", tip.to_hex()).as_bytes())?;
        crate::reflog::record(&self.dir, new, Some(tip), Some(tip))?;
        if matches!(self.head()?, Head::Branch(b) if b == old) {
            write_atomic(&self.wt.join("HEAD"), format!("ref: {new}\n").as_bytes())?;
        }
        fs::remove_file(self.dir.join("refs").join(ref_file(old)))?;
        Ok(())
    }

    pub fn branches(&self) -> Result<Vec<(String, Id)>> {
        let mut out = Vec::new();
        for entry in fs::read_dir(self.dir.join("refs"))? {
            let name = ref_name(&entry?.file_name().to_string_lossy());
            if let Some(id) = self.branch(&name)? {
                out.push((name, id));
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }

    /// The commit HEAD points at, or None on a branch with no commits yet.
    pub fn head_commit(&self) -> Result<Option<Id>> {
        match self.head()? {
            Head::Branch(b) => self.branch(&b),
            Head::Detached(id) => Ok(Some(id)),
        }
    }

    /// Move the current branch (or detached HEAD) to `id`.
    pub fn advance_head(&self, id: &Id) -> Result<()> {
        match self.head()? {
            Head::Branch(b) => self.set_branch(&b, id),
            Head::Detached(_) => self.set_head(&Head::Detached(*id)),
        }
    }

    /// Resolve `HEAD`, a branch name, or a commit id prefix, optionally followed by `~N`.
    /// A commit, from any of git's ways of naming one: a branch, remote branch
    /// (origin/main), tag, id prefix, HEAD or @, a reflog entry (main@{2}), followed
    /// by any of ~N, ^, ^N, ^0 and ^{commit}.
    pub fn resolve(&self, rev: &str) -> Result<Id> {
        let id = self.resolve_object(rev)?;
        self.peel(id).with_context(|| format!("{rev} isn't a commit"))
    }

    /// Any object: everything `resolve` takes, plus ids of trees, files and tags,
    /// REV:path (a file or folder in that commit; :path for HEAD's), REV^{tree},
    /// and REV^{} (an annotated tag's target).
    pub fn resolve_object(&self, spec: &str) -> Result<Id> {
        if let Some((rev, path)) = spec.split_once(':') {
            let commit = self.resolve(if rev.is_empty() { "HEAD" } else { rev })?;
            let tree = self.read_commit(&commit)?.tree;
            let path = path.trim_matches('/');
            if path.is_empty() {
                return Ok(tree);
            }
            return crate::objects::entry_at(&self.store, &tree, path)?
                .map(|(_, id)| id)
                .with_context(|| format!("{path} isn't in {}", if rev.is_empty() { "HEAD" } else { rev }));
        }
        let cut = spec.find(['~', '^']).unwrap_or(spec.len());
        let (base, mut suffix) = spec.split_at(cut);
        let mut id = self.resolve_base(base).with_context(|| format!("unknown revision: {spec}"))?;
        while !suffix.is_empty() {
            let op = suffix.as_bytes()[0];
            suffix = &suffix[1..];
            if op == b'^' && suffix.starts_with('{') {
                let end = suffix.find('}').with_context(|| format!("bad revision: {spec}"))?;
                let want = &suffix[1..end];
                suffix = &suffix[end + 1..];
                id = match want {
                    "" => self.peel_tags(id)?,
                    "commit" => self.peel(id)?,
                    "tree" => match self.store.get(&id)?.0 {
                        Kind::Tree => id,
                        _ => self.read_commit(&self.peel(id)?)?.tree,
                    },
                    other => bail!("^{{{other}}} isn't supported (use ^{{}}, ^{{commit}} or ^{{tree}})"),
                };
                continue;
            }
            let digits = suffix.bytes().take_while(u8::is_ascii_digit).count();
            let n: Option<usize> = if digits > 0 { Some(suffix[..digits].parse()?) } else { None };
            suffix = &suffix[digits..];
            id = self.peel(id)?;
            if op == b'~' {
                for _ in 0..n.unwrap_or(1) {
                    id = *self.read_commit(&id)?.parents.first().context("history is shorter than that")?;
                }
            } else {
                match n.unwrap_or(1) {
                    0 => {}
                    k => id = *self.read_commit(&id)?.parents.get(k - 1).with_context(|| format!("{} has no parent number {k}", short(&id)))?,
                }
            }
        }
        Ok(id)
    }

    /// The object a name points at, before any ~ or ^ (tags not peeled).
    fn resolve_base(&self, base: &str) -> Result<Id> {
        let reflog_pos = base.strip_suffix('}').and_then(|b| b.rsplit_once("@{")).and_then(|(name, n)| n.parse::<usize>().ok().map(|n| (name, n)));
        if let Some((name, n)) = reflog_pos {
            return crate::reflog::resolve(self, if name.is_empty() { "HEAD" } else { name }, n);
        }
        if base == "HEAD" || base == "@" || base.is_empty() {
            return self.head_commit()?.context("there are no commits yet");
        }
        if let Some(id) = self.branch(base)? {
            return Ok(id);
        }
        if let Some((remote, branch)) = base.split_once('/') {
            if let Some(id) = self.tracking(remote, branch)? {
                return Ok(id);
            }
        }
        if let Some(id) = self.tag(base)? {
            return Ok(id);
        }
        self.store.find_prefix(base)
    }

    /// Follow annotated tags to whatever they finally point at (any kind).
    fn peel_tags(&self, mut id: Id) -> Result<Id> {
        for _ in 0..16 {
            let (kind, data) = self.store.get(&id)?;
            if kind != Kind::Tag {
                return Ok(id);
            }
            id = Tag::decode(&data)?.object;
        }
        bail!("tags nested too deeply at {}", short(&id))
    }

    /// A commit. At a shallow clone's boundary it has no parents (they weren't
    /// fetched), so every walk through history stops there by itself.
    pub fn read_commit(&self, id: &Id) -> Result<Commit> {
        let mut c = Commit::decode(&self.store.get_kind(id, Kind::Commit)?)?;
        if !c.parents.is_empty() && self.shallow().contains(id) {
            c.parents.clear();
        }
        Ok(c)
    }

    /// Follow annotated tags until reaching a commit.
    pub fn peel(&self, mut id: Id) -> Result<Id> {
        for _ in 0..16 {
            let (kind, data) = self.store.get(&id)?;
            match kind {
                Kind::Commit => return Ok(id),
                Kind::Tag => id = Tag::decode(&data)?.object,
                _ => bail!("{} isn't a commit", short(&id)),
            }
        }
        bail!("tags nested too deeply at {}", short(&id))
    }

    // ------------------------------------------------------------ tags

    /// What tag `name` points at: a commit (lightweight tag) or a tag object (annotated).
    pub fn tag(&self, name: &str) -> Result<Option<Id>> {
        if validate_branch(name).is_err() {
            return Ok(None);
        }
        match fs::read_to_string(self.dir.join("tags").join(ref_file(name))) {
            Ok(s) => Ok(Some(parse_hex(s.trim())?)),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn set_tag(&self, name: &str, id: &Id) -> Result<()> {
        validate_branch(name).with_context(|| format!("invalid tag name {name:?}"))?;
        if self.tag(name)?.is_none() {
            check_clash(self.tags()?.into_iter().map(|(t, _)| t), name, "tag")?;
        }
        fs::create_dir_all(self.dir.join("tags"))?;
        write_atomic(&self.dir.join("tags").join(ref_file(name)), format!("{}\n", id.to_hex()).as_bytes())
    }

    pub fn delete_tag(&self, name: &str) -> Result<bool> {
        if validate_branch(name).is_err() {
            return Ok(false);
        }
        match fs::remove_file(self.dir.join("tags").join(ref_file(name))) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    /// All tags, sorted by name.
    pub fn tags(&self) -> Result<Vec<(String, Id)>> {
        let mut out = Vec::new();
        let Ok(entries) = fs::read_dir(self.dir.join("tags")) else {
            return Ok(out);
        };
        for entry in entries {
            let name = ref_name(&entry?.file_name().to_string_lossy());
            if let Some(id) = self.tag(&name)? {
                out.push((name, id));
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }

    /// Move branch `name` from `old` to `new`, failing if someone else moved it first.
    /// The lock file makes concurrent pushes safe.
    pub fn cas_branch(&self, name: &str, old: Option<Id>, new: &Id) -> Result<()> {
        validate_branch(name)?;
        if old.is_none() {
            check_clash(self.branches()?.into_iter().map(|(b, _)| b), name, "branch")?;
        }
        let path = self.dir.join("refs").join(ref_file(name));
        let lock = PathBuf::from(format!("{}.lock", path.display()));
        let mut file = match fs::OpenOptions::new().write(true).create_new(true).open(&lock) {
            Ok(f) => f,
            Err(e) if e.kind() == ErrorKind::AlreadyExists => bail!("{name} is being updated by someone else; try again"),
            Err(e) => return Err(e.into()),
        };
        let result = (|| {
            if self.branch(name)? != old {
                bail!("{name} changed while you were pushing; pull and try again");
            }
            file.write_all(format!("{}\n", new.to_hex()).as_bytes())?;
            file.sync_all()?;
            fs::rename(&lock, &path)?;
            crate::reflog::record(&self.dir, name, old, Some(*new))?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&lock);
        }
        result
    }

    // ------------------------------------------------------------ remotes

    /// Configured remotes as (name, url), from `.copse/remotes`.
    pub fn remotes(&self) -> Result<Vec<(String, String)>> {
        match fs::read_to_string(self.dir.join("remotes")) {
            Ok(text) => Ok(text
                .lines()
                .filter_map(|l| l.split_once(' '))
                .map(|(n, u)| (n.to_string(), u.to_string()))
                .collect()),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e.into()),
        }
    }

    pub fn save_remotes(&self, remotes: &[(String, String)]) -> Result<()> {
        let text: String = remotes.iter().map(|(n, u)| format!("{n} {u}\n")).collect();
        write_atomic(&self.dir.join("remotes"), text.as_bytes())
    }

    pub fn remote_url(&self, name: &str) -> Result<String> {
        self.remotes()?
            .into_iter()
            .find(|(n, _)| n == name)
            .map(|(_, u)| u)
            .with_context(|| format!("there is no remote called {name}; add one with `copse remote add {name} <url>`"))
    }

    /// Where `remote`'s `branch` was the last time we fetched or pushed (shown as remote/branch).
    pub fn tracking(&self, remote: &str, branch: &str) -> Result<Option<Id>> {
        if validate_remote(remote).is_err() || validate_branch(branch).is_err() {
            return Ok(None);
        }
        match fs::read_to_string(self.dir.join("tracking").join(remote).join(ref_file(branch))) {
            Ok(s) => Ok(Some(parse_hex(s.trim())?)),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Remove a branch name (not its commits). Returns whether it existed.
    pub fn delete_branch(&self, name: &str) -> Result<bool> {
        validate_branch(name)?;
        let before = self.branch(name)?;
        match fs::remove_file(self.dir.join("refs").join(ref_file(name))) {
            Ok(()) => {
                crate::reflog::record(&self.dir, name, before, None)?; // kept, so it can be found again
                Ok(true)
            }
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    /// Delete branch `name` only if it still points at `old` (for pushes that delete).
    pub fn cas_delete_branch(&self, name: &str, old: &Id) -> Result<()> {
        validate_branch(name)?;
        let path = self.dir.join("refs").join(ref_file(name));
        let lock = PathBuf::from(format!("{}.lock", path.display()));
        match fs::OpenOptions::new().write(true).create_new(true).open(&lock) {
            Ok(_) => {}
            Err(e) if e.kind() == ErrorKind::AlreadyExists => bail!("{name} is being updated by someone else; try again"),
            Err(e) => return Err(e.into()),
        }
        let result = (|| {
            if self.branch(name)? != Some(*old) {
                bail!("{name} changed since you looked; fetch and try again");
            }
            fs::remove_file(&path)?;
            crate::reflog::record(&self.dir, name, Some(*old), None)?;
            Ok(())
        })();
        let _ = fs::remove_file(&lock);
        result
    }

    pub fn delete_tracking(&self, remote: &str, branch: &str) -> Result<()> {
        validate_remote(remote)?;
        validate_branch(branch)?;
        match fs::remove_file(self.dir.join("tracking").join(remote).join(ref_file(branch))) {
            Err(e) if e.kind() != ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        }
    }

    pub fn set_tracking(&self, remote: &str, branch: &str, id: &Id) -> Result<()> {
        validate_remote(remote)?;
        validate_branch(branch)?;
        let dir = self.dir.join("tracking").join(remote);
        fs::create_dir_all(&dir)?;
        write_atomic(&dir.join(ref_file(branch)), format!("{}\n", id.to_hex()).as_bytes())
    }

    /// All remote-tracking refs as ("remote/branch", id).
    pub fn tracking_refs(&self) -> Result<Vec<(String, Id)>> {
        let mut out = Vec::new();
        let Ok(remotes) = fs::read_dir(self.dir.join("tracking")) else {
            return Ok(out);
        };
        for remote in remotes {
            let remote = remote?.file_name().to_string_lossy().into_owned();
            for branch in fs::read_dir(self.dir.join("tracking").join(&remote))? {
                let branch = ref_name(&branch?.file_name().to_string_lossy());
                if let Some(id) = self.tracking(&remote, &branch)? {
                    out.push((format!("{remote}/{branch}"), id));
                }
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }
}

fn init_dir(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir.join("refs"))?;
    // Pre-create all 256 fan-out directories so object writes never need mkdir.
    for i in 0..256u32 {
        fs::create_dir_all(dir.join("objects").join(format!("{i:02x}")))?;
    }
    fs::write(dir.join("HEAD"), "ref: main\n")?;
    Ok(())
}

/// Branch and tag names follow git's rules, so anything git accepts (such as
/// `feature/login`) works, and anything copse creates can be pushed to git.
pub fn validate_branch(name: &str) -> Result<()> {
    let bad = name.is_empty()
        || name == "HEAD"
        || name == "@"
        || name.starts_with('-')
        || name.starts_with('/')
        || name.ends_with('/')
        || name.ends_with('.')
        || name.contains("//")
        || name.contains("..")
        || name.contains("@{")
        || name.chars().any(|c| matches!(c, '\\' | '~' | '^' | ':' | '?' | '*' | '[') || c.is_whitespace() || c.is_control())
        || name.split('/').any(|part| part.starts_with('.') || part.ends_with(".lock"));
    if bad {
        bail!("invalid name {name:?}: names can use slashes (feature/login), but not spaces, `..`, `~^:?*[\\`, or parts starting with `.`");
    }
    Ok(())
}

/// Remote names can't contain slashes, so `origin/feature/login` always means
/// branch `feature/login` of remote `origin`.
pub fn validate_remote(name: &str) -> Result<()> {
    validate_branch(name)?;
    if name.contains('/') {
        bail!("invalid remote name {name:?}: remote names can't contain slashes");
    }
    Ok(())
}

/// Each branch, tag or remote-tracking name is stored as one file. A `/` in the
/// name is encoded, so `feature/login` is the file `feature%2Flogin`: no nested
/// folders, no empty folders left behind, no file-versus-folder clashes.
pub fn ref_file(name: &str) -> String {
    name.replace('%', "%25").replace('/', "%2F")
}

pub fn ref_name(file: &str) -> String {
    file.replace("%2F", "/").replace("%25", "%")
}

/// git can't have both `feature` and `feature/login` (one would have to be both a
/// file and a folder), so copse refuses too: everything stays pushable to git.
fn check_clash(existing: impl Iterator<Item = String>, name: &str, kind: &str) -> Result<()> {
    for other in existing {
        if other != name && (other.starts_with(&format!("{name}/")) || name.starts_with(&format!("{other}/"))) {
            bail!("can't create {kind} {name}: it clashes with the existing {kind} {other} (git can't have both)");
        }
    }
    Ok(())
}

/// Write via a temp file and rename, so a crash never leaves a half-written file.
/// Replace a small state file (a ref, HEAD, the index...) all at once, crash-safely:
/// objects it may name are flushed first, then the new contents, then the rename.
/// An existing lock belongs to another writer (or an interrupted operation) and
/// must never be overwritten or removed by this writer.
pub fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    use std::io::Write;
    crate::durable::flush_pending()?;
    let mut tmp = OsString::from(path.as_os_str());
    tmp.push(".lock");
    let tmp = PathBuf::from(tmp);
    let mut f = match fs::OpenOptions::new().write(true).create_new(true).open(&tmp) {
        Ok(f) => f,
        Err(e) if e.kind() == ErrorKind::AlreadyExists => {
            bail!("cannot lock {}: {} already exists (another operation may be running)", path.display(), tmp.display());
        }
        Err(e) => return Err(e).with_context(|| format!("creating lock {}", tmp.display())),
    };
    let result = (|| -> Result<()> {
        f.write_all(data)?;
        crate::durable::sync_file_fully(&f)?;
        drop(f);
        fs::rename(&tmp, path)?;
        Ok(())
    })();
    if let Err(e) = result {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    // The rename released our lock. A later error must not remove a new
    // writer's lock at the same path.
    if let Some(dir) = path.parent() {
        crate::durable::sync_dir(dir)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn atomic_test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("copse-atomic-{name}-{}", std::process::id()));
        fs::create_dir(&dir).unwrap();
        dir
    }

    #[test]
    fn atomic_write_preserves_another_writers_lock() {
        let dir = atomic_test_dir("contention");
        let path = dir.join("state");
        let lock = dir.join("state.lock");
        fs::write(&path, b"original").unwrap();
        let mut owner = fs::OpenOptions::new().write(true).create_new(true).open(&lock).unwrap();
        owner.write_all(b"pending").unwrap();

        assert!(write_atomic(&path, b"competing").is_err());
        assert_eq!(fs::read(&path).unwrap(), b"original");
        assert_eq!(fs::read(&lock).unwrap(), b"pending");

        // The writer holding the lock can still finish publishing its own bytes.
        owner.write_all(b" update").unwrap();
        drop(owner);
        fs::rename(&lock, &path).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"pending update");
        write_atomic(&path, b"next").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"next");
        assert!(!lock.exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn atomic_write_cleans_its_lock_when_rename_fails() {
        let dir = atomic_test_dir("failure");
        let path = dir.join("state");
        fs::create_dir(&path).unwrap();
        fs::write(path.join("keep"), b"original").unwrap();

        assert!(write_atomic(&path, b"replacement").is_err());
        assert_eq!(fs::read(path.join("keep")).unwrap(), b"original");
        assert!(!dir.join("state.lock").exists());
        fs::remove_file(path.join("keep")).unwrap();
        fs::remove_dir(path).unwrap();
        write_atomic(&dir.join("state"), b"retry").unwrap();
        assert_eq!(fs::read(dir.join("state")).unwrap(), b"retry");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn atomic_write_does_not_follow_a_lock_symlink() {
        let dir = atomic_test_dir("symlink");
        let path = dir.join("state");
        let target = dir.join("other");
        let lock = dir.join("state.lock");
        fs::write(&path, b"original").unwrap();
        fs::write(&target, b"untouched").unwrap();
        std::os::unix::fs::symlink(&target, &lock).unwrap();

        assert!(write_atomic(&path, b"replacement").is_err());
        assert_eq!(fs::read(&path).unwrap(), b"original");
        assert_eq!(fs::read(&target).unwrap(), b"untouched");
        assert!(fs::symlink_metadata(&lock).unwrap().file_type().is_symlink());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn names_follow_gits_rules() {
        for ok in ["main", "feature/login", "release/1.0", "users/me/wip", "fix-123", "v1.0", "a%b"] {
            assert!(validate_branch(ok).is_ok(), "{ok} should be allowed");
        }
        for bad in ["", "HEAD", "@", "-x", "/x", "x/", "x//y", "a..b", "x.", ".hidden", "a/.b", "x.lock", "a/b.lock",
                    "a b", "a~1", "a^", "a:b", "a?", "a*", "a[b", "a\\b", "a@{1}"] {
            assert!(validate_branch(bad).is_err(), "{bad:?} should be refused");
        }
        assert!(validate_remote("origin").is_ok());
        assert!(validate_remote("my/remote").is_err());
    }

    #[test]
    fn names_round_trip_through_file_names() {
        for name in ["main", "feature/login", "a/b/c", "100%", "x%2Fy", "%25"] {
            let file = ref_file(name);
            assert!(!file.contains('/'), "{file} must be a single file name");
            assert_eq!(ref_name(&file), name);
        }
    }

    #[test]
    fn clashing_names_are_refused() {
        let existing = || ["feature".to_string(), "fix/one".to_string()].into_iter();
        assert!(check_clash(existing(), "feature/login", "branch").is_err());
        assert!(check_clash(existing(), "fix", "branch").is_err());
        assert!(check_clash(existing(), "fix/two", "branch").is_ok());
        assert!(check_clash(existing(), "features", "branch").is_ok());
    }
}
