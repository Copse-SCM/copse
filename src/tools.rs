//! `copse clean` and `copse grep`.
//!
//!   copse clean [-f] [-x] [PATHS]   remove new files that were never committed
//!                                   (without -f, just list them; -x includes ignored files)
//!   copse grep PATTERN [REV] [-- PATHS]
//!                                   search your files (or REV's) with a regular expression

use crate::cmd::paint;
use crate::merge::MergeState;
use crate::objects::{flatten, Kind, Mode};
use crate::rebase::RebaseState;
use crate::repo::Repo;
use crate::worktree::{changes, ignored_paths, read_content, snapshot, tracked_paths, Index};
use anyhow::{bail, Context, Result};
use rayon::prelude::*;
use std::fs;

fn under(specs: &[String], path: &str) -> bool {
    specs.is_empty() || specs.iter().any(|s| s.is_empty() || path == s || path.starts_with(&format!("{s}/")))
}

/// `copse clean [-f] [-x] [PATHS]`
pub fn clean(force: bool, ignored: bool, paths: &[String]) -> Result<()> {
    let repo = Repo::find()?;
    if let Some(r) = RebaseState::load(&repo)? {
        bail!("{}", r.busy());
    }
    if MergeState::load(&repo)?.is_some() {
        bail!("a merge is in progress; finish it with `copse commit` or undo it with `copse merge --abort` first");
    }
    let specs = paths.iter().map(|p| crate::restore::repo_path(&repo, p)).collect::<Result<Vec<_>>>()?;
    let index = Index::load(&repo.wt)?;
    let ch = changes(&index, &snapshot(&repo.root, &index, None)?);
    let new_files: Vec<String> = ch.added.iter().filter(|p| under(&specs, p)).cloned().collect();
    let ignored_files: Vec<String> =
        if ignored { ignored_paths(&repo.root).into_iter().filter(|p| under(&specs, p)).collect() } else { Vec::new() };
    if new_files.is_empty() && ignored_files.is_empty() {
        println!("Nothing to clean.");
        return Ok(());
    }

    let verb = if force { "Removing" } else { "Would remove" };
    for p in &new_files {
        println!("{verb} {p}");
    }
    for p in &ignored_files {
        println!("{verb} {p} {}", paint("2", "(ignored)"));
    }
    if !force {
        println!("Run `copse clean -f` to remove them.");
        return Ok(());
    }

    // New files can be brought back from a backup stash; ignored files (build output
    // and the like) aren't kept.
    if !new_files.is_empty() {
        crate::stash::backup(&repo, "before clean")?;
    }
    for p in new_files.iter().chain(&ignored_files) {
        let full = crate::objects::disk_path(&repo.root, p);
        fs::remove_file(&full).with_context(|| format!("removing {p}"))?;
        let mut dir = full.parent();
        while let Some(d) = dir {
            if d == repo.root || fs::remove_dir(d).is_err() {
                break;
            }
            dir = d.parent();
        }
    }
    println!("Removed {} file(s).", new_files.len() + ignored_files.len());
    Ok(())
}

pub struct GrepArgs<'a> {
    pub pattern: &'a str,
    pub rev: Option<&'a str>,
    pub paths: &'a [String],
    pub ignore_case: bool,
    pub files_only: bool,
    pub count: bool,
}

/// `copse grep PATTERN [REV] [-- PATHS]`. Exits with status 1 when nothing matches, like grep.
pub fn grep(args: GrepArgs) -> Result<()> {
    let repo = Repo::find_any()?;
    let re = regex::RegexBuilder::new(args.pattern)
        .case_insensitive(args.ignore_case)
        .build()
        .with_context(|| format!("{:?} isn't a valid pattern", args.pattern))?;
    let specs = if repo.is_bare() {
        args.paths.iter().map(|p| p.trim_end_matches('/').to_string()).collect()
    } else {
        args.paths.iter().map(|p| crate::restore::repo_path(&repo, p)).collect::<Result<Vec<_>>>()?
    };

    // What to search: files in a commit, or the working directory.
    let (prefix, sources): (String, Vec<(String, Option<crate::objects::Id>)>) = match args.rev {
        Some(rev) => {
            let id = repo.resolve(rev)?;
            let tree = flatten(&repo.store, &repo.read_commit(&id)?.tree)?;
            let files = tree.into_iter().filter(|(p, (m, _))| *m != Mode::Link && *m != Mode::Submodule && under(&specs, p)).map(|(p, (_, id))| (p, Some(id))).collect();
            (format!("{rev}:"), files)
        }
        None => {
            if repo.is_bare() {
                bail!("this repository has no working files; give a revision to search: copse grep PATTERN REV");
            }
            (String::new(), tracked_paths(&repo.root).into_iter().filter(|p| under(&specs, p)).map(|p| (p, None)).collect())
        }
    };

    let mut results: Vec<(String, Vec<(usize, String)>)> = sources
        .par_iter()
        .filter_map(|(path, id)| {
            let bytes = match id {
                Some(id) => repo.store.get_kind(id, Kind::Blob).ok()?,
                None => read_content(&crate::objects::disk_path(&repo.root, path), Mode::File).ok()?,
            };
            let text = std::str::from_utf8(&bytes).ok().filter(|t| !t.contains('\0'))?; // skip binary files
            let hits: Vec<(usize, String)> =
                text.lines().enumerate().filter(|(_, l)| re.is_match(l)).map(|(i, l)| (i + 1, l.to_string())).collect();
            (!hits.is_empty()).then(|| (path.clone(), hits))
        })
        .collect();
    results.sort_by(|a, b| a.0.cmp(&b.0));
    if results.is_empty() {
        std::process::exit(1);
    }
    for (path, hits) in &results {
        let shown = paint("35", &format!("{prefix}{path}"));
        if args.files_only {
            println!("{shown}");
        } else if args.count {
            println!("{shown}:{}", hits.len());
        } else {
            for (n, line) in hits {
                let lit = re.replace_all(line, |c: &regex::Captures| paint("1;31", &c[0]));
                println!("{shown}:{}:{lit}", paint("32", &n.to_string()));
            }
        }
    }
    Ok(())
}
