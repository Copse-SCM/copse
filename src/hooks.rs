//! Hooks: your own scripts that copse runs at certain moments, from `.copse/hooks/`
//! (or the folder `core.hooksPath` names, so a team can commit them, e.g. `.githooks`).
//! Each is an executable file named after its moment, run from the top of the
//! repository:
//!
//!   pre-commit      before committing; a non-zero exit cancels the commit
//!   commit-msg      given the path of a file holding the message; may edit it;
//!                   a non-zero exit cancels the commit
//!   post-commit     after committing
//!   pre-push        given the remote's name and URL, with one line per ref on stdin
//!                   ("<name> <commit id>", or "<name> delete"); non-zero cancels the push
//!   pre-rebase      given what's being rebased onto; non-zero cancels the rebase
//!   post-merge      after a merge (or fast-forward) completes
//!   post-checkout   given the old commit, the new commit and 1 (a branch switch)
//!
//! `copse commit --no-verify` and `copse push --no-verify` skip the hooks that can cancel.

use crate::repo::Repo;
use anyhow::{bail, Context, Result};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};

/// Run hook `name` if there is one. Returns its exit code, or None if there's no hook.
pub fn run(repo: &Repo, name: &str, args: &[&str], stdin: Option<&str>) -> Result<Option<i32>> {
    let path = crate::config::hooks_dir(repo).join(name);
    let Ok(md) = std::fs::metadata(&path) else { return Ok(None) };
    if !md.is_file() {
        return Ok(None);
    }
    if md.permissions().mode() & 0o111 == 0 {
        eprintln!("copse: hint: the {name} hook was ignored because it isn't executable (chmod +x {})", path.display());
        return Ok(None);
    }
    let mut child = Command::new(&path)
        .args(args)
        .current_dir(&repo.root)
        .env("COPSE_DIR", &repo.dir)
        .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
        .spawn()
        .with_context(|| format!("can't run the {name} hook ({})", path.display()))?;
    if let (Some(text), Some(mut pipe)) = (stdin, child.stdin.take()) {
        // A hook that doesn't read its input closes the pipe early; that's fine.
        let _ = pipe.write_all(text.as_bytes());
    }
    let status = child.wait()?;
    Ok(Some(status.code().unwrap_or(128)))
}

/// Run a hook that may cancel `what`: a non-zero exit stops the command.
pub fn check(repo: &Repo, name: &str, args: &[&str], stdin: Option<&str>, what: &str) -> Result<()> {
    match run(repo, name, args, stdin)? {
        Some(0) | None => Ok(()),
        Some(code) => bail!("the {name} hook stopped the {what} (exit {code}); use --no-verify to skip it"),
    }
}

/// Run a hook that only reports (post-*): its exit status is ignored.
pub fn notify(repo: &Repo, name: &str, args: &[&str]) {
    if let Err(e) = run(repo, name, args, None) {
        eprintln!("copse: warning: {e:#}");
    }
}
