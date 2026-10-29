//! Where things live on disk.
//!
//! File replacements use a private temporary file per writer, so concurrent
//! saves cannot truncate or rename a temporary file another save still owns.

use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{ErrorKind, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde::de::DeserializeOwned;

/// Per-user runtime directory for sockets: `/tmp/manymux-<uid>`, worked out from
/// the uid alone and from nothing else.
///
/// One node per user per machine only holds if every way into that machine
/// agrees on where the socket is, and `$XDG_RUNTIME_DIR` does not agree with
/// itself. An ssh login through `pam_systemd` sets it; an ssh server that hands
/// `mm agent` straight to a shell, a system unit running as this user, and a
/// `cron` job all leave it unset. Each of those would find no node at the path
/// it worked out and start a second one, so which sessions you got back would
/// depend on how you logged in. A uid-keyed directory is the same answer from
/// all of them, which is why tmux keys its socket the same way.
///
/// It also stays short, which socket paths have to: they are capped at ~104
/// bytes on macOS.
pub fn runtime_dir() -> PathBuf {
    // Android has no `/tmp` at all: everything outside an app's own directories
    // is read-only, so the path below has nowhere to land. Termux makes one
    // under its prefix and exports it as `TMPDIR`, which is already private to
    // the one uid the whole sandbox runs as.
    if cfg!(target_os = "android")
        && let Some(tmp) = std::env::var_os("TMPDIR")
    {
        return PathBuf::from(tmp).join("manymux");
    }
    PathBuf::from(format!("/tmp/manymux-{}", uid()))
}

/// Socket the node listens on for the owner of this machine.
pub fn socket() -> PathBuf {
    runtime_dir().join("manymux.sock")
}

/// The runtime directory, created 0700, refusing one that is not ours.
///
/// `/tmp` is world-writable, so a directory there is only private if we are the
/// ones who made it: another user who gets there first owns the directory the
/// socket goes in, and can replace the socket with one of their own for a
/// client to hand its keystrokes to. Under `$XDG_RUNTIME_DIR` that was not
/// possible, and moving out of it is what makes the check necessary.
pub fn ensure_runtime_dir() -> Result<PathBuf> {
    let dir = runtime_dir();
    ensure_private_dir(&dir)?;
    check_runtime_dir(&dir)?;
    Ok(dir)
}

/// Whether `dir` is a directory of ours that nobody else can reach into.
fn check_runtime_dir(dir: &Path) -> Result<()> {
    // Not `metadata`: a symlink planted at that path is followed by the create
    // in the caller, which would land the socket wherever it points.
    let meta = fs::symlink_metadata(dir).with_context(|| format!("checking {}", dir.display()))?;
    if !meta.is_dir() {
        bail!("{} is not a directory", dir.display());
    }
    if meta.uid() != uid() {
        bail!(
            "{} is owned by uid {}, not by you (uid {}). A socket in it would \
             not be yours either; remove it, or name another with `--socket`.",
            dir.display(),
            meta.uid(),
            uid()
        );
    }
    let mode = meta.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        bail!(
            "{} is reachable by other users (mode {mode:o}); `chmod 700` it",
            dir.display()
        );
    }
    Ok(())
}

/// Sockets an older build may have left a node listening on.
///
/// The socket used to follow `$XDG_RUNTIME_DIR`. A node started that way is
/// still running and still owns its sessions, and nothing will look for it at
/// the path above, so it is worth naming when a new node is about to start
/// beside it.
pub fn legacy_sockets() -> Vec<PathBuf> {
    let current = socket();
    let mut dirs = Vec::new();
    if let Some(dir) = dirs::runtime_dir() {
        dirs.push(dir.join("manymux"));
    }
    // The variable is unset far more often than the directory is missing, and
    // this is the path it would have named.
    let run_user = PathBuf::from(format!("/run/user/{}", uid()));
    if run_user.is_dir() {
        dirs.push(run_user.join("manymux"));
    }
    let mut sockets: Vec<PathBuf> = dirs
        .into_iter()
        .map(|dir| dir.join("manymux.sock"))
        .filter(|path| *path != current && path.exists())
        .collect();
    sockets.sort();
    sockets.dedup();
    sockets
}

