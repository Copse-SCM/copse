use crate::merge::MergeState;
use crate::objects::{flatten, short, write_tree, Commit, Id, Kind, Mode};
use crate::repo::{Head, Repo};
use crate::worktree::{changes, refresh, snapshot, stat_of, Changes, Index};
use anyhow::{bail, Context, Result};
use rayon::prelude::*;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ffi::OsStr;
use std::fs;
use std::io::IsTerminal;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::OnceLock;

// ---------------------------------------------------------------- output helpers

pub(crate) fn paint(code: &str, s: &str) -> String {
    static COLOR: OnceLock<bool> = OnceLock::new();
    let on = *COLOR.get_or_init(|| std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none());
    if on {
        format!("\x1b[{code}m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

pub(crate) fn format_time(secs: i64) -> String {
    format!("{} UTC", civil_time(secs))
}

/// "YYYY-MM-DD HH:MM" for seconds since the epoch (no timezone applied).
pub(crate) fn civil_time(secs: i64) -> String {
    // Days-since-epoch to civil date (Howard Hinnant's algorithm).
    let z = secs.div_euclid(86_400) + 719_468;
    let rem = secs.rem_euclid(86_400);
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02} {:02}:{:02}", rem / 3600, rem % 3600 / 60)
}

pub(crate) fn head_label(repo: &Repo) -> Result<String> {
    Ok(match repo.head()? {
        Head::Branch(b) => b,
        Head::Detached(id) => format!("detached {}", short(&id)),
    })
}

/// `copse status --porcelain`: a stable, plain format for scripts and editors.
/// One fact per line, never coloured, never reworded between versions:
///   branch NAME | branch (detached) ID
///   upstream origin/NAME AHEAD BEHIND        (when there's a remote branch)
///   state merging|rebasing|cherry-picking|reverting|bisecting
///   A PATH | M PATH | D PATH                 (changes, sorted by path)
///   C PATH                                   (files with conflict markers)
fn status_porcelain(repo: &Repo, ch: &Changes) -> Result<()> {
    match repo.head()? {
        Head::Branch(b) => println!("branch {b}"),
        Head::Detached(id) => println!("branch (detached) {}", id.to_hex()),
    }
    if let Some((up, ahead, behind)) = upstream(repo) {
        println!("upstream {up} {ahead} {behind}");
    }
    let mut conflicted: Vec<String> = Vec::new();
    if let Some(m) = MergeState::load(repo)? {
        println!("state merging");
        conflicted = m.unresolved(repo);
    }
    if let Some(r) = crate::rebase::RebaseState::load(repo)? {
        println!("state {}", match r.op {
            crate::rebase::Op::Rebase => "rebasing",
            crate::rebase::Op::CherryPick => "cherry-picking",
            crate::rebase::Op::Revert => "reverting",
        });
        conflicted = r.unresolved(repo);
    }
    if repo.wt.join("BISECT_STATE").exists() {
        println!("state bisecting");
    }
    let mut lines: Vec<(&str, char)> = Vec::new();
    lines.extend(ch.added.iter().map(|p| (p.as_str(), 'A')));
    lines.extend(ch.modified.iter().map(|p| (p.as_str(), 'M')));
    lines.extend(ch.deleted.iter().map(|p| (p.as_str(), 'D')));
    lines.sort();
    for (p, k) in lines {
        let k = if conflicted.iter().any(|c| c == p) { 'C' } else { k };
        println!("{k} {p}");
    }
    Ok(())
}

pub(crate) fn print_changes(ch: &Changes) {
    let mut lines: Vec<(&str, char, &str)> = Vec::with_capacity(ch.len());
    lines.extend(ch.added.iter().map(|p| (p.as_str(), 'A', "32")));
    lines.extend(ch.modified.iter().map(|p| (p.as_str(), 'M', "33")));
    lines.extend(ch.deleted.iter().map(|p| (p.as_str(), 'D', "31")));
    lines.sort_unstable();
    for (path, tag, color) in lines {
        println!("  {} {path}", paint(color, &tag.to_string()));
    }
}

// ---------------------------------------------------------------- commands

pub fn init(dir: Option<&Path>, bare: bool) -> Result<()> {
    let root = match dir {
        Some(d) => {
            fs::create_dir_all(d)?;
            d.canonicalize()?
        }
        None => std::env::current_dir()?,
    };
    if bare {
        Repo::init_bare(&root)?;
        println!("Initialized an empty bare copse repository in {}", root.display());
    } else {
        Repo::init(&root)?;
        println!("Initialized an empty copse repository in {}", root.join(".copse").display());
    }
    Ok(())
}

/// The current branch's remote counterpart, and how far apart they are:
/// ("origin/main", ahead, behind).
fn upstream(repo: &Repo) -> Option<(String, usize, usize)> {
    let Ok(Head::Branch(b)) = repo.head() else { return None };
    let local = repo.branch(&b).ok()??;
    let remote = repo.tracking("origin", &b).ok()??;
    if !repo.store.contains(&remote) {
        return None;
    }
    let (ahead, behind) = crate::merge::ahead_behind(repo, local, remote)?;
    Some((format!("origin/{b}"), ahead, behind))
}

pub fn status(porcelain: bool) -> Result<()> {
    let repo = Repo::find()?;
    let mut index = Index::load(&repo.wt)?;
    let files = snapshot(&repo.root, &index, None)?;
    let ch = changes(&index, &files);
    if refresh(&mut index, &files) {
        index.save(&repo.wt)?;
    }
    if porcelain {
        return status_porcelain(&repo, &ch);
    }

    match repo.head()? {
        Head::Branch(b) => println!("On branch {}", paint("1", &b)),
        Head::Detached(id) => println!("HEAD detached at {}", paint("33", &short(&id))),
    }
    if let Some((up, ahead, behind)) = upstream(&repo) {
        println!("{}", match (ahead, behind) {
            (0, 0) => format!("In step with {up}."),
            (a, 0) => format!("{a} commit(s) ahead of {up}: `copse push` sends them."),
            (0, b) => format!("{b} commit(s) behind {up}: `copse pull` brings them in."),
            (a, b) => format!("{a} commit(s) ahead of {up} and {b} behind: `copse pull` merges them (or `--rebase`), then push."),
        });
    }
    if repo.head_commit()?.is_none() {
        println!("No commits yet");
    }
    if let Some(m) = MergeState::load(&repo)? {
        let unresolved = m.unresolved(&repo);
        println!("Merging {} ({} file(s) still have conflict markers)", m.label, unresolved.len());
        for p in &unresolved {
            println!("  {} {p}", paint("31", "C"));
        }
        println!("Fix the conflicts, then run `copse commit`. To give up, run `copse merge --abort`.");
    }
    if let Some(r) = crate::rebase::RebaseState::load(&repo)? {
        let what = match r.op {
            crate::rebase::Op::Rebase => format!("Rebasing {} onto {}", r.branch, r.onto_label),
            crate::rebase::Op::CherryPick => format!("Cherry-picking onto {}", r.branch),
            crate::rebase::Op::Revert => format!("Reverting on {}", r.branch),
        };
        println!("{what} ({} of {} commits done)", r.done, r.total);
        if r.paused_for_edit {
            println!("Paused to edit {}.", r.paused_on(&repo).unwrap_or_default());
            println!("Change files, then `copse rebase --continue` (or `copse commit` first to split it).");
        }
        let unresolved = r.unresolved(&repo);
        for p in &unresolved {
            println!("  {} {p}", paint("31", "C"));
        }
        if unresolved.is_empty() && !r.conflicts.is_empty() {
            println!("Conflicts are fixed: run `copse {} --continue`.", r.op.name());
        } else if !unresolved.is_empty() {
            println!("Fix the conflicts, then run `copse {} --continue` (or `--skip`, `--abort`).", r.op.name());
        }
    }
    if repo.wt.join("BISECT_STATE").exists() {
        println!("Bisecting: mark this commit with `copse bisect good` or `bad`; `copse bisect reset` ends it.");
    }
    let stashes = crate::stash::load(&repo)?.len();
    if stashes > 0 {
        println!("{stashes} stash(es) saved; see `copse stash list`");
    }
    if ch.is_empty() {
        println!("Nothing changed.");
    } else {
        print_changes(&ch);
        println!(
            "{} added, {} modified, {} deleted",
            ch.added.len(),
            ch.modified.len(),
            ch.deleted.len()
        );
    }
    Ok(())
}

pub fn commit(message: Option<&str>, paths: &[String], amend: bool, no_verify: bool, sign: Option<bool>, patch: bool) -> Result<()> {
    let repo = Repo::find()?;
    if patch && amend {
        bail!("-p and --amend don't go together; commit the picked changes, then amend if you need to");
    }
    // Committing is allowed while an interactive rebase is paused for `edit` (that's how
    // you split a commit); otherwise a rebase must be finished first.
    if let Some(r) = crate::rebase::RebaseState::load(&repo)?.filter(|r| !r.paused_for_edit) {
        bail!("{}", r.busy());
    }
    let merge = MergeState::load(&repo)?;
    if merge.is_some() && !paths.is_empty() {
        bail!("a merge is in progress: its result has to be committed as a whole, so leave out the paths");
    }
    if merge.is_some() && patch {
        bail!("a merge is in progress: its result has to be committed as a whole, so leave out -p");
    }
    if merge.is_some() && amend {
        bail!("a merge is in progress: commit it first (`copse commit`), then amend if you need to");
    }
    let head = repo.head_commit()?;
    let amended = match (amend, head) {
        (true, Some(h)) => Some(repo.read_commit(&h)?),
        (true, None) => bail!("there's no commit to amend yet"),
        (false, _) => None,
    };
    let mut message = match (message.map(str::trim), &merge, &amended) {
        (Some(m), _, _) if !m.is_empty() => m.to_string(),
        (Some(_), _, _) => bail!("the commit message is empty"),
        (None, Some(m), _) => format!("Merge {} into {}", m.label, head_label(&repo)?),
        (None, None, Some(old)) => old.message.clone(),
        (None, None, None) if patch => String::new(), // asked for after picking
        (None, None, None) => bail!("give a commit message with -m \"...\""),
    };
    if let Some(m) = &merge {
        let unresolved = m.unresolved(&repo);
        if !unresolved.is_empty() {
            for p in &unresolved {
                println!("  {} {p}", paint("31", "C"));
            }
            bail!("{} file(s) still have conflict markers; fix them before committing", unresolved.len());
        }
    }

    // pre-commit may check or tidy the files (a formatter, say) before they're read.
    if !no_verify {
        crate::hooks::check(&repo, "pre-commit", &[], None, "commit")?;
    }

    let mut index = Index::load(&repo.wt)?;
    let files = snapshot(&repo.root, &index, Some(&repo.store))?;
    let ch = changes(&index, &files);

    // With paths, only changes under them are committed; everything else keeps the
    // last commit's version (and stays an uncommitted change).
    let specs = paths.iter().map(|p| crate::restore::repo_path(&repo, p)).collect::<Result<Vec<_>>>()?;
    let in_scope = |p: &str| specs.is_empty() || specs.iter().any(|s| s.is_empty() || p == s || p.starts_with(&format!("{s}/")));
    for (typed, spec) in paths.iter().zip(&specs) {
        let under = |p: &str| spec.is_empty() || p == spec || p.starts_with(&format!("{spec}/"));
        if !files.iter().any(|f| under(&f.path)) && !index.entries.keys().any(|p| under(p)) {
            bail!("{typed} doesn't match any file");
        }
    }
    let count = |list: &[String]| list.iter().filter(|p| in_scope(p)).count();
    let (added, modified, deleted) = (count(&ch.added), count(&ch.modified), count(&ch.deleted));
    let committing = added + modified + deleted;
    if committing == 0 && merge.is_none() && amended.is_none() && (head.is_some() || files.is_empty()) {
        println!("{}", if specs.is_empty() { "Nothing to commit." } else { "Nothing to commit under those paths." });
        return Ok(());
    }

    // -p: ask about each change, and commit only what's picked.
    let picked = if patch {
        let stdin = std::io::stdin();
        let mut input = stdin.lock();
        let picked = crate::hunks::pick(&repo, &index, &files, &ch, &in_scope, &mut input)?;
        if picked.is_empty() {
            println!("Nothing picked, so nothing was committed.");
            return Ok(());
        }
        if message.is_empty() {
            print!("Message for this commit: ");
            std::io::Write::flush(&mut std::io::stdout())?;
            let mut line = String::new();
            std::io::BufRead::read_line(&mut input, &mut line)?;
            message = line.trim().to_string();
            if message.is_empty() {
                bail!("the commit message is empty, so nothing was committed");
            }
        }
        Some(picked)
    } else {
        None
    };

    // commit-msg may check or rewrite the message.
    if !no_verify {
        let path = repo.wt.join("COMMIT_EDITMSG");
        fs::write(&path, format!("{}\n", message.trim_end()))?;
        crate::hooks::check(&repo, "commit-msg", &[path.to_str().unwrap_or_default()], None, "commit")?;
        let edited = fs::read_to_string(&path)?;
        let _ = fs::remove_file(&path);
        let edited: Vec<&str> = edited.lines().filter(|l| !l.starts_with('#')).collect();
        message = edited.join("\n").trim().to_string();
        if message.is_empty() {
            bail!("the commit-msg hook left the message empty, so nothing was committed");
        }
    }

    let mut tree_files: BTreeMap<String, (Mode, Id)> = match &picked {
        // The last commit, with just the picked changes on top.
        Some(picked) => {
            let mut t: BTreeMap<String, (Mode, Id)> = index.entries.iter().map(|(p, e)| (p.clone(), (e.mode, Id::from_bytes(e.id)))).collect();
            for (p, v) in picked {
                match v {
                    Some(v) => t.insert(p.clone(), *v),
                    None => t.remove(p),
                };
            }
            t
        }
        None => index.entries.iter().filter(|(p, _)| !in_scope(p)).map(|(p, e)| (p.clone(), (e.mode, Id::from_bytes(e.id)))).collect(),
    };
    if picked.is_none() {
        tree_files.extend(files.iter().filter(|f| in_scope(&f.path)).map(|f| (f.path.clone(), (f.mode, f.id))));
    }
    let tree = write_tree(&repo.store, tree_files.iter().map(|(p, (m, id))| (p.as_str(), *m, *id)))?;

    let title = message.lines().next().unwrap_or_default().to_string();
    let mut commit = match &amended {
        Some(old) => Commit::rewritten(old, tree, old.parents.clone(), message.clone()),
        None => Commit::new(tree, head.into_iter().chain(merge.as_ref().map(|m| m.other)).collect(), message.clone()),
    };
    if sign.unwrap_or_else(crate::signing::sign_by_default) {
        crate::signing::sign_commit(&repo, &mut commit)?;
    }
    crate::reflog::set_reason(if amended.is_some() { format!("commit --amend: {title}") } else { format!("commit: {title}") });
    let id = repo.store.put(Kind::Commit, &commit.encode())?;
    repo.advance_head(&id)?;

    // The index now describes the new commit: committed paths take your files' state.
    let (mut added, mut modified, mut deleted) = (added, modified, deleted);
    match &picked {
        Some(picked) => {
            let on_disk: std::collections::HashMap<&str, &crate::worktree::File> = files.iter().map(|f| (f.path.as_str(), f)).collect();
            (added, modified, deleted) = (0, 0, 0);
            for (p, v) in picked {
                let was = index.entries.contains_key(p);
                match v {
                    None => {
                        index.entries.remove(p);
                        deleted += 1;
                    }
                    Some((m, blob)) => {
                        if was { modified += 1 } else { added += 1 }
                        // Committed exactly as on disk: cache its stat. Partly: leave a
                        // stat that won't match, so the rest still shows as a change.
                        let entry = match on_disk.get(p.as_str()) {
                            Some(f) if f.id == *blob && f.mode == *m => f.entry(),
                            _ => crate::worktree::Entry { size: 0, mtime_ns: 0, mode: *m, id: *blob.as_bytes() },
                        };
                        index.entries.insert(p.clone(), entry);
                    }
                }
            }
        }
        None => {
            index.entries.retain(|p, _| !in_scope(p));
            index.entries.extend(files.iter().filter(|f| in_scope(&f.path)).map(|f| (f.path.clone(), f.entry())));
        }
    }
    index.save(&repo.wt)?;
    MergeState::clear(&repo)?;

    let note = if amended.is_some() { " (amended)" } else if picked.is_some() { " (picked changes)" } else { "" };
    println!("[{} {}] {title}{note}", head_label(&repo)?, paint("33", &short(&id)));
    println!("  {added} added, {modified} modified, {deleted} deleted");
    // What's still uncommitted (unpicked changes, and anything outside the paths).
    let left = changes(&index, &files).len();
    if left > 0 {
        println!("  {left} other change(s) left uncommitted");
    }
    crate::hooks::notify(&repo, "post-commit", &[]);
    Ok(())
}

pub fn checkout(target: &str, force: bool) -> Result<()> {
    let repo = Repo::find()?;
    if let Some(r) = crate::rebase::RebaseState::load(&repo)? {
        bail!("{}", r.busy());
    }
    let mut index = Index::load(&repo.wt)?;

    let (new_head, commit_id) = match repo.branch(target)? {
        Some(id) => {
            if let Some(other) = crate::worktrees::checked_out_elsewhere(&repo, target)? {
                bail!("branch {target} is checked out in the worktree at {}; a branch can be in only one worktree at a time", other.display());
            }
            (Head::Branch(target.to_string()), id)
        }
        None => {
            let id = repo.resolve(target)?;
            (Head::Detached(id), id)
        }
    };

    let files = snapshot(&repo.root, &index, None)?;
    let ch = changes(&index, &files);
    if !force && !ch.is_empty() {
        print_changes(&ch);
        bail!("you have {} uncommitted change(s); commit them first, or use --force to discard them", ch.len());
    }

    // Files a half-finished merge added aren't in the index, so name them for removal.
    let merge = MergeState::load(&repo)?;
    let merge_added = merge.map(|m| m.added).unwrap_or_default();

    let target_files = flatten(&repo.store, &repo.read_commit(&commit_id)?.tree)?;
    let old_commit = repo.head_commit()?;
    let (written, removed) = apply_tree(&repo, &mut index, &files, &target_files, &merge_added)?;
    index.save(&repo.wt)?;
    repo.set_head(&new_head)?;
    MergeState::clear(&repo)?;
    let zero = "0".repeat(64);
    let old_hex = old_commit.map_or(zero, |id| id.to_hex().to_string());
    crate::hooks::notify(&repo, "post-checkout", &[&old_hex, &commit_id.to_hex(), "1"]);

    match new_head {
        Head::Branch(b) => println!("Switched to branch {b}"),
        Head::Detached(id) => println!("HEAD is now at {} (detached)", short(&id)),
    }
    println!("  {written} file(s) written, {removed} removed");
    Ok(())
}

/// Make the working directory match `target` (path -> mode, blob id) and point the index at it.
/// `files` is a fresh snapshot of the working directory. Committed files missing from the
/// target are removed, plus any paths in `extra_remove`; other untracked files are left alone.
/// The caller saves the index. Returns (files written, files removed).
pub(crate) fn apply_tree(
    repo: &Repo,
    index: &mut Index,
    files: &[crate::worktree::File],
    target_files: &BTreeMap<String, (Mode, Id)>,
    extra_remove: &[String],
) -> Result<(usize, usize)> {
    let current: HashMap<&str, (Mode, Id)> = files.iter().map(|f| (f.path.as_str(), (f.mode, f.id))).collect();
    // Sparse checkout: files outside the checked-out folders that aren't on disk stay off it.
    let sparse = crate::sparse::Sparse::load(&repo.wt);
    let absent: HashSet<&str> = files.iter().filter(|f| f.absent).map(|f| f.path.as_str()).collect();
    let off_disk = |p: &str| absent.contains(p) || (sparse.outside(p) && !current.contains_key(p));

    let to_remove: BTreeSet<String> = index
        .entries
        .keys()
        .chain(extra_remove)
        .filter(|p| !target_files.contains_key(*p) && current.contains_key(p.as_str()) && !absent.contains(p.as_str()))
        .cloned()
        .collect();
    for path in &to_remove {
        let full = repo.root.join(path);
        if full.is_dir() && !fs::symlink_metadata(&full).map(|m| m.file_type().is_symlink()).unwrap_or(false) {
            // A submodule that's no longer here: remove its folder only if it's empty.
            if fs::remove_dir(&full).is_err() {
                eprintln!("copse: note: left the submodule folder {path} in place (it has files); delete it yourself if you don't need it");
            }
            continue;
        }
        fs::remove_file(&full).with_context(|| format!("removing {path}"))?;
        let mut dir = full.parent();
        while let Some(d) = dir {
            if d == repo.root || fs::remove_dir(d).is_err() {
                break;
            }
            dir = d.parent();
        }
    }

    // Write every file whose content or mode differs from the target, in parallel.
    let to_write: Vec<(&str, Mode, Id)> = target_files
        .iter()
        .filter(|(p, want)| current.get(p.as_str()) != Some(*want) && !off_disk(p))
        .map(|(p, (mode, id))| (p.as_str(), *mode, *id))
        .collect();
    // LFS contents it'll need, downloaded in one go (the rules come from what's
    // being checked out, since its .gitattributes may not be on disk yet).
    let lfs = crate::lfs::Attrs::for_files(repo, target_files);
    crate::lfs::before_checkout(repo, &lfs, &to_write);
    // A partial clone fetches the contents it's about to write in one go.
    if repo.is_partial() {
        let ids: Vec<Id> = to_write.iter().filter(|(_, m, _)| *m != Mode::Submodule).map(|(_, _, id)| *id).collect();
        repo.store.prefetch(&ids)?;
    }
    to_write
        .par_iter()
        .try_for_each(|(path, mode, id)| write_file(repo, path, *mode, id).with_context(|| format!("writing {path}")))?;

    crate::lfs::after_checkout(repo, &lfs, &to_write)?;

    // Rebuild the index: re-stat written files, reuse scan results for the rest.
    let written: HashSet<&str> = to_write.iter().map(|(p, _, _)| *p).collect();
    let scanned: HashMap<&str, &crate::worktree::File> = files.iter().map(|f| (f.path.as_str(), f)).collect();
    let entries: Result<Vec<_>> = target_files
        .par_iter()
        .map(|(path, (mode, id))| {
            let (size, mtime_ns) = if written.contains(path.as_str()) {
                let (size, mtime, _) = stat_of(&fs::symlink_metadata(repo.root.join(path))?);
                (size, mtime)
            } else if off_disk(path) {
                (0, 0) // not on disk (sparse checkout): nothing to cache
            } else {
                let f = scanned[path.as_str()];
                (f.size, f.mtime_ns)
            };
            Ok((path.clone(), crate::worktree::Entry { size, mtime_ns, mode: *mode, id: *id.as_bytes() }))
        })
        .collect();
    index.entries = entries?.into_iter().collect();
    Ok((to_write.len(), to_remove.len()))
}

pub(crate) fn write_file(repo: &Repo, path: &str, mode: Mode, id: &Id) -> Result<()> {
    let full = repo.root.join(path);
    // Never write through a symlink: if a folder on the way is a link, the file
    // would land wherever it points, perhaps outside the project (git refuses too).
    let mut at = repo.root.clone();
    let parts: Vec<&str> = path.split('/').collect();
    for part in &parts[..parts.len().saturating_sub(1)] {
        at.push(part);
        if fs::symlink_metadata(&at).is_ok_and(|m| m.file_type().is_symlink()) {
            bail!("refusing to write {path}: {} is a symlink, and writing through it could put the file somewhere else", at.strip_prefix(&repo.root).unwrap_or(&at).display());
        }
    }
    if mode == Mode::Submodule {
        // An empty folder for it; `copse submodule update --init` fills it in.
        fs::create_dir_all(&full)?;
        return Ok(());
    }
    if let Some(parent) = full.parent() {
        fs::create_dir_all(parent)?;
    }
    match fs::symlink_metadata(&full) {
        Ok(md) if md.is_dir() => bail!("a directory is in the way"),
        Ok(_) => fs::remove_file(&full)?,
        Err(_) => {}
    }
    if mode == Mode::Link {
        let data = repo.store.get_kind(id, Kind::Blob)?;
        std::os::unix::fs::symlink(OsStr::from_bytes(&data), &full)?;
    } else {
        // An LFS pointer for an LFS path: write the contents it stands for.
        if repo.store.stored_len(id).unwrap_or(0) <= 2048 {
            if let Some(p) = crate::lfs::Pointer::parse(&repo.store.get_kind(id, Kind::Blob)?) {
                if crate::lfs::Attrs::load(&repo.root).tracked(path) {
                    crate::lfs::smudge(&repo.dir, &p, &full)?;
                    set_exec(&full, mode == Mode::Exec)?;
                    return Ok(());
                }
            }
        }
        // Streamed, so a huge file never has to fit in memory.
        let mut out = std::io::BufWriter::with_capacity(1 << 16, fs::File::create(&full)?);
        repo.store.copy_blob_to(id, &mut out)?;
        out.into_inner().map_err(|e| e.into_error())?;
        set_exec(&full, mode == Mode::Exec)?;
    }
    Ok(())
}

fn set_exec(path: &Path, exec: bool) -> Result<()> {
    let mut perms = fs::metadata(path)?.permissions();
    let m = perms.mode();
    // Grant execute wherever read is granted, or strip all execute bits.
    perms.set_mode(if exec { m | ((m & 0o444) >> 2) } else { m & !0o111 });
    fs::set_permissions(path, perms)?;
    Ok(())
}
