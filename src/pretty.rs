//! `log --format` / `--pretty`: git's named formats (oneline, short, medium, full,
//! fuller, reference, raw) and its `%` placeholders, and git's date styles
//! (`--date=`). Ids are copse's own; everything else matches git's output.

use crate::objects::{short, Commit, Id, Signature};
use anyhow::{bail, Result};
use std::collections::HashMap;
use std::io::IsTerminal;

/// `--date=`: how dates are written.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum DateMode {
    /// "Sun Nov 8 00:35:12 2026 -0500" (git's default)
    #[default]
    Default,
    Iso,
    IsoStrict,
    Rfc,
    Short,
    Raw,
    Unix,
    Relative,
}

pub fn parse_date_mode(s: Option<&str>) -> Result<DateMode> {
    Ok(match s {
        None | Some("default") | Some("local") => DateMode::Default,
        Some("iso") | Some("iso8601") => DateMode::Iso,
        Some("iso-strict") | Some("iso8601-strict") => DateMode::IsoStrict,
        Some("rfc") | Some("rfc2822") => DateMode::Rfc,
        Some("short") => DateMode::Short,
        Some("raw") => DateMode::Raw,
        Some("unix") => DateMode::Unix,
        Some("relative") => DateMode::Relative,
        Some(other) => bail!("--date={other} isn't supported; copse knows default, iso, iso-strict, rfc, short, raw, unix and relative"),
    })
}

/// A parsed `--format` / `--pretty`.
#[derive(Clone, Debug, PartialEq)]
pub enum Format {
    Named(&'static str),
    /// Placeholders; `terminator` (tformat) ends every entry with a newline,
    /// otherwise (format:) entries are separated by one.
    Custom { template: String, terminator: bool },
}

impl Format {
    pub fn parse(s: &str) -> Result<Format> {
        const NAMED: [&str; 7] = ["oneline", "short", "medium", "full", "fuller", "reference", "raw"];
        if let Some(n) = NAMED.iter().find(|n| **n == s) {
            return Ok(Format::Named(n));
        }
        if let Some(t) = s.strip_prefix("format:") {
            return Ok(Format::Custom { template: t.to_string(), terminator: false });
        }
        if let Some(t) = s.strip_prefix("tformat:") {
            return Ok(Format::Custom { template: t.to_string(), terminator: true });
        }
        if s.contains('%') {
            return Ok(Format::Custom { template: s.to_string(), terminator: true });
        }
        bail!("unknown format {s:?}: use oneline, short, medium, full, fuller, reference, raw, or a format with %-placeholders")
    }

