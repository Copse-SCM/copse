//! A `.git` view: a real git repository kept next to `.copse`, so editors and tools
//! that read `.git` directly (VS Code's change markers, blame, history views, `git
//! status`, `git log`) see the same branches, history and changes copse does.
//!
//!   copse git-view on      create it (and keep it up to date after every command)
//!   copse git-view off     remove it
//!   copse git-view         say whether it's on
//!
//! copse writes git's own formats: loose objects (zlib), refs, HEAD, and an index
//! matching copse's (so changes since the last commit show as unstaged changes).
//! Updates are incremental: only commits not already in the view are written.
//!
//! The view is read-only for git. A permanent `.git/index.lock` stops every git
//! command that would change files or the index (checkout, commit, merge, reset,
//! stash) before it touches anything, while read-only ones (status, log, diff,
//! blame) carry on without it; a `reference-transaction` hook refuses branch and
//! tag moves too. Commit, switch and merge with copse; read with anything.

use crate::gitwire::{GitId, GitMap, Translator};
use crate::objects::{flatten, Id, Kind, Mode};
use crate::repo::{Head, Repo};
use crate::worktree::Index;
use anyhow::{bail, Context, Result};
use flate2::{write::ZlibEncoder, Compression};
use sha1::{Digest, Sha1};
use std::fs;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

const MARKER: &str = "gitview";
const READ_ONLY_HOOK: &str = "#!/bin/sh\n\
# This .git is a read-only view kept by copse (`copse git-view`).\n\
# Branch and HEAD moves made with git are refused, so history can't fork.\n\
# (An empty update, as git makes when it only fetches missing contents, is fine.)\n\
if [ \"$1\" = prepared ] && [ -z \"$COPSE_GITVIEW_WRITE\" ] && grep -q . ; then\n\
  echo \"This .git folder is a read-only view kept by copse. Use copse to commit, switch branches or merge.\" >&2\n\
  exit 1\n\
fi\n";

fn hex(g: &GitId) -> String {
    g.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn is_on(repo: &Repo) -> bool {
    repo.dir.join(MARKER).exists()
}

fn git_dir(repo: &Repo) -> PathBuf {
    crate::objects::disk_path(&repo.root, ".git")
}

/// Write one object in git's loose format, unless it's already there.
fn write_object(git: &Path, kind: &str, body: &[u8]) -> Result<GitId> {
    let header = format!("{kind} {}\0", body.len());
    let mut h = Sha1::new();
    h.update(header.as_bytes());
    h.update(body);
    let id: GitId = h.finalize().into();
    let x = hex(&id);
    let path = git.join("objects").join(&x[..2]).join(&x[2..]);
    if path.exists() {
        return Ok(id);
    }
    fs::create_dir_all(path.parent().unwrap())?;
    let mut z = ZlibEncoder::new(Vec::with_capacity(body.len() / 2 + 32), Compression::fast());
    z.write_all(header.as_bytes())?;
    z.write_all(body)?;
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, z.finish()?)?;
    fs::rename(&tmp, &path)?;
    Ok(id)
}

/// Every commit/tag a ref names: branches, tags, remote branches, HEAD.
fn tips(repo: &Repo) -> Result<Vec<Id>> {
    let mut t: Vec<Id> = repo.branches()?.into_iter().map(|(_, id)| id).collect();
    t.extend(repo.tags()?.into_iter().map(|(_, id)| id));
    t.extend(repo.tracking_refs()?.into_iter().map(|(_, id)| id));
    t.extend(repo.head_commit()?);
    t.sort_unstable_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
    t.dedup();
    Ok(t)
}

