//! `copse` on its own: a terminal interface over the same commands.
//!
//!   1 Changes    what changed, with the diff; pick files, commit, discard
//!   2 History    the commit graph, with each commit's details and diff
//!   3 Branches   switch, create, delete, merge; pull and push
//!   4 Undo       every move of HEAD as a timeline; undo, or go back to any point
//!
//! Screens read the repository directly; every action runs the real `copse`
//! command and shows what it printed, so the TUI does exactly what the command
//! line does. `copse ui --replay KEYS` drives it without a terminal and prints the
//! screen, for tests.

mod data;

use crate::repo::Repo;
use anyhow::Result;
use data::{BranchRow, Change, LogRow, UndoRow};
use ratatui::backend::Backend;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Tabs, Wrap};
use ratatui::{Frame, Terminal};
use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Tab {
    Changes,
    History,
    Branches,
    Undo,
}

const TABS: [Tab; 4] = [Tab::Changes, Tab::History, Tab::Branches, Tab::Undo];

impl Tab {
    fn title(self) -> &'static str {
        match self {
            Tab::Changes => "1 Changes",
            Tab::History => "2 History",
            Tab::Branches => "3 Branches",
            Tab::Undo => "4 Undo",
        }
    }
}

/// What an Enter in a text box or a "y" in a confirmation does.
#[derive(Clone, Debug)]
enum Pending {
    Commit,
    NewBranch { at: Option<String> },
    /// Run `copse ARGS`.
    Run(Vec<String>),
}

#[derive(Clone, Debug)]
enum Mode {
    Normal,
    Input { prompt: String, text: String, then: Pending },
    Confirm { question: String, then: Pending },
    Output { title: String, text: String, ok: bool, scroll: u16 },
    Help,
}

struct App {
    exe: PathBuf,
    repo: Repo,
    tab: Tab,
    mode: Mode,
    header: String,
    changes: Vec<Change>,
    picked: BTreeSet<String>,
    history: Vec<LogRow>,
    branches: Vec<BranchRow>,
    undo: Vec<UndoRow>,
    /// Selected row on each screen.
    sel: HashMap<&'static str, usize>,
    detail_scroll: u16,
    /// The detail pane's text, keyed by what it shows (so it's loaded once).
    detail: (String, Vec<String>),
    message: String,
    quit: bool,
}

impl App {
    fn new(exe: PathBuf) -> Result<App> {
        let repo = Repo::find()?;
        let mut app = App {
            exe,
            repo,
            tab: Tab::Changes,
            mode: Mode::Normal,
            header: String::new(),
            changes: vec![],
            picked: BTreeSet::new(),
            history: vec![],
            branches: vec![],
            undo: vec![],
            sel: HashMap::new(),
            detail_scroll: 0,
            detail: (String::new(), vec![]),
            message: "? for help · q to quit".into(),
            quit: false,
        };
        app.reload();
        // Start the Branches screen on the branch you're on.
        if let Some(i) = app.branches.iter().position(|b| b.current) {
            app.sel.insert("branches", i);
        }
        Ok(app)
    }

    /// Re-read everything (after an action, or on r).
    fn reload(&mut self) {
        if let Ok(r) = Repo::find() {
            self.repo = r; // fresh: shallow/partial/config state may have changed
        }
        self.changes = data::load_changes(&self.repo).unwrap_or_default();
        let present: BTreeSet<String> = self.changes.iter().map(|c| c.path.clone()).collect();
        self.picked.retain(|p| present.contains(p));
        self.history = data::load_history(&self.exe, &self.repo.root, 500);
        self.branches = data::load_branches(&self.repo).unwrap_or_default();
        self.undo = data::load_undo(&self.repo).unwrap_or_default();
        self.header = data::header(&self.repo, self.changes.len());
        self.detail.0.clear();
        for (key, len) in [("changes", self.changes.len()), ("history", self.history.len()), ("branches", self.branches.len()), ("undo", self.undo.len())] {
            let s = self.sel.entry(key).or_insert(0);
            if *s >= len {
                *s = len.saturating_sub(1);
            }
        }
        self.load_detail();
    }

