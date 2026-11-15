//! Settings: `.copse/config` for one repository, and a global file for all of them
//! (`$COPSE_CONFIG_GLOBAL`, else `$XDG_CONFIG_HOME/copse/config`, else
//! `~/.config/copse/config`). The repository's file wins. Same layout as git's:
//!
//!   [user]
//!       name = Ada Lovelace
//!       email = ada@example.com
//!   [alias]
//!       st = status
//!       last = log -n 1
//!   [core]
//!       editor = nano
//!       excludesFile = ~/.config/copse/ignore
//!       hooksPath = .githooks
//!
//! Section and key names ignore case (a subsection, as in `[gpg "ssh"]`, doesn't);
//! values are read by git's rules (see `parse`). `copse config`
//! reads and writes them.

use crate::cmd::paint;
use crate::repo::{write_atomic, Repo};
use anyhow::{bail, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

/// Every setting copse uses, for `copse config` to check names against.
const KNOWN: &[(&str, &str)] = &[
    ("user.name", "your name, recorded on commits (else $COPSE_AUTHOR, then git's user.name)"),
    ("user.email", "your email, recorded on commits (else $COPSE_EMAIL, then git's user.email)"),
    ("core.editor", "editor for rebase lists and messages (after $COPSE_EDITOR)"),
    ("core.excludesfile", "an extra ignore file for every repository (default ~/.config/copse/ignore)"),
    ("core.hookspath", "folder of hook scripts, relative to the repository (default .copse/hooks)"),
    ("core.fsync", "false to skip flushing writes to disk (faster, but a crash can lose recent work)"),
    ("core.trustctime", "false to ignore files' change times when telling if they changed (for tools or filesystems that alter them)"),
    ("core.checkstat", "minimal to tell changed files by size and modification time only (no change time or inode number)"),
    ("user.signingkey", "the key to sign commits with: an SSH key file, or a GPG key id"),
    ("gpg.format", "ssh or openpgp (default openpgp)"),
    ("gpg.program", "the gpg to run for openpgp signatures (default gpg)"),
    ("gpg.ssh.allowedsignersfile", "who to trust when checking SSH signatures (git's format)"),
    ("commit.gpgsign", "true to sign every commit"),
    ("tag.gpgsign", "true to sign every annotated tag"),
    ("lfs.url", "the Git LFS server to use (default: the one git-lfs derives from the remote)"),
    ("gc.pruneexpire", "how long gc keeps unused objects: now, never, or e.g. 14d, 2w (default 2w)"),
];

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").filter(|h| !h.is_empty()).map(PathBuf::from)
}

/// `~/x` -> `$HOME/x`.
pub fn expand_home(path: &str) -> PathBuf {
    match (path.strip_prefix("~/"), home()) {
        (Some(rest), Some(h)) => h.join(rest),
        _ => PathBuf::from(path),
    }
}

/// The folder for copse's global files (config, ignore).
pub fn global_dir() -> Option<PathBuf> {
    match std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        Some(x) => Some(PathBuf::from(x).join("copse")),
        None => home().map(|h| h.join(".config").join("copse")),
    }
}

pub fn global_path() -> Option<PathBuf> {
    match std::env::var_os("COPSE_CONFIG_GLOBAL").filter(|v| !v.is_empty()) {
        Some(p) => Some(PathBuf::from(p)),
        None => global_dir().map(|d| d.join("config")),
    }
}

fn repo_path() -> Option<PathBuf> {
    Repo::find_any().ok().map(|r| r.dir.join("config"))
}

/// One item in a config file, and the lines it takes up.
enum Line {
    /// `[core]` -> "core"; `[gpg "ssh"]` -> "gpg.ssh" (the subsection keeps its case).
    Section(String),
    Entry { key: String, value: String },
}

