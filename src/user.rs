//! Who this node runs as.
//!
//! A node started by launchd or systemd inherits almost no environment: no
//! `SHELL`, often no `HOME`, and a bare `PATH`. Trusting those would hand you a
//! `/bin/sh` prompt in an empty environment instead of your own shell, which is
//! the difference between a session that feels like logging in and one that
//! does not. So the account details come from the passwd database, the way
//! `login` and `sshd` find them, rather than from whatever the service manager
//! happened to pass down.

use std::ffi::CStr;
use std::sync::OnceLock;

/// The account this process runs as.
#[derive(Debug, Clone)]
pub struct User {
    pub name: String,
    pub home: String,
    /// Login shell from the passwd entry.
    pub shell: String,
}

/// This process's account, or `None` if the passwd database has no entry for
/// it (a container with no `/etc/passwd`, say).
pub fn current() -> Option<&'static User> {
    static USER: OnceLock<Option<User>> = OnceLock::new();
    USER.get_or_init(lookup).as_ref()
}

/// The user's login shell, falling back the way `login` does.
pub fn shell() -> String {
    current()
        .map(|user| user.shell.clone())
        .filter(|shell| !shell.is_empty())
        // A machine with no passwd entry, or an interactive run where the
        // caller deliberately set SHELL.
        .or_else(|| std::env::var("SHELL").ok())
        .filter(|shell| !shell.is_empty())
        .unwrap_or_else(|| "/bin/sh".to_string())
}

fn lookup() -> Option<User> {
    // SAFETY: getpwuid returns a pointer into static storage, valid until the
    // next getpw* call. This runs once, behind a OnceLock, and every field is
    // copied out before returning.
    unsafe {
        let entry = libc::getpwuid(libc::getuid());
        if entry.is_null() {
            return None;
        }
        let entry = &*entry;
        Some(User {
            name: string(entry.pw_name),
            home: string(entry.pw_dir),
            shell: string(entry.pw_shell),
        })
    }
}

/// SAFETY: `ptr` must be null or a valid NUL-terminated C string.
unsafe fn string(ptr: *const libc::c_char) -> String {
    if ptr.is_null() {
        return String::new();
    }
    unsafe { CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_login_shell_is_a_real_path() {
        let shell = shell();
        assert!(
            shell.starts_with('/'),
            "a login shell should be an absolute path, got {shell:?}"
        );
        assert!(std::path::Path::new(&shell).exists(), "{shell} is missing");
    }

    /// The whole point: this must not come from `SHELL`, which a service does
    /// not have.
    #[test]
    fn the_shell_is_found_without_an_environment() {
        let user = current().expect("this test host has a passwd entry");
        assert!(user.shell.starts_with('/'), "{:?}", user.shell);
        assert!(!user.name.is_empty());
        assert!(user.home.starts_with('/'), "{:?}", user.home);
    }
}