    fn key(&self) -> &'static str {
        match self.tab {
            Tab::Changes => "changes",
            Tab::History => "history",
            Tab::Branches => "branches",
            Tab::Undo => "undo",
        }
    }

    fn len(&self) -> usize {
        match self.tab {
            Tab::Changes => self.changes.len(),
            Tab::History => self.history.len(),
            Tab::Branches => self.branches.len(),
            Tab::Undo => self.undo.len(),
        }
    }

    fn selected(&self) -> usize {
        self.sel.get(self.key()).copied().unwrap_or(0)
    }

    fn move_by(&mut self, delta: isize) {
        let len = self.len();
        if len == 0 {
            return;
        }
        let mut i = self.selected() as isize;
        loop {
            i = (i + delta).clamp(0, len as isize - 1);
            // In the history, skip lines that are only graph.
            let skip = self.tab == Tab::History && self.history[i as usize].id.is_none();
            if !skip || i == 0 || i == len as isize - 1 {
                break;
            }
        }
        self.sel.insert(self.key(), i as usize);
        self.detail_scroll = 0;
        self.load_detail();
    }

    /// The right-hand pane: the selected file's diff, or commit's details.
    fn load_detail(&mut self) {
        let (key, args): (String, Vec<String>) = match self.tab {
            Tab::Changes => match self.changes.get(self.selected()) {
                Some(c) => (format!("diff {}", c.path), vec!["diff".into(), "--".into(), c.path.clone()]),
                None => (String::new(), vec![]),
            },
            Tab::History => match self.history.get(self.selected()).and_then(|r| r.id.clone()) {
                Some(id) => (format!("show {id}"), vec!["show".into(), id]),
                None => (String::new(), vec![]),
            },
            Tab::Branches => match self.branches.get(self.selected()) {
                Some(b) => (format!("log {}", b.name), vec!["log".into(), "--oneline".into(), "-n".into(), "30".into(), b.name.clone()]),
                None => (String::new(), vec![]),
            },
            Tab::Undo => match self.undo.get(self.selected()).and_then(|u| u.id) {
                Some(id) => (format!("stat {id}"), vec!["show".into(), "--stat".into(), id.to_hex().to_string()]),
                None => (String::new(), vec![]),
            },
        };
        if key == self.detail.0 {
            return;
        }
        let lines = if args.is_empty() {
            vec![]
        } else {
            let refs: Vec<&str> = args.iter().map(String::as_str).collect();
            data::run_copse(&self.exe, &self.repo.root, &refs).1.lines().map(str::to_string).collect()
        };
        self.detail = (key, lines);
    }

    /// Run a copse command, show its output, and re-read everything.
    fn run(&mut self, args: &[String]) {
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let (ok, text) = data::run_copse(&self.exe, &self.repo.root, &refs);
        let title = format!("copse {}", args.join(" "));
        self.reload();
        let text = if text.trim().is_empty() { "(done)".to_string() } else { text };
        self.message = format!("{} {}", if ok { "✓" } else { "✗" }, title);
        self.mode = Mode::Output { title, text, ok, scroll: 0 };
    }

    fn confirm(&mut self, question: impl Into<String>, args: Vec<&str>) {
        self.mode = Mode::Confirm { question: question.into(), then: Pending::Run(args.into_iter().map(String::from).collect()) };
    }

    fn finish(&mut self, then: Pending, text: String) {
        self.mode = Mode::Normal;
        match then {
            Pending::Commit => {
                let msg = text.trim();
                if msg.is_empty() {
                    self.message = "Commit cancelled: the message was empty.".into();
                    return;
                }
                let mut args = vec!["commit".to_string(), "-m".into(), msg.to_string()];
                args.extend(self.picked.iter().cloned());
                self.picked.clear();
                self.run(&args);
            }
            Pending::NewBranch { at } => {
                let name = text.trim();
                if name.is_empty() {
                    self.message = "No branch made: the name was empty.".into();
                    return;
                }
                let mut args = vec!["branch".to_string(), name.to_string()];
                args.extend(at);
                self.run(&args);
            }
            Pending::Run(args) => self.run(&args),
        }
    }

    fn handle(&mut self, k: KeyEvent) {
        if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c') {
            self.quit = true;
            return;
        }
        match std::mem::replace(&mut self.mode, Mode::Normal) {
            Mode::Input { prompt, mut text, then } => {
                match k.code {
                    KeyCode::Enter => return self.finish(then, text),
                    KeyCode::Esc => {
                        self.message = "Cancelled.".into();
                        return;
                    }
                    KeyCode::Backspace => {
                        text.pop();
                    }
                    KeyCode::Char(c) => text.push(c),
                    _ => {}
                }
                self.mode = Mode::Input { prompt, text, then };
            }
            Mode::Confirm { question, then } => match k.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => self.finish(then, String::new()),
                KeyCode::Char('n') | KeyCode::Esc | KeyCode::Char('q') => self.message = "Cancelled.".into(),
                _ => self.mode = Mode::Confirm { question, then },
            },
            Mode::Output { title, text, ok, scroll } => match k.code {
                KeyCode::Down | KeyCode::Char('j') => self.mode = Mode::Output { title, text, ok, scroll: scroll.saturating_add(1) },
                KeyCode::Up | KeyCode::Char('k') => self.mode = Mode::Output { title, text, ok, scroll: scroll.saturating_sub(1) },
                _ => {} // any other key closes it
            },
            Mode::Help => {}
            Mode::Normal => self.normal_key(k),
        }
    }

    fn normal_key(&mut self, k: KeyEvent) {
        match k.code {
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Char('?') => self.mode = Mode::Help,
            KeyCode::Char(c @ '1'..='4') => self.switch(TABS[(c as u8 - b'1') as usize]),
            KeyCode::Tab => self.switch(TABS[(TABS.iter().position(|t| *t == self.tab).unwrap() + 1) % 4]),
            KeyCode::BackTab => self.switch(TABS[(TABS.iter().position(|t| *t == self.tab).unwrap() + 3) % 4]),
            KeyCode::Down | KeyCode::Char('j') => self.move_by(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_by(-1),
            KeyCode::PageDown | KeyCode::Char('J') => self.detail_scroll = self.detail_scroll.saturating_add(10),
            KeyCode::PageUp | KeyCode::Char('K') => self.detail_scroll = self.detail_scroll.saturating_sub(10),
            KeyCode::Home | KeyCode::Char('g') => self.move_by(-(self.len() as isize)),
            KeyCode::End | KeyCode::Char('G') => self.move_by(self.len() as isize),
            KeyCode::Char('r') => {
                self.reload();
                self.message = "Refreshed.".into();
            }
            KeyCode::Char('F') => self.run(&["fetch".into()]),
            KeyCode::Char('L') => self.run(&["pull".into()]),
            KeyCode::Char('P') => self.run(&["push".into()]),
            KeyCode::Char(c) => self.screen_key(c),
            KeyCode::Enter => self.screen_key('\n'),
            _ => {}
        }
    }

    fn switch(&mut self, tab: Tab) {
        self.tab = tab;
        self.detail_scroll = 0;
        self.load_detail();
    }

    fn screen_key(&mut self, c: char) {
        let i = self.selected();
        match self.tab {
            Tab::Changes => {
                let cur = self.changes.get(i).cloned();
                match c {
                    ' ' => {
                        if let Some(ch) = cur {
                            if !self.picked.remove(&ch.path) {
                                self.picked.insert(ch.path);
                            }
                            self.move_by(1);
                        }
                    }
                    'a' => {
                        if self.picked.len() == self.changes.len() {
                            self.picked.clear();
                        } else {
                            self.picked = self.changes.iter().map(|c| c.path.clone()).collect();
                        }
                    }
                    'c' => {
                        if self.changes.is_empty() {
                            self.message = "Nothing to commit.".into();
                            return;
                        }
                        let what = if self.picked.is_empty() { format!("all {} change(s)", self.changes.len()) } else { format!("{} picked file(s)", self.picked.len()) };
                        self.mode = Mode::Input { prompt: format!("Commit {what}: message"), text: String::new(), then: Pending::Commit };
                    }
                    'x' => {
                        if let Some(ch) = cur {
                            // New files are removed with clean (which keeps a backup); others restored.
                            let (q, args) = if ch.kind == 'A' {
                                (format!("Delete the new file {}? (a backup is kept as a stash)", ch.path), vec!["clean", "-f", ch.path.as_str()])
                            } else {
                                (format!("Discard your changes to {}? (a backup is kept as a stash)", ch.path), vec!["restore", ch.path.as_str()])
                            };
                            let args: Vec<String> = args.into_iter().map(String::from).collect();
                            self.mode = Mode::Confirm { question: q, then: Pending::Run(args) };
                        }
                    }
                    's' => self.confirm("Stash all your changes?", vec!["stash"]),
                    _ => {}
                }
            }
            Tab::History => {
                let Some(id) = self.history.get(i).and_then(|r| r.id.clone()) else { return };
                let title = self.history[i].title.clone();
                match c {
                    'v' => self.mode = Mode::Confirm { question: format!("Revert {id} \"{title}\"? (adds a commit that undoes it)"), then: Pending::Run(vec!["revert".into(), id]) },
                    'o' | '\n' => self.mode = Mode::Confirm { question: format!("Check out {id} (detached)?"), then: Pending::Run(vec!["checkout".into(), id]) },
                    'b' => self.mode = Mode::Input { prompt: format!("New branch at {id}: name"), text: String::new(), then: Pending::NewBranch { at: Some(id) } },
                    'p' => self.mode = Mode::Confirm { question: format!("Cherry-pick {id} \"{title}\" onto the current branch?"), then: Pending::Run(vec!["cherry-pick".into(), id]) },
                    _ => {}
                }
            }
            Tab::Branches => {
                let Some(b) = self.branches.get(i).cloned() else {
                    if c == 'n' {
                        self.mode = Mode::Input { prompt: "New branch: name".into(), text: String::new(), then: Pending::NewBranch { at: None } };
                    }
                    return;
                };
                match c {
                    '\n' | 's' => {
                        let target = if b.remote { b.name.clone() } else { b.name.clone() };
                        self.mode = Mode::Confirm { question: format!("Switch to {}?", b.name), then: Pending::Run(vec!["checkout".into(), target]) }
                    }
                    'n' => self.mode = Mode::Input { prompt: "New branch (from where you are): name".into(), text: String::new(), then: Pending::NewBranch { at: None } },
                    'd' if !b.remote => self.mode = Mode::Confirm { question: format!("Delete branch {}?", b.name), then: Pending::Run(vec!["branch".into(), "-d".into(), b.name]) },
                    'm' => self.mode = Mode::Confirm { question: format!("Merge {} into the current branch?", b.name), then: Pending::Run(vec!["merge".into(), b.name]) },
                    _ => {}
                }
            }
            Tab::Undo => match c {
                'u' => {
                    let what = self.undo.first().map(|u| u.reason.clone()).unwrap_or_default();
                    self.mode = Mode::Confirm { question: format!("Undo \"{what}\"? (run it again to redo)"), then: Pending::Run(vec!["undo".into()]) }
                }
                '\n' => {
                    if let Some(id) = self.undo.get(i).and_then(|u| u.id) {
                        let hex = id.to_hex().to_string();
                        self.mode = Mode::Confirm {
                            question: format!("Go back to {} (HEAD@{{{i}}})? The branch and your files move there; uncommitted work is stashed first; undo reverses it.", &hex[..10]),
                            then: Pending::Run(vec!["reset".into(), "--hard".into(), hex]),
                        }
                    }
                }
                _ => {}
            },
        }
    }
}

