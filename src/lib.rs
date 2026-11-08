//! copse as a library: the repository, object store, history and git protocol code
//! the `copse` command is built on, for programs such as Copse Server.

pub mod branches;
pub mod cmd;
pub mod config;
pub mod diffs;
pub mod durable;
pub mod ffwd;
pub mod gc;
pub mod githttp;
pub mod gitimport;
pub mod gitserve;
pub mod gitview;
pub mod gitwire;
pub mod hooks;
pub mod hunks;
pub mod identity;
pub mod integrity;
pub mod investigate;
pub mod lfs;
pub mod logs;
pub mod merge;
pub mod objects;
pub mod pack;
pub mod plumbing;
pub mod pretty;
pub mod rebase;
pub mod reflog;
pub mod remote;
pub mod renames;
pub mod repo;
pub mod restore;
pub mod signing;
pub mod sparse;
pub mod stash;
pub mod submodules;
pub mod tags;
pub mod tools;
pub mod transport;
pub mod tui;
pub mod worktree;
pub mod worktrees;

/// The version: Cargo's, or the full name a release or nightly build was given
/// (COPSE_BUILD at compile time, e.g. "2026.11.09-dev+abc1234" for a nightly).
pub const VERSION: &str = match option_env!("COPSE_BUILD") {
    Some(v) => v,
    None => env!("CARGO_PKG_VERSION"),
};

/// Whether `name` is one of copse's own commands (so an alias can't replace it).
/// The `copse` binary registers its command list here; a library user has none.
pub static BUILTIN_COMMANDS: std::sync::OnceLock<fn(&str) -> bool> = std::sync::OnceLock::new();

pub fn is_builtin_command(name: &str) -> bool {
    BUILTIN_COMMANDS.get().is_some_and(|f| f(name))
}
