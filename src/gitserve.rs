//! git's smart HTTP protocol, server side: serve a copse repository to git (and copse)
//! clients, so `git clone`, `git fetch` and `git push` work against it.
//!
//!   GET  <repo>/info/refs?service=git-upload-pack    [`info_refs`]
//!   POST <repo>/git-upload-pack                       [`upload_pack`]: clone and fetch
//!   POST <repo>/git-receive-pack                      [`receive_pack`]: push
//!
//! The repository stays in copse's format. Its history is translated into git's on the
//! way out (once: `.copse/gitmap` remembers every translation), and pushed git objects
//! are translated into copse's on the way in, keeping their git ids, so a clone gets
//! exactly the history that was pushed.
//!
//! This is the stateless ("--stateless-rpc") form of protocol v0 that every git client
//! speaks over HTTP: each POST carries the whole request. Negotiation uses
//! multi_ack_detailed; shallow clones (`--depth`) and partial clones
//! (`--filter=blob:none`) are supported. No deltas are sent, and none are needed.
//!
//! Nothing here knows about users or permissions: the caller decides who may read,
//! and [`receive_pack`] asks the caller about every ref a push wants to move. Callers
//! should let only one request at a time change a repository (one push, or the first
//! translation of new history).

use crate::gitimport::import_pack;
use crate::gitwire::{hex, parse_git_id, pkt, read_pkt, write_git_pack, GitId, GitMap, Translator, FLUSH, NO_ID};
use crate::objects::{Id, Kind, Tag};
use crate::repo::{Head, Repo};
use crate::transport::objects_to_send_shallow;
use anyhow::{bail, Context, Result};
use std::collections::HashSet;
use std::io::Write;

/// git's two services: fetching from us, and pushing to us.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Service {
    UploadPack,
    ReceivePack,
}

impl Service {
    pub fn from_name(name: &str) -> Option<Service> {
        match name {
            "git-upload-pack" => Some(Service::UploadPack),
            "git-receive-pack" => Some(Service::ReceivePack),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Service::UploadPack => "git-upload-pack",
            Service::ReceivePack => "git-receive-pack",
        }
    }

    /// The Content-Type of the advertisement (`info/refs`).
    pub fn advertisement_type(self) -> String {
        format!("application/x-{}-advertisement", self.name())
    }

    /// The Content-Type of the reply to a POST.
    pub fn result_type(self) -> String {
        format!("application/x-{}-result", self.name())
    }
}

fn agent() -> String {
    format!("agent=copse/{}", crate::VERSION)
}

/// Every branch and tag as git ids (translating any history not translated yet),
/// with annotated tags' targets, and the branch HEAD names if it exists.
struct Refs {
    head: Option<(String, GitId)>,
    head_name: Option<String>,
    /// (full ref name, git id, peeled git id for annotated tags)
    list: Vec<(String, GitId, Option<GitId>)>,
}

fn git_refs(repo: &Repo) -> Result<Refs> {
    let mut tr = Translator::new(repo)?;
    let mut list = Vec::new();
    let branches = repo.branches()?;
    for (name, id) in &branches {
        list.push((format!("refs/heads/{name}"), tr.translate_any(*id)?, None));
    }
    for (name, id) in repo.tags()? {
        let git = match tr.translate_any(id) {
            Ok(g) => g,
            Err(_) => continue, // e.g. a tag of a single file: git clients can't use it from here
        };
        let peeled = match repo.store.get(&id)?.0 {
            Kind::Tag => Some(tr.translate_any(repo.peel(id)?)?),
            _ => None,
        };
        list.push((format!("refs/tags/{name}"), git, peeled));
    }
    tr.map.save()?;
    let head_name = match repo.head()? {
        Head::Branch(b) => Some(b),
        Head::Detached(_) => None,
    };
    let head = head_name.as_ref().and_then(|b| {
        let full = format!("refs/heads/{b}");
        list.iter().find(|(n, _, _)| *n == full).map(|(_, g, _)| (full.clone(), *g))
    });
    Ok(Refs { head, head_name, list })
}