/// Write objects as one git pack (with its index) marked as coming from the promisor
/// remote. That's how git stores a partial clone's objects, and it tells git that
/// files they mention but that aren't here are on the remote, not lost.
fn write_promisor_pack(git: &Path, objects: &[(u8, Vec<u8>)]) -> Result<()> {
    use flate2::{write::ZlibEncoder, Compression, Crc};
    use std::io::Write;
    if objects.is_empty() {
        return Ok(());
    }
    let names = ["", "commit", "tree", "blob", "tag"];
    let mut pack = Vec::new();
    pack.extend_from_slice(b"PACK");
    pack.extend_from_slice(&2u32.to_be_bytes());
    pack.extend_from_slice(&(objects.len() as u32).to_be_bytes());
    let mut entries: Vec<(GitId, u32, u64)> = Vec::with_capacity(objects.len()); // id, crc, offset
    for (kind, body) in objects {
        let id = crate::gitwire::git_hash(names[*kind as usize], body);
        let offset = pack.len() as u64;
        let mut header = Vec::new();
        let (mut size, mut byte) = (body.len() >> 4, ((*kind) << 4) | (body.len() & 0x0f) as u8);
        while size > 0 {
            header.push(byte | 0x80);
            byte = (size & 0x7f) as u8;
            size >>= 7;
        }
        header.push(byte);
        let mut z = ZlibEncoder::new(header, Compression::fast());
        z.write_all(body)?;
        let entry = z.finish()?;
        let mut crc = Crc::new();
        crc.update(&entry);
        pack.extend_from_slice(&entry);
        entries.push((id, crc.sum(), offset));
    }
    let pack_sum: [u8; 20] = Sha1::digest(&pack).into();
    pack.extend_from_slice(&pack_sum);
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    entries.dedup_by(|a, b| a.0 == b.0);
    let mut idx = Vec::new();
    idx.extend_from_slice(&[0xff, b't', b'O', b'c']);
    idx.extend_from_slice(&2u32.to_be_bytes());
    let mut count = 0u32;
    for first in 0..=255u8 {
        count += entries.iter().filter(|e| e.0[0] == first).count() as u32;
        idx.extend_from_slice(&count.to_be_bytes());
    }
    for e in &entries {
        idx.extend_from_slice(&e.0);
    }
    for e in &entries {
        idx.extend_from_slice(&e.1.to_be_bytes());
    }
    for e in &entries {
        anyhow::ensure!(e.2 < 0x8000_0000, "the .git view's pack would be over 2 GB");
        idx.extend_from_slice(&(e.2 as u32).to_be_bytes());
    }
    idx.extend_from_slice(&pack_sum);
    let idx_sum: [u8; 20] = Sha1::digest(&idx).into();
    idx.extend_from_slice(&idx_sum);
    let dir = git.join("objects").join("pack");
    fs::create_dir_all(&dir)?;
    let name = format!("pack-{}", hex(&pack_sum));
    fs::write(dir.join(format!("{name}.pack")), &pack)?;
    fs::write(dir.join(format!("{name}.promisor")), b"")?;
    fs::write(dir.join(format!("{name}.idx")), &idx)?; // last: it makes the pack visible
    Ok(())
}

/// A value or section name in git's config syntax: quoted, so `#`, `;` and
/// spaces are kept, with `"` and `\` escaped. None for a newline, which can't be.
fn quoted(v: &str) -> Option<String> {
    if v.contains(['\n', '\r', '\0']) {
        return None;
    }
    Some(format!("\"{}\"", v.replace('\\', "\\\\").replace('"', "\\\"")))
}

/// `.git/config`: git's own settings, plus copse's git remotes (so tools such as
/// gh, and editors' remote and ahead/behind views, know where the code lives) and,
/// for each branch origin also has, origin's as its upstream (as `copse status`
/// treats it). Copse-only remotes are left out, since git can't talk to them.
/// Rewritten only when it would change.
fn write_config(repo: &Repo, git: &Path) -> Result<()> {
    let partial = repo.is_partial().then(|| fs::read_to_string(repo.dir.join("promisor")).unwrap_or_default().trim().to_string());
    let remotes: Vec<(String, String)> = repo
        .remotes()?
        .into_iter()
        .filter_map(|(name, url)| Some((name, crate::remote::git_url(&url)?.to_string())))
        .collect();
    if let Some(name) = &partial {
        if !remotes.iter().any(|(n, _)| n == name) {
            bail!("the .git view needs every file's contents, and this is a partial clone of a copse repository (git can't fetch from it); `copse git-view off` to stop trying");
        }
    }
    let mut text = format!(
        "[core]\n\trepositoryformatversion = {}\n\tfilemode = true\n\tbare = false\n\tlogallrefupdates = false\n",
        if partial.is_some() { 1 } else { 0 }
    );
    if let Some(name) = &partial {
        text += &format!("[extensions]\n\tpartialClone = {name}\n");
    }
    for (name, url) in &remotes {
        let fetch = format!("+refs/heads/*:refs/remotes/{name}/*");
        let (Some(qname), Some(qurl), Some(qfetch)) = (quoted(name), quoted(url), quoted(&fetch)) else { continue };
        text += &format!("[remote {qname}]\n\turl = {qurl}\n\tfetch = {qfetch}\n");
        if partial.as_deref() == Some(name.as_str()) {
            text += "\tpromisor = true\n\tpartialclonefilter = blob:none\n";
        }
    }
    if remotes.iter().any(|(n, _)| n == "origin") {
        for (branch, _) in repo.branches()? {
            if repo.tracking("origin", &branch)?.is_none() {
                continue;
            }
            let (Some(qbranch), Some(merge)) = (quoted(&branch), quoted(&format!("refs/heads/{branch}"))) else { continue };
            text += &format!("[branch {qbranch}]\n\tremote = origin\n\tmerge = {merge}\n");
        }
    }
    let path = git.join("config");
    if fs::read_to_string(&path).ok().as_deref() != Some(text.as_str()) {
        crate::repo::write_atomic(&path, text.as_bytes())?;
    }
    Ok(())
}

