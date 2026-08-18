//! The Android client's half of manymux.
//!
//! An app cannot fork the `ssh` binary that [`manymux::ssh`] is built on, and
//! it has no terminal to hand the session's bytes to. This crate is the two
//! things that follow from that, and nothing else: an ssh transport that ends
//! in the `(read, write)` pair [`manymux::client::Stream::from_halves`] takes,
//! and a screen the app can paint, driven by `avt` on this side of the wire
//! rather than the node's.
//!
//! Everything between those two is the library's already. The protocol, the
//! attach, the liveness deadline and the reconnect ladder are linked in from a
//! `--no-default-features` build of the crate one directory up, so a fix there
//! is a fix here.

uniffi::setup_scaffolding!();

/// The one kind of lock that may be held across an await.
///
/// Everything else here takes a `std` one through `manymux::lock::held`, which
/// `clippy::await_holding_lock` enforces. The exception earns its name in
/// [`machine::Connections`], where what the lock protects *is* the thing being
/// waited for: two callers wanting a machine at once should share one
/// handshake, and serialising them means holding the lock while it happens.
pub type AsyncMutex<T> = tokio::sync::Mutex<T>;

pub mod ffi;
pub mod keys;
pub mod machine;
pub mod screen;
pub mod session;
pub mod ssh;

/// What this build of the client core is, as `0.1.0 (a1b2c3d4)`.
///
/// The app's own version says what the APK is; this says what it is speaking,
/// which is the half that has to agree with a node. Exported because the two
/// can disagree: the Kotlin is rebuilt by Gradle and the library by cargo, and
/// an app showing both is one that can answer "did that install take" without
/// anybody having to guess.
#[uniffi::export]
pub fn core_version() -> String {
    manymux::VERSION.to_string()
}

#[cfg(test)]
mod tests {
    use super::core_version;

    /// The contract this crate exists to keep: the client core links, and it
    /// links without the `desktop` feature dragging a PTY in behind it.
    #[test]
    fn the_client_core_is_linked_in() {
        assert!(core_version().contains('('), "{}", core_version());
    }
}