/// The reply to `GET info/refs?service=...`: the service line, then every ref with
/// the capabilities on the first line.
pub fn info_refs(repo: &Repo, service: Service) -> Result<Vec<u8>> {
    let refs = git_refs(repo)?;
    let mut caps = match service {
        Service::UploadPack => vec![
            "multi_ack_detailed".to_string(),
            "shallow".into(),
            "filter".into(),
            "include-tag".into(),
            "no-progress".into(),
            "allow-tip-sha1-in-want".into(),
            "allow-reachable-sha1-in-want".into(),
        ],
        // no-thin: every pushed pack must be complete on its own.
        Service::ReceivePack => vec!["report-status".to_string(), "delete-refs".into(), "no-thin".into()],
    };
    if let (Service::UploadPack, Some(b)) = (service, &refs.head_name) {
        if refs.head.is_some() {
            caps.push(format!("symref=HEAD:refs/heads/{b}"));
        }
    }
    caps.push("object-format=sha1".into());
    caps.push(agent());
    let caps = caps.join(" ");

    let mut out = pkt(format!("# service={}\n", service.name()).as_bytes());
    out.extend_from_slice(FLUSH);
    let mut lines: Vec<(String, GitId)> = Vec::new();
    if service == Service::UploadPack {
        if let Some((_, g)) = &refs.head {
            lines.push(("HEAD".into(), *g));
        }
    }
    for (name, g, peeled) in &refs.list {
        lines.push((name.clone(), *g));
        if let (Some(p), Service::UploadPack) = (peeled, service) {
            lines.push((format!("{name}^{{}}"), *p));
        }
    }
    if lines.is_empty() {
        out.extend(pkt(format!("{} capabilities^{{}}\0{caps}\n", hex(&NO_ID)).as_bytes()));
    }
    for (i, (name, g)) in lines.iter().enumerate() {
        let line = if i == 0 { format!("{} {name}\0{caps}\n", hex(g)) } else { format!("{} {name}\n", hex(g)) };
        out.extend(pkt(line.as_bytes()));
    }
    out.extend_from_slice(FLUSH);
    Ok(out)
}

/// Read every pkt-line of a request body; None stands for a flush.
fn pkt_lines(body: &mut &[u8]) -> Result<Vec<Option<Vec<u8>>>> {
    let mut out = Vec::new();
    while !body.is_empty() {
        if body.starts_with(b"PACK") {
            break;
        }
        out.push(read_pkt(body).context("garbled request")?);
    }
    Ok(out)
}

fn text(line: &[u8]) -> &str {
    let s = std::str::from_utf8(line).unwrap_or("");
    s.strip_suffix('\n').unwrap_or(s)
}