/// A config file's sections and settings, read by git's rules: `#` and `;` start
/// a comment outside quotes; a value may be quoted in whole or in part, with
/// `\"`, `\\`, `\n`, `\t` and `\b` escapes; a `\` at the end of a line carries
/// the value on to the next; whitespace around a value is dropped unless quoted;
/// a key on its own means true; a setting can follow a section on its line.
/// Each item comes with the lines it spans.
fn parse(text: &str) -> Vec<(std::ops::Range<usize>, Line)> {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let start = i;
        let mut rest = lines[i].trim_start();
        if rest.starts_with('[') {
            let Some((name, after)) = section_header(rest) else {
                i += 1; // malformed: skipped
                continue;
            };
            out.push((start..start + 1, Line::Section(name)));
            rest = after.trim_start();
        }
        if rest.is_empty() || rest.starts_with('#') || rest.starts_with(';') {
            i += 1;
            continue;
        }
        let key_len = rest.find(|c: char| !(c.is_ascii_alphanumeric() || c == '-')).unwrap_or(rest.len());
        if key_len == 0 {
            i += 1; // not a setting: skipped
            continue;
        }
        let key = rest[..key_len].to_ascii_lowercase();
        let after_key = rest[key_len..].trim_start();
        let value = match after_key.strip_prefix('=') {
            Some(v) => {
                let (value, last) = parse_value(&lines, i, v);
                i = last;
                value
            }
            None => "true".into(), // a key on its own
        };
        out.push((start..i + 1, Line::Entry { key, value }));
        i += 1;
    }
    out
}

/// `[name]` or `[name "Sub"]` at the start of `t`: (section, the rest of the line).
fn section_header(t: &str) -> Option<(String, &str)> {
    let inner = &t[1..];
    let close = inner.find(']')?;
    let head = &inner[..close];
    let name = match head.split_once('"') {
        Some((section, sub)) => {
            let sub = sub.strip_suffix('"')?.replace("\\\"", "\"").replace("\\\\", "\\");
            format!("{}.{sub}", section.trim().to_ascii_lowercase())
        }
        None => head.trim().to_ascii_lowercase(),
    };
    Some((name, &inner[close + 1..]))
}

/// A value starting at `first` (the text after `=` on line `at`), following `\`
/// line continuations. Returns it and the index of the line it ends on.
fn parse_value(lines: &[&str], mut at: usize, first: &str) -> (String, usize) {
    let (mut out, mut gap) = (String::new(), String::new());
    let mut quoted = false;
    let mut chars: Vec<char> = first.chars().collect();
    let mut k = 0;
    loop {
        let Some(&c) = chars.get(k) else { break };
        k += 1;
        match c {
            '\\' => match chars.get(k) {
                None if at + 1 < lines.len() => {
                    at += 1; // continued on the next line
                    chars = lines[at].chars().collect();
                    k = 0;
                    continue;
                }
                None => break,
                Some(&e) => {
                    k += 1;
                    out.push_str(&std::mem::take(&mut gap));
                    out.push(match e {
                        'n' => '\n',
                        't' => '\t',
                        'b' => '\x08',
                        other => other, // \" and \\ (and anything else, as written)
                    });
                }
            },
            '"' => {
                out.push_str(&std::mem::take(&mut gap));
                quoted = !quoted;
            }
            '#' | ';' if !quoted => break,
            c if c.is_whitespace() && !quoted => {
                if !out.is_empty() {
                    gap.push(c); // kept only if something follows
                }
            }
            c => {
                out.push_str(&std::mem::take(&mut gap));
                out.push(c);
            }
        }
    }
    (out, at)
}

/// A setting's name as stored: section and key in lower case, a subsection as written.
fn normalize(key: &str) -> String {
    match (key.split_once('.'), key.rsplit_once('.')) {
        (Some((section, _)), Some((middle, name))) if middle.len() > section.len() => {
            format!("{}{}.{}", section.to_ascii_lowercase(), &middle[section.len()..], name.to_ascii_lowercase())
        }
        _ => key.to_ascii_lowercase(),
    }
}

/// All (section.key, value) pairs in a file, in order.
fn read_file(path: &Path) -> Vec<(String, String)> {
    let Ok(text) = fs::read_to_string(path) else { return Vec::new() };
    let mut section = String::new();
    let mut out = Vec::new();
    for (_, item) in parse(&text) {
        match item {
            Line::Section(s) => section = s,
            Line::Entry { key, value } if !section.is_empty() => out.push((format!("{section}.{key}"), value)),
            _ => {}
        }
    }
    out
}

/// A setting's value: the repository's file, then the global one (the last
/// occurrence in a file wins, as in git).
pub fn get(key: &str) -> Option<String> {
    let key = normalize(key);
    for path in [repo_path(), global_path()].into_iter().flatten() {
        if let Some((_, v)) = read_file(&path).into_iter().filter(|(k, _)| *k == key).last() {
            return Some(v);
        }
    }
    None
}

