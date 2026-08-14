//! Where things live on disk.

use std::path::PathBuf;

use anyhow::{Context, Result};

/// Per-user runtime directory for sockets. Prefers `$XDG_RUNTIME_DIR`, which is
/// already 0700 and cleaned on logout; falls back to a uid-scoped directory
/// under `/tmp` on systems without one (macOS).
///
/// Unix socket paths are capped at ~104 bytes on macOS, so this stays short
/// rather than nesting under Application Support.
pub fn runtime_dir() -> PathBuf {
    if let Some(dir) = dirs::runtime_dir() {
        return dir.join("manymux");
    }
    // Android has no `/tmp` at all: everything outside an app's own directories
    // is read-only, so the fallback below has nowhere to land. Termux makes one
    // under its prefix and exports it as `TMPDIR`, which is already private to
    // the one uid the whole sandbox runs as.
    if cfg!(target_os = "android")
        && let Some(tmp) = std::env::var_os("TMPDIR")
    {
        return PathBuf::from(tmp).join("manymux");
    }
    // SAFETY: getuid always succeeds and touches no memory.
    let uid = unsafe { libc::getuid() };
    PathBuf::from(format!("/tmp/manymux-{uid}"))
}

/// Socket the node listens on for the owner of this machine.
pub fn socket() -> PathBuf {
    runtime_dir().join("manymux.sock")
}

/// Config directory: the list of machines to watch, and nothing secret. Keys
/// and access policy are ssh's, not ours.
///
/// `MM_CONFIG_DIR` overrides it, which is how two nodes run on one machine
/// (tests, and trying it out before committing to it).
pub fn config_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("MM_CONFIG_DIR") {
        return PathBuf::from(dir);
    }
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("manymux")
}

/// The config directory belonging to `home`, for a system-wide unit that will
/// run as someone else. Same answer [`config_dir`] would give in that account,
/// worked out without their environment to ask.
pub fn config_dir_for(home: &std::path::Path) -> PathBuf {
    if cfg!(target_os = "macos") {
        home.join("Library/Application Support/manymux")
    } else {
        home.join(".config/manymux")
    }
}

/// Create a directory only its owner can enter. Sockets and keys live in these,
/// so the mode is load-bearing rather than tidiness.
///
/// The mode applies to directories this creates, never to ones that already
/// exist: a socket path of `/tmp/x.sock` must not lead us to chmod `/tmp`.
pub fn ensure_private_dir(path: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;

    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .with_context(|| format!("creating {}", path.display()))
}

/// Write a file only its owner can read, replacing any existing one.
///
/// The write goes to a temp file and is renamed into place, so a crash or a
/// concurrent reader never sees a half-written identity or allowlist.
pub fn write_private_file(path: &std::path::Path, contents: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    if let Some(dir) = path.parent() {
        ensure_private_dir(dir)?;
    }
    let temp = path.with_extension("tmp");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&temp)
        .with_context(|| format!("creating {}", temp.display()))?;
    file.write_all(contents)
        .and_then(|()| file.sync_all())
        .with_context(|| format!("writing {}", temp.display()))?;
    drop(file);
    std::fs::rename(&temp, path).with_context(|| format!("installing {}", path.display()))
}