/// The reply to `POST git-upload-pack`: what the client is missing, as a git pack
/// (or, mid-negotiation, which of its commits we have too).
pub fn upload_pack(repo: &Repo, mut body: &[u8], out: &mut impl Write) -> Result<()> {
    let (mut wants, mut haves, mut client_shallow) = (Vec::new(), Vec::new(), Vec::new());
    let (mut caps, mut depth, mut skip_blobs, mut done) = (HashSet::new(), None::<usize>, false, false);
    // Flushes after the first (which ends the wants): each ends a round of haves.
    let mut rounds = 0;
    let mut flushes = 0;
    for line in pkt_lines(&mut body)? {
        let Some(line) = line else {
            flushes += 1;
            if flushes > 1 {
                rounds += 1;
            }
            continue;
        };
        let (main, line_caps) = match line.iter().position(|&b| b == 0) {
            Some(i) => (&line[..i], Some(&line[i + 1..])),
            None => (&line[..], None),
        };
        let main = text(main);
        let (word, rest) = main.split_once(' ').unwrap_or((main, ""));
        match word {
            "want" => {
                let mut parts = rest.split(' ');
                wants.push(parse_git_id(parts.next().unwrap_or(""))?);
                // Capabilities follow the first want, after a space (or, rarely, a NUL).
                caps.extend(parts.map(str::to_string));
                if let Some(c) = line_caps {
                    caps.extend(text(c).split(' ').map(str::to_string));
                }
            }
            "have" => haves.push(parse_git_id(rest)?),
            "shallow" => client_shallow.push(parse_git_id(rest)?),
            "deepen" => depth = Some(rest.parse::<usize>().ok().filter(|d| *d > 0).context("bad deepen")?),
            "deepen-since" | "deepen-not" => bail!("shallow clones by date or by ref aren't supported; use --depth"),
            "filter" if rest == "blob:none" => skip_blobs = true,
            "filter" => bail!("only the blob:none filter is supported"),
            "done" => done = true,
            _ => bail!("unexpected line in fetch request: {main:?}"),
        }
    }
    if wants.is_empty() {
        return Ok(()); // nothing wanted: nothing to say
    }

    let map = GitMap::load(repo)?;
    let mine = |g: &GitId| map.copse(g).filter(|id| repo.store.contains(id));
    let mut want_ids = Vec::new();
    for g in &wants {
        match mine(g) {
            Some(id) => want_ids.push(id),
            None => {
                out.write_all(&pkt(format!("ERR upload-pack: not our ref {}\n", hex(g)).as_bytes()))?;
                return Ok(());
            }
        }
    }
    let client_shallow_ids: HashSet<Id> = client_shallow.iter().filter_map(|g| mine(g)).collect();

    // Shallow information comes first in every reply to a shallow client.
    let shallow_reply = depth.is_some() || !client_shallow.is_empty();
    let common: Vec<(GitId, Id)> = haves.iter().filter_map(|g| mine(g).map(|id| (*g, id))).collect();
    // Deepening an existing shallow clone: walk from the tips again, so commits below
    // its old boundary are reached (the client ignores what it already has).
    let deepening = depth.is_some() && !client_shallow_ids.is_empty();
    let have_ids: Vec<Id> = if deepening { Vec::new() } else { common.iter().map(|(_, id)| *id).collect() };
    let walk_shallow = if deepening { HashSet::new() } else { client_shallow_ids.clone() };

    let tips: Vec<Id> = want_ids.clone();
    let (mut objects, boundary) = if shallow_reply || done {
        let commit_wants: Vec<Id> = tips.iter().copied().filter(|id| matches!(repo.store.get(id).map(|(k, _)| k), Ok(Kind::Commit | Kind::Tag))).collect();
        let (mut objects, boundary) = objects_to_send_shallow(repo, &commit_wants, &have_ids, &walk_shallow, depth, skip_blobs)?;
        // Single files and folders asked for by id (a partial clone filling itself in).
        for id in tips.iter().filter(|id| !commit_wants.contains(id)) {
            objects.push(*id);
        }
        (objects, boundary)
    } else {
        (Vec::new(), Vec::new())
    };

    if shallow_reply {
        let boundary_set: HashSet<Id> = boundary.iter().copied().collect();
        let sent: HashSet<Id> = objects.iter().copied().collect();
        for id in &boundary {
            if let Some(g) = map.git(id) {
                out.write_all(&pkt(format!("shallow {}\n", hex(&g)).as_bytes()))?;
            }
        }
        for id in &client_shallow_ids {
            let parents_sent = repo.read_commit_raw(id).map(|c| c.parents.iter().all(|p| sent.contains(p))).unwrap_or(false);
            if !boundary_set.contains(id) && parents_sent {
                if let Some(g) = map.git(id) {
                    out.write_all(&pkt(format!("unshallow {}\n", hex(&g)).as_bytes()))?;
                }
            }
        }
        out.write_all(FLUSH)?;
    }

    let detailed = caps.contains("multi_ack_detailed");
    if !done && rounds == 0 {
        // Only the wants (a shallow client's first request): the shallow list is the
        // whole answer.
        return Ok(());
    }
    if !done {
        // A negotiation round: say which of its commits we share, then NAK.
        if detailed {
            for (g, _) in &common {
                out.write_all(&pkt(format!("ACK {} common\n", hex(g)).as_bytes()))?;
            }
            if let Some((g, _)) = common.last() {
                out.write_all(&pkt(format!("ACK {} ready\n", hex(g)).as_bytes()))?;
            }
        }
        out.write_all(&pkt(b"NAK\n"))?;
        return Ok(());
    }
    match common.last() {
        Some((g, _)) => out.write_all(&pkt(format!("ACK {}\n", hex(g)).as_bytes()))?,
        None => out.write_all(&pkt(b"NAK\n"))?,
    }

    // Annotated tags pointing at what we're sending come along if asked for.
    if caps.contains("include-tag") {
        let sent: HashSet<Id> = objects.iter().copied().collect();
        for (_, id) in repo.tags()? {
            if let Ok((Kind::Tag, data)) = repo.store.get(&id) {
                if sent.contains(&Tag::decode(&data)?.object) && !sent.contains(&id) {
                    objects.push(id);
                }
            }
        }
    }
    let tr = Translator::new(repo)?;
    for id in &objects {
        if tr.map.git(id).is_none() {
            bail!("internal error: {} has no git id", crate::objects::short(id));
        }
    }
    let mut pack = Vec::new();
    write_git_pack(&mut pack, &tr, &objects)?;
    if caps.contains("side-band-64k") || caps.contains("side-band") {
        let size = if caps.contains("side-band-64k") { 65515 } else { 995 };
        for chunk in pack.chunks(size) {
            let mut data = vec![1u8];
            data.extend_from_slice(chunk);
            out.write_all(&pkt(&data))?;
        }
        out.write_all(FLUSH)?;
    } else {
        out.write_all(&pack)?;
    }
    Ok(())
}