    /// Named formats are followed by a blank line between commits; one-line ones aren't.
    pub fn is_one_line(&self) -> bool {
        matches!(self, Format::Named("oneline") | Format::Named("reference"))
    }
}

/// What a commit's labels look like: " (HEAD -> main, tag: v1, origin/main)".
pub struct Decorations {
    pub labels: HashMap<Id, Vec<String>>,
    pub head: Option<Id>,
    pub head_branch: Option<String>,
}

impl Decorations {
    fn list(&self, id: &Id) -> Vec<String> {
        let mut out = Vec::new();
        let names = self.labels.get(id).cloned().unwrap_or_default();
        if self.head == Some(*id) {
            match &self.head_branch {
                Some(b) if names.contains(b) => out.push(format!("HEAD -> {b}")),
                _ => out.push("HEAD".to_string()),
            }
        }
        for n in names {
            if self.head == Some(*id) && self.head_branch.as_deref() == Some(n.as_str()) {
                continue;
            }
            out.push(n);
        }
        out
    }
}

fn colors_on() -> bool {
    std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none()
}

/// Days since the epoch -> (year, month, day), and the time of day.
fn civil(secs: i64) -> (i64, i64, i64, i64, i64, i64, i64) {
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
    let weekday = (secs.div_euclid(86_400) + 4).rem_euclid(7); // 1970-01-01 was a Thursday
    (year, month, day, rem / 3600, rem % 3600 / 60, rem % 60, weekday)
}

fn tz_offset(tz: &str) -> i64 {
    let sign = if tz.starts_with('-') { -1 } else { 1 };
    let d = tz.trim_start_matches(['+', '-']);
    match (d.get(..2).and_then(|h| h.parse::<i64>().ok()), d.get(2..4).and_then(|m| m.parse::<i64>().ok())) {
        (Some(h), Some(m)) => sign * (h * 3600 + m * 60),
        _ => 0,
    }
}

const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/// git's relative dates ("3 hours ago", "2 years, 1 month ago").
pub fn relative(then: i64, now: i64) -> String {
    let plural = |n: i64, unit: &str| format!("{n} {unit}{}", if n == 1 { "" } else { "s" });
    if then > now {
        return "in the future".into();
    }
    let mut diff = now - then;
    if diff < 90 {
        return format!("{} ago", plural(diff, "second"));
    }
    diff = (diff + 30) / 60;
    if diff < 90 {
        return format!("{} ago", plural(diff, "minute"));
    }
    diff = (diff + 30) / 60;
    if diff < 36 {
        return format!("{} ago", plural(diff, "hour"));
    }
    diff = (diff + 12) / 24;
    if diff < 14 {
        return format!("{} ago", plural(diff, "day"));
    }
    if diff < 70 {
        return format!("{} ago", plural((diff + 3) / 7, "week"));
    }
    if diff < 365 {
        return format!("{} ago", plural((diff + 15) / 30, "month"));
    }
    if diff < 1825 {
        let total_months = (diff * 12 * 2 + 365) / (365 * 2);
        let (years, months) = (total_months / 12, total_months % 12);
        if months > 0 {
            return format!("{}, {} ago", plural(years, "year"), plural(months, "month"));
        }
        return format!("{} ago", plural(years, "year"));
    }
    format!("{} ago", plural((diff + 183) / 365, "year"))
}

pub fn date(secs: i64, tz: &str, mode: DateMode) -> String {
    let off = tz_offset(tz);
    let (y, mo, d, h, mi, s, wd) = civil(secs + off);
    match mode {
        DateMode::Default => format!("{} {} {d} {h:02}:{mi:02}:{s:02} {y} {tz}", DAYS[wd as usize], MONTHS[(mo - 1) as usize]),
        DateMode::Iso => format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02} {tz}"),
        DateMode::IsoStrict => {
            let zone = if off == 0 { "Z".to_string() } else { format!("{}:{}", &tz[..3], &tz[3..]) };
            format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}{zone}")
        }
        DateMode::Rfc => format!("{}, {d} {} {y} {h:02}:{mi:02}:{s:02} {tz}", DAYS[wd as usize], MONTHS[(mo - 1) as usize]),
        DateMode::Short => format!("{y:04}-{mo:02}-{d:02}"),
        DateMode::Raw => format!("{secs} {tz}"),
        DateMode::Unix => secs.to_string(),
        DateMode::Relative => relative(secs, crate::identity::now().0),
    }
}

fn email(sig: &Signature) -> &str {
    sig.who.split_once(" <").map(|(_, e)| e.trim_end_matches('>')).unwrap_or("")
}

/// git's subject: the first paragraph, its lines joined with spaces.
fn subject(message: &str) -> String {
    message.lines().take_while(|l| !l.trim().is_empty()).map(str::trim).collect::<Vec<_>>().join(" ")
}

/// Everything after the subject's paragraph.
fn body(message: &str) -> String {
    let mut lines = message.lines().skip_while(|l| l.trim().is_empty());
    for l in lines.by_ref() {
        if l.trim().is_empty() {
            break;
        }
    }
    let rest: Vec<&str> = lines.skip_while(|l| l.trim().is_empty()).collect();
    if rest.is_empty() {
        String::new()
    } else {
        rest.join("\n") + "\n"
    }
}

/// %f: the subject made safe for a file name, as git does.
fn sanitized(subject: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for c in subject.chars() {
        if c.is_ascii_alphanumeric() || c == '.' || c == '_' {
            if c == '.' && out.ends_with('.') {
                continue;
            }
            out.push(c);
            dash = false;
        } else if !dash && !out.is_empty() {
            out.push('-');
            dash = true;
        }
    }
    while out.ends_with(['-', '.']) {
        out.pop();
    }
    out.trim_start_matches('.').to_string()
}

fn indented(message: &str) -> String {
    message.lines().map(|l| format!("    {l}\n")).collect()
}

