//! Plumbing: small, stable commands for scripts, as git has them.
//!
//!   cat-file     an object's type, size or contents
//!   rev-parse    names to ids, and facts about the repository
//!   ls-tree      a folder listing from a commit
//!   ls-files     the files copse knows about, or the ones changed / untracked
//!   hash-object  the id a file would get (and store it, with -w)
//!
//! Ids are copse's (BLAKE3); the output formats are git's.

use crate::objects::{gitlink_hex, short, Commit, Id, Kind, Mode, Store, Tag};
use crate::repo::{Head, Repo};
use anyhow::{bail, Context, Result};
use std::io::{Read, Write};

pub fn kind_name(k: Kind) -> &'static str {
    match k {
        Kind::Blob => "blob",
        Kind::Tree => "tree",
        Kind::Commit => "commit",
        Kind::Tag => "tag",
    }
}

fn parse_kind(s: &str) -> Result<Kind> {
    Ok(match s {
        "blob" => Kind::Blob,
        "tree" => Kind::Tree,
        "commit" => Kind::Commit,
        "tag" => Kind::Tag,
        other => bail!("unknown object type {other:?} (blob, tree, commit or tag)"),
    })
}

pub fn git_mode(mode: Mode) -> &'static str {
    match mode {
        Mode::File => "100644",
        Mode::Exec => "100755",
        Mode::Link => "120000",
        Mode::Dir => "040000",
        Mode::Submodule => "160000",
    }
}

fn entry_type(mode: Mode) -> &'static str {
    match mode {
        Mode::Dir => "tree",
        Mode::Submodule => "commit",
        _ => "blob",
    }
}

fn entry_id(mode: Mode, id: &Id) -> String {
    if mode == Mode::Submodule {
        gitlink_hex(id) // a submodule's pinned commit is a git id
    } else {
        id.to_hex().to_string()
    }
}

/// A tree listing, one entry per line: "MODE TYPE ID\tNAME" (git's format).
fn tree_lines(store: &Store, tree: &Id, prefix: &str, recursive: bool, dirs_only: bool, name_only: bool, out: &mut Vec<u8>) -> Result<()> {
    for e in crate::objects::decode_tree(&store.get_kind(tree, Kind::Tree)?)? {
        let path = format!("{prefix}{}", e.name);
        if recursive && e.mode == Mode::Dir {
            tree_lines(store, &e.id, &format!("{path}/"), recursive, dirs_only, name_only, out)?;
            continue;
        }
        if dirs_only && e.mode != Mode::Dir {
            continue;
        }
        if name_only {
            writeln!(out, "{path}")?;
        } else {
            writeln!(out, "{} {} {}\t{path}", git_mode(e.mode), entry_type(e.mode), entry_id(e.mode, &e.id))?;
        }
    }
    Ok(())
}

/// What `cat-file -p` prints for an object.
fn pretty(repo: &Repo, kind: Kind, data: &[u8]) -> Result<Vec<u8>> {
    Ok(match kind {
        Kind::Blob => data.to_vec(),
        Kind::Tree => {
            let mut out = Vec::new();
            for e in crate::objects::decode_tree(data)? {
                writeln!(out, "{} {} {}\t{}", git_mode(e.mode), entry_type(e.mode), entry_id(e.mode, &e.id), e.name)?;
            }
            out
        }
        Kind::Commit => {
            let c = Commit::decode(data)?;
            let mut t = format!("tree {}\n", c.tree.to_hex());
            for p in &c.parents {
                t += &format!("parent {}\n", p.to_hex());
            }
            t += &format!("author {} {} {}\ncommitter {} {} {}\n", c.author.who, c.author.time, c.author.tz, c.committer.who, c.committer.time, c.committer.tz);
            t += &String::from_utf8_lossy(&c.extra);
            if !c.extra.is_empty() && !c.extra.ends_with(b"\n") {
                t.push('\n');
            }
            t += &format!("\n{}", c.message);
            let _ = repo;
            t.into_bytes()
        }
        Kind::Tag => {
            let tag = Tag::decode(data)?;
            let mut t = format!("object {}\ntype {}\ntag {}\n", tag.object.to_hex(), kind_name(tag.target_kind), tag.name);
            if let Some(s) = &tag.tagger {
                t += &format!("tagger {} {} {}\n", s.who, s.time, s.tz);
            }
            t += &String::from_utf8_lossy(&tag.extra);
            t += &format!("\n{}", tag.message);
            t.into_bytes()
        }
    })
}