/// What a push asks to do to one ref.
#[derive(Clone, Debug)]
pub struct RefChange {
    /// "refs/heads/main" or "refs/tags/v1".
    pub refname: String,
    /// The branch or tag name, without "refs/heads/" or "refs/tags/".
    pub name: String,
    pub is_tag: bool,
    /// What the client thinks it is now (None: it thinks it doesn't exist).
    pub old: Option<Id>,
    /// What it should become (None: delete it).
    pub new: Option<Id>,
    /// For a branch that already existed: whether `new` doesn't contain `old`, so
    /// the push replaces commits (a force-push).
    pub forced: bool,
}

/// What became of one ref in a push.
#[derive(Clone, Debug)]
pub struct RefResult {
    pub change: RefChange,
    /// None if it was done, or why not.
    pub error: Option<String>,
}

/// The reply to `POST git-receive-pack`, and what happened to each ref.
/// `allow` is asked about every change before it's made: an error refuses it
/// (and its message reaches the person pushing).
pub fn receive_pack(repo: &Repo, mut body: &[u8], allow: &mut dyn FnMut(&RefChange) -> Result<(), String>) -> Result<(Vec<u8>, Vec<RefResult>)> {
    let mut commands: Vec<(GitId, GitId, String)> = Vec::new();
    let mut caps = HashSet::new();
    let mut shallow = HashSet::new();
    for line in pkt_lines(&mut body)? {
        let Some(line) = line else { break };
        let (main, line_caps) = match line.iter().position(|&b| b == 0) {
            Some(i) => (&line[..i], Some(&line[i + 1..])),
            None => (&line[..], None),
        };
        if let Some(c) = line_caps {
            caps.extend(text(c).split(' ').map(str::to_string));
        }
        let main = text(main);
        if let Some(g) = main.strip_prefix("shallow ") {
            shallow.insert(parse_git_id(g)?);
            continue;
        }
        let mut parts = main.splitn(3, ' ');
        let (old, new, name) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""), parts.next().unwrap_or(""));
        commands.push((parse_git_id(old)?, parse_git_id(new)?, name.to_string()));
    }
    // What's left is the pack (absent when the push only deletes).
    let pack = body;

    let mut report = Vec::new();
    let mut results = Vec::new();
    let mut map = GitMap::load(repo)?;
    let unpacked = if pack.is_empty() {
        Ok(())
    } else {
        import_pack(repo, &mut map, pack, &shallow, false).and_then(|_| map.save()).map(|_| ())
    };
    if let Err(e) = &unpacked {
        report.extend(pkt(format!("unpack {}\n", format!("{e:#}").replace('\n', " ")).as_bytes()));
    } else {
        report.extend(pkt(b"unpack ok\n"));
        // Commits a shallow client pushed whose parents we don't have become our boundary.
        if !shallow.is_empty() {
            let mut ours = repo.shallow();
            let before = ours.len();
            for g in &shallow {
                if let Some(id) = map.copse(g) {
                    if repo.read_commit_raw(&id).map(|c| c.parents.iter().any(|p| !repo.store.contains(p))).unwrap_or(false) {
                        ours.insert(id);
                    }
                }
            }
            if ours.len() != before {
                repo.set_shallow(&ours)?;
            }
        }
    }

    for (old, new, refname) in commands {
        let outcome = match &unpacked {
            Err(_) => Err((None, "unpacker error".to_string())),
            Ok(()) => apply(repo, &map, old, new, &refname, allow),
        };
        let line = match &outcome {
            Ok(_) => format!("ok {refname}\n"),
            Err((_, why)) => format!("ng {refname} {}\n", why.replace('\n', " ")),
        };
        report.extend(pkt(line.as_bytes()));
        match outcome {
            Ok(change) => results.push(RefResult { change, error: None }),
            Err((Some(change), why)) => results.push(RefResult { change, error: Some(why) }),
            Err((None, _)) => {}
        }
    }
    report.extend_from_slice(FLUSH);
    let reply = if caps.contains("report-status") { report } else { Vec::new() };
    Ok((reply, results))
}

