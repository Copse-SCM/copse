//! Signed commits, in git's format, so GitHub and git itself can verify them.
//!
//! What's signed is the commit exactly as git sees it (its git encoding, with git
//! ids for the tree and parents), and the signature is stored as git stores it: a
//! `gpgsig` header. So a signed commit pushed to GitHub shows as Verified.
//!
//! Settings, the same names git uses (copse's own config first, then git's, so
//! signing you've set up for git just works):
//!   user.signingKey   an SSH key file (private, or public with ssh-agent), or a GPG key id
//!   gpg.format        "ssh" or "openpgp" (default openpgp, as in git)
//!   commit.gpgSign    true to sign every commit (`copse commit -S` signs one)
//!   gpg.ssh.allowedSignersFile   who to trust when checking SSH signatures
//!   gpg.program       the gpg to run (default gpg)

use crate::gitwire::Translator;
use crate::objects::{short, Commit, Id};
use crate::repo::Repo;
use anyhow::{bail, Context, Result};
use std::io::Write;
use std::process::{Command, Stdio};

/// A signing setting: copse's config, else git's.
fn setting(key: &str) -> Option<String> {
    crate::config::get(key).or_else(|| crate::identity::git_config(key))
}

#[derive(Clone, Copy, PartialEq)]
enum Format {
    Ssh,
    OpenPgp,
}

fn format() -> Format {
    match setting("gpg.format").map(|f| f.to_ascii_lowercase()).as_deref() {
        Some("ssh") => Format::Ssh,
        _ => Format::OpenPgp,
    }
}

/// Whether commits should be signed by default (commit.gpgSign).
pub fn sign_by_default() -> bool {
    setting("commit.gpgsign").is_some_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "true" | "yes" | "on" | "1"))
}

fn run(cmd: &mut Command, input: &[u8], what: &str) -> Result<(bool, String, String)> {
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("can't run {what}"))?;
    child.stdin.take().unwrap().write_all(input)?;
    let out = child.wait_with_output()?;
    Ok((out.status.success(), String::from_utf8_lossy(&out.stdout).into_owned(), String::from_utf8_lossy(&out.stderr).into_owned()))
}

/// Sign `payload`, returning an armored signature.
fn sign(payload: &[u8]) -> Result<String> {
    let key = setting("user.signingkey");
    match format() {
        Format::Ssh => {
            let key = key.context("set user.signingKey to your SSH key, e.g. `copse config --global user.signingKey ~/.ssh/id_ed25519`")?;
            let key = crate::config::expand_home(&key);
            let (ok, out, err) = run(Command::new("ssh-keygen").args(["-Y", "sign", "-n", "git", "-f"]).arg(&key).arg("-q"), payload, "ssh-keygen")?;
            if !ok || !out.contains("BEGIN SSH SIGNATURE") {
                bail!("ssh-keygen couldn't sign with {}: {}", key.display(), err.trim());
            }
            Ok(out)
        }
        Format::OpenPgp => {
            let program = setting("gpg.program").unwrap_or_else(|| "gpg".into());
            let mut cmd = Command::new(&program);
            cmd.args(["--batch", "--armor", "--detach-sign"]);
            if let Some(k) = &key {
                cmd.args(["--local-user", k]);
            }
            let (ok, out, err) = run(&mut cmd, payload, &program)?;
            if !ok || !out.contains("BEGIN PGP SIGNATURE") {
                bail!("{program} couldn't sign: {}", err.trim());
            }
            Ok(out)
        }
    }
}

/// The signature as a git `gpgsig` header: continuation lines start with a space.
fn header(signature: &str) -> Vec<u8> {
    let mut out = b"gpgsig".to_vec();
    for line in signature.trim_end_matches('\n').split('\n') {
        out.push(b' ');
        out.extend_from_slice(line.as_bytes());
        out.push(b'\n');
    }
    out
}

