//! Git LFS: large files kept on an LFS server, with small pointer files in history,
//! exactly as git-lfs does it, so repositories that use LFS work with copse and
//! round-trip unchanged.
//!
//!   copse lfs track "*.psd"     store matching files with LFS (adds to .gitattributes)
//!   copse lfs untrack "*.psd"
//!   copse lfs ls-files          LFS files in the current commit
//!   copse lfs pull              download any missing LFS contents and check them out
//!   copse lfs push [remote]     upload the current branch's LFS contents
//!
//! How it fits together (git-lfs's design):
//!   - history holds a pointer for each LFS file: its SHA-256 and size (spec v1);
//!   - contents live in `.copse/lfs/objects/aa/bb/<sha256>`, and on the LFS server;
//!   - checking out writes the real contents (downloading what's missing);
//!   - snapshots turn your files back into pointers (and cache the contents);
//!   - pushing to a git server first uploads contents the server doesn't have.
//! The server is `lfs.url` (copse config or `.lfsconfig`), else the one git-lfs
//! would use for the remote: `<repository URL>.git/info/lfs`, over HTTPS.

use crate::githttp::Http;
use crate::objects::{flatten, Id, Kind, Mode, Store};
use crate::repo::Repo;
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

const VERSION_LINE: &str = "version https://git-lfs.github.com/spec/v1\n";
const MEDIA: &str = "application/vnd.git-lfs+json";

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Pointer {
    /// SHA-256 of the contents, hex.
    pub oid: String,
    pub size: u64,
}

impl Pointer {
    /// The pointer file's text, byte for byte as git-lfs writes it.
    pub fn text(&self) -> String {
        format!("{VERSION_LINE}oid sha256:{}\nsize {}\n", self.oid, self.size)
    }

    pub fn parse(data: &[u8]) -> Option<Pointer> {
        if data.len() > 1024 {
            return None;
        }
        let text = std::str::from_utf8(data).ok()?;
        let rest = text.strip_prefix(VERSION_LINE).or_else(|| text.strip_prefix("version https://hawser.github.com/spec/v1\n"))?;
        let (mut oid, mut size) = (None, None);
        for line in rest.lines() {
            if let Some(o) = line.strip_prefix("oid sha256:") {
                oid = Some(o.to_string());
            } else if let Some(s) = line.strip_prefix("size ") {
                size = s.parse().ok();
            }
        }
        let oid = oid.filter(|o| o.len() == 64 && o.bytes().all(|b| b.is_ascii_hexdigit()))?;
        Some(Pointer { oid: oid.to_ascii_lowercase(), size: size? })
    }
}

/// Which paths LFS handles: `.gitattributes` lines with `filter=lfs`.
pub struct Attrs {
    matcher: Option<ignore::gitignore::Gitignore>,
}

impl Attrs {
    pub fn load(root: &Path) -> Attrs {
        let Ok(text) = fs::read_to_string(root.join(".gitattributes")) else { return Attrs { matcher: None } };
        Self::from_text(root, &text)
    }

    /// The rules in the `.gitattributes` of a snapshot about to be checked out
    /// (it may not be on disk yet).
    pub fn for_files(repo: &Repo, files: &crate::merge::Files) -> Attrs {
        match files.get(".gitattributes").and_then(|(_, id)| repo.store.get_kind(id, Kind::Blob).ok()) {
            Some(data) => Self::from_text(&repo.root, &String::from_utf8_lossy(&data)),
            None => Attrs { matcher: None },
        }
    }

    fn from_text(root: &Path, text: &str) -> Attrs {
        let mut b = ignore::gitignore::GitignoreBuilder::new(root);
        let mut any = false;
        for line in text.lines() {
            let mut words = line.split_whitespace();
            let Some(pattern) = words.next().filter(|p| !p.starts_with('#')) else { continue };
            let attrs: Vec<&str> = words.collect();
            if attrs.contains(&"filter=lfs") {
                any |= b.add_line(None, pattern).is_ok();
            } else if attrs.iter().any(|a| *a == "-filter" || *a == "!filter") {
                let _ = b.add_line(None, &format!("!{pattern}"));
            }
        }
        Attrs { matcher: if any { b.build().ok() } else { None } }
    }

    pub fn active(&self) -> bool {
        self.matcher.is_some()
    }

    pub fn tracked(&self, path: &str) -> bool {
        self.matcher.as_ref().is_some_and(|m| m.matched(path, false).is_ignore())
    }
}

fn cache_path(dir: &Path, oid: &str) -> PathBuf {
    dir.join("lfs").join("objects").join(&oid[..2]).join(&oid[2..4]).join(oid)
}