type Refused = (Option<RefChange>, String);

fn apply(repo: &Repo, map: &GitMap, old: GitId, new: GitId, refname: &str, allow: &mut dyn FnMut(&RefChange) -> Result<(), String>) -> Result<RefChange, Refused> {
    let (name, is_tag) = if let Some(b) = refname.strip_prefix("refs/heads/") {
        (b.to_string(), false)
    } else if let Some(t) = refname.strip_prefix("refs/tags/") {
        (t.to_string(), true)
    } else {
        return Err((None, "only branches and tags can be pushed here".into()));
    };
    if crate::repo::validate_branch(&name).is_err() {
        return Err((None, "not a valid name".into()));
    }
    let lookup = |g: GitId| -> Result<Option<Id>, Refused> {
        if g == NO_ID {
            return Ok(None);
        }
        match map.copse(&g).filter(|id| repo.store.contains(id)) {
            Some(id) => Ok(Some(id)),
            None => Err((None, format!("this server doesn't have {}", hex(&g)))),
        }
    };
    let new_id = lookup(new)?;
    let current = if is_tag { repo.tag(&name) } else { repo.branch(&name) }.map_err(|e| (None, format!("{e:#}")))?;
    // The client's idea of where the ref is must match ours.
    let old_id = if old == NO_ID { None } else { map.copse(&old) };
    if old_id != current {
        return Err((None, if current.is_some() && old == NO_ID { "already exists".into() } else { "stale info: it changed since you fetched; fetch and try again".into() }));
    }
    if let Some(id) = new_id {
        let kind = repo.store.get(&id).map(|(k, _)| k).map_err(|e| (None, format!("{e:#}")))?;
        if !is_tag && kind != Kind::Commit {
            return Err((None, "a branch must point at a commit".into()));
        }
    }
    let forced = match (current, new_id, is_tag) {
        (Some(o), Some(n), false) => !crate::merge::is_ancestor(repo, o, n).unwrap_or(false),
        (Some(_), Some(_), true) => true,
        _ => false,
    };
    let change = RefChange { refname: refname.to_string(), name: name.clone(), is_tag, old: current, new: new_id, forced };
    if let Err(why) = allow(&change) {
        return Err((Some(change), why));
    }
    let done = match (new_id, is_tag) {
        (None, false) => {
            if matches!(repo.head(), Ok(Head::Branch(b)) if b == name) {
                return Err((Some(change), "this is the repository's main branch, so it can't be deleted".into()));
            }
            repo.cas_delete_branch(&name, &current.expect("deleting a branch that exists"))
        }
        (Some(n), false) => repo.cas_branch(&name, current, &n),
        (None, true) => repo.delete_tag(&name).map(|_| ()),
        (Some(n), true) => repo.set_tag(&name, &n),
    };
    match done {
        Ok(()) => Ok(change),
        Err(e) => Err((Some(change), format!("{e:#}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_repository_advertises_capabilities() {
        let dir = std::env::temp_dir().join(format!("copse-gitserve-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let repo = Repo::init_bare(&dir).unwrap();
        let out = info_refs(&repo, Service::ReceivePack).unwrap();
        let s = String::from_utf8_lossy(&out);
        assert!(s.starts_with("001f# service=git-receive-pack\n0000"));
        assert!(s.contains("capabilities^{}\0report-status"));
        assert!(s.ends_with("0000"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
