//! Who is committing, and when (with the local timezone, as git records it).
//!
//! Name:  $COPSE_AUTHOR, else copse's `user.name` setting, else git's, else $USER.
//! Email: $COPSE_EMAIL,  else copse's `user.email` setting, else git's, else empty.
//! Reading git's settings means commits pushed to GitHub are attributed to the
//! same account as commits made with git, with no extra setup.

use std::process::Command;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

pub(crate) fn git_config(key: &str) -> Option<String> {
    let out = Command::new("git").args(["config", "--get", key]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let value = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!value.is_empty()).then_some(value)
}

fn env(key: &str) -> Option<String> {
    std::env::var(key).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

/// Names and emails can't contain the characters git uses to delimit them.
fn clean(s: &str) -> String {
    s.chars().filter(|c| !matches!(c, '<' | '>' | '\n' | '\0')).collect::<String>().trim().to_string()
}

pub fn name() -> &'static str {
    static NAME: OnceLock<String> = OnceLock::new();
    NAME.get_or_init(|| {
        clean(
            &env("COPSE_AUTHOR")
                .or_else(|| crate::config::get("user.name").filter(|v| !v.trim().is_empty()))
                .or_else(|| git_config("user.name"))
                .or_else(|| env("USER"))
                .unwrap_or_else(|| "unknown".into()),
        )
    })
}

pub fn email() -> &'static str {
    static EMAIL: OnceLock<String> = OnceLock::new();
    EMAIL.get_or_init(|| {
        clean(
            &env("COPSE_EMAIL")
                .or_else(|| crate::config::get("user.email").filter(|v| !v.trim().is_empty()))
                .or_else(|| git_config("user.email"))
                .unwrap_or_default(),
        )
    })
}

/// "Name <email>", the form git stores.
pub fn signature() -> String {
    format!("{} <{}>", name(), email())
}

/// Seconds since the epoch, and the local UTC offset formatted like git ("+0100").
pub fn now() -> (i64, String) {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    (secs, format_offset(local_offset(secs)))
}

pub fn format_offset(offset_secs: i64) -> String {
    let sign = if offset_secs < 0 { '-' } else { '+' };
    let minutes = offset_secs.abs() / 60;
    format!("{sign}{:02}{:02}", minutes / 60, minutes % 60)
}

/// The local timezone's offset from UTC at `secs`, in seconds (via the C library,
/// so daylight saving time is handled).
fn local_offset(secs: i64) -> i64 {
    // `struct tm` has this layout on both macOS and glibc Linux (64-bit).
    #[repr(C)]
    struct Tm {
        tm_sec: i32,
        tm_min: i32,
        tm_hour: i32,
        tm_mday: i32,
        tm_mon: i32,
        tm_year: i32,
        tm_wday: i32,
        tm_yday: i32,
        tm_isdst: i32,
        tm_gmtoff: i64,
        tm_zone: *const u8,
    }
    extern "C" {
        fn localtime_r(time: *const i64, result: *mut Tm) -> *mut Tm;
    }
    let mut tm = std::mem::MaybeUninit::<Tm>::zeroed();
    let ok = unsafe { !localtime_r(&secs, tm.as_mut_ptr()).is_null() };
    if ok {
        unsafe { tm.assume_init().tm_gmtoff }
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::format_offset;

    #[test]
    fn formats_offsets_like_git() {
        assert_eq!(format_offset(0), "+0000");
        assert_eq!(format_offset(3600), "+0100");
        assert_eq!(format_offset(-5 * 3600), "-0500");
        assert_eq!(format_offset(5 * 3600 + 1800), "+0530");
        assert_eq!(format_offset(-(9 * 3600 + 1800)), "-0930");
    }
}