pub fn cached(dir: &Path, p: &Pointer) -> bool {
    cache_path(dir, &p.oid).exists()
}

/// "Clean" a working file: its pointer (and with `keep`, put its contents in the
/// cache). A file that's already a pointer (contents never downloaded) stays one.
pub fn clean(dir: &Path, file: &Path, keep: bool) -> Result<Vec<u8>> {
    let mut f = fs::File::open(file)?;
    let mut head = vec![0u8; 1025];
    let n = read_up_to(&mut f, &mut head)?;
    if Pointer::parse(&head[..n]).is_some() {
        return Ok(head[..n].to_vec());
    }
    let mut hasher = Sha256::new();
    let tmp = dir.join("lfs").join("tmp").join(format!("{}-{}", std::process::id(), rand_suffix()));
    let mut out = if keep {
        fs::create_dir_all(tmp.parent().unwrap())?;
        Some(fs::File::create(&tmp)?)
    } else {
        None
    };
    let mut size = 0u64;
    let mut chunk = head[..n].to_vec();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        hasher.update(&chunk);
        if let Some(o) = out.as_mut() {
            o.write_all(&chunk)?;
        }
        size += chunk.len() as u64;
        let m = f.read(&mut buf)?;
        if m == 0 {
            break;
        }
        chunk = buf[..m].to_vec();
    }
    let oid: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
    if let Some(o) = out {
        crate::durable::sync_file(&o)?;
        let dest = cache_path(dir, &oid);
        if dest.exists() {
            fs::remove_file(&tmp)?;
        } else {
            fs::create_dir_all(dest.parent().unwrap())?;
            fs::rename(&tmp, &dest)?;
        }
    }
    Ok(Pointer { oid, size }.text().into_bytes())
}

fn rand_suffix() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    N.fetch_add(1, Ordering::Relaxed)
}

fn read_up_to(f: &mut fs::File, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        let m = f.read(&mut buf[n..])?;
        if m == 0 {
            break;
        }
        n += m;
    }
    Ok(n)
}

/// The blob a working file is stored as, for snapshots: its pointer, if LFS
/// handles that path.
pub fn snapshot_blob(dir: &Path, file: &Path, store: Option<&Store>) -> Result<Id> {
    let pointer = clean(dir, file, store.is_some())?;
    let id = Store::hash(Kind::Blob, &pointer);
    if let Some(s) = store {
        s.put_with_id(&id, Kind::Blob, &pointer)?;
    }
    Ok(id)
}

/// "Smudge": write the contents a pointer stands for to `dest`. False if they
/// aren't downloaded (the pointer is written instead, as git-lfs does when told to skip).
pub fn smudge(dir: &Path, p: &Pointer, dest: &Path) -> Result<bool> {
    let src = cache_path(dir, &p.oid);
    if src.exists() {
        fs::copy(&src, dest).with_context(|| format!("writing {}", dest.display()))?;
        Ok(true)
    } else {
        fs::write(dest, p.text())?;
        Ok(false)
    }
}

// ---------------------------------------------------------------- the server

/// The LFS server: lfs.url (copse config, then `.lfsconfig`), else derived from
/// the remote's address the way git-lfs derives it.
fn endpoint(repo: &Repo, remote: &str) -> Result<String> {
    if let Some(u) = crate::config::get("lfs.url") {
        return Ok(u.trim_end_matches('/').to_string());
    }
    if let Ok(text) = fs::read_to_string(repo.root.join(".lfsconfig")) {
        let mut in_lfs = false;
        for line in text.lines() {
            let t = line.trim();
            if t.starts_with('[') {
                in_lfs = t.trim_matches(['[', ']']).trim().eq_ignore_ascii_case("lfs");
            } else if in_lfs {
                if let Some((k, v)) = t.split_once('=') {
                    if k.trim().eq_ignore_ascii_case("url") {
                        return Ok(v.trim().trim_end_matches('/').to_string());
                    }
                }
            }
        }
    }
    let url = repo.remote_url(remote)?;
    let url = url.strip_prefix("git::").unwrap_or(&url).trim_end_matches('/').to_string();
    let https = if url.starts_with("https://") || url.starts_with("http://") {
        url
    } else if let Some(rest) = url.strip_prefix("ssh://") {
        let rest = rest.split_once('@').map_or(rest, |(_, r)| r);
        format!("https://{}", rest)
    } else if let Some((host, path)) = url.split_once(':').filter(|(h, _)| !h.contains('/')) {
        let host = host.split_once('@').map_or(host, |(_, h)| h);
        format!("https://{host}/{}", path.trim_start_matches('/'))
    } else {
        bail!("{remote} is a local folder, so there's no LFS server to use; set one with `copse config lfs.url URL`");
    };
    let base = if https.ends_with(".git") { https } else { format!("{https}.git") };
    Ok(format!("{base}/info/lfs"))
}

