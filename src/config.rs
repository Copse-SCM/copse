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
//! Section and key names ignore case; values are kept as written. `copse config`
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

/// One parsed line of a config file.
enum Line {
    Section(String),
    Entry { key: String, value: String },
    Other,
}

fn parse_line(raw: &str) -> Line {
    let t = raw.trim();
    if t.is_empty() || t.starts_with('#') || t.starts_with(';') {
        return Line::Other;
    }
    if let Some(inner) = t.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
        return Line::Section(inner.trim().to_ascii_lowercase());
    }
    match t.split_once('=') {
        Some((k, v)) => Line::Entry { key: k.trim().to_ascii_lowercase(), value: unquote(v.trim()) },
        None => Line::Entry { key: t.to_ascii_lowercase(), value: "true".into() }, // a bare key means true, as in git
    }
}

fn unquote(v: &str) -> String {
    match v.strip_prefix('"').and_then(|r| r.strip_suffix('"')) {
        Some(inner) => inner.replace("\\\"", "\"").replace("\\\\", "\\"),
        None => v.to_string(),
    }
}

/// All (section.key, value) pairs in a file, in order.
fn read_file(path: &Path) -> Vec<(String, String)> {
    let Ok(text) = fs::read_to_string(path) else { return Vec::new() };
    let mut section = String::new();
    let mut out = Vec::new();
    for raw in text.lines() {
        match parse_line(raw) {
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
    let key = key.to_ascii_lowercase();
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
    let (section, name) = key.rsplit_once('.').with_context(|| format!("{key:?} needs a section, like user.name"))?;
    let (section, name) = (section.to_ascii_lowercase(), name.to_ascii_lowercase());
    let text = fs::read_to_string(path).unwrap_or_default();
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let mut current = String::new();
    let mut section_end: Option<usize> = None; // where to add a new key in an existing section
    let mut found = Vec::new();
    for (i, raw) in lines.iter().enumerate() {
        match parse_line(raw) {
            Line::Section(s) => current = s,
            Line::Entry { key: k, .. } if current == section && k == name => found.push(i),
            _ => {}
        }
        if current == section && !raw.trim().is_empty() {
            section_end = Some(i + 1);
        }
    }
    let quoted = |v: &str| {
        if v.starts_with(' ') || v.ends_with(' ') || v.contains('#') || v.contains(';') || v.contains('"') {
            format!("\"{}\"", v.replace('\\', "\\\\").replace('"', "\\\""))
        } else {
            v.to_string()
        }
    };
    let changed = match value {
        Some(v) => {
            let line = format!("\t{name} = {}", quoted(v));
            if let Some(&last) = found.last() {
                // Replace the one that counts; drop earlier duplicates.
                lines[last] = line;
                for &i in found.iter().rev().skip(1) {
                    lines.remove(i);
                }
            } else if let Some(end) = section_end {
                lines.insert(end, line);
            } else {
                lines.push(format!("[{section}]"));
                lines.push(line);
            }
            true
        }
        None => {
            for &i in found.iter().rev() {
                lines.remove(i);
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