/// Split a commit's extra headers into its signature (if any) and the rest.
fn split_signature(extra: &[u8]) -> (Option<String>, Vec<u8>) {
    let text = String::from_utf8_lossy(extra);
    let mut sig: Option<String> = None;
    let mut rest = String::new();
    let mut in_sig = false;
    for line in text.split_inclusive('\n') {
        if let Some(first) = line.strip_prefix("gpgsig ") {
            sig = Some(first.to_string());
            in_sig = true;
        } else if in_sig && line.starts_with(' ') {
            sig.as_mut().unwrap().push_str(&line[1..]);
        } else {
            in_sig = false;
            rest.push_str(line);
        }
    }
    (sig, rest.into_bytes())
}

/// Store a commit made by merge, rebase, cherry-pick or revert, signing it first
/// if commit.gpgSign is on (as git does for every commit it makes).
pub fn store(repo: &Repo, mut commit: Commit) -> Result<Id> {
    if sign_by_default() {
        sign_commit(repo, &mut commit)?;
    }
    repo.store.put(crate::objects::Kind::Commit, &commit.encode())
}

/// Sign `commit` (which mustn't be signed yet): adds a gpgsig header covering its git form.
pub fn sign_commit(repo: &Repo, commit: &mut Commit) -> Result<()> {
    let mut tr = Translator::new(repo)?;
    let body = tr.unsigned_body(commit)?;
    tr.map.save()?; // the git ids worked out on the way are useful later
    let signature = sign(&body)?;
    let mut extra = header(&signature);
    extra.extend_from_slice(&commit.extra);
    commit.extra = extra;
    Ok(())
}

pub enum Verdict {
    Unsigned,
    Good { signer: String, trusted: bool },
    Bad(String),
}

/// Check a commit's signature against its git form.
pub fn verify(repo: &Repo, id: &Id) -> Result<Verdict> {
    let commit = repo.read_commit_raw(id)?;
    let (Some(signature), rest) = split_signature(&commit.extra) else { return Ok(Verdict::Unsigned) };
    let mut unsigned = commit.clone();
    unsigned.extra = rest;
    let mut tr = Translator::new(repo)?;
    let payload = tr.unsigned_body(&unsigned)?;
    check(&signature, &payload, &email_of(&commit.author.who))
}

fn email_of(who: &str) -> String {
    who.rsplit_once('<').map(|(_, e)| e.trim_end_matches('>').to_string()).unwrap_or_default()
}

/// Check `signature` over `payload`, made (for SSH) by the key of `email`.
fn check(signature: &str, payload: &[u8], email: &str) -> Result<Verdict> {
    let dir = std::env::temp_dir().join(format!("copse-verify-{}-{}", std::process::id(), payload.len()));
    std::fs::create_dir_all(&dir)?;
    let sig_path = dir.join("sig");
    std::fs::write(&sig_path, signature)?;
    let result = (|| -> Result<Verdict> {
        if signature.contains("BEGIN SSH SIGNATURE") {
            match setting("gpg.ssh.allowedsignersfile") {
                Some(allowed) => {
                    let allowed = crate::config::expand_home(&allowed);
                    let (ok, out, err) = run(
                        Command::new("ssh-keygen").args(["-Y", "verify", "-n", "git", "-I", email, "-f"]).arg(&allowed).arg("-s").arg(&sig_path),
                        payload,
                        "ssh-keygen",
                    )?;
                    Ok(if ok { Verdict::Good { signer: out.trim().to_string(), trusted: true } } else { Verdict::Bad(format!("{}{}", out.trim(), err.trim())) })
                }
                None => {
                    let (ok, out, err) =
                        run(Command::new("ssh-keygen").args(["-Y", "check-novalidate", "-n", "git", "-s"]).arg(&sig_path), payload, "ssh-keygen")?;
                    Ok(if ok { Verdict::Good { signer: out.trim().to_string(), trusted: false } } else { Verdict::Bad(err.trim().to_string()) })
                }
            }
        } else {
            let program = setting("gpg.program").unwrap_or_else(|| "gpg".into());
            let (ok, _, err) = run(Command::new(&program).args(["--batch", "--verify"]).arg(&sig_path).arg("-"), payload, &program)?;
            let signer = err.lines().find(|l| l.contains("Good signature")).unwrap_or("").trim().to_string();
            Ok(if ok { Verdict::Good { signer, trusted: true } } else { Verdict::Bad(err.trim().to_string()) })
        }
    })();
    let _ = std::fs::remove_dir_all(&dir);
    result
}