/// `copse cat-file -t|-s|-p|-e OBJECT` or `copse cat-file TYPE OBJECT`.
pub fn cat_file(mode: &str, object: &str) -> Result<()> {
    let repo = Repo::find_any()?;
    let found = repo.resolve_object(object);
    if mode == "-e" {
        // Exit status only: 0 if it exists.
        return match found {
            Ok(id) if repo.store.contains(&id) => Ok(()),
            _ => std::process::exit(1),
        };
    }
    let id = found?;
    let (kind, data) = repo.store.get(&id)?;
    let mut stdout = std::io::stdout().lock();
    match mode {
        "-t" => writeln!(stdout, "{}", kind_name(kind))?,
        "-s" => writeln!(stdout, "{}", data.len())?,
        "-p" => stdout.write_all(&pretty(&repo, kind, &data)?)?,
        want => {
            let want = parse_kind(want)?;
            // As git does, a tag or commit is followed to the type asked for.
            let (kind, data) = if want != kind {
                let target = match (want, kind) {
                    (Kind::Commit, Kind::Tag) => repo.peel(id)?,
                    (Kind::Tree, _) => repo.resolve_object(&format!("{}^{{tree}}", id.to_hex()))?,
                    _ => bail!("{object} is a {}, not a {}", kind_name(kind), kind_name(want)),
                };
                repo.store.get(&target)?
            } else {
                (kind, data)
            };
            let _ = kind;
            stdout.write_all(&data)?;
        }
    }
    Ok(())
}

pub struct RevParse<'a> {
    pub revs: &'a [String],
    pub short: bool,
    pub abbrev_ref: bool,
    pub verify: bool,
    pub quiet: bool,
    pub show_toplevel: bool,
    pub git_dir: bool,
    pub inside_work_tree: bool,
    pub is_bare: bool,
}

/// `copse rev-parse`
pub fn rev_parse(a: RevParse) -> Result<()> {
    let repo = match Repo::find_any() {
        Ok(r) => r,
        Err(_) if a.inside_work_tree => {
            println!("false");
            return Ok(());
        }
        Err(e) => return Err(e),
    };
    if a.show_toplevel {
        if repo.is_bare() {
            bail!("a bare repository has no working folder");
        }
        println!("{}", repo.root.display());
    }
    if a.git_dir {
        println!("{}", repo.wt.display());
    }
    if a.inside_work_tree {
        println!("{}", !repo.is_bare());
    }
    if a.is_bare {
        println!("{}", repo.is_bare());
    }
    if a.verify && a.revs.len() != 1 {
        bail!("--verify takes exactly one revision");
    }
    for rev in a.revs {
        if a.abbrev_ref {
            // The branch name a revision is (HEAD -> its branch), or the name itself.
            let name = match rev.as_str() {
                "HEAD" | "@" => match repo.head()? {
                    Head::Branch(b) => b,
                    Head::Detached(_) => "HEAD".to_string(),
                },
                other => {
                    repo.resolve_object(other)?;
                    other.to_string()
                }
            };
            println!("{name}");
            continue;
        }
        let id = match repo.resolve_object(rev) {
            Ok(id) => id,
            Err(_) if a.quiet => std::process::exit(1),
            Err(e) => return Err(e),
        };
        println!("{}", if a.short { short(&id) } else { id.to_hex().to_string() });
    }
    Ok(())
}

