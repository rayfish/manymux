//! A machine with an `mm` somewhere on it, or nowhere.
//!
//! Shared by the ladder tests and the ssh ones, which ask the same question of
//! two different transports: which spelling of `mm` answers, and what happens
//! to a machine where none of them does.

// Each test binary uses part of this, never all of it.
#![allow(dead_code)]

use std::path::PathBuf;

/// Where `mm` is on the machine being stood up.
pub enum Mm {
    /// On the PATH, so the first rung answers.
    OnPath,
    /// Only in the home directory, so the first rung is a 127.
    InHome,
    /// Nowhere, so both rungs are.
    Missing,
    /// On the PATH but not working: it complains and exits non-zero, which is
    /// a machine answering badly rather than a spelling being wrong.
    Broken,
}

/// A directory that stands in for a machine: a PATH and a home.
pub struct World {
    pub dir: PathBuf,
    pub broken: bool,
}

impl World {
    pub fn where_mm_is(name: &str, mm: Mm) -> Self {
        let dir = std::env::temp_dir().join(format!("mm-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        // `mm` is a symlink to the stub agent rather than a script, so there is
        // no file being written that the shell might find itself executing.
        let agent = PathBuf::from(env!("CARGO_BIN_EXE_stub-agent"));
        let put_it_at = match mm {
            Mm::OnPath | Mm::Broken => Some(dir.join("bin/mm")),
            Mm::InHome => Some(dir.join("home/.local/bin/mm")),
            Mm::Missing => None,
        };
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        if let Some(path) = put_it_at {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::os::unix::fs::symlink(&agent, &path).unwrap();
        }

        Self {
            dir,
            broken: matches!(mm, Mm::Broken),
        }
    }
}
