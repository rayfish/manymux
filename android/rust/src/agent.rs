//! Keys somebody else is holding.
//!
//! The app's own key ([`crate::keys::Identity`]) is the one a phone has, and on
//! a phone this module never does anything: nothing there sets `SSH_AUTH_SOCK`
//! and there is no socket to reach across the app sandbox even where something
//! did. What it is for is the other end this crate is driven from, the `reach`
//! example on a desktop, where the keys the machines already know are in an
//! agent and pasting a fresh one into every `authorized_keys` to try something
//! out is a poor trade.
//!
//! The agent is passed in rather than looked up here, and that is deliberate.
//! Read out of the environment down in the connect, a test run on a machine
//! with an agent would offer whoever is running it their own keys, and the
//! suite would pass or fail by what somebody had loaded that morning.
//! [`Agent::in_the_environment`] is called at the edges, by a caller that has
//! decided it wants one.

use std::path::PathBuf;

use anyhow::{Context, Result};
use russh::Signer;
use russh::keys::HashAlg;
use russh::keys::agent::AgentIdentity;
use russh::keys::agent::client::AgentClient;
use russh::keys::ssh_key::PublicKey;
use tokio::net::UnixStream;

/// An ssh agent, by the socket it answers on.
#[derive(Clone, Debug)]
pub struct Agent {
    at: PathBuf,
}

impl Agent {
    /// The one answering on this socket.
    pub fn on(at: impl Into<PathBuf>) -> Self {
        Self { at: at.into() }
    }

    /// The one this process was told about, where it was told about one.
    pub fn in_the_environment() -> Option<Self> {
        std::env::var_os("SSH_AUTH_SOCK")
            .filter(|at| !at.is_empty())
            .map(Self::on)
    }

    /// Open it and ask what it has.
    ///
    /// One connection for the whole ladder rather than one per key: the same
    /// [`Held`] answers every signature, which is what an agent is for.
    pub async fn open(&self) -> Result<Held> {
        let mut client = AgentClient::connect_uds(&self.at)
            .await
            .with_context(|| format!("reaching the ssh agent on {}", self.at.display()))?;
        let holding = client
            .request_identities()
            .await
            .context("asking the ssh agent what it is holding")?;
        Ok(Held { client, holding })
    }
}

/// A live connection to an agent, and what it said it had.
pub struct Held {
    client: AgentClient<UnixStream>,
    holding: Vec<AgentIdentity>,
}

impl Held {
    /// The plain public keys it is holding, in the order it named them, which
    /// is the order it would have somebody try them.
    ///
    /// Certificates are left out rather than handled badly. They go to the far
    /// end by a different request (`authenticate_certificate_with`) and mean a
    /// CA this device knows nothing about, so an agent holding one is an agent
    /// whose other keys are tried and whose certificate is not.
    pub fn keys(&self) -> Vec<PublicKey> {
        self.holding
            .iter()
            .filter_map(|identity| match identity {
                AgentIdentity::PublicKey { key, .. } => Some(key.clone()),
                AgentIdentity::Certificate { .. } => None,
            })
            .collect()
    }
}

impl Signer for Held {
    type Error = anyhow::Error;

    async fn auth_sign(
        &mut self,
        key: &AgentIdentity,
        hash_alg: Option<HashAlg>,
        to_sign: Vec<u8>,
    ) -> Result<Vec<u8>> {
        self.client
            .sign_request(key, hash_alg, to_sign)
            .await
            .context("asking the ssh agent to sign")
    }
}
