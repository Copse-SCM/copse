//! `log --graph`: the branch lines drawn beside each commit, laid out exactly as
//! git draws them. This is a port of git's own graph.c state machine (without
//! colours): each commit is drawn in up to six kinds of line,
//!
//!   padding      `| |`     lines carried on unchanged
//!   skip         `...`     part of the graph left out
//!   pre-commit   `| \`     room made for an octopus merge
//!   commit       `* |`     the commit itself (`*-.` for an octopus merge)
//!   post-merge   `|\ \`    a merge's parents opening
//!   collapsing   `|/ /`    lines that meet moving left
//!
//! and every line is padded to the graph's width, so text after it lines up
//! (git keeps those trailing spaces, and so does copse).

use crate::objects::Id;

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Padding,
    Skip,
    PreCommit,
    Commit,
    PostMerge,
    Collapsing,
}

const MERGE_CHARS: [char; 3] = ['/', '|', '\\'];

pub struct Graph {
    commit: Option<Id>,
    /// The commit's parents that are drawn.
    parents: Vec<Id>,
    width: i32,
    expansion_row: i32,
    state: State,
    prev_state: State,
    commit_index: i32,
    prev_commit_index: i32,
    merge_layout: i32,
    edges_added: i32,
    prev_edges_added: i32,
    columns: Vec<Id>,
    new_columns: Vec<Id>,
    mapping: Vec<i32>,
    old_mapping: Vec<i32>,
    mapping_size: usize,
}

impl Default for Graph {
    fn default() -> Self {
        Self::new()
    }
}

impl Graph {
    pub fn new() -> Graph {
        Graph {
            commit: None,
            parents: Vec::new(),
            width: 0,
            expansion_row: 0,
            state: State::Padding,
            prev_state: State::Padding,
            commit_index: 0,
            prev_commit_index: 0,
            merge_layout: 0,
            edges_added: 0,
            prev_edges_added: 0,
            columns: Vec::new(),
            new_columns: Vec::new(),
            mapping: Vec::new(),
            old_mapping: Vec::new(),
            mapping_size: 0,
        }
    }

    fn num_parents(&self) -> i32 {
        self.parents.len() as i32
    }

    fn num_columns(&self) -> i32 {
        self.columns.len() as i32
    }

    /// Start drawing `commit`, whose drawn parents are `parents`.
    pub fn update(&mut self, commit: Id, parents: &[Id]) {
        self.commit = Some(commit);
        self.parents = parents.to_vec();
        self.prev_commit_index = self.commit_index;
        self.update_columns();
        self.expansion_row = 0;
        self.state = if self.state != State::Padding {
            State::Skip
        } else if self.needs_pre_commit_line() {
            State::PreCommit
        } else {
            State::Commit
        };
    }

    /// Whether everything for the current commit has been drawn.
    pub fn is_finished(&self) -> bool {
        self.state == State::Padding
    }

    fn set_state(&mut self, s: State) {
        self.prev_state = self.state;
        self.state = s;
    }

    fn num_dashed_parents(&self) -> i32 {
        self.num_parents() + self.merge_layout - 3
    }

    fn num_expansion_rows(&self) -> i32 {
        self.num_dashed_parents() * 2
    }

    fn needs_pre_commit_line(&self) -> bool {
        self.num_parents() >= 3 && self.commit_index < self.num_columns() - 1 && self.expansion_row < self.num_expansion_rows()
    }

    fn find_new_column(&self, id: &Id) -> Option<usize> {
        self.new_columns.iter().position(|c| c == id)
    }

    fn insert_into_new_columns(&mut self, commit: Id, idx: i32) {
        let i = match self.find_new_column(&commit) {
            Some(i) => i as i32,
            None => {
                self.new_columns.push(commit);
                self.new_columns.len() as i32 - 1
            }
        };
        let mapping_idx;
        if self.num_parents() > 1 && idx > -1 && self.merge_layout == -1 {
            // The first parent of a merge: lay the merge out according to whether
            // that parent's column is to the left of it.
            let dist = idx - i;
            let shift = if dist > 1 { 2 * dist - 3 } else { 1 };
            self.merge_layout = if dist > 0 { 0 } else { 1 };
            self.edges_added = self.num_parents() + self.merge_layout - 2;
            mapping_idx = self.width + (self.merge_layout - 1) * shift;
            self.width += 2 * self.merge_layout;
        } else if self.edges_added > 0 && self.width >= 2 && i == self.mapping[(self.width - 2) as usize] {
            // A merge added columns, but this parent is the last existing one:
            // the two edges join at once.
            mapping_idx = self.width - 2;
            self.edges_added = -1;
        } else {
            mapping_idx = self.width;
            self.width += 2;
        }
        self.mapping[mapping_idx as usize] = i;
    }