/// One commit in a named format, ending with a newline.
fn named(name: &str, id: &Id, c: &Commit, deco: &Decorations, dates: DateMode) -> String {
    let paint = |s: String| crate::cmd::paint("33", &s);
    // Like git's log.decorate=auto: labels on a terminal, none when piped.
    let d = if colors_on() { deco.list(id) } else { vec![] };
    let d = if d.is_empty() { String::new() } else { format!(" ({})", d.join(", ")) };
    let first_paragraph: String = c.message.lines().take_while(|l| !l.trim().is_empty()).map(|l| format!("    {l}\n")).collect();
    let merge = if c.parents.len() > 1 { format!("Merge: {}\n", c.parents.iter().map(short).collect::<Vec<_>>().join(" ")) } else { String::new() };
    match name {
        "oneline" => format!("{}{d} {}\n", paint(id.to_hex().to_string()), subject(&c.message)),
        "reference" => format!("{} ({}, {})\n", short(id), subject(&c.message), date(c.author.time, &c.author.tz, DateMode::Short)),
        "raw" => {
            let mut t = format!("{}\ntree {}\n", paint(format!("commit {}", id.to_hex())), c.tree.to_hex());
            for p in &c.parents {
                t += &format!("parent {}\n", p.to_hex());
            }
            t += &format!("author {} {} {}\ncommitter {} {} {}\n\n", c.author.who, c.author.time, c.author.tz, c.committer.who, c.committer.time, c.committer.tz);
            t + &indented(&c.message)
        }
        "short" => format!("{}{d}\n{merge}Author: {}\n\n{first_paragraph}", paint(format!("commit {}", id.to_hex())), c.author.who),
        "full" => format!("{}{d}\n{merge}Author: {}\nCommit: {}\n\n{}", paint(format!("commit {}", id.to_hex())), c.author.who, c.committer.who, indented(&c.message)),
        "fuller" => format!(
            "{}{d}\n{merge}Author:     {}\nAuthorDate: {}\nCommit:     {}\nCommitDate: {}\n\n{}",
            paint(format!("commit {}", id.to_hex())),
            c.author.who,
            date(c.author.time, &c.author.tz, dates),
            c.committer.who,
            date(c.committer.time, &c.committer.tz, dates),
            indented(&c.message)
        ),
        _ => format!(
            "{}{d}\n{merge}Author: {}\nDate:   {}\n\n{}",
            paint(format!("commit {}", id.to_hex())),
            c.author.who,
            date(c.author.time, &c.author.tz, dates),
            indented(&c.message)
        ),
    }
}

fn color_code(name: &str) -> Option<&'static str> {
    Some(match name {
        "reset" => "0",
        "normal" => "39",
        "black" => "30",
        "red" => "31",
        "green" => "32",
        "yellow" => "33",
        "blue" => "34",
        "magenta" => "35",
        "cyan" => "36",
        "white" => "37",
        "bold" => "1",
        "dim" => "2",
        "ul" | "underline" => "4",
        _ => return None,
    })
}

