//! `copse log`: history, like `git log`.
//!
//! All of history is shown (merged branches included), newest first, and never a
//! parent before its children (git's --topo-order). Options:
//!   --oneline, --graph, -p (changes), --stat, --first-parent, --all,
//!   --author TEXT, --grep TEXT, -n N, starting revisions, and -- PATHS to show only
//!   commits that touched those paths.

use crate::cmd::{format_time, paint};
use crate::objects::{short, Commit, Id};
use crate::repo::Repo;
use anyhow::Result;
use std::collections::{BinaryHeap, HashMap, HashSet};

pub struct LogArgs<'a> {
    pub revs: &'a [String],
    pub limit: usize,
    pub oneline: bool,
    pub graph: bool,
    pub patch: bool,
    pub stat: bool,
    pub first_parent: bool,
    pub all: bool,
    pub author: Option<&'a str>,
    pub grep: Option<&'a str>,
    pub paths: &'a [String],
    /// --format / --pretty
    pub format: Option<crate::pretty::Format>,
    pub date: crate::pretty::DateMode,
}

/// Whether a commit changed anything under `spec`. As in git, a merge counts only if
/// it differs from every parent (otherwise the change belongs to the merged commits).
fn touches(repo: &Repo, c: &Commit, spec: &str) -> Result<bool> {
    let here = crate::objects::entry_at(&repo.store, &c.tree, spec)?;
    if c.parents.is_empty() {
        return Ok(here.is_some());
    }
    for p in &c.parents {
        if crate::objects::entry_at(&repo.store, &repo.read_commit(p)?.tree, spec)? == here {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Commits reachable from `tips`, children always before parents. Newest first by
/// default; with `group_branches`, like git's --topo-order, a merged branch's commits
/// are listed together, which keeps graph lines from crossing back and forth.
fn walk(repo: &Repo, tips: &[Id], first_parent: bool, group_branches: bool) -> Result<Vec<(Id, Commit)>> {
    let parents_of = |c: &Commit| -> Vec<Id> { if first_parent { c.parents.iter().take(1).copied().collect() } else { c.parents.clone() } };
    let mut commits: HashMap<Id, Commit> = HashMap::new();
    let mut stack: Vec<Id> = tips.to_vec();
    while let Some(id) = stack.pop() {
        if commits.contains_key(&id) {
            continue;
        }
        let c = repo.read_commit(&id)?;
        stack.extend(parents_of(&c));
        commits.insert(id, c);
    }
    let mut children: HashMap<Id, usize> = HashMap::new();
    for c in commits.values() {
        for p in parents_of(c) {
            *children.entry(p).or_default() += 1;
        }
    }
    let mut heads: Vec<(i64, [u8; 32])> = commits
        .iter()
        .filter(|(id, _)| children.get(*id).copied().unwrap_or(0) == 0)
        .map(|(id, c)| (c.committer.time, *id.as_bytes()))
        .collect();
    heads.sort();
    // Ready commits: a priority queue (newest first) or, for grouping, a stack.
    let mut by_date: BinaryHeap<(i64, [u8; 32])> = BinaryHeap::new();
    let mut stack: Vec<(i64, [u8; 32])> = Vec::new();
    for h in heads {
        if group_branches { stack.push(h) } else { by_date.push(h) }
    }
    let mut order = Vec::with_capacity(commits.len());
    loop {
        let next = if group_branches { stack.pop() } else { by_date.pop() };
        let Some((_, bytes)) = next else { break };
        let id = Id::from_bytes(bytes);
        let c = commits.remove(&id).expect("each commit is ready once");
        for p in parents_of(&c) {
            let left = children.get_mut(&p).expect("parent was counted");
            *left -= 1;
            if *left == 0 {
                if let Some(pc) = commits.get(&p) {
                    let item = (pc.committer.time, *p.as_bytes());
                    if group_branches { stack.push(item) } else { by_date.push(item) }
                }
            }
        }
        order.push((id, c));
    }
    Ok(order)
}

/// The next line of graph drawn beside a commit's text (empty without --graph).
fn graph_prefix(graph: &mut Option<crate::graph::Graph>) -> String {
    graph.as_mut().map(|g| g.next_line().0).unwrap_or_default()
}

/// What's left of the graph once a commit's text is out (a merge's lines
/// opening, lines meeting), each on its own line.
fn graph_rest(graph: &mut Option<crate::graph::Graph>) {
    if let Some(g) = graph.as_mut() {
        while !g.is_finished() {
            println!("{}", g.next_line().0);
        }
    }
}

pub fn log(args: LogArgs) -> Result<()> {
    let repo = Repo::find_any()?;
    let mut tips = Vec::new();
    if args.all {
        tips.extend(repo.branches()?.into_iter().map(|(_, id)| id));
        tips.extend(repo.tracking_refs()?.into_iter().map(|(_, id)| id));
        for (_, id) in repo.tags()? {
            tips.extend(repo.peel(id).ok());
        }
        tips.extend(repo.head_commit()?);
    }
    for rev in args.revs {
        tips.push(repo.resolve(rev)?);
    }
    if tips.is_empty() {
        match repo.head_commit()? {
            Some(h) => tips.push(h),
            None => {
                println!("No commits yet.");
                return Ok(());
            }
        }
    }
    let unique: HashSet<Id> = tips.iter().copied().collect();
    let tips: Vec<Id> = unique.into_iter().collect();

    let mut labels: HashMap<Id, Vec<String>> = HashMap::new();
    for (name, bid) in repo.branches()?.into_iter().chain(repo.tracking_refs()?) {
        labels.entry(bid).or_default().push(name);
    }
    for (name, id) in repo.tags()? {
        if let Ok(commit) = repo.peel(id) {
            labels.entry(commit).or_default().push(format!("tag: {name}"));
        }
    }

    let specs = if repo.is_bare() {
        args.paths.iter().map(|p| p.trim_end_matches('/').to_string()).collect()
    } else {
        args.paths.iter().map(|p| crate::restore::repo_path(&repo, p)).collect::<Result<Vec<_>>>()?
    };
    let author = args.author.map(str::to_lowercase);
    let grep = args.grep.map(str::to_lowercase);
    let filtering = !specs.is_empty() || author.is_some() || grep.is_some();
    // Branch lines only make sense when every commit is drawn.
    let mut graph = (args.graph && !filtering).then(crate::graph::Graph::new);
    if args.graph && filtering {
        println!("{}", paint("2", "(--graph is left out when filtering commits)"));
    }

    let deco = crate::pretty::Decorations {
        labels: labels.clone(),
        head: repo.head_commit()?,
        head_branch: match repo.head()? {
            crate::repo::Head::Branch(b) => Some(b),
            crate::repo::Head::Detached(_) => None,
        },
    };

    let mut shown = 0;
    // Whether the last entry ended without a newline of its own (format:%s, say).
    let mut missing_newline = false;
    for (id, c) in walk(&repo, &tips, args.first_parent, graph.is_some())? {
        if shown >= args.limit {
            break;
        }
        if let Some(a) = &author {
            if !c.author.who.to_lowercase().contains(a.as_str()) {
                continue;
            }
        }
        if let Some(g) = &grep {
            if !c.message.to_lowercase().contains(g.as_str()) {
                continue;
            }
        }
        if !specs.is_empty() {
            let mut any = false;
            for s in &specs {
                if touches(&repo, &c, s)? {
                    any = true;
                    break;
                }
            }
            if !any {
                continue;
            }
        }
        shown += 1;
        let c = c.for_display();

        let parents: Vec<Id> = if args.first_parent { c.parents.iter().take(1).copied().collect() } else { c.parents.clone() };
        // As in git: the graph moves on to this commit, then a separator line
        // (between multi-line entries) is drawn with the graph's padding, then any
        // lines before the commit's own, and its text starts beside that one.
        if let Some(g) = graph.as_mut() {
            g.update(id, &parents);
        }
        // Entries that don't end themselves (format:, and the named formats but
        // oneline) are separated by a newline: after the graph's padding only if the
        // last entry ended with a newline of its own (else it'd be a gap in it).
        let separated = match &args.format {
            Some(f @ crate::pretty::Format::Named(_)) => !f.is_one_line(),
            Some(crate::pretty::Format::Custom { terminator, .. }) => !terminator,
            None => false,
        };
        if shown > 1 && separated {
            let pad = if missing_newline { String::new() } else { graph.as_mut().map(|g| g.padding_line()).unwrap_or_default() };
            println!("{pad}");
        }
        let mut row = String::new();
        if let Some(g) = graph.as_mut() {
            loop {
                let (line, is_commit) = g.next_line();
                if is_commit {
                    row = line;
                    break;
                }
                println!("{line}");
            }
        }
        let labels_text = labels.get(&id).map(|n| paint("32", &format!(" ({})", n.join(", ")))).unwrap_or_default();
        let title = c.message.lines().next().unwrap_or_default();
        if let Some(format @ crate::pretty::Format::Named(_)) = &args.format {
            let text = crate::pretty::render(format, &id, &c, &deco, args.date);
            let text = text.strip_suffix('\n').unwrap_or(&text);
            let mut lines = text.split('\n');
            let first = lines.next().unwrap_or_default();
            let mut out = format!("{row}{first}");
            for l in lines {
                out += &format!("\n{}{l}", graph_prefix(&mut graph));
            }
            println!("{out}");
            missing_newline = false;
        } else if let Some(format @ crate::pretty::Format::Custom { terminator, .. }) = &args.format {
            // As git prints a custom format beside the graph: each line after the
            // first after the graph's next line (none after a final newline), then
            // what's left of the graph, then tformat:'s newline (after the padding,
            // when the text ended with a newline of its own).
            crate::pretty::set_line_start(crate::pretty::display_width(&row));
            let text = crate::pretty::render(format, &id, &c, &deco, args.date);
            let ends_with_newline = text.ends_with('\n');
            let mut out = row.clone();
            let mut rest = text.as_str();
            while let Some(i) = rest.find('\n') {
                out.push_str(&rest[..=i]);
                rest = &rest[i + 1..];
                if !rest.is_empty() {
                    out.push_str(&graph_prefix(&mut graph));
                }
            }
            out.push_str(rest);
            print!("{out}");
            if graph.as_ref().is_some_and(|g| !g.is_finished()) {
                let mut left = Vec::new();
                while let Some(g) = graph.as_mut().filter(|g| !g.is_finished()) {
                    left.push(g.next_line().0);
                }
                print!("{}{}{}", if ends_with_newline { "" } else { "\n" }, left.join("\n"), if ends_with_newline { "\n" } else { "" });
            }
            if *terminator {
                let pad = if ends_with_newline { graph.as_mut().map(|g| g.padding_line()).unwrap_or_default() } else { String::new() };
                println!("{pad}");
            }
            missing_newline = !ends_with_newline;
        } else if args.oneline {
            println!("{row}{}{labels_text} {title}", paint("33", &short(&id)));
        } else {
            println!("{row}{}{labels_text}  {}  {}", paint("33", &short(&id)), format_time(c.time()), c.author.name());
            for line in c.message.lines() {
                println!("{}    {line}", graph_prefix(&mut graph));
            }
            println!("{}", graph_prefix(&mut graph).trim_end());
        }
        graph_rest(&mut graph);
        // A merge's changes aren't shown (git's default too). Otherwise, after an
        // entry longer than one line, a separating line, and each line of the
        // changes after the graph's padding, as git does.
        if (args.patch || args.stat) && parents.len() <= 1 {
            let one_line = match &args.format {
                Some(f @ crate::pretty::Format::Named(_)) => f.is_one_line(),
                Some(crate::pretty::Format::Custom { .. }) => false,
                None => args.oneline,
            };
            let pad = graph.as_mut().map(|g| g.padding_line()).unwrap_or_default();
            if !one_line && args.format.is_some() {
                println!("{pad}");
            }
            crate::diffs::commit_changes(&repo, &id, &specs, args.stat && !args.patch, &pad)?;
            if args.format.is_none() && !args.oneline {
                println!(); // copse's own format: a blank line before the next commit
            }
        }
    }
    if shown > 0 && matches!(&args.format, Some(crate::pretty::Format::Custom { terminator: false, .. })) && !(args.patch || args.stat) {
        // format: puts newlines only between entries; end the output tidily on a terminal.
        use std::io::IsTerminal;
        if std::io::stdout().is_terminal() {
            println!();
        }
    }
    Ok(())
}