/// Bring the view up to date. Cheap when nothing changed.
pub fn sync(repo: &Repo) -> Result<()> {
    if repo.is_bare() || !is_on(repo) {
        return Ok(());
    }
    let git = git_dir(repo);
    {
        // Keep the read-only hook current (older views had a stricter one).
        use std::os::unix::fs::PermissionsExt;
        let hook = git.join("hooks").join("reference-transaction");
        if fs::read_to_string(&hook).ok().as_deref() != Some(READ_ONLY_HOOK) {
            fs::create_dir_all(hook.parent().unwrap())?;
            fs::write(&hook, READ_ONLY_HOOK)?;
            fs::set_permissions(&hook, fs::Permissions::from_mode(0o755))?;
        }
    }
    write_config(repo, &git)?;
    // A shallow clone: git is told where history stops (.git/shallow), by git id.
    let shallow = repo.shallow();
    if shallow.is_empty() {
        let _ = fs::remove_file(git.join("shallow"));
    } else {
        let map = GitMap::load(repo)?;
        if !map.knows_all(&shallow) {
            bail!("the .git view needs all of history (this is a shallow clone of a copse repository); `copse fetch --unshallow`, or `copse git-view off`");
        }
        let mut lines: Vec<String> = shallow.iter().filter_map(|id| map.git(id)).map(|g| format!("{}\n", hex(&g))).collect();
        lines.sort();
        fs::write(git.join("shallow"), lines.concat())?;
    }
    let tips = tips(repo)?;

    // Objects: everything the refs reach that an earlier sync didn't write.
    let done_path = repo.dir.join("gitview-tips");
    let done: Vec<Id> = fs::read_to_string(&done_path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| crate::objects::parse_hex(l).ok())
        .filter(|id| repo.store.contains(id))
        .collect();
    let mut tr = Translator::new(repo)?;
    let new_tips: Vec<Id> = tips.iter().copied().filter(|t| !done.contains(t)).collect();
    if !new_tips.is_empty() {
        for t in &new_tips {
            tr.translate_any(*t)?;
        }
        let objects = crate::transport::objects_to_send(repo, &new_tips, &done)?;
        let partial = repo.is_partial();
        let mut for_pack = Vec::new();
        for id in objects {
            if crate::objects::is_gitlink(&id) && !repo.store.contains(&id) {
                continue; // not downloaded (partial clone): git fetches it from origin when it needs it
            }
            let (kind, body) = tr.git_object(&id)?;
            if partial {
                for_pack.push((kind, body));
                continue;
            }
            let name = match kind {
                1 => "commit",
                2 => "tree",
                3 => "blob",
                _ => "tag",
            };
            write_object(&git, name, &body)?;
        }
        write_promisor_pack(&git, &for_pack)?;
        tr.map.save()?;
        let text: String = tips.iter().map(|t| format!("{}\n", t.to_hex())).collect();
        crate::repo::write_atomic(&done_path, text.as_bytes())?;
    }
    let map = GitMap::load(repo)?;
    let g = |id: &Id| map.git(id).with_context(|| format!("internal error: {} has no git id", crate::objects::short(id)));

    // Refs: rewritten to match copse exactly (stale ones removed).
    let refs = git.join("refs");
    for sub in ["heads", "tags", "remotes"] {
        let _ = fs::remove_dir_all(refs.join(sub));
        fs::create_dir_all(refs.join(sub))?;
    }
    let write_ref = |path: PathBuf, id: &GitId| -> Result<()> {
        fs::create_dir_all(path.parent().unwrap())?;
        fs::write(path, format!("{}\n", hex(id)))?;
        Ok(())
    };
    for (name, id) in repo.branches()? {
        write_ref(refs.join("heads").join(&name), &g(&id)?)?;
    }
    for (name, id) in repo.tags()? {
        write_ref(refs.join("tags").join(&name), &g(&id)?)?;
    }
    for (name, id) in repo.tracking_refs()? {
        write_ref(refs.join("remotes").join(&name), &g(&id)?)?;
    }
    let _ = fs::remove_file(git.join("packed-refs"));
    let head = match repo.head()? {
        Head::Branch(b) => format!("ref: refs/heads/{b}\n"),
        Head::Detached(id) => format!("{}\n", hex(&g(&id)?)),
    };
    fs::write(git.join("HEAD"), head)?;

    write_index(repo, &git, &map)?;
    fs::write(git.join("index.lock"), LOCK_NOTE)?;
    Ok(())
}

