//! Stamps the binary with the commit it was built from.
//!
//! Every nightly is version 0.1.0: the tag rolls, the version does not. So the
//! version alone cannot answer the only question anyone asks it, which is
//! whether this is the build they just installed. The commit can.
//!
//! `MM_COMMIT` in the environment wins, for a build with no git around it (a
//! source tarball, a distro package) and for CI, which already knows the sha it
//! checked out.

use std::process::Command;

fn main() {
    // Naming any of these turns off cargo's default "rerun when anything in the
    // package changed", so the sources have to be named too. Between them: a
    // commit, a branch switch, or an edit all restamp the build.
    for path in ["src", "Cargo.toml", ".git/HEAD", ".git/refs/heads"] {
        println!("cargo:rerun-if-changed={path}");
    }
    println!("cargo:rerun-if-env-changed=MM_COMMIT");

    let commit = std::env::var("MM_COMMIT")
        .ok()
        .filter(|commit| !commit.trim().is_empty())
        .or_else(from_git)
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=MM_COMMIT={commit}");
}

/// The commit this tree is on, marked `-dirty` when it is not what is checked
/// in. An uncommitted build claiming a commit it does not match is exactly the
/// confusion this exists to end.
fn from_git() -> Option<String> {
    let commit = git(&["rev-parse", "--short=8", "HEAD"])?;
    let dirty = git(&["status", "--porcelain"]).is_some_and(|out| !out.is_empty());
    Some(if dirty {
        format!("{commit}-dirty")
    } else {
        commit
    })
}

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}
