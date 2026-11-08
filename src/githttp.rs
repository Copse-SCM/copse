//! git's "smart HTTP" protocol: the same conversation as over SSH, carried in
//! HTTP(S) requests.
//!
//!   GET  <repo>/info/refs?service=git-upload-pack   -> the server's branches
//!   POST <repo>/git-upload-pack                      -> wants/haves in, pack out
//!   POST <repo>/git-receive-pack                     -> commands + pack in, status out
//!
//! Signing in uses git's own credential helpers (`git credential fill`), so a login
//! saved for git (such as the macOS Keychain entry for GitHub) works for copse too.
//! copse asks only when the server answers 401, saves credentials that work
//! (`approve`) and forgets ones that are rejected (`reject`), exactly like git.

use crate::gitwire::{read_advertisement, read_pkt, GitId};
use anyhow::{anyhow, bail, Context, Result};
use std::collections::HashSet;
use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::time::Duration;

/// GitHub only speaks the smart protocol to clients whose agent starts with "git/".
pub fn user_agent() -> String {
    format!("git/2.0 (copse {})", crate::VERSION)
}

struct Credential {
    username: String,
    password: String,
    /// From a credential helper (so it should be approved or rejected afterwards),
    /// rather than written into the address.
    from_helper: bool,
    approved: bool,
}

pub struct Http {
    base: String,
    service: &'static str,
    agent: ureq::Agent,
    auth: Option<Credential>,
}

impl Http {
    pub fn new(url: &str, service: &'static str) -> Http {
        let agent = ureq::AgentBuilder::new()
            .user_agent(&user_agent())
            .timeout_connect(Duration::from_secs(30))
            .redirects(5)
            .max_idle_connections_per_host(16) // dumb servers: many small requests at once
            .try_proxy_from_env(true)
            .build();
        let (base, auth) = split_userinfo(url.trim_end_matches('/'));
        Http { base, service, agent, auth }
    }

    /// The server's branches and capabilities.
    pub fn discover(&mut self) -> Result<(Vec<(String, GitId)>, HashSet<String>)> {
        let url = format!("{}/info/refs?service=git-{}", self.base, self.service);
        let resp = self.send(|agent, auth| {
            let mut req = agent.get(&url);
            if let Some(a) = auth {
                req = req.set("Authorization", a);
            }
            req.call()
        })?;
        // If the repository moved (renamed, transferred), keep using its new address.
        if let Some((base, _)) = resp.get_url().split_once("/info/refs") {
            self.base = split_userinfo(base).0;
        }
        if resp.content_type() != format!("application/x-git-{}-advertisement", self.service) {
            // An older "dumb" server (plain files, e.g. cgit): its branch list is plain text.
            // (Plain web servers label it text/plain or application/octet-stream.)
            if self.service == "receive-pack" && matches!(resp.content_type(), "text/plain" | "application/octet-stream" | "") {
                bail!("{} only serves plain files (git's read-only \"dumb\" protocol), so it can't take pushes; push over SSH, or to a server that speaks git's smart protocol", redact(&self.base));
            }
            if self.service == "upload-pack" && matches!(resp.content_type(), "text/plain" | "application/octet-stream" | "") {
                let mut body = String::new();
                resp.into_reader().take(64 << 20).read_to_string(&mut body)?;
                if let Some(refs) = parse_dumb_refs(&body) {
                    let mut caps: HashSet<String> = ["dumb".to_string()].into();
                    if let Some(head) = self.get_maybe("HEAD")? {
                        if let Some(b) = String::from_utf8_lossy(&head).trim().strip_prefix("ref: refs/heads/") {
                            caps.insert(format!("symref=HEAD:refs/heads/{b}"));
                        }
                    }
                    return Ok((refs, caps));
                }
            }
            bail!("{} doesn't look like a git repository (check the address)", redact(&self.base));
        }
        let mut body = Vec::new();
        resp.into_reader().take(256 << 20).read_to_end(&mut body)?;
        let mut r: &[u8] = &body;
        let first = read_pkt(&mut r)?.context("empty reply from the git server")?;
        if !first.starts_with(b"# service=") {
            bail!("unexpected reply from the git server");
        }
        if read_pkt(&mut r)?.is_some() {
            bail!("unexpected reply from the git server");
        }
        read_advertisement(&mut r)
    }

