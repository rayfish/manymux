//! Persistent terminal sessions you can leave and come back to.
//!
//! The node owns the PTY, so disconnecting a client never touches the child
//! process. Other machines are reached by running `mm agent` there over ssh,
//! which means your existing ssh setup decides both how to get there and who is
//! allowed in: manymux keeps no keys and no allowlist of its own.
//!
//! Building without the `desktop` feature drops the parts that need a real
//! terminal or a PTY, leaving the client core that a mobile app links against.

/// This build, as `0.1.0 (a1b2c3d4)`.
///
/// The nightly tag rolls over many builds while the version number stays put,
/// so the version on its own cannot answer the one question anyone asks it:
/// whether this is the build they just installed. `build.rs` stamps in the
/// commit, and everything that says what it is running says this.
pub const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), " (", env!("MM_COMMIT"), ")");

pub mod client;
pub mod config;
pub mod hosts;
pub mod lock;
pub mod notify;
pub mod proto;
pub mod settings;
pub mod ssh;
pub mod style;
pub mod term;
pub mod user;

#[cfg(feature = "desktop")]
pub mod clipboard;
#[cfg(feature = "desktop")]
pub mod ipc;
#[cfg(feature = "desktop")]
pub mod log;
#[cfg(feature = "desktop")]
pub mod node;
#[cfg(feature = "desktop")]
pub mod service;
#[cfg(feature = "desktop")]
pub mod signature;
#[cfg(feature = "desktop")]
pub mod update;

#[cfg(test)]
mod tests {
    /// The point of stamping the commit in: two nightlies share a version
    /// number, so anything that identifies a build has to carry more than one.
    #[test]
    fn the_version_names_the_commit_it_was_built_from() {
        let version = super::VERSION;
        assert!(
            version.starts_with(env!("CARGO_PKG_VERSION")),
            "the package version comes first: {version}"
        );
        let commit = version
            .split_once('(')
            .and_then(|(_, rest)| rest.strip_suffix(')'))
            .expect("a commit in parentheses");
        assert!(!commit.is_empty(), "{version}");
        // No git and no `MM_COMMIT`: the build cannot say what it is, which is
        // the state this whole thing exists to prevent.
        assert_ne!(commit, "unknown", "set MM_COMMIT for a build outside git");
    }
}