fn default_remote(repo: &Repo) -> Result<String> {
    let remotes = repo.remotes()?;
    if remotes.iter().any(|(n, _)| n == "origin") {
        return Ok("origin".into());
    }
    remotes.into_iter().next().map(|(n, _)| n).context("there's no remote to get LFS files from; set `copse config lfs.url URL`")
}

/// One batch API call: which objects to download or upload, and where.
fn batch(http: &mut Http, url: &str, operation: &str, pointers: &[Pointer]) -> Result<Vec<Value>> {
    let body = json!({
        "operation": operation,
        "transfers": ["basic"],
        "objects": pointers.iter().map(|p| json!({"oid": p.oid, "size": p.size})).collect::<Vec<_>>(),
    })
    .to_string();
    let target = format!("{url}/objects/batch");
    let resp = http.send(|agent, auth| {
        let mut req = agent.post(&target).set("Accept", MEDIA).set("Content-Type", MEDIA);
        if let Some(a) = auth {
            req = req.set("Authorization", a);
        }
        req.send_string(&body)
    })?;
    let v: Value = serde_json::from_reader(resp.into_reader()).context("the LFS server's reply isn't valid JSON")?;
    Ok(v.get("objects").and_then(Value::as_array).cloned().unwrap_or_default())
}

fn request_with_headers(agent: &ureq::Agent, method: &str, href: &str, headers: &Value) -> ureq::Request {
    let mut req = agent.request(method, href);
    if let Some(h) = headers.as_object() {
        for (k, v) in h {
            if let Some(v) = v.as_str() {
                req = req.set(k, v);
            }
        }
    }
    req
}

/// Download contents into the cache. Returns how many arrived.
pub fn download(repo: &Repo, pointers: &[Pointer]) -> Result<usize> {
    let missing: Vec<Pointer> = pointers.iter().filter(|p| !cached(&repo.dir, p)).cloned().collect();
    if missing.is_empty() {
        return Ok(0);
    }
    let remote = default_remote(repo)?;
    let url = endpoint(repo, &remote)?;
    let mut http = Http::new(&url, "upload-pack");
    let agent = ureq::AgentBuilder::new().user_agent(&crate::githttp::user_agent()).build();
    let mut got = 0;
    for obj in batch(&mut http, &url, "download", &missing)? {
        let oid = obj["oid"].as_str().unwrap_or_default().to_string();
        let Some(p) = missing.iter().find(|p| p.oid == oid) else { continue };
        if let Some(err) = obj.get("error") {
            bail!("the LFS server couldn't send {}: {}", &oid[..12], err["message"].as_str().unwrap_or("unknown error"));
        }
        let action = &obj["actions"]["download"];
        let href = action["href"].as_str().with_context(|| format!("the LFS server gave no download address for {}", &oid[..12]))?;
        let resp = request_with_headers(&agent, "GET", href, &action["header"])
            .call()
            .map_err(|e| anyhow::anyhow!("downloading LFS object {}: {e}", &oid[..12]))?;
        // Check what arrived really is that object before keeping it.
        let tmp = repo.dir.join("lfs").join("tmp").join(format!("dl-{}-{}", std::process::id(), rand_suffix()));
        fs::create_dir_all(tmp.parent().unwrap())?;
        let mut out = fs::File::create(&tmp)?;
        let mut hasher = Sha256::new();
        let mut reader = resp.into_reader();
        let mut buf = vec![0u8; 1 << 20];
        let mut size = 0u64;
        loop {
            let n = reader.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            out.write_all(&buf[..n])?;
            size += n as u64;
        }
        let actual: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
        if actual != p.oid || size != p.size {
            let _ = fs::remove_file(&tmp);
            bail!("LFS object {} arrived damaged (its contents don't match its id)", &oid[..12]);
        }
        let dest = cache_path(&repo.dir, &p.oid);
        fs::create_dir_all(dest.parent().unwrap())?;
        fs::rename(&tmp, &dest)?;
        got += 1;
    }
    Ok(got)
}

