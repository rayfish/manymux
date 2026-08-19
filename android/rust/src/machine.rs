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

use anyhow::{Context, Result, anyhow};
use manymux::lock::held;
use russh::client::{self, AuthResult, Handle};
use russh::keys::key::PrivateKeyWithHashAlg;
use russh::keys::ssh_key::PublicKey;
use russh::{ChannelMsg, Disconnect, Error as SshError, MethodKind};
use tokio::io::AsyncWriteExt;
use tokio::time::timeout;

use crate::AsyncMutex;
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
    /// A free-form string rather than a parsed host, which is the one place
    /// this crate takes the loose type on purpose: it crosses the boundary as
    /// a record, it is whatever somebody typed into a field, and the thing
    /// that decides whether it is a name or an address is the resolver.
    pub address: String,
    pub port: u16,
    pub user: String,
}

/// A machine saying no, when the answer is one somebody can act on.
///
/// Everything else that goes wrong here is a sentence to read: a name that did
/// not resolve, a port with nothing behind it, a radio that slept. These two
/// are the failures with a button behind them, so they are a type rather than
/// wording: the app decides what to offer by matching on this, and a message
/// reworded next month must not be able to turn the button that fixes it into
/// no button at all.
///
/// The sentence lives here with the kind, rather than at the screen, because
/// there is exactly one right way to say each of these and both ways in (the
/// list, an attach that could not get back) want it.
#[derive(Debug)]
pub enum Rebuff {
    /// The account has never been given this device's public half. The
    /// ordinary state of every machine the first time a phone is pointed at
    /// it, and the one failure where what fixes it is held by the app.
    Key {
        at: String,
        user: String,
        fingerprint: String,
    },
    /// The machine presented a host key that is not the one written down,
    /// which is either a machine that was reinstalled or somebody in the
    /// middle. Only the person reading it can say which.
    Host {
        at: String,
        had: String,
        now: String,
    },
}

impl std::fmt::Display for Rebuff {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Key {
                at,
                user,
                fingerprint,
            } => write!(
                out,
                "{at} would not take this device's key for {user}: add it to that \
                 account's `authorized_keys` ({fingerprint})"
            ),
            Self::Host { at, had, now } => write!(
                out,
                "the host key for {at} has changed: it was {had} and is now {now}. \
                 If that machine was reinstalled, forget the old key; otherwise \
                 somebody is in the middle."
            ),
        }
    }
}

impl std::error::Error for Rebuff {}

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

/// Whether a machine that did not let this device in said so, or went.
///
/// The distinction has to be made because russh does not make it. A failure to
/// authenticate comes back as `AuthResult::Failure` whether the far end sent
/// one or the session ended under the question: `wait_recv_reply` answers a
/// closed channel with an empty `MethodSet`, which is also, exactly, what a
/// machine offering the `none` method alone sends when it says no, since RFC
/// 4252 keeps `none` out of the ways to continue and leaves it with nothing to
/// list. So neither the result nor the methods in it can be read for this.
enum Answer {
    /// The far end said something. Either it is still there, or it closed the
    /// connection itself once there was nothing left to ask it, which is a
    /// machine turning this device away rather than a connection going.
    Said,
    /// The connection broke, which is the session ending in an error rather
    /// than ending.
    Went(anyhow::Error),
}

/// How long a session that has already ended is given to say how.
///
/// It has ended, so this expires only if `is_closed` and the task disagree.
/// The fallback is [`Answer::Said`], since blaming the network is the claim
/// that needs the evidence.
const LAST_WORD: Duration = Duration::from_secs(2);

/// Ask the session how it ended, which is the one place the difference is
/// recorded.
///
/// Consuming the handle is the point rather than a cost: this is only reached
/// where there is nothing left to ask the far end, so what is left of the
/// connection is its outcome.
async fn answer(handle: Handle<Trusting>) -> Answer {
    // Still there, so it replied: a session cannot both be running and have
    // been the reason the reply was empty.
    if !handle.is_closed() {
        return Answer::Said;
    }
    match timeout(LAST_WORD, handle).await {
        Ok(Ok(())) => Answer::Said,
        // The one error that is an answer. russh raises it in exactly one
        // place, on reading a failure that named no way to continue, and it
        // sends the reply on before it does: a machine offering `none` alone
        // ends every refusal this way, so reading it as a broken connection
        // would put a network problem in front of somebody whose network is
        // fine.
        Ok(Err(error)) if names(&error, &SshError::NoAuthMethod) => Answer::Said,
        Ok(Err(error)) => Answer::Went(error),
        Err(_) => Answer::Said,
    }
}

