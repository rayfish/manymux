//! A real ssh server, in this process, for the tests to reach.
//!
//! `tests/remote.rs` in the crate above stands ssh in with a shell script,
//! because what it is testing sits above the transport. Here the transport *is*
//! what is under test, so nothing may stand in for it: the key exchange, the
//! public-key auth, the channel and the exit status all have to be the real
//! ones. russh ships the server half, so the far end is a genuine sshd without
//! anything having to be installed or a port having to be opened.
//!
//! It listens on loopback, on a port the kernel picks.

// Each test binary uses part of this, never all of it.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use russh::keys::PrivateKey;
use russh::server::{self, Auth, Handler, Msg, Server, Session};
use russh::{Channel, ChannelId, Disconnect};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::process::{ChildStdin, Command};

/// A server running on loopback, with a host key of the caller's choosing.
///
/// The key is the caller's because the point of half these tests is what
/// happens when it changes.
pub struct Sshd {
    pub port: u16,
    /// How many TCP connections have been accepted, which is the only place a
    /// test can see the difference between a second channel and a second
    /// handshake. They cost wildly different amounts on a phone and look
    /// identical from above.
    accepted: Arc<AtomicUsize>,
}

impl Sshd {
    /// How many separate ssh connections the client has made.
    pub fn connections(&self) -> usize {
        self.accepted.load(Ordering::Relaxed)
    }
}

/// How long the machine keeps answering, and how it stops.
#[derive(Clone, Copy)]
pub struct Serving {
    /// Connections after this many are accepted and never spoken to.
    pub until: usize,
    /// How many commands a connection serves before it is disconnected.
    ///
    /// Counted per connection rather than switched on, because a command
    /// ending and a connection going are different machines and the client
    /// tells them apart: a command that exits is a session that ended on a
    /// machine still sitting there, and the connection is worth keeping, since
    /// the next attach rides it and skips a handshake. Only the connection
    /// going puts the client back to making one.
    pub commands: usize,
}

impl Serving {
    pub fn forever() -> Self {
        Self {
            until: usize::MAX,
            commands: usize::MAX,
        }
    }

    /// One connection that answers a listing and an attach and then goes, with
    /// every connection after it accepted and never spoken to.
    ///
    /// The shape of a phone whose radio slept in the middle of a session: what
    /// it was on is gone, and what it can reach now answers TCP and nothing
    /// else.
    pub fn going_mid_session() -> Self {
        Self {
            until: 1,
            commands: 2,
        }
    }
}

/// Whether the far end has been given this device's key.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Welcome {
    /// Any key gets in. Which key it was is the client's business, and testing
    /// sshd's authorisation would be testing sshd.
    AnyKey,
    /// None does, which is what an account nobody has pasted this device's
    /// public half into looks like from here. It is the ordinary state of
    /// every machine the first time a phone is pointed at it, so it is worth a
    /// server of its own rather than being read off a connection that ended.
    NoKey,
}

impl Sshd {
    /// Start one, serving commands in `root` with `extra` in their environment.
    pub async fn listening(root: &Path, key: PrivateKey, extra: &[(&str, String)]) -> Self {
        Self::listening_until(root, key, extra, Serving::forever()).await
    }

    /// One that takes no key at all, however good the connection to it is.
    pub async fn refusing(root: &Path, key: PrivateKey) -> Self {
        Self::serving(root, key, &[], Serving::forever(), Welcome::NoKey).await
    }

    /// The same, but the connections after `serving.until` are accepted and
    /// never spoken to.
    ///
    /// A TCP that connects to something which never sends a banner is what a
    /// captive portal, a middlebox or a wedged sshd looks like from here, and
    /// it is the shape a client with no deadline on its attempt hangs on
    /// forever.
    pub async fn listening_until(
        root: &Path,
        key: PrivateKey,
        extra: &[(&str, String)],
        serving: Serving,
    ) -> Self {
        Self::serving(root, key, extra, serving, Welcome::AnyKey).await
    }

    async fn serving(
        root: &Path,
        key: PrivateKey,
        extra: &[(&str, String)],
        serving: Serving,
        welcome: Welcome,
    ) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let config = Arc::new(server::Config {
            keys: vec![key],
            ..Default::default()
        });
        let root = root.to_path_buf();
        let extra: Vec<(String, String)> = extra
            .iter()
            .map(|(name, value)| ((*name).to_string(), value.clone()))
            .collect();
        let accepted = Arc::new(AtomicUsize::new(0));
        let counting = Arc::clone(&accepted);
        tokio::spawn(async move {
            let mut machine = Machine {
                root,
                extra,
                commands: serving.commands,
                welcome,
            };
            let mut taken = 0usize;
            while let Ok((socket, from)) = listener.accept().await {
                taken += 1;
                counting.store(taken, Ordering::Relaxed);
                if taken > serving.until {
                    // Held open and never spoken to. Dropping it would be a
                    // refusal, which is a different thing entirely: a refusal
                    // fails at once and this is what never finishes.
                    tokio::spawn(async move {
                        let _holding = socket;
                        std::future::pending::<()>().await;
                    });
                    continue;
                }
                let handler = machine.new_client(Some(from));
                let config = Arc::clone(&config);
                tokio::spawn(async move {
                    if let Ok(session) = server::run_stream(config, socket, handler).await {
                        let _ = session.await;
                    }
                });
            }
        });