// ---------------------------------------------------------------- drawing

fn diff_line(l: &str) -> Line<'static> {
    let style = if l.starts_with("+++") || l.starts_with("---") {
        Style::default().add_modifier(Modifier::BOLD)
    } else if l.starts_with('+') {
        Style::default().fg(Color::Green)
    } else if l.starts_with('-') {
        Style::default().fg(Color::Red)
    } else if l.starts_with("@@") {
        Style::default().fg(Color::Cyan)
    } else if l.starts_with("diff ") || l.starts_with("commit ") || l.starts_with("tag ") {
        Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    Line::styled(l.to_string(), style)
}

fn draw(f: &mut Frame, app: &mut App) {
    let [top, main, bottom] = Layout::vertical([Constraint::Length(2), Constraint::Min(3), Constraint::Length(1)]).areas(f.area());
    let [header, tabs] = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(top);
    f.render_widget(Paragraph::new(Line::styled(format!(" copse  ·  {}", app.header), Style::default().add_modifier(Modifier::BOLD))), header);
    let selected = TABS.iter().position(|t| *t == app.tab).unwrap();
    f.render_widget(
        Tabs::new(TABS.iter().map(|t| t.title())).select(selected).highlight_style(Style::default().fg(Color::Black).bg(Color::Cyan)).divider(" "),
        tabs,
    );

    let [list_area, detail_area] = Layout::horizontal([Constraint::Percentage(45), Constraint::Percentage(55)]).areas(main);
    draw_list(f, app, list_area);
    let title = match app.tab {
        Tab::Changes => " Diff ",
        Tab::History => " Commit ",
        Tab::Branches => " Recent commits ",
        Tab::Undo => " What it changed ",
    };
    let lines: Vec<Line> = app.detail.1.iter().map(|l| diff_line(l)).collect();
    f.render_widget(Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(title)).scroll((app.detail_scroll, 0)), detail_area);

    let hints = match app.tab {
        Tab::Changes => "space pick · a all · c commit · x discard · s stash",
        Tab::History => "enter/o check out · v revert · p cherry-pick · b branch here",
        Tab::Branches => "enter switch · n new · d delete · m merge",
        Tab::Undo => "u undo last · enter go back to this point",
    };
    let status = format!(" {}  │  {hints}  │  F fetch · L pull · P push · ? help", app.message);
    f.render_widget(Paragraph::new(Line::styled(status, Style::default().fg(Color::DarkGray))), bottom);

    match &app.mode {
        Mode::Normal => {}
        Mode::Input { prompt, text, .. } => {
            let area = centered(f.area(), 70, 3);
            f.render_widget(Clear, area);
            f.render_widget(Paragraph::new(format!("{text}▏")).block(Block::default().borders(Borders::ALL).title(format!(" {prompt} (enter to finish, esc to cancel) "))), area);
        }
        Mode::Confirm { question, .. } => {
            let area = centered(f.area(), 70, 4);
            f.render_widget(Clear, area);
            f.render_widget(
                Paragraph::new(vec![Line::from(question.clone()), Line::styled("y = yes · n = no", Style::default().fg(Color::DarkGray))])
                    .wrap(Wrap { trim: false })
                    .block(Block::default().borders(Borders::ALL).title(" Are you sure? ")),
                area,
            );
        }
        Mode::Output { title, text, ok, scroll } => {
            let height = (text.lines().count() as u16 + 2).clamp(3, f.area().height.saturating_sub(4));
            let area = centered(f.area(), 90, height);
            f.render_widget(Clear, area);
            let color = if *ok { Color::Green } else { Color::Red };
            f.render_widget(
                Paragraph::new(text.clone())
                    .scroll((*scroll, 0))
                    .block(Block::default().borders(Borders::ALL).border_style(Style::default().fg(color)).title(format!(" {title}  (any key to close) "))),
                area,
            );
        }
        Mode::Help => {
            let text = "Screens: 1 Changes · 2 History · 3 Branches · 4 Undo (or tab / shift-tab)\n\
                        Move: j/k or arrows · g/G top/bottom · J/K or PgDn/PgUp scroll the right-hand pane\n\
                        Anywhere: F fetch · L pull · P push · r refresh · q quit\n\n\
                        Changes:  space pick a file (commit only picked ones) · a pick all/none\n          \
                        c commit (asks for a message) · x discard a file's changes · s stash\n\
                        History:  enter/o check out · v revert · p cherry-pick · b make a branch here\n\
                        Branches: enter switch · n new branch · d delete · m merge into current\n\
                        Undo:     u undo the last change (again to redo) · enter go back to that point\n\n\
                        Every action runs the same copse command you'd type, and shows what it said.\n\
                        Anything that would lose work keeps a backup (a stash), and undo can reverse it.";
            let area = centered(f.area(), 90, 15);
            f.render_widget(Clear, area);
            f.render_widget(Paragraph::new(text).block(Block::default().borders(Borders::ALL).title(" Help (any key to close) ")), area);
        }
    }
}