/// Every alias, for `copse help`-style listings and alias expansion.
pub fn aliases() -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for path in [global_path(), repo_path()].into_iter().flatten() {
        for (k, v) in read_file(&path) {
            if let Some(name) = k.strip_prefix("alias.") {
                out.retain(|(n, _)| n != name);
                out.push((name.to_string(), v));
            }
        }
    }
    out
}

/// Set (or with `value` None, remove) `key` in the file at `path`, keeping
/// everything else in it, comments included.
fn write_key(path: &Path, key: &str, value: Option<&str>) -> Result<bool> {
    let key = normalize(key);
    let (section, name) = key.rsplit_once('.').with_context(|| format!("{key:?} needs a section, like user.name"))?;
    let (section, name) = (section.to_string(), name.to_string());
    let text = fs::read_to_string(path).unwrap_or_default();
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let mut current = String::new();
    let mut section_end: Option<usize> = None; // where to add a new key in an existing section
    let mut found: Vec<std::ops::Range<usize>> = Vec::new();
    for (span, item) in parse(&text) {
        match item {
            Line::Section(s) => current = s,
            Line::Entry { key: k, .. } if current == section && k == name => found.push(span.clone()),
            _ => {}
        }
        if current == section {
            section_end = Some(span.end);
        }
    }
    // Written so git (and copse) read back exactly this value.
    let quoted = |v: &str| {
        let escaped = v.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n").replace('\t', "\\t");
        if v.starts_with(char::is_whitespace) || v.ends_with(char::is_whitespace) || v.contains(['#', ';']) || escaped != v {
            format!("\"{escaped}\"")
        } else {
            v.to_string()
        }
    };
    let changed = match value {
        Some(v) => {
            let line = format!("\t{name} = {}", quoted(v));
            if let Some(last) = found.pop() {
                // Replace the one that counts (all its lines); drop earlier duplicates.
                lines.splice(last, [line]);
                for span in found.iter().rev() {
                    lines.drain(span.clone());
                }
            } else if let Some(end) = section_end {
                lines.insert(end, line);
            } else {
                lines.push(match section.split_once('.') {
                    Some((s, sub)) => format!("[{s} \"{}\"]", sub.replace('\\', "\\\\").replace('"', "\\\"")),
                    None => format!("[{section}]"),
                });
                lines.push(line);
            }
            true
        }
        None => {
            for span in found.iter().rev() {
                lines.drain(span.clone());
            }
            !found.is_empty()
        }
    };
    if changed {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut out = lines.join("\n");
        out.push('\n');
        write_atomic(path, out.as_bytes())?;
    }
    Ok(changed)
}

pub struct ConfigArgs<'a> {
    pub key: Option<&'a str>,
    pub value: Option<&'a str>,
    pub global: bool,
    pub unset: bool,
    pub list: bool,
}

/// `copse config [--global] [--list | KEY [VALUE] | --unset KEY]`
pub fn config(args: ConfigArgs) -> Result<()> {
    let file = |global: bool| -> Result<PathBuf> {
        if global {
            global_path().context("can't find your home folder for the global config; set $COPSE_CONFIG_GLOBAL")
        } else {
            repo_path().context("not inside a copse repository; use --global for settings that apply everywhere")
        }
    };
    if args.list || args.key.is_none() {
        let mut any = false;
        for (label, path) in [("global", global_path()), ("repository", repo_path())] {
            let Some(path) = path else { continue };
            if args.global && label == "repository" {
                continue;
            }
            for (k, v) in read_file(&path) {
                println!("{k}={v}  {}", paint("2", &format!("({label})")));
                any = true;
            }
        }
        if !any {
            println!("No settings yet. Set one with `copse config user.name \"Your Name\"` (add --global for every repository).");
            println!("Settings copse uses:");
            for (k, what) in KNOWN {
                println!("  {k:<18} {what}");
            }
            println!("  alias.NAME         a shortcut: `copse config alias.st status` makes `copse st` run `copse status`");
        }
        return Ok(());
    }
    let key = args.key.unwrap();
    let lower = key.to_ascii_lowercase();
    if !lower.contains('.') {
        bail!("{key:?} needs a section, like user.name or alias.st");
    }
    if lower.starts_with("alias.") {
        let name = &key["alias.".len()..];
        if name.is_empty() || name.contains(char::is_whitespace) {
            bail!("an alias name can't be empty or contain spaces");
        }
        if crate::is_builtin_command(name) {
            bail!("`{name}` is already a copse command; an alias can't replace it");
        }
    }
    if args.unset {
        let path = file(args.global)?;
        if !write_key(&path, key, None)? {
            bail!("{key} isn't set in {}", path.display());
        }
        println!("Removed {key}.");
        return Ok(());
    }
    match args.value {
        Some(v) => {
            if !lower.starts_with("alias.") && !KNOWN.iter().any(|(k, _)| *k == lower) {
                eprintln!("copse: note: {key} isn't a setting copse uses (it's saved anyway)");
            }
            let path = file(args.global)?;
            write_key(&path, key, Some(v))?;
            println!("Set {key} = {v}{}", if args.global { " (for every repository)" } else { "" });
        }
        None => {
            let found = if args.global { global_path().and_then(|p| read_file(&p).into_iter().filter(|(k, _)| *k == lower).last().map(|(_, v)| v)) } else { get(key) };
            match found {
                Some(v) => println!("{v}"),
                None => std::process::exit(1), // unset, like `git config KEY`
            }
        }
    }
    Ok(())
}