    fn update_columns(&mut self) {
        std::mem::swap(&mut self.columns, &mut self.new_columns);
        self.new_columns.clear();
        let max_new_columns = (self.columns.len() + self.parents.len()) as i32;
        self.mapping_size = (2 * max_new_columns) as usize;
        // The mapping the last line was drawn from becomes the old one: a commit
        // line just after lines collapsed bends to meet them (`* /`).
        let room = self.mapping_size + 4;
        self.old_mapping = std::mem::replace(&mut self.mapping, vec![-1; room]);
        if self.old_mapping.len() < room {
            self.old_mapping.resize(room, -1);
        }
        self.width = 0;
        self.prev_edges_added = self.edges_added;
        self.edges_added = 0;
        let commit = self.commit.expect("update sets it");
        let mut seen_this = false;
        let num_columns = self.columns.len();
        for i in 0..=num_columns {
            let col_commit = if i == num_columns {
                if seen_this {
                    break;
                }
                commit
            } else {
                self.columns[i]
            };
            if col_commit == commit {
                seen_this = true;
                self.commit_index = i as i32;
                self.merge_layout = -1;
                for p in self.parents.clone() {
                    self.insert_into_new_columns(p, i as i32);
                }
                // The commit always takes up at least two columns.
                if self.parents.is_empty() {
                    self.width += 2;
                }
            } else {
                self.insert_into_new_columns(col_commit, -1);
            }
        }
        while self.mapping_size > 1 && self.mapping[self.mapping_size - 1] < 0 {
            self.mapping_size -= 1;
        }
    }

    fn is_mapping_correct(&self) -> bool {
        (0..self.mapping_size).all(|i| {
            let target = self.mapping[i];
            target < 0 || target == (i / 2) as i32
        })
    }

    /// The next line of the drawing, and whether it's the commit's own line.
    pub fn next_line(&mut self) -> (String, bool) {
        let mut line = String::new();
        let mut shown = false;
        match self.state {
            State::Padding => self.padding_output(&mut line),
            State::Skip => self.skip_line(&mut line),
            State::PreCommit => self.pre_commit_line(&mut line),
            State::Commit => {
                self.commit_line(&mut line);
                shown = true;
            }
            State::PostMerge => self.post_merge_line(&mut line),
            State::Collapsing => self.collapsing_line(&mut line),
        }
        self.pad(&mut line);
        (line, shown)
    }

    /// A line that leaves every branch line as it is (between a commit's entries).
    pub fn padding_line(&mut self) -> String {
        if self.state != State::Commit {
            return self.next_line().0;
        }
        let mut line = String::new();
        for c in &self.columns {
            line.push('|');
            if Some(*c) == self.commit && self.parents.len() > 2 {
                line.push_str(&" ".repeat((self.parents.len() - 2) * 2));
            } else {
                line.push(' ');
            }
        }
        self.pad(&mut line);
        self.prev_state = State::Padding;
        line
    }

    fn pad(&self, line: &mut String) {
        let have = line.chars().count() as i32;
        if have < self.width {
            line.push_str(&" ".repeat((self.width - have) as usize));
        }
    }

    fn padding_output(&self, line: &mut String) {
        for _ in &self.new_columns {
            line.push_str("| ");
        }
    }

    fn skip_line(&mut self, line: &mut String) {
        line.push_str("...");
        let next = if self.needs_pre_commit_line() { State::PreCommit } else { State::Commit };
        self.set_state(next);
    }

    fn pre_commit_line(&mut self, line: &mut String) {
        let mut seen_this = false;
        for i in 0..self.columns.len() {
            if Some(self.columns[i]) == self.commit {
                seen_this = true;
                line.push('|');
                line.push_str(&" ".repeat(self.expansion_row.max(0) as usize));
            } else if seen_this && self.expansion_row == 0 {
                if self.prev_state == State::PostMerge && self.prev_commit_index < i as i32 {
                    line.push('\\');
                } else {
                    line.push('|');
                }
            } else if seen_this && self.expansion_row > 0 {
                line.push('\\');
            } else {
                line.push('|');
            }
            line.push(' ');
        }
        self.expansion_row += 1;
        if !self.needs_pre_commit_line() {
            self.set_state(State::Commit);
        }
    }