    /// A file from a "dumb" server (objects, packs, HEAD), or None if it isn't there.
    pub fn get_maybe(&mut self, path: &str) -> Result<Option<Vec<u8>>> {
        let url = format!("{}/{path}", self.base);
        let result = self.send(|agent, auth| {
            let mut req = agent.get(&url);
            if let Some(a) = auth {
                req = req.set("Authorization", a);
            }
            req.call()
        });
        match result {
            Ok(resp) => {
                let mut body = Vec::new();
                resp.into_reader().take(4 << 30).read_to_end(&mut body)?;
                Ok(Some(body))
            }
            Err(e) if e.to_string().contains("(404)") => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Several files from a "dumb" server at once (None for each that isn't there).
    /// The first goes through the normal sign-in; the rest reuse that login, in parallel.
    pub fn get_many(&mut self, paths: &[String]) -> Result<Vec<Option<Vec<u8>>>> {
        use rayon::prelude::*;
        let Some((first, rest)) = paths.split_first() else { return Ok(Vec::new()) };
        let mut out = vec![self.get_maybe(first)?];
        let header = self.auth.as_ref().map(|c| format!("Basic {}", base64(format!("{}:{}", c.username, c.password).as_bytes())));
        let (agent, base) = (self.agent.clone(), self.base.clone());
        let pool = rayon::ThreadPoolBuilder::new().num_threads(8).build()?;
        let fetched: Vec<Result<Option<Vec<u8>>>> = pool.install(|| {
            rest.par_iter()
                .map(|p| {
                    let mut req = agent.get(&format!("{base}/{p}"));
                    if let Some(h) = &header {
                        req = req.set("Authorization", h);
                    }
                    match req.call() {
                        Ok(resp) => {
                            let mut body = Vec::new();
                            resp.into_reader().take(4 << 30).read_to_end(&mut body)?;
                            Ok(Some(body))
                        }
                        Err(ureq::Error::Status(404, _)) => Ok(None),
                        Err(e) => Err(anyhow!("couldn't fetch {p} from {}: {e}", redact(&base))),
                    }
                })
                .collect()
        });
        for f in fetched {
            out.push(f?);
        }
        Ok(out)
    }

    /// Send a whole request (wants and haves, or commands and a pack) and return
    /// the server's reply as a stream.
    pub fn post(&mut self, body: &[u8]) -> Result<Box<dyn Read + Send>> {
        let url = format!("{}/git-{}", self.base, self.service);
        let content_type = format!("application/x-git-{}-request", self.service);
        let accept = format!("application/x-git-{}-result", self.service);
        let resp = self.send(|agent, auth| {
            let mut req = agent.post(&url).set("Content-Type", &content_type).set("Accept", &accept);
            if let Some(a) = auth {
                req = req.set("Authorization", a);
            }
            req.send_bytes(body)
        })?;
        Ok(Box::new(resp.into_reader()))
    }

    /// Make a request, signing in if the server asks for it.
    pub fn send(&mut self, request: impl Fn(&ureq::Agent, Option<&str>) -> Result<ureq::Response, ureq::Error>) -> Result<ureq::Response> {
        let shown = redact(&self.base);
        loop {
            let header = self.auth.as_ref().map(|c| format!("Basic {}", base64(format!("{}:{}", c.username, c.password).as_bytes())));
            match request(&self.agent, header.as_deref()) {
                Ok(resp) => {
                    if let Some(c) = self.auth.as_mut() {
                        if c.from_helper && !c.approved {
                            let _ = run_helper("approve", &helper_input(&self.base, Some(c))); // remember what worked
                            c.approved = true;
                        }
                    }
                    return Ok(resp);
                }
                Err(ureq::Error::Status(401, _)) => match self.auth.take() {
                    None => {
                        self.auth = Some(fill(&self.base)?);
                        continue; // try again, signed in
                    }
                    Some(c) => {
                        if c.from_helper {
                            let _ = run_helper("reject", &helper_input(&self.base, Some(&c))); // forget what didn't
                        }
                        bail!(
                            "sign-in to {shown} failed (401). For GitHub, the password must be a personal access \
                             token with access to this repository, not your account password"
                        );
                    }
                },
                Err(ureq::Error::Status(403, _)) => {
                    bail!("{shown} refused access (403): this account or token isn't allowed to {}", self.action())
                }
                Err(ureq::Error::Status(404, _)) => {
                    bail!("{shown}: repository not found (404). Check the address, and that you have access to it")
                }
                Err(ureq::Error::Status(code, resp)) => bail!("{shown}: the server replied {code} {}", resp.status_text()),
                Err(ureq::Error::Transport(t)) => bail!("couldn't reach {shown}: {t}"),
            }
        }
    }

    fn action(&self) -> &'static str {
        if self.service == "receive-pack" {
            "push to this repository"
        } else {
            "read this repository"
        }
    }
}

/// A dumb server's info/refs: "<id>\t<ref>" per line. None if it isn't that.
fn parse_dumb_refs(body: &str) -> Option<Vec<(String, GitId)>> {
    let mut refs = Vec::new();
    for line in body.lines().filter(|l| !l.trim().is_empty()) {
        let (id, name) = line.split_once('\t')?;
        if id.len() != 40 {
            return None;
        }
        let mut g = [0u8; 20];
        for (i, b) in g.iter_mut().enumerate() {
            *b = u8::from_str_radix(&id[i * 2..i * 2 + 2], 16).ok()?;
        }
        refs.push((name.trim().to_string(), g));
    }
    Some(refs)
}

/// `https://user:token@host/path` -> (`https://host/path`, those credentials).
fn split_userinfo(url: &str) -> (String, Option<Credential>) {
    let Some((scheme, rest)) = url.split_once("://") else {
        return (url.to_string(), None);
    };
    let authority_end = rest.find('/').unwrap_or(rest.len());
    let Some(at) = rest[..authority_end].rfind('@') else {
        return (url.to_string(), None);
    };
    let base = format!("{scheme}://{}", &rest[at + 1..]);
    match rest[..at].split_once(':') {
        Some((user, pass)) => {
            let cred = Credential { username: user.to_string(), password: pass.to_string(), from_helper: false, approved: false };
            (base, Some(cred))
        }
        None => (base, None),
    }
}

/// An address that's safe to print: any password in it is replaced by ***.
pub fn redact(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else { return url.to_string() };
    let authority_end = rest.find('/').unwrap_or(rest.len());
    match rest[..authority_end].rfind('@') {
        Some(at) => match rest[..at].split_once(':') {
            Some((user, _)) => format!("{scheme}://{user}:***@{}", &rest[at + 1..]),
            None => url.to_string(),
        },
        None => url.to_string(),
    }
}

/// What git's credential helpers expect on stdin.
fn helper_input(base: &str, cred: Option<&Credential>) -> String {
    let (protocol, rest) = base.split_once("://").unwrap_or(("https", base));
    let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
    let mut s = format!("protocol={protocol}\nhost={host}\n");
    if !path.is_empty() {
        s += &format!("path={path}\n");
    }
    if let Some(c) = cred {
        s += &format!("username={}\npassword={}\n", c.username, c.password);
    }
    s + "\n"
}

fn run_helper(action: &str, input: &str) -> Result<String> {
    let mut child = Command::new("git")
        .args(["credential", action])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .context("signing in uses git's credential helpers, but git isn't installed")?;
    child.stdin.take().unwrap().write_all(input.as_bytes())?;
    let out = child.wait_with_output()?;
    if !out.status.success() {
        bail!("git credential {action} failed");
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn fill(base: &str) -> Result<Credential> {
    let shown = redact(base);
    let out = run_helper("fill", &helper_input(base, None)).map_err(|_| {
        anyhow!(
            "{shown} needs you to sign in, and no saved login was found. For GitHub, create a personal access \
             token and sign in once with git (or run `git credential approve`); copse uses the same saved login"
        )
    })?;
    let field = |key: &str| out.lines().find_map(|l| l.strip_prefix(&format!("{key}="))).map(str::to_string);
    match (field("username"), field("password")) {
        (Some(username), Some(password)) => Ok(Credential { username, password, from_helper: true, approved: false }),
        _ => bail!("{shown} needs you to sign in, and no saved login was found"),
    }
}

fn base64(data: &[u8]) -> String {
    const CHARS: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
    for chunk in data.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |acc, (i, &b)| acc | (b as u32) << (16 - 8 * i));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(CHARS[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_standard() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"user:token"), "dXNlcjp0b2tlbg==");
    }

    #[test]
    fn passwords_never_shown() {
        assert_eq!(redact("https://me:secret@github.com/me/r.git"), "https://me:***@github.com/me/r.git");
        assert_eq!(redact("https://github.com/me/r.git"), "https://github.com/me/r.git");
        let (base, cred) = split_userinfo("https://me:secret@host:8443/r.git");
        assert_eq!(base, "https://host:8443/r.git");
        assert_eq!(cred.map(|c| (c.username, c.password)), Some(("me".into(), "secret".into())));
    }

    #[test]
    fn credential_helper_input() {
        assert_eq!(helper_input("https://github.com/me/r.git", None), "protocol=https\nhost=github.com\npath=me/r.git\n\n");
    }
}