/// Upload cached contents the server doesn't have yet. Returns how many were sent.
pub fn upload(repo: &Repo, remote: &str, pointers: &[Pointer]) -> Result<usize> {
    if pointers.is_empty() {
        return Ok(0);
    }
    for p in pointers {
        if !cached(&repo.dir, p) {
            bail!("the contents of LFS object {} aren't here, so they can't be uploaded (`copse lfs pull` first?)", &p.oid[..12]);
        }
    }
    let url = endpoint(repo, remote)?;
    let mut http = Http::new(&url, "receive-pack");
    let objects = batch(&mut http, &url, "upload", pointers)?;
    let agent = ureq::AgentBuilder::new().user_agent(&crate::githttp::user_agent()).build();
    let mut sent = 0;
    for obj in objects {
        let oid = obj["oid"].as_str().unwrap_or_default().to_string();
        let Some(p) = pointers.iter().find(|p| p.oid == oid) else { continue };
        if let Some(err) = obj.get("error") {
            bail!("the LFS server won't take {}: {}", &oid[..12], err["message"].as_str().unwrap_or("unknown error"));
        }
        let Some(action) = obj["actions"].get("upload") else { continue }; // already there
        let href = action["href"].as_str().context("the LFS server gave no upload address")?;
        let data = fs::read(cache_path(&repo.dir, &p.oid))?;
        request_with_headers(&agent, "PUT", href, &action["header"])
            .set("Content-Type", "application/octet-stream")
            .send_bytes(&data)
            .map_err(|e| anyhow::anyhow!("uploading LFS object {}: {e}", &oid[..12]))?;
        if let Some(verify) = obj["actions"].get("verify") {
            let href = verify["href"].as_str().unwrap_or_default();
            request_with_headers(&agent, "POST", href, &verify["header"])
                .set("Content-Type", MEDIA)
                .send_string(&json!({"oid": p.oid, "size": p.size}).to_string())
                .map_err(|e| anyhow::anyhow!("verifying LFS object {}: {e}", &oid[..12]))?;
        }
        sent += 1;
    }
    Ok(sent)
}

/// LFS pointers among these blobs (small ones only are read).
pub fn pointers_in(repo: &Repo, blobs: &[Id]) -> Result<Vec<Pointer>> {
    let mut out = Vec::new();
    for id in blobs {
        if repo.store.stored_len(id).unwrap_or(u64::MAX) > 2048 {
            continue;
        }
        if let Ok((Kind::Blob, data)) = repo.store.get(id) {
            if let Some(p) = Pointer::parse(&data) {
                if !out.contains(&p) {
                    out.push(p);
                }
            }
        }
    }
    Ok(out)
}

/// Before pushing to a git server: upload the LFS contents in the objects being sent.
pub fn before_push(repo: &Repo, remote: &str, objects: &[Id]) -> Result<()> {
    let pointers = pointers_in(repo, objects)?;
    if pointers.is_empty() {
        return Ok(());
    }
    let n = upload(repo, remote, &pointers)?;
    if n > 0 {
        println!("Uploaded {n} LFS file(s)");
    }
    Ok(())
}

/// Before a checkout writes files: download LFS contents it'll need, in one go.
/// Problems are warnings: the pointers get written instead, and `copse lfs pull`
/// can try again.
pub fn before_checkout(repo: &Repo, attrs: &Attrs, files: &[(&str, Mode, Id)]) {
    if !attrs.active() {
        return;
    }
    let ids: Vec<Id> = files.iter().filter(|(p, m, _)| matches!(m, Mode::File | Mode::Exec) && attrs.tracked(p)).map(|(_, _, id)| *id).collect();
    let pointers = match pointers_in(repo, &ids) {
        Ok(p) => p,
        Err(_) => return,
    };
    if pointers.iter().all(|p| cached(&repo.dir, p)) {
        return;
    }
    match download(repo, &pointers) {
        Ok(n) if n > 0 => eprintln!("Downloaded {n} LFS file(s)"),
        Ok(_) => {}
        Err(e) => eprintln!("copse: warning: LFS files couldn't be downloaded, so their pointers were written instead ({e:#}). `copse lfs pull` tries again."),
    }
}

/// After a checkout wrote files: LFS pointers it wrote as text (because the rules
/// weren't on disk yet) become their contents. Returns the paths rewritten.
pub fn after_checkout(repo: &Repo, attrs: &Attrs, files: &[(&str, Mode, Id)]) -> Result<Vec<String>> {
    let mut done = Vec::new();
    if !attrs.active() {
        return Ok(done);
    }
    for (path, mode, id) in files {
        if !matches!(mode, Mode::File | Mode::Exec) || !attrs.tracked(path) {
            continue;
        }
        if let Some(p) = pointers_in(repo, &[*id])?.into_iter().next() {
            let full = repo.root.join(path);
            let is_pointer = fs::read(&full).map(|d| Pointer::parse(&d).is_some()).unwrap_or(false);
            if is_pointer && cached(&repo.dir, &p) {
                smudge(&repo.dir, &p, &full)?;
                done.push(path.to_string());
            }
        }
    }
    Ok(done)
}