fn centered(area: Rect, width_pct: u16, height: u16) -> Rect {
    let w = (area.width * width_pct / 100).max(20).min(area.width);
    let h = height.min(area.height);
    Rect { x: area.x + (area.width - w) / 2, y: area.y + (area.height.saturating_sub(h)) / 2, width: w, height: h }
}

fn draw_list(f: &mut Frame, app: &mut App, area: Rect) {
    let (title, items): (String, Vec<ListItem>) = match app.tab {
        Tab::Changes => {
            let title = if app.picked.is_empty() { " Changes ".to_string() } else { format!(" Changes ({} picked) ", app.picked.len()) };
            let items = if app.changes.is_empty() {
                vec![ListItem::new(Line::styled("Nothing changed since the last commit.", Style::default().fg(Color::DarkGray)))]
            } else {
                app.changes
                    .iter()
                    .map(|c| {
                        let color = match c.kind {
                            'A' => Color::Green,
                            'D' => Color::Red,
                            _ => Color::Yellow,
                        };
                        let mark = if app.picked.contains(&c.path) { "[x] " } else { "[ ] " };
                        ListItem::new(Line::from(vec![Span::raw(mark), Span::styled(format!("{} ", c.kind), Style::default().fg(color)), Span::raw(c.path.clone())]))
                    })
                    .collect()
            };
            (title, items)
        }
        Tab::History => (
            " History ".into(),
            app.history
                .iter()
                .map(|r| {
                    let mut spans = vec![Span::styled(r.graph.clone(), Style::default().fg(Color::Magenta))];
                    if let Some(id) = &r.id {
                        spans.push(Span::styled(format!("{id} "), Style::default().fg(Color::Yellow)));
                        if !r.labels.is_empty() {
                            spans.push(Span::styled(format!("({}) ", r.labels), Style::default().fg(Color::Cyan)));
                        }
                        spans.push(Span::raw(r.title.clone()));
                    }
                    ListItem::new(Line::from(spans))
                })
                .collect(),
        ),
        Tab::Branches => (
            " Branches ".into(),
            app.branches
                .iter()
                .map(|b| {
                    let mark = if b.current { "* " } else { "  " };
                    let name_style = if b.current {
                        Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)
                    } else if b.remote {
                        Style::default().fg(Color::Red)
                    } else {
                        Style::default()
                    };
                    let ab = match b.ahead_behind {
                        Some((0, 0)) => " =".to_string(),
                        Some((a, 0)) => format!(" ↑{a}"),
                        Some((0, bh)) => format!(" ↓{bh}"),
                        Some((a, bh)) => format!(" ↑{a}↓{bh}"),
                        None => String::new(),
                    };
                    ListItem::new(Line::from(vec![
                        Span::raw(mark),
                        Span::styled(b.name.clone(), name_style),
                        Span::styled(ab, Style::default().fg(Color::Cyan)),
                        Span::styled(format!("  {} ", crate::objects::short(&b.id)), Style::default().fg(Color::Yellow)),
                        Span::raw(b.title.clone()),
                    ]))
                })
                .collect(),
        ),
        Tab::Undo => (
            " Undo timeline (newest first) ".into(),
            app.undo
                .iter()
                .map(|u| {
                    let id = u.id.map(|i| crate::objects::short(&i)).unwrap_or_else(|| "(none)    ".into());
                    ListItem::new(Line::from(vec![
                        Span::styled(format!("HEAD@{{{}}} ", u.n), Style::default().fg(Color::Magenta)),
                        Span::styled(format!("{id} "), Style::default().fg(Color::Yellow)),
                        Span::styled(format!("{:>8}  ", data::ago(u.time)), Style::default().fg(Color::DarkGray)),
                        Span::raw(u.reason.clone()),
                        Span::styled(if u.title.is_empty() { String::new() } else { format!("  → {}", u.title) }, Style::default().fg(Color::DarkGray)),
                    ]))
                })
                .collect(),
        ),
    };
    let mut state = ListState::default();
    if app.len() > 0 {
        state.select(Some(app.selected()));
    }
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(title))
        .highlight_style(Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD))
        .highlight_symbol("▶ ");
    f.render_stateful_widget(list, area, &mut state);
}