/// Whether an error is, under whatever it was wrapped in, this one.
fn names(error: &anyhow::Error, ssh: &SshError) -> bool {
    error
        .downcast_ref::<SshError>()
        .is_some_and(|found| std::mem::discriminant(found) == std::mem::discriminant(ssh))
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
                Some(rebuff) => anyhow!(rebuff),
                None => error,
            })
            .with_context(|| format!("connecting to {}", machine.at()))?;

        // Ask before offering anything, which is what every ssh client does
        // and what this did not. The `none` method is how a client finds out
        // what the far end will take, and on a machine reached through a mesh
        // it is also the whole of the answer: the peer has already been
        // identified by the link the connection arrived over, so its ssh has
        // no `authorized_keys` anywhere in it and admits the session here.
        // Opening with a key instead, this was told no by machines that would
        // have let it straight in, and then said the key was the problem. That
        // it worked from a terminal on the same phone and not from the app is
        // the shape of the bug: `ssh` asks first.
        let offered = match handle
            .authenticate_none(&machine.user)
            .await
            .with_context(|| format!("asking {} how to log in", machine.at()))?
        {
            AuthResult::Success => return Ok(Self { handle }),
            AuthResult::Failure {
                remaining_methods, ..
            } => remaining_methods,
        };
        // A machine that will not take a key is one where no key would have
        // changed the answer, so somebody sent off to paste one has been sent
        // nowhere. What decides is on the machine.
        if !offered.contains(&MethodKind::PublicKey) {
            return Err(match answer(handle).await {
                Answer::Said => anyhow!(
                    "{} would not let {} in and never asked for a key: whatever admits \
                     people to that machine has not been told about this device",
                    machine.at(),
                    machine.user
                ),
                Answer::Went(error) => error.context(format!(
                    "the connection to {} went before it said how to log in",
                    machine.at()
                )),
            });
        }

        let key = PrivateKeyWithHashAlg::new(identity.key(), None);
        let allowed = handle
            .authenticate_publickey(&machine.user, key)
            .await
            .with_context(|| format!("authenticating as {} on {}", machine.user, machine.at()))?;
        if !allowed.success() {
            // The same question again, for the same reason: a phone loses
            // connections in the middle of things, and a session that ended
            // under the offer comes back from russh looking exactly like a
            // machine that read the key and said no. Told apart wrongly, this
            // sends somebody to another device to paste a key into a machine
            // that never looked at one, and the same screen comes back
            // afterwards.
            return Err(match answer(handle).await {
                Answer::Said => Rebuff::Key {
                    at: machine.at(),
                    user: machine.user.clone(),
                    fingerprint: identity.fingerprint(),
                }
                .into(),
                Answer::Went(error) => error.context(format!(
                    "the connection to {} went while this device's key was being offered",
                    machine.at()
                )),
            });
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

    /// Whether this connection has gone, so nothing tries to open a channel on
    /// the far end of a radio that slept.
    pub fn is_closed(&self) -> bool {
        self.handle.is_closed()
    }
}

/// The ssh connections this device is holding on to.
///
/// `manymux::ssh` gets this free from OpenSSH: `ControlMaster=auto` puts every
/// command to a host on one connection, so the second one skips the handshake.
/// There is no ssh binary here and so no control socket, which leaves the
/// sharing to be done in the process.
///
/// It is worth more here than it is there. A phone's model is a list you leave
/// a session for and come back through, and a key exchange plus a public-key
/// auth on a radio that has to wake up for them is most of the time between
/// tapping a row and seeing the session. Without this, every glance at the list
/// pays it twice.
///
/// One machine, because that is what this version reaches. A second entry is a
/// map keyed by [`Machine::at`] and nothing else about this changes.
pub struct Connections {
    /// Async, and the one lock here that is: what it protects is the thing
    /// being waited for, so two callers arriving together share one handshake
    /// rather than racing to make two. A `std` lock cannot be held across the
    /// connect, and dropping it to connect is the race.
    holding: AsyncMutex<Option<Held>>,
}

/// One machine's connection, and which machine it is to.
struct Held {
    at: String,
    connection: Arc<Connection>,
}

impl Default for Connections {
    fn default() -> Self {
        Self::none()
    }
}

impl Connections {
    /// Holding nothing yet.
    pub fn none() -> Self {
        Self {
            holding: AsyncMutex::new(None),
        }
    }

    /// The connection to `machine`, opening one if what is held is to another
    /// machine, has gone, or was never there.
    pub async fn to(
        &self,
        machine: &Machine,
        identity: &Identity,
        known: &KnownHosts,
    ) -> Result<Arc<Connection>> {
        let at = machine.at();
        let mut holding = self.holding.lock().await;
        if let Some(held) = holding.as_ref()
            && held.at == at
            && !held.connection.is_closed()
        {
            return Ok(Arc::clone(&held.connection));
        }
        let connection = Arc::new(Connection::open(machine, identity, known).await?);
        *holding = Some(Held {
            at,
            connection: Arc::clone(&connection),
        });
        Ok(connection)
    }

    /// Let go of what is held, after something on it failed.
    ///
    /// A connection that answered a moment ago and has stopped answering is
    /// not always closed: a radio that slept leaves one that looks open and
    /// never completes anything. So a failure says so here rather than waiting
    /// for `is_closed` to notice, which on that connection it never will.
    pub async fn forget(&self) {
        *self.holding.lock().await = None;
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
    refused: Arc<Mutex<Option<Rebuff>>>,
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
                *held(&self.refused) = Some(Rebuff::Host {
                    at: self.at.clone(),
                    had,
                    now: offered.fingerprint(Default::default()).to_string(),
                });
                Ok(false)
            }
        }
    }
}