pub fn uid() -> u32 {
    // SAFETY: getuid always succeeds and touches no memory.
    unsafe { libc::getuid() }
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
pub fn config_dir_for(home: &Path) -> PathBuf {
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
pub fn ensure_private_dir(path: &Path) -> Result<()> {
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .with_context(|| format!("creating {}", path.display()))
}

/// A TOML file read into `T`, or `T`'s default when there is no file yet.
pub fn load_toml<T: DeserializeOwned + Default>(path: &Path) -> Result<T> {
    if !path.exists() {
        return Ok(T::default());
    }
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

/// Write `value` as TOML only its owner can read. `what` names it in the error
/// when it cannot be encoded, as in "encoding the host list".
pub fn save_toml<T: Serialize>(path: &Path, value: &T, what: &str) -> Result<()> {
    let text = toml::to_string_pretty(value).with_context(|| format!("encoding {what}"))?;
    write_private_file(path, text.as_bytes())
}

/// Serialize changes across processes, reading only after taking the lock.
/// The lock has its own file because saving replaces the TOML file's inode.
pub fn change_toml<T, R>(
    path: &Path,
    what: &str,
    change: impl FnOnce(&mut T) -> Result<R>,
) -> Result<R>
where
    T: Serialize + DeserializeOwned + Default,
{
    if let Some(parent) = path.parent() {
        ensure_private_dir(parent)?;
    }
    let lock_path = path.with_extension("lock");
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(&lock_path)
        .with_context(|| format!("opening {}", lock_path.display()))?;
    lock.lock()
        .with_context(|| format!("locking {}", lock_path.display()))?;
    let mut value: T = load_toml(path)?;
    let result = change(&mut value)?;
    save_toml(path, &value, what)?;
    Ok(result)
}

/// Write a file only its owner can read, replacing any existing one.
///
/// The write goes to a temp file and is renamed into place, so a crash or a
/// concurrent reader never sees a half-written identity or allowlist.
pub fn write_private_file(path: &Path, contents: &[u8]) -> Result<()> {
    if let Some(dir) = path.parent() {
        ensure_private_dir(dir)?;
    }
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let (temp, mut file) = loop {
        let next = NEXT.fetch_add(1, Ordering::Relaxed);
        let temp = path.with_extension(format!("{}-{next}.tmp", std::process::id()));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)
        {
            Ok(file) => break (temp, file),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("creating {}", temp.display()));
            }
        }
    };
    let written = file
        .write_all(contents)
        .and_then(|()| file.sync_all())
        .with_context(|| format!("writing {}", temp.display()));
    drop(file);
    let installed = written.and_then(|()| {
        fs::rename(&temp, path).with_context(|| format!("installing {}", path.display()))
    });
    if installed.is_err() {
        // Best effort: preserve the write error if cleanup also fails.
        let _ = fs::remove_file(&temp);
    }
    installed
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};
    use std::thread;

    /// The whole point of the uid-keyed path: an ssh login that set up an
    /// environment, one that set up none, and a service started at boot all
    /// have to reach the same node, or sessions come back only for whoever
    /// logged in the way the last one did.
    #[test]
    fn the_socket_is_the_same_however_you_logged_in() {
        let socket = socket();
        assert_eq!(
            socket,
            PathBuf::from(format!("/tmp/manymux-{}", uid())).join("manymux.sock")
        );
        // Nothing read from the environment, so there is nothing to disagree
        // about: this asserts the path is a function of the uid alone.
        assert_eq!(socket, socket_the_long_way());
    }

    fn socket_the_long_way() -> PathBuf {
        PathBuf::from(format!("/tmp/manymux-{}/manymux.sock", uid()))
    }

    /// macOS caps socket paths at ~104 bytes, and a node that cannot bind is
    /// worse than one in an unusual place.
    #[test]
    fn the_socket_path_is_short_enough_to_bind() {
        assert!(socket().as_os_str().len() < 104);
    }

    #[test]
    fn a_runtime_directory_somebody_else_owns_is_refused() {
        let dir = std::env::temp_dir().join(format!("manymux-perm-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        DirBuilder::new()
            .recursive(true)
            .mode(0o755)
            .create(&dir)
            .unwrap();
        // Ours, but readable by everyone, which for the directory a socket goes
        // in is the same class of mistake as it being someone else's.
        assert!(check_runtime_dir(&dir).is_err());

        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(check_runtime_dir(&dir).is_ok());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_symlink_where_the_runtime_directory_should_be_is_refused() {
        let dir = std::env::temp_dir().join(format!("manymux-link-{}", std::process::id()));
        let _ = fs::remove_file(&dir);
        std::os::unix::fs::symlink("/tmp", &dir).unwrap();
        // It is a directory as far as `metadata` is concerned, which is exactly
        // how a socket ends up somewhere it was never meant to be.
        assert!(check_runtime_dir(&dir).is_err());
        let _ = fs::remove_file(&dir);
    }

    /// Otherwise the update that moves the socket tells everyone their own node
    /// is an old one they should go and look for.
    #[test]
    fn the_socket_in_use_is_never_reported_as_one_left_behind() {
        assert!(!legacy_sockets().contains(&socket()));
    }

    #[test]
    fn concurrent_writes_each_install_a_complete_private_file() {
        let dir = std::env::temp_dir().join(format!("manymux-writes-{}", std::process::id()));
        ensure_private_dir(&dir).unwrap();
        let path = dir.join("settings.toml");
        let start = Arc::new(Barrier::new(8));
        let writers: Vec<_> = (0..8u8)
            .map(|byte| {
                let path = path.clone();
                let start = Arc::clone(&start);
                thread::spawn(move || {
                    let contents = vec![byte; 256 * 1024];
                    start.wait();
                    write_private_file(&path, &contents)
                })
            })
            .collect();
        for writer in writers {
            writer.join().unwrap().unwrap();
        }
        let contents = fs::read(&path).unwrap();
        assert_eq!(contents.len(), 256 * 1024);
        assert!(contents.iter().all(|byte| *byte == contents[0]));
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::remove_dir_all(dir).unwrap();
    }
}