// ---------------------------------------------------------------- running

fn event_loop<B: Backend>(terminal: &mut Terminal<B>, app: &mut App) -> Result<()> {
    let mut idle = 0u32;
    while !app.quit {
        terminal.draw(|f| draw(f, app))?;
        if event::poll(Duration::from_millis(500))? {
            idle = 0;
            if let Event::Key(k) = event::read()? {
                if k.kind == KeyEventKind::Press {
                    app.handle(k);
                }
            }
        } else {
            // Pick up changes made outside (an editor saving a file) every few seconds.
            idle += 1;
            if idle % 6 == 0 && matches!(app.mode, Mode::Normal) {
                app.reload();
            }
        }
    }
    Ok(())
}

/// `copse` / `copse ui`
pub fn run() -> Result<()> {
    use ratatui::crossterm::{execute, terminal};
    let mut app = App::new(data::exe())?;
    terminal::enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, terminal::EnterAlternateScreen)?;
    // Put the terminal back even if something panics.
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = terminal::disable_raw_mode();
        let _ = execute!(std::io::stdout(), terminal::LeaveAlternateScreen);
        hook(info);
    }));
    let mut term = Terminal::new(ratatui::backend::CrosstermBackend::new(stdout))?;
    let result = event_loop(&mut term, &mut app);
    terminal::disable_raw_mode()?;
    execute!(term.backend_mut(), terminal::LeaveAlternateScreen)?;
    term.show_cursor()?;
    result
}