// ---------------------------------------------------------------- commands

pub enum LfsCmd<'a> {
    Track(&'a [String]),
    Untrack(&'a [String]),
    LsFiles,
    Pull,
    Push(Option<&'a str>),
}

pub fn lfs(cmd: LfsCmd) -> Result<()> {
    let repo = Repo::find()?;
    let attrs_path = repo.root.join(".gitattributes");
    match cmd {
        LfsCmd::Track(patterns) => {
            if patterns.is_empty() {
                let text = fs::read_to_string(&attrs_path).unwrap_or_default();
                let tracked: Vec<&str> = text.lines().filter(|l| l.contains("filter=lfs")).collect();
                if tracked.is_empty() {
                    println!("No LFS patterns yet. `copse lfs track \"*.psd\"` adds one.");
                }
                for l in tracked {
                    println!("  {}", l.split_whitespace().next().unwrap_or(""));
                }
                return Ok(());
            }
            let mut text = fs::read_to_string(&attrs_path).unwrap_or_default();
            for p in patterns {
                if text.lines().any(|l| l.split_whitespace().next() == Some(p.as_str()) && l.contains("filter=lfs")) {
                    println!("{p} is already tracked by LFS.");
                    continue;
                }
                if !text.is_empty() && !text.ends_with('\n') {
                    text.push('\n');
                }
                text += &format!("{p} filter=lfs diff=lfs merge=lfs -text\n");
                println!("Tracking {p} with LFS.");
            }
            fs::write(&attrs_path, text)?;
            println!("Commit .gitattributes (and the files) to record it.");
        }
        LfsCmd::Untrack(patterns) => {
            let text = fs::read_to_string(&attrs_path).unwrap_or_default();
            let kept: Vec<&str> =
                text.lines().filter(|l| !(l.contains("filter=lfs") && patterns.iter().any(|p| l.split_whitespace().next() == Some(p.as_str())))).collect();
            fs::write(&attrs_path, if kept.is_empty() { String::new() } else { format!("{}\n", kept.join("\n")) })?;
            println!("Untracked {}. Commit .gitattributes to record it.", patterns.join(", "));
        }
        LfsCmd::LsFiles => {
            let head = repo.head_commit()?.context("there are no commits yet")?;
            for (path, (mode, id)) in flatten(&repo.store, &repo.read_commit(&head)?.tree)? {
                if mode == Mode::Submodule || mode == Mode::Dir {
                    continue;
                }
                if let Some(p) = pointers_in(&repo, &[id])?.into_iter().next() {
                    let mark = if cached(&repo.dir, &p) { "*" } else { "-" };
                    println!("{} {mark} {path}  ({} bytes)", &p.oid[..10], p.size);
                }
            }
        }
        LfsCmd::Pull => {
            let head = repo.head_commit()?.context("there are no commits yet")?;
            let tree = flatten(&repo.store, &repo.read_commit(&head)?.tree)?;
            let attrs = Attrs::load(&repo.root);
            let mut pointers = Vec::new();
            let mut paths = Vec::new();
            for (path, (mode, id)) in &tree {
                if matches!(mode, Mode::File | Mode::Exec) && attrs.tracked(path) {
                    if let Some(p) = pointers_in(&repo, &[*id])?.into_iter().next() {
                        pointers.push(p.clone());
                        paths.push((path.clone(), p));
                    }
                }
            }
            let n = download(&repo, &pointers)?;
            let mut written = 0;
            for (path, p) in paths {
                let full = repo.root.join(&path);
                let on_disk_is_pointer = fs::read(&full).map(|d| Pointer::parse(&d).is_some()).unwrap_or(true);
                if on_disk_is_pointer && smudge(&repo.dir, &p, &full)? {
                    written += 1;
                }
            }
            println!("Downloaded {n} LFS file(s); wrote {written} into your files.");
        }
        LfsCmd::Push(remote) => {
            let remote = match remote {
                Some(r) => r.to_string(),
                None => default_remote(&repo)?,
            };
            let head = repo.head_commit()?.context("there are no commits yet")?;
            let blobs: Vec<Id> = crate::transport::objects_to_send(&repo, &[head], &[])?;
            let pointers = pointers_in(&repo, &blobs)?;
            let n = upload(&repo, &remote, &pointers)?;
            println!("Uploaded {n} LFS file(s) to {remote} ({} already there).", pointers.len() - n);
        }
    }
    Ok(())
}