const LOCK_NOTE: &str = "This .git folder is a read-only view kept by copse (`copse git-view`).
This lock stops git commands that would change files, so copse stays in charge:
commit, switch branches and merge with copse. `copse git-view off` removes the view.
";

/// git's index (version 3, for the skip-worktree flag): copse's index as git sees
/// it, so `git status` reports exactly copse's changes as unstaged ones.
fn write_index(repo: &Repo, git: &Path, map: &GitMap) -> Result<()> {
    let index = Index::load(&repo.wt)?;
    let sparse = crate::sparse::Sparse::load(&repo.wt);
    // The index describes HEAD's files; their git ids come from HEAD's tree.
    let head_files = match repo.head_commit()? {
        Some(h) => flatten(&repo.store, &repo.read_commit(&h)?.tree)?,
        None => Default::default(),
    };
    let mut entries: Vec<(&String, &crate::worktree::Entry)> = index.entries.iter().collect();
    entries.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    let mut out = Vec::new();
    out.extend_from_slice(b"DIRC");
    out.extend_from_slice(&3u32.to_be_bytes());
    let count_at = out.len();
    out.extend_from_slice(&0u32.to_be_bytes());
    let mut count = 0u32;
    for (path, e) in entries {
        let id = Id::from_bytes(e.id);
        let sha: GitId = match e.mode {
            Mode::Submodule => crate::objects::gitlink_sha(&id),
            // A file named by its git id (partial clone of a git server).
            _ if crate::objects::is_gitlink(&id) => crate::objects::gitlink_sha(&id),
            _ => match map.git(&id) {
                Some(s) => s,
                None if head_files.get(path).map(|(_, i)| *i) == Some(id) => continue, // shouldn't happen
                None => {
                    // Not in HEAD (say, mid-merge): hash it the way git would.
                    let body = repo.store.get_kind(&id, Kind::Blob)?;
                    let mut h = Sha1::new();
                    h.update(format!("blob {}\0", body.len()).as_bytes());
                    h.update(&body);
                    h.finalize().into()
                }
            },
        };
        let md = fs::symlink_metadata(crate::objects::disk_path(&repo.root, path)).ok();
        // The file's real stat goes in only if copse's own index vouches that the
        // file still holds this entry's content (same size, mtime and mode as when
        // copse recorded it). Otherwise git would see a stat matching the file on
        // disk next to the old content's id, trust it, and miss the edit. A blank
        // stat makes git look at the content instead (git does the same for files
        // it can't vouch for).
        let unchanged = md.as_ref().is_some_and(|m| {
            let meta = crate::worktree::stat_of(m);
            e.stat_matches(&meta) && meta.mode == e.mode
        });
        let (ctime, ctime_ns, mtime, mtime_ns, dev, ino, uid, gid, size) = match &md {
            Some(m) if unchanged => (m.ctime(), m.ctime_nsec(), m.mtime(), m.mtime_nsec(), m.dev(), m.ino(), m.uid(), m.gid(), m.size()),
            _ => (0, 0, 0, 0, 0, 0, 0, 0, 0),
        };
        let mode: u32 = match e.mode {
            Mode::File => 0o100644,
            Mode::Exec => 0o100755,
            Mode::Link => 0o120000,
            Mode::Submodule => 0o160000,
            Mode::Dir => continue,
        };
        let start = out.len();
        for v in [ctime as u32, ctime_ns as u32, mtime as u32, mtime_ns as u32, dev as u32, ino as u32, mode, uid, gid, size as u32] {
            out.extend_from_slice(&v.to_be_bytes());
        }
        out.extend_from_slice(&sha);
        let skip = sparse.outside(path) && md.is_none();
        let name_len = path.len().min(0xfff) as u16;
        let flags = name_len | if skip { 0x4000 } else { 0 }; // 0x4000: extended flags follow
        out.extend_from_slice(&flags.to_be_bytes());
        if skip {
            out.extend_from_slice(&0x4000u16.to_be_bytes()); // skip-worktree
        }
        out.extend_from_slice(path.as_bytes());
        // NUL-terminated, padded so the entry's length is a multiple of 8.
        let len = out.len() - start;
        let pad = 8 - (len % 8);
        out.extend(std::iter::repeat(0u8).take(pad));
        count += 1;
    }
    out[count_at..count_at + 4].copy_from_slice(&count.to_be_bytes());
    let digest: GitId = Sha1::digest(&out).into();
    out.extend_from_slice(&digest);
    let tmp = git.join("index.copse-tmp");
    fs::write(&tmp, &out)?;
    fs::rename(&tmp, git.join("index"))?;
    Ok(())
}