        Self { port, accepted }
    }
}

/// The far end: a home directory and a PATH, and a shell to run commands with.
struct Machine {
    root: PathBuf,
    /// How many commands each connection serves before it is disconnected.
    commands: usize,
    /// What the tests need in the environment of whatever is run. The far end
    /// of an ssh is reached through a shell, so there is nowhere else to put
    /// it, and setting it in this process would leak into every test running
    /// beside this one.
    extra: Vec<(String, String)>,
    welcome: Welcome,
}

impl Server for Machine {
    type Handler = Shell;

    fn new_client(&mut self, _: Option<std::net::SocketAddr>) -> Shell {
        Shell {
            root: self.root.clone(),
            extra: self.extra.clone(),
            commands: self.commands,
            welcome: self.welcome,
            served: 0,
            stdin: None,
        }
    }
}

/// One connection's worth of shell.
struct Shell {
    root: PathBuf,
    extra: Vec<(String, String)>,
    commands: usize,
    welcome: Welcome,
    /// How many commands this connection has been asked for.
    served: usize,
    /// Held so that what the client writes reaches the command's stdin, which
    /// is the whole protocol once a rung has answered.
    stdin: Option<ChildStdin>,
}

impl Handler for Shell {
    type Error = russh::Error;

    /// Whatever this machine was started to say. Which key it was is the
    /// client's business, and testing sshd's authorisation would be testing
    /// sshd.
    async fn auth_publickey(
        &mut self,
        _: &str,
        _: &russh::keys::ssh_key::PublicKey,
    ) -> Result<Auth, Self::Error> {
        if self.welcome == Welcome::NoKey {
            return Ok(Auth::Reject {
                proceed_with_methods: None,
                partial_success: false,
            });
        }
        Ok(Auth::Accept)
    }

    async fn channel_open_session(
        &mut self,
        _: Channel<Msg>,
        reply: server::ChannelOpenHandle,
        _: &mut Session,
    ) -> Result<(), Self::Error> {
        reply.accept().await;
        Ok(())
    }

    async fn data(
        &mut self,
        _: ChannelId,
        data: &[u8],
        _: &mut Session,
    ) -> Result<(), Self::Error> {
        if let Some(stdin) = self.stdin.as_mut() {
            let _ = stdin.write_all(data).await;
            let _ = stdin.flush().await;
        }
        Ok(())
    }

    async fn exec_request(
        &mut self,
        channel: ChannelId,
        command: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let command = String::from_utf8_lossy(command).to_string();
        let mut child = Command::new("/bin/sh")
            .arg("-c")
            .arg(&command)
            .env("PATH", self.root.join("bin"))
            .env("HOME", self.root.join("home"))
            .envs(
                self.extra
                    .iter()
                    .map(|(name, value)| (name.as_str(), value.as_str())),
            )
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();

        self.stdin = child.stdin.take();
        let mut out = child.stdout.take().unwrap();
        let mut err = child.stderr.take().unwrap();
        let handle = session.handle();
        self.served += 1;
        let going = self.served >= self.commands;

        session.channel_success(channel)?;

        tokio::spawn(async move {
            let complaining = {
                let handle = handle.clone();
                tokio::spawn(async move {
                    let mut buffer = [0u8; 4096];
                    while let Ok(read) = err.read(&mut buffer).await {
                        if read == 0 {
                            break;
                        }
                        let _ = handle
                            .extended_data(channel, 1, buffer[..read].to_vec())
                            .await;
                    }
                })
            };

            let mut buffer = [0u8; 4096];
            while let Ok(read) = out.read(&mut buffer).await {
                if read == 0 {
                    break;
                }
                if handle.data(channel, buffer[..read].to_vec()).await.is_err() {
                    break;
                }
            }
            let _ = complaining.await;

            let status = child.wait().await.ok().and_then(|status| status.code());
            // The eof goes out first and the status after it, which is legal and
            // is the ordering that tells a careful client from a lucky one: a
            // client that stops reading the channel when the output ends never
            // sees the status, and 127 is the only sign a machine has no `mm`
            // on it. sshd is entitled to this order, so the tests use it.
            let _ = handle.eof(channel).await;
            if let Some(status) = status {
                let _ = handle.exit_status_request(channel, status as u32).await;
            }
            let _ = handle.close(channel).await;
            // A machine that went, rather than a command that finished. The
            // client keeps the connection across the second and only the first
            // puts it back to making one.
            if going {
                let _ = handle
                    .disconnect(Disconnect::ByApplication, String::new(), String::new())
                    .await;
            }
        });

        Ok(())
    }
}
