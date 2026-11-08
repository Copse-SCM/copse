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

/// ASCII branch lines, one lane per line of history being followed.
struct Graph {
    lanes: Vec<Option<Id>>,
}

impl Graph {
    fn lane_chars(&self, star: Option<usize>) -> String {
        let mut out = String::new();
        for (i, lane) in self.lanes.iter().enumerate() {
            out.push(if Some(i) == star { '*' } else if lane.is_some() { '|' } else { ' ' });
            out.push(' ');
        }
        out
    }

    /// A line where the given lanes bend diagonally (`\` opening, `/` joining). As in
    /// git, the diagonal sits between columns: `|\` and `|/`.
    fn transition(&self, bending: &[usize], diagonal: char) -> String {
        let mut cells = vec![' '; self.lanes.len() * 2];
        for (i, lane) in self.lanes.iter().enumerate() {
            if bending.contains(&i) {
                cells[(2 * i).saturating_sub(1)] = diagonal;
            } else if lane.is_some() {
                cells[2 * i] = '|';
            }
        }
        cells.into_iter().collect()
    }

    /// Lines drawn before commit `id` (other lines joining it), its own row, and the
    /// lines drawn after it (a merge opening lines for its other parents).
    fn rows_for(&mut self, id: &Id, parents: &[Id]) -> (Vec<String>, String, Vec<String>) {
        let mut before = Vec::new();
        if let Some(lane) = self.lanes.iter().position(|l| *l == Some(*id)) {
            let joining: Vec<usize> = (0..self.lanes.len()).filter(|&i| i != lane && self.lanes[i] == Some(*id)).collect();
            if !joining.is_empty() {
                before.push(self.transition(&joining, '/'));
                for i in joining {
                    self.lanes[i] = None;
                }
                while self.lanes.last() == Some(&None) {
                    self.lanes.pop();
                }
            }
        }
        let lane = match self.lanes.iter().position(|l| *l == Some(*id)) {
            Some(i) => i,
            None => match self.lanes.iter().position(Option::is_none) {
                Some(free) => {
                    self.lanes[free] = Some(*id);
                    free
                }
                None => {
                    self.lanes.push(Some(*id));
                    self.lanes.len() - 1
                }
            },
        };
        let row = self.lane_chars(Some(lane));
        let mut after = Vec::new();

        self.lanes[lane] = parents.first().copied();
        // A merge opens a lane for each further parent not already being followed.
        for p in parents.iter().skip(1) {
            if self.lanes.contains(&Some(*p)) {
                continue;
            }
            let slot = match self.lanes.iter().skip(lane + 1).position(Option::is_none) {
                Some(off) => lane + 1 + off,
                None => {
                    self.lanes.push(None);
                    self.lanes.len() - 1
                }
            };
            self.lanes[slot] = Some(*p);
            after.push(self.transition(&[slot], '\\'));
        }
        while self.lanes.last() == Some(&None) {
            self.lanes.pop();
        }
        (before, row, after)
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
    let mut graph = (args.graph && !filtering).then(|| Graph { lanes: Vec::new() });
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
        let (before, row, after, rail) = match graph.as_mut() {
            Some(g) => {
                let (before, row, after) = g.rows_for(&id, &parents);
                let rail = g.lane_chars(None);
                (before, row, after, rail)
            }
            None => (Vec::new(), String::new(), Vec::new(), String::new()),
        };
        for line in before {
            println!("{}", line.trim_end());
        }
        let labels_text = labels.get(&id).map(|n| paint("32", &format!(" ({})", n.join(", ")))).unwrap_or_default();
        let title = c.message.lines().next().unwrap_or_default();
        if let Some(format) = &args.format {
            // A blank line between multi-line entries; format: (not tformat:) separates with a newline.
            let custom_separator = matches!(format, crate::pretty::Format::Custom { terminator: false, .. });
            if shown > 1 && (custom_separator || matches!(format, crate::pretty::Format::Named(_)) && !format.is_one_line()) {
                println!("{}", rail.trim_end());
            }
            let text = crate::pretty::render(format, &id, &c, &deco, args.date);
            let text = text.strip_suffix('\n').unwrap_or(&text);
            let mut lines = text.split('\n');
            let first = lines.next().unwrap_or_default();
            let mut out = format!("{row}{first}");
            for l in lines {
                out += &format!("\n{rail}{l}");
            }
            if custom_separator {
                print!("{out}");
            } else {
                println!("{out}");
            }
        } else if args.oneline {
            println!("{row}{}{labels_text} {title}", paint("33", &short(&id)));
        } else {
            println!("{row}{}{labels_text}  {}  {}", paint("33", &short(&id)), format_time(c.time()), c.author.name());
            for line in c.message.lines() {
                println!("{rail}    {line}");
            }
            println!("{}", rail.trim_end());
        }
        for line in after {
            println!("{}", line.trim_end());
        }
        if args.patch || args.stat {
            if matches!(&args.format, Some(crate::pretty::Format::Custom { terminator: false, .. })) {
                println!();
            }
            crate::diffs::commit_changes(&repo, &id, &specs, args.stat && !args.patch)?;
            println!();
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