pub fn git_view(action: Option<bool>) -> Result<()> {
    let repo = Repo::find()?;
    let git = git_dir(&repo);
    match action {
        None => {
            if is_on(&repo) {
                println!("The .git view is on: tools that read .git see copse's branches, history and changes.");
            } else {
                println!("The .git view is off. `copse git-view on` creates one for editors and tools that read .git.");
            }
        }
        Some(true) => {
            if is_on(&repo) {
                sync(&repo)?;
                println!("The .git view is already on (and now up to date).");
                return Ok(());
            }
            if git.exists() {
                bail!("there's already a .git folder here; copse won't overwrite it. Move it away first if it isn't needed");
            }
            for d in ["objects", "refs/heads", "refs/tags", "hooks", "info"] {
                fs::create_dir_all(git.join(d))?;
            }
            fs::write(git.join("description"), "A read-only view of a copse repository (copse git-view).\n")?;
            // Keep git's own ignore rules out of the way of copse's files.
            fs::write(git.join("info").join("exclude"), ".copse/\n")?;
            let hook = git.join("hooks").join("reference-transaction");
            fs::write(&hook, READ_ONLY_HOOK)?;
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&hook, fs::Permissions::from_mode(0o755))?;
            fs::write(repo.dir.join(MARKER), "on\n")?;
            if let Err(e) = sync(&repo) {
                let _ = fs::remove_file(repo.dir.join(MARKER));
                let _ = fs::remove_dir_all(&git);
                return Err(e);
            }
            println!("Made a .git view. Editors and tools that read .git now see copse's branches, history and changes;");
            println!("copse keeps it up to date. It's read-only for git: commit and switch branches with copse.");
        }
        Some(false) => {
            if !is_on(&repo) {
                println!("The .git view is already off.");
                return Ok(());
            }
            fs::remove_file(repo.dir.join(MARKER))?;
            let _ = fs::remove_file(repo.dir.join("gitview-tips"));
            if git.join("description").exists() && fs::read_to_string(git.join("description"))?.contains("copse git-view") {
                fs::remove_dir_all(&git)?;
            }
            println!("Removed the .git view.");
        }
    }
    Ok(())
}

/// Called after every command: keep the view current, without failing the command.
pub fn after_command() {
    let Ok(repo) = Repo::find() else { return };
    if !is_on(&repo) {
        return;
    }
    if let Err(e) = sync(&repo) {
        eprintln!("copse: warning: couldn't update the .git view: {e:#}");
    }
}