/// Split an alias into words, honouring 'single' and "double" quotes.
pub fn split_words(s: &str) -> Result<Vec<String>> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut quote: Option<char> = None;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some('"'), '\\') => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            (Some(_), c) => cur.push(c),
            (None, '\'' | '"') => {
                quote = Some(c);
                in_word = true;
            }
            (None, '\\') => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                    in_word = true;
                }
            }
            (None, c) if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            (None, c) => {
                cur.push(c);
                in_word = true;
            }
        }
    }
    if quote.is_some() {
        bail!("unmatched quote in alias: {s}");
    }
    if in_word {
        words.push(cur);
    }
    Ok(words)
}

/// Extra ignore files that apply to every repository: core.excludesFile, else
/// ~/.config/copse/ignore. Plus `.copse/exclude`, for rules only you need in one
/// repository (not shared through commits).
pub fn ignore_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    match get("core.excludesfile") {
        Some(p) => out.push(expand_home(&p)),
        None => out.extend(global_dir().map(|d| d.join("ignore"))),
    }
    if let Ok(repo) = Repo::open_path(root) {
        out.push(repo.dir.join("exclude"));
    }
    out.retain(|p| p.is_file());
    out
}

/// The folder hooks live in.
pub fn hooks_dir(repo: &Repo) -> PathBuf {
    match get("core.hookspath") {
        Some(p) => {
            let p = expand_home(&p);
            if p.is_absolute() { p } else { crate::objects::disk_path(&repo.root, p) }
        }
        None => repo.dir.join("hooks"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_alias_words() {
        assert_eq!(split_words("log -n 1").unwrap(), ["log", "-n", "1"]);
        assert_eq!(split_words(r#"commit -m "two words" 'and more'"#).unwrap(), ["commit", "-m", "two words", "and more"]);
        assert_eq!(split_words(r#"x "" y"#).unwrap(), ["x", "", "y"]);
        assert!(split_words("a 'b").is_err());
    }

    #[test]
    fn writes_and_rewrites_keys() {
        let dir = std::env::temp_dir().join(format!("copse-config-test-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let p = dir.join("config");
        fs::write(&p, "# mine\n[user]\n\tname = Old\n[core]\n\teditor = vi\n").unwrap();
        write_key(&p, "user.name", Some("New Name")).unwrap();
        write_key(&p, "user.email", Some("a@b.c")).unwrap();
        write_key(&p, "alias.lg", Some("log --oneline # short")).unwrap();
        let all = read_file(&p);
        assert!(all.contains(&("user.name".into(), "New Name".into())));
        assert!(all.contains(&("user.email".into(), "a@b.c".into())));
        assert!(all.contains(&("alias.lg".into(), "log --oneline # short".into())));
        assert!(fs::read_to_string(&p).unwrap().starts_with("# mine\n"));
        assert!(write_key(&p, "core.editor", None).unwrap());
        assert!(!read_file(&p).iter().any(|(k, _)| k == "core.editor"));
        let _ = fs::remove_dir_all(&dir);
    }
}
