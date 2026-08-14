//! Files pasted into a session from the clipboard of the machine the client is
//! sitting at.
//!
//! The host end of it. The bytes arrive over the attached stream, get written
//! down here, and what actually reaches the program is the path they were
//! written to. That indirection is the whole trick: no terminal carries an
//! image, but every program on the far side of one can open a file, and the
//! ones worth pasting a screenshot into (`claude` above all) take a path in
//! their prompt and read it themselves.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};

/// How long a pasted file is kept. Long enough to still be there when you come
/// back to the conversation, short enough that a habit of pasting screenshots
/// does not quietly fill a tmpfs.
const KEEP: Duration = Duration::from_secs(24 * 60 * 60);

/// Extensions a paste may claim. The kind comes from the client, which sniffed
/// the bytes, but it ends up in a filename, so it is checked against a list
/// here rather than trusted to be a word.
const KINDS: &[&str] = &["png", "jpg", "gif", "webp"];

/// Distinguishes two pastes in the same second.
static NEXT: AtomicU64 = AtomicU64::new(0);

/// Where pasted files land: beside the socket, in a directory only this user
/// can enter, on a filesystem that is cleaned out at logout on most machines.
pub fn dir() -> PathBuf {
    crate::config::runtime_dir().join("pastes")
}

/// Write a pasted file down, and say where it went.
pub fn write(kind: &str, data: &[u8]) -> Result<PathBuf> {
    let dir = dir();
    crate::config::ensure_private_dir(&dir)?;
    prune(&dir);

    let path = dir.join(name(kind));
    crate::config::write_private_file(&path, data)
        .with_context(|| format!("writing the pasted file to {}", path.display()))?;
    Ok(path)
}

/// A name that sorts by when it was pasted and collides with nothing.
fn name(kind: &str) -> String {
    let seconds = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or_default();
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    format!("paste-{seconds}-{n}.{}", extension(kind))
}

/// The extension to use, which is the one the client sniffed if it is one we
/// know and `bin` otherwise. Never the client's string as given: it becomes
/// part of a path.
fn extension(kind: &str) -> &str {
    let kind = kind.trim().to_ascii_lowercase();
    KINDS
        .iter()
        .find(|known| ***known == *kind)
        .copied()
        .unwrap_or("bin")
}

/// What the program sees: the path, and a space so the next word does not run
/// into it.
///
/// Wrapped as a paste when the program asked for bracketed paste, because that
/// is what it asked the question for. A shell that gets an unbracketed path
/// would be fine, but a full-screen editor would take each character as a
/// command.
pub fn typed(path: &Path, bracketed: bool) -> String {
    let path = path.display();
    if bracketed {
        format!("\x1b[200~{path}\x1b[201~ ")
    } else {
        format!("{path} ")
    }
}

/// Remove pastes nobody is coming back for.
///
/// Best effort by design: this runs on the way to writing a file, and a
/// directory that cannot be tidied is no reason to refuse the paste.
fn prune(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let stale = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .map(|at| at.elapsed().map(|age| age > KEEP).unwrap_or(false))
            .unwrap_or(false);
        if stale {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_kind_from_the_wire_cannot_shape_the_path() {
        assert_eq!(extension("png"), "png");
        assert_eq!(extension("PNG"), "png");
        // The point of the list: nothing here can climb out of the directory
        // or hide a file somewhere else.
        assert_eq!(extension("../../.ssh/authorized_keys"), "bin");
        assert_eq!(extension("png/../x"), "bin");
        assert_eq!(extension(""), "bin");
    }

    #[test]
    fn names_do_not_collide_within_a_second() {
        assert_ne!(name("png"), name("png"));
        assert!(name("png").ends_with(".png"));
    }

    #[test]
    fn a_path_is_pasted_the_way_the_program_asked_for() {
        let path = Path::new("/run/user/1000/manymux/pastes/paste-1-0.png");
        assert_eq!(
            typed(path, true),
            "\x1b[200~/run/user/1000/manymux/pastes/paste-1-0.png\x1b[201~ "
        );
        assert_eq!(
            typed(path, false),
            "/run/user/1000/manymux/pastes/paste-1-0.png "
        );
    }
}
