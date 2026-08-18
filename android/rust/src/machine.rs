//! One machine, reached over an ssh connection made in this process.
//!
//! The connection is the thing `src/ssh.rs` one directory up gets by running
//! the `ssh` binary. An app has no binary to run, so this is where the
//! `ControlMaster`, the `~/.ssh/config` and sshd's own idea of who you are stop
//! being available, and where the app has to carry an address, a key and a note
//! of the host key instead (see [`crate::keys`]).
//!
//! What comes out the other end is the same pair of byte halves, so everything
//! above it is the library's unchanged.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use manymux::lock::held;
use russh::client::{self, Handle};
use russh::keys::key::PrivateKeyWithHashAlg;
use russh::keys::ssh_key::PublicKey;
use russh::{ChannelMsg, Disconnect};
use tokio::io::AsyncWriteExt;

use crate::keys::{Identity, KnownHosts, Verdict};
use crate::ssh::{Ending, Exec, Remote};

/// How much of the far end's output to hold before the channel stops reading.
///
/// This is where transport backpressure comes from: a session printing faster
/// than the phone can take it fills this, the pump stops reading the channel,
/// and the ssh window closes behind it. Unbounded here would mean a phone
/// buffering a `yes` on a machine it cannot keep up with until it is killed.
const HOLD: usize = 64 * 1024;

/// Where a machine is and who to be on it.
///
/// Only the app knows this. It is the phone's stand-in for `~/.ssh/config`, and
/// the reason `src/hosts.rs`'s rule that nothing stores an address survives:
/// the library still stores none.
#[derive(Clone, Debug, uniffi::Record)]
pub struct Machine {
    pub address: String,
    pub port: u16,
    pub user: String,
}

impl Machine {
    /// How this machine is written down in [`KnownHosts`].
    ///
    /// The port is part of it because two ports on one address can be two
    /// machines, which on a phone reaching things through forwards is the
    /// ordinary case rather than the exotic one. The user is not, because a
    /// host key belongs to the machine and not to whoever logs in.
    pub fn at(&self) -> String {
        format!("{}:{}", self.address, self.port)
    }
}

/// An ssh connection to one machine.
///
/// Commands are run on it by [`Exec::open`], which is what climbing the ladder
/// and attaching both go through.
pub struct Connection {
    handle: Handle<Trusting>,
}

impl Connection {
    /// Connect, check the host key, and authenticate with this device's key.
    pub async fn open(machine: &Machine, identity: &Identity, known: &KnownHosts) -> Result<Self> {
        let config = Arc::new(client::Config {
            // A phone's connection is dropped by things a desktop's is not: a
            // sleeping radio, a handover, a NAT that forgot. Keepalives are how
            // a connection that has died gets noticed rather than sitting there
            // looking open, and there is no `ControlMaster` to notice it for us.
            keepalive_interval: Some(Duration::from_secs(15)),
            keepalive_max: 3,
            ..Default::default()
        });

        let refused = Arc::new(Mutex::new(None));
        let handler = Trusting {
            at: machine.at(),
            known: known.clone(),
            refused: Arc::clone(&refused),
        };

        let mut handle = client::connect(config, (machine.address.as_str(), machine.port), handler)
            .await
            // A key that did not match is the one failure russh reports as an
            // ordinary connection error, and it is the one that has something
            // worth reading behind it.
            .map_err(|error| match held(&refused).take() {
                Some(said) => anyhow!(said),
                None => error,
            })
            .with_context(|| format!("connecting to {}", machine.at()))?;

        let key = PrivateKeyWithHashAlg::new(identity.key(), None);
        let allowed = handle
            .authenticate_publickey(&machine.user, key)
            .await
            .with_context(|| format!("authenticating as {} on {}", machine.user, machine.at()))?;
        if !allowed.success() {
            bail!(
                "{} would not take this device's key for {}: add it to that account's \
                 `authorized_keys` ({})",
                machine.at(),
                machine.user,
                identity.fingerprint()
            );
        }

        Ok(Self { handle })
    }

    /// Close the connection, saying so rather than letting it time out.
    pub async fn close(&self) {
        let _ = self
            .handle
            .disconnect(Disconnect::ByApplication, "", "")
            .await;
    }
}

impl Exec for Connection {
    async fn open(&self, command: &str) -> Result<Remote> {
        let channel = self.handle.channel_open_session().await?;
        channel.exec(true, command).await?;
        let (mut incoming, outgoing) = channel.split();

        let (from_there, mut into_here) = tokio::io::simplex(HOLD);
        let (out_of_here, to_there) = tokio::io::simplex(HOLD);
        let (ending, watch) = Ending::new();

        tokio::spawn(async move {
            let mut at_eof = false;
            while let Some(message) = incoming.wait().await {
                match message {
                    ChannelMsg::Data { data } => {
                        if into_here.write_all(&data).await.is_err() {
                            break;
                        }
                    }
                    // Held rather than printed, the way `client::relay` holds
                    // it: on a rung that turns out to have no `mm` this is the
                    // probe working, and printing it would put `mm: command not
                    // found` in front of somebody on every command to a machine
                    // that keeps its copy at home.
                    ChannelMsg::ExtendedData { data, .. } => ending.said(&data),
                    ChannelMsg::ExitStatus { exit_status } => {
                        ending.ended(Some(exit_status as i32))
                    }
                    // The eof is not the end of the channel, and reading it as
                    // one is the mistake this whole module is shaped around: a
                    // status may follow it, and 127 is the only sign a machine
                    // has no `mm` on it. So the reader is told there is no more
                    // output and the loop carries on until the channel closes.
                    ChannelMsg::Eof => {
                        if !at_eof {
                            at_eof = true;
                            let _ = into_here.shutdown().await;
                        }
                    }
                    ChannelMsg::Close => break,
                    _ => {}
                }
            }
            if !at_eof {
                let _ = into_here.shutdown().await;
            }
        });

        tokio::spawn(async move {
            // `data` reads until this end is shut, honouring the channel's
            // window as it goes, which is the backpressure in the other
            // direction.
            let _ = outgoing.data(out_of_here).await;
            let _ = outgoing.eof().await;
        });

        Ok(Remote {
            reader: Box::new(from_there),
            writer: Box::new(to_there),
            watch,
        })
    }
}

/// Trust on first use, and a refusal worth reading on any use after.
struct Trusting {
    at: String,
    known: KnownHosts,
    /// Why the key was refused, for [`Connection::open`] to report instead of
    /// russh's own account of a connection that ended.
    refused: Arc<Mutex<Option<String>>>,
}

impl client::Handler for Trusting {
    type Error = anyhow::Error;

    async fn check_server_key(&mut self, offered: &PublicKey) -> Result<bool> {
        match self.known.verdict(&self.at, offered)? {
            // Written down before the connection is allowed to proceed, or a
            // machine accepted once and never recorded would be accepted with
            // a different key just as happily the next time.
            Verdict::New => {
                self.known.remember(&self.at, offered)?;
                Ok(true)
            }
            Verdict::Known => Ok(true),
            // Not overwritten, and not prompted about here: whether this is a
            // machine that was reinstalled or somebody in the middle is not a
            // question this layer can answer, and the answer is `forget`.
            Verdict::Changed { had } => {
                *held(&self.refused) = Some(format!(
                    "the host key for {} has changed: it was {had} and is now {}. \
                     If that machine was reinstalled, forget the old key; otherwise \
                     somebody is in the middle.",
                    self.at,
                    offered.fingerprint(Default::default())
                ));
                Ok(false)
            }
        }
    }
}
