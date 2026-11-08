//! Crash safety: getting data onto the disk before anything points at it.
//!
//! The rule is the usual one for content-addressed stores: an object must be safely
//! on disk before a ref (a branch, HEAD, the index, a stash list) can name it. So:
//!   - each object file is flushed (fsync) before it's renamed into place, and the
//!     folders it was renamed into are noted;
//!   - before any small state file (a ref, HEAD, the index...) is replaced, those
//!     folders are flushed, then the new file is flushed, renamed over the old one,
//!     and its folder flushed too.
//! A crash can then lose the last command's work, but never leave a branch pointing
//! at an object that isn't there. Pack files are flushed when written (pack.rs).
//!
//! On macOS, `fsync` hands the data to the drive; the drive's own cache is only
//! drained by the much slower F_FULLFSYNC, which copse uses for refs only.
//! `COPSE_FSYNC=0` (or `core.fsync = false`) turns all of this off, for throwaway
//! repositories and test runs where speed matters more.

use std::collections::HashSet;
use std::fs::File;
use std::io;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

extern "C" {
    fn fsync(fd: i32) -> i32;
}

/// Whether to flush at all (on unless turned off).
pub fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        let off = |v: &str| matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "false" | "no" | "off");
        match std::env::var("COPSE_FSYNC") {
            Ok(v) => !off(&v),
            Err(_) => !crate::config::get("core.fsync").is_some_and(|v| off(&v)),
        }
    })
}

/// Flush a file's contents to the drive (plain fsync).
pub fn sync_file(f: &File) -> io::Result<()> {
    if !enabled() {
        return Ok(());
    }
    if unsafe { fsync(f.as_raw_fd()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Flush a file all the way to stable storage (F_FULLFSYNC on macOS): for refs.
pub fn sync_file_fully(f: &File) -> io::Result<()> {
    if !enabled() {
        return Ok(());
    }
    f.sync_all()
}

/// Flush a folder, so renames and new names in it survive a crash.
pub fn sync_dir(dir: &Path) -> io::Result<()> {
    if !enabled() {
        return Ok(());
    }
    let f = File::open(dir)?;
    if unsafe { fsync(f.as_raw_fd()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn pending() -> &'static Mutex<HashSet<PathBuf>> {
    static DIRS: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
    DIRS.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Remember that a new object was renamed into `dir`; flushed before the next ref update.
pub fn note_dir(dir: &Path) {
    if enabled() {
        pending().lock().unwrap().insert(dir.to_path_buf());
    }
}

/// Flush every folder objects were added to since the last call.
pub fn flush_pending() -> io::Result<()> {
    let dirs: Vec<PathBuf> = pending().lock().unwrap().drain().collect();
    for d in dirs {
        match sync_dir(&d) {
            // A folder deleted since (a temporary one, say) has nothing left to flush.
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            other => other?,
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_folder_deleted_before_flushing_is_skipped() {
        let dir = std::env::temp_dir().join(format!("copse-durable-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        super::note_dir(&dir);
        std::fs::remove_dir_all(&dir).unwrap();
        super::flush_pending().unwrap();
    }
}