// ---------------------------------------------------------------- tags

/// Whether annotated tags should be signed by default (tag.gpgSign).
pub fn sign_tags_by_default() -> bool {
    setting("tag.gpgsign").is_some_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "true" | "yes" | "on" | "1"))
}

/// Where a signed tag's signature starts in its message (git appends it there).
fn tag_signature_start(message: &str) -> Option<usize> {
    ["-----BEGIN PGP SIGNATURE-----", "-----BEGIN SSH SIGNATURE-----"].iter().filter_map(|m| message.rfind(m)).max()
}

/// Sign an annotated tag the way git does: over its git form, with the signature
/// appended to its message.
pub fn sign_tag(repo: &Repo, tag: &mut crate::objects::Tag) -> Result<()> {
    let mut tr = Translator::new(repo)?;
    let payload = tr.tag_payload(tag)?;
    tr.map.save()?;
    let signature = sign(&payload)?;
    tag.message.push_str(&signature);
    Ok(())
}

/// Check an annotated tag's signature.
pub fn verify_tag(repo: &Repo, id: &Id) -> Result<Verdict> {
    let (kind, data) = repo.store.get(id)?;
    if kind != crate::objects::Kind::Tag {
        return Ok(Verdict::Unsigned); // a lightweight tag has nothing to sign
    }
    let tag = crate::objects::Tag::decode(&data)?;
    let Some(at) = tag_signature_start(&tag.message) else { return Ok(Verdict::Unsigned) };
    let mut unsigned = tag.clone();
    unsigned.message = tag.message[..at].to_string();
    let mut tr = Translator::new(repo)?;
    let payload = tr.tag_payload(&unsigned)?;
    let email = tag.tagger.as_ref().map(|t| email_of(&t.who)).unwrap_or_default();
    check(&tag.message[at..], &payload, &email)
}

/// `copse verify-tag NAME...`: exits 1 unless every tag has a good signature.
pub fn verify_tags(names: &[String]) -> Result<()> {
    let repo = Repo::find_any()?;
    if names.is_empty() {
        bail!("say which tag(s): copse verify-tag v1.0");
    }
    let mut all_good = true;
    for name in names {
        let id = repo.tag(name)?.with_context(|| format!("there's no tag called {name}"))?;
        match verify_tag(&repo, &id)? {
            Verdict::Unsigned => {
                println!("{name}: not signed");
                all_good = false;
            }
            Verdict::Good { signer, trusted } => {
                println!("{name}: good signature{}", if signer.is_empty() { String::new() } else { format!(": {signer}") });
                if !trusted {
                    println!("  (the signature is valid; set gpg.ssh.allowedSignersFile to also check whose key it is)");
                }
            }
            Verdict::Bad(why) => {
                println!("{name}: BAD signature: {why}");
                all_good = false;
            }
        }
    }
    if !all_good {
        std::process::exit(1);
    }
    Ok(())
}

/// `copse verify-commit [REV...]`: exits 1 unless every commit has a good signature.
pub fn verify_commits(revs: &[String]) -> Result<()> {
    let repo = Repo::find_any()?;
    let revs: Vec<String> = if revs.is_empty() { vec!["HEAD".into()] } else { revs.to_vec() };
    let mut all_good = true;
    for rev in &revs {
        let id = repo.resolve(rev)?;
        let label = format!("{} ({rev})", short(&id));
        match verify(&repo, &id)? {
            Verdict::Unsigned => {
                println!("{label}: not signed");
                all_good = false;
            }
            Verdict::Good { signer, trusted } => {
                println!("{label}: good signature{}", if signer.is_empty() { String::new() } else { format!(": {signer}") });
                if !trusted {
                    println!("  (the signature is valid; set gpg.ssh.allowedSignersFile to also check whose key it is)");
                }
            }
            Verdict::Bad(why) => {
                println!("{label}: BAD signature: {why}");
                all_good = false;
            }
        }
    }
    if !all_good {
        std::process::exit(1);
    }
    Ok(())
}