/// Expand a template's placeholders for one commit.
pub fn expand(template: &str, id: &Id, c: &Commit, deco: &Decorations, dates: DateMode) -> String {
    let mut out = String::new();
    let chars: Vec<char> = template.chars().collect();
    let color = colors_on();
    let mut i = 0;
    let sig_field = |sig: &Signature, f: char| -> Option<String> {
        Some(match f {
            'n' => sig.name().to_string(),
            'e' => email(sig).to_string(),
            'l' => email(sig).split('@').next().unwrap_or("").to_string(),
            'd' => date(sig.time, &sig.tz, dates),
            'D' => date(sig.time, &sig.tz, DateMode::Rfc),
            'r' => date(sig.time, &sig.tz, DateMode::Relative),
            't' => sig.time.to_string(),
            'i' => date(sig.time, &sig.tz, DateMode::Iso),
            'I' => date(sig.time, &sig.tz, DateMode::IsoStrict),
            's' => date(sig.time, &sig.tz, DateMode::Short),
            _ => return None,
        })
    };
    while i < chars.len() {
        let ch = chars[i];
        if ch != '%' || i + 1 >= chars.len() {
            out.push(ch);
            i += 1;
            continue;
        }
        let next = chars[i + 1];
        let mut used = 2;
        let piece: Option<String> = match next {
            '%' => Some("%".into()),
            'n' => Some("\n".into()),
            'H' => Some(id.to_hex().to_string()),
            'h' => Some(short(id)),
            'T' => Some(c.tree.to_hex().to_string()),
            't' => Some(short(&c.tree)),
            'P' => Some(c.parents.iter().map(|p| p.to_hex().to_string()).collect::<Vec<_>>().join(" ")),
            'p' => Some(c.parents.iter().map(short).collect::<Vec<_>>().join(" ")),
            's' => Some(subject(&c.message)),
            'f' => Some(sanitized(c.message.lines().next().unwrap_or(""))),
            'b' => Some(body(&c.message)),
            'B' => Some(if c.message.ends_with('\n') { c.message.clone() } else { format!("{}\n", c.message) }),
            'e' => Some(String::new()), // encoding: always UTF-8, which git leaves blank
            'd' => {
                let d = deco.list(id);
                Some(if d.is_empty() { String::new() } else { format!(" ({})", d.join(", ")) })
            }
            'D' => Some(deco.list(id).join(", ")),
            'a' | 'c' => {
                used = 3;
                let sig = if next == 'a' { &c.author } else { &c.committer };
                chars.get(i + 2).and_then(|f| sig_field(sig, *f))
            }
            'x' => {
                used = 4;
                chars.get(i + 2..i + 4).and_then(|h| u8::from_str_radix(&h.iter().collect::<String>(), 16).ok()).map(|b| (b as char).to_string())
            }
            'C' => {
                // %Cred, %Cgreen, %Cblue, %Creset, %C(name) / %C(auto,name) / %C(bold red)
                let rest: String = chars[i + 2..].iter().collect();
                let (spec, len) = if rest.starts_with('(') {
                    match rest.find(')') {
                        Some(end) => (rest[1..end].to_string(), end + 1),
                        None => (String::new(), 0),
                    }
                } else {
                    ["reset", "red", "green", "blue"].iter().find(|n| rest.starts_with(**n)).map(|n| (n.to_string(), n.len())).unwrap_or_default()
                };
                if len == 0 {
                    None
                } else {
                    used = 2 + len;
                    let words: Vec<&str> = spec.split([',', ' ']).filter(|w| !w.is_empty() && *w != "auto" && *w != "always").collect();
                    let codes: Vec<&str> = words.iter().filter_map(|w| color_code(w)).collect();
                    Some(if color && !codes.is_empty() { format!("\x1b[{}m", codes.join(";")) } else { String::new() })
                }
            }
            _ => None,
        };
        match piece {
            Some(p) => {
                out.push_str(&p);
                i += used;
            }
            None => {
                // Not a placeholder copse knows: written as it is, as git does.
                out.push('%');
                i += 1;
            }
        }
    }
    out
}

/// One commit, formatted, with a trailing newline where the format has one.
pub fn render(format: &Format, id: &Id, c: &Commit, deco: &Decorations, dates: DateMode) -> String {
    match format {
        Format::Named(n) => named(n, id, c, deco, dates),
        Format::Custom { template, .. } => expand(template, id, c, deco, dates),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_like_git() {
        // 2026-11-08 05:35:12 UTC, a Sunday
        let t = 1_794_116_112;
        assert_eq!(date(t, "-0500", DateMode::Default), "Sun Nov 8 00:35:12 2026 -0500");
        assert_eq!(date(t, "-0500", DateMode::Iso), "2026-11-08 00:35:12 -0500");
        assert_eq!(date(t, "-0500", DateMode::IsoStrict), "2026-11-08T00:35:12-05:00");
        assert_eq!(date(t, "+0000", DateMode::IsoStrict), "2026-11-08T05:35:12Z");
        assert_eq!(date(t, "+0530", DateMode::Rfc), "Sun, 8 Nov 2026 11:05:12 +0530");
        assert_eq!(relative(0, 30), "30 seconds ago");
        assert_eq!(relative(0, 3 * 3600), "3 hours ago");
        assert_eq!(relative(0, 86_400 * 400), "1 year, 1 month ago");
    }

    #[test]
    fn subjects_and_bodies() {
        let m = "Fix the thing\nacross two lines\n\nThe body.\n\nMore.\n";
        assert_eq!(subject(m), "Fix the thing across two lines");
        assert_eq!(body(m), "The body.\n\nMore.\n");
        assert_eq!(sanitized("Fix: the [thing] (now)!"), "Fix-the-thing-now");
    }
}
