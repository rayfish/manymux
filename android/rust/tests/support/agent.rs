//! An ssh agent, in this process, for the tests to be given.
//!
//! The real thing rather than a stand-in, for the reason the sshd beside it is
//! real: what is under test is a client talking a protocol, and the whole of
//! the bug this was written for was a client not saying something. russh ships
//! the agent's server half as well as its client half, so this costs a socket
//! in the test's own directory and no binary anywhere.

// Each test binary uses part of this, never all of it.
#![allow(dead_code)]

use std::path::{Path, PathBuf};

use russh::keys::PrivateKey;
use russh::keys::agent::client::AgentClient;
use russh::keys::agent::server::{self, Agent};
use tokio::net::UnixListener;
use tokio_stream::wrappers::UnixListenerStream;

/// An agent answering on a socket, holding whatever it was started with.
pub struct Agency {
    pub at: PathBuf,
}

/// One that signs whatever it is asked to.
///
/// The confirmation an agent can insist on is a person at a desk pressing
/// something, which is not a thing this suite has.
#[derive(Clone)]
struct Obliging;

impl Agent for Obliging {}

impl Agency {
    /// Start one on `at`, holding `keys` in the order given.
    ///
    /// The order is the point of the parameter: an agent names its keys in the
    /// order they went in, and a client is expected to try them that way.
    pub async fn holding(at: &Path, keys: &[PrivateKey]) -> Self {
        let listener = UnixListener::bind(at).unwrap();
        tokio::spawn(server::serve(UnixListenerStream::new(listener), Obliging));

        let mut client = AgentClient::connect_uds(at).await.unwrap();
        for key in keys {
            client.add_identity(key, &[]).await.unwrap();
        }
        Self {
            at: at.to_path_buf(),
        }
    }
}