/// `copse ls-tree [-r] [-d] [--name-only] REV [PATH]`
pub fn ls_tree(rev: &str, path: Option<&str>, recursive: bool, dirs_only: bool, name_only: bool) -> Result<()> {
    let repo = Repo::find_any()?;
    let id = repo.resolve_object(rev)?;
    let tree = match repo.store.get(&id)?.0 {
        Kind::Tree => id,
        _ => repo.read_commit(&repo.peel(id)?)?.tree,
    };
    let mut out = Vec::new();
    match path.map(|p| p.trim_matches('/')).filter(|p| !p.is_empty()) {
        None => tree_lines(&repo.store, &tree, "", recursive, dirs_only, name_only, &mut out)?,
        Some(p) => {
            let (mode, eid) = crate::objects::entry_at(&repo.store, &tree, p)?.with_context(|| format!("{p} isn't in {rev}"))?;
            if mode == Mode::Dir && recursive {
                tree_lines(&repo.store, &eid, &format!("{p}/"), true, dirs_only, name_only, &mut out)?;
            } else if name_only {
                writeln!(out, "{p}")?;
            } else {
                writeln!(out, "{} {} {}\t{p}", git_mode(mode), entry_type(mode), entry_id(mode, &eid))?;
            }
        }
    }
    std::io::stdout().write_all(&out)?;
    Ok(())
}

/// `copse ls-files [-m] [-d] [-o]`: the files in the last commit (copse has no
/// staging area, so these are the files it tracks), or the modified, deleted or
/// untracked ones.
pub fn ls_files(modified: bool, deleted: bool, others: bool) -> Result<()> {
    let repo = Repo::find()?;
    let index = crate::worktree::Index::load(&repo.wt)?;
    let mut out = Vec::new();
    if !modified && !deleted && !others {
        for p in index.entries.keys() {
            writeln!(out, "{p}")?;
        }
    } else {
        let files = crate::worktree::snapshot(&repo.root, &index, None)?;
        let ch = crate::worktree::changes(&index, &files);
        let mut list: Vec<&String> = Vec::new();
        if modified {
            list.extend(&ch.modified);
            list.extend(&ch.deleted); // git counts deleted files as modified too
        } else if deleted {
            list.extend(&ch.deleted);
        }
        if others {
            list.extend(&ch.added);
        }
        list.sort();
        list.dedup();
        for p in list {
            writeln!(out, "{p}")?;
        }
    }
    std::io::stdout().write_all(&out)?;
    Ok(())
}

/// `copse hash-object [-w] [--stdin] [FILE...]`: a file's id as copse stores it.
pub fn hash_object(files: &[String], write: bool, stdin: bool) -> Result<()> {
    let store = if write { Some(Repo::find()?) } else { None };
    let mut inputs: Vec<Vec<u8>> = Vec::new();
    if stdin {
        let mut buf = Vec::new();
        std::io::stdin().read_to_end(&mut buf)?;
        inputs.push(buf);
    }
    for f in files {
        inputs.push(std::fs::read(f).with_context(|| format!("reading {f}"))?);
    }
    if inputs.is_empty() {
        bail!("name a file, or use --stdin");
    }
    for data in inputs {
        let id = match &store {
            Some(repo) => repo.store.put(Kind::Blob, &data)?,
            None => Store::hash(Kind::Blob, &data),
        };
        println!("{}", id.to_hex());
    }
    Ok(())
}

/// `copse show REV:PATH`: a file's contents, or a folder's listing, as git shows them.
pub fn show_path(repo: &Repo, spec: &str) -> Result<()> {
    let id = repo.resolve_object(spec)?;
    let (kind, data) = repo.store.get(&id)?;
    let mut stdout = std::io::stdout().lock();
    match kind {
        Kind::Blob => stdout.write_all(&data)?,
        Kind::Tree => {
            writeln!(stdout, "tree {spec}\n")?;
            for e in crate::objects::decode_tree(&data)? {
                writeln!(stdout, "{}{}", e.name, if e.mode == Mode::Dir { "/" } else { "" })?;
            }
        }
        other => bail!("{spec} is a {}", kind_name(other)),
    }
    Ok(())
}