/// `copse ui --replay KEYS [--size WxH]`: press KEYS (comma-separated: j, k, enter,
/// esc, space, tab, backtab, up, down, pgdn, pgup, "text:some words"), then print
/// the screen as text. For tests: the same App, drawn to an in-memory terminal.
pub fn replay(keys: &str, size: &str) -> Result<()> {
    let (w, h) = size.split_once('x').and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?))).unwrap_or((120, 30));
    let mut app = App::new(data::exe())?;
    let mut term = Terminal::new(ratatui::backend::TestBackend::new(w, h))?;
    let press = |app: &mut App, code: KeyCode| app.handle(KeyEvent::new(code, KeyModifiers::NONE));
    for token in keys.split(',').map(str::trim).filter(|t| !t.is_empty()) {
        term.draw(|f| draw(f, &mut app))?;
        if let Some(text) = token.strip_prefix("text:") {
            for c in text.chars() {
                press(&mut app, KeyCode::Char(c));
            }
            continue;
        }
        let code = match token {
            "enter" => KeyCode::Enter,
            "esc" => KeyCode::Esc,
            "space" => KeyCode::Char(' '),
            "tab" => KeyCode::Tab,
            "backtab" => KeyCode::BackTab,
            "up" => KeyCode::Up,
            "down" => KeyCode::Down,
            "pgdn" => KeyCode::PageDown,
            "pgup" => KeyCode::PageUp,
            "backspace" => KeyCode::Backspace,
            t if t.chars().count() == 1 => KeyCode::Char(t.chars().next().unwrap()),
            t => anyhow::bail!("unknown key {t:?}"),
        };
        press(&mut app, code);
        if app.quit {
            break;
        }
    }
    term.draw(|f| draw(f, &mut app))?;
    let buf = term.backend().buffer();
    for y in 0..buf.area.height {
        let mut line = String::new();
        for x in 0..buf.area.width {
            line.push_str(buf[(x, y)].symbol());
        }
        println!("{}", line.trim_end());
    }
    Ok(())
}