    fn commit_line(&mut self, line: &mut String) {
        let mut seen_this = false;
        let num_columns = self.columns.len();
        for i in 0..=num_columns {
            let col_commit = if i == num_columns {
                if seen_this {
                    break;
                }
                self.commit
            } else {
                Some(self.columns[i])
            };
            if col_commit == self.commit {
                seen_this = true;
                line.push('*');
                if self.parents.len() > 2 {
                    let dashed = self.num_dashed_parents();
                    for d in 0..dashed {
                        line.push('-');
                        line.push(if d == dashed - 1 { '.' } else { '-' });
                    }
                }
            } else if seen_this && self.edges_added > 1 {
                line.push('\\');
            } else if seen_this && self.edges_added == 1 {
                if self.prev_state == State::PostMerge && self.prev_edges_added > 0 && self.prev_commit_index < i as i32 {
                    line.push('\\');
                } else {
                    line.push('|');
                }
            } else if self.prev_state == State::Collapsing
                && self.old_mapping.get(2 * i + 1) == Some(&(i as i32))
                && self.mapping.get(2 * i).is_some_and(|m| *m < i as i32)
            {
                line.push('/');
            } else {
                line.push('|');
            }
            line.push(' ');
        }
        let next = if self.parents.len() > 1 {
            State::PostMerge
        } else if self.is_mapping_correct() {
            State::Padding
        } else {
            State::Collapsing
        };
        self.set_state(next);
    }

    fn post_merge_line(&mut self, line: &mut String) {
        let mut seen_this = false;
        let mut parent_seen = false;
        let first_parent = self.parents.first().copied();
        let num_columns = self.columns.len();
        for i in 0..=num_columns {
            let col_commit = if i == num_columns {
                if seen_this {
                    break;
                }
                self.commit
            } else {
                Some(self.columns[i])
            };
            if col_commit == self.commit {
                seen_this = true;
                let mut idx = self.merge_layout.max(0) as usize;
                for j in 0..self.parents.len() {
                    line.push(MERGE_CHARS[idx.min(2)]);
                    if idx == 2 {
                        if self.edges_added > 0 || j + 1 < self.parents.len() {
                            line.push(' ');
                        }
                    } else {
                        idx += 1;
                    }
                }
                if self.edges_added == 0 {
                    line.push(' ');
                }
            } else if seen_this {
                line.push(if self.edges_added > 0 { '\\' } else { '|' });
                line.push(' ');
            } else {
                line.push('|');
                if self.merge_layout != 0 || i as i32 != self.commit_index - 1 {
                    line.push(if parent_seen { '_' } else { ' ' });
                }
            }
            if col_commit.is_some() && col_commit == first_parent {
                parent_seen = true;
            }
        }
        let next = if self.is_mapping_correct() { State::Padding } else { State::Collapsing };
        self.set_state(next);
    }

    fn collapsing_line(&mut self, line: &mut String) {
        let mut used_horizontal = false;
        let mut horizontal_edge: i32 = -1;
        let mut horizontal_edge_target: i32 = -1;
        std::mem::swap(&mut self.mapping, &mut self.old_mapping);
        if self.mapping.len() < self.old_mapping.len() {
            self.mapping.resize(self.old_mapping.len(), -1);
        }
        for i in 0..self.mapping_size {
            self.mapping[i] = -1;
        }
        for i in 0..self.mapping_size {
            let target = self.old_mapping[i];
            if target < 0 {
                continue;
            }
            let i32_ = i as i32;
            if target * 2 == i32_ {
                self.mapping[i] = target;
            } else if i >= 1 && self.mapping[i - 1] < 0 {
                // Nothing to the left: move left by one.
                self.mapping[i - 1] = target;
                if horizontal_edge == -1 {
                    horizontal_edge = i32_;
                    horizontal_edge_target = target;
                    let mut j = target * 2 + 3;
                    while j < i32_ - 2 {
                        self.mapping[j as usize] = target;
                        j += 2;
                    }
                }
            } else if i >= 1 && self.mapping[i - 1] == target {
                // Joins the line to its left, which has the same parent.
            } else if i >= 2 {
                // Crosses over the line to its left.
                self.mapping[i - 2] = target;
                if horizontal_edge == -1 {
                    horizontal_edge_target = target;
                    horizontal_edge = i32_ - 1;
                    let mut j = target * 2 + 3;
                    while j < i32_ - 2 {
                        self.mapping[j as usize] = target;
                        j += 2;
                    }
                }
            }
        }
        if self.mapping_size > 0 && self.mapping[self.mapping_size - 1] < 0 {
            self.mapping_size -= 1;
        }
        for i in 0..self.mapping_size {
            let target = self.mapping[i];
            let i32_ = i as i32;
            if target < 0 {
                line.push(' ');
            } else if target * 2 == i32_ {
                line.push('|');
            } else if target == horizontal_edge_target && i32_ != horizontal_edge - 1 {
                // All but the first segment of the horizontal edge stop here.
                if i32_ != target * 2 + 3 {
                    self.mapping[i] = -1;
                }
                used_horizontal = true;
                line.push('_');
            } else {
                if used_horizontal && i32_ < horizontal_edge {
                    self.mapping[i] = -1;
                }
                line.push('/');
            }
        }
        if self.is_mapping_correct() {
            self.set_state(State::Padding);
        }
    }
}
