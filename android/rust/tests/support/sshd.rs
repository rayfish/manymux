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

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use russh::keys::PrivateKey;
use russh::server::{self, Auth, Handler, Msg, Server, Session};
use russh::{Channel, ChannelId};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::process::{ChildStdin, Command};

/// A server running on loopback, with a host key of the caller's choosing.
///
/// The key is the caller's because the point of half these tests is what
/// happens when it changes.
pub struct Sshd {
    pub port: u16,
}

impl Sshd {
    /// Start one, serving commands in `root`.
    pub async fn listening(root: &Path, key: PrivateKey) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let config = Arc::new(server::Config {
            keys: vec![key],
            ..Default::default()
        });
        let root = root.to_path_buf();
        tokio::spawn(async move {
            let mut machine = Machine { root };
            let _ = machine.run_on_socket(config, &listener).await;
        });

        Self { port }
    }
}

/// The far end: a home directory and a PATH, and a shell to run commands with.
struct Machine {
    root: PathBuf,
}

impl Server for Machine {
    type Handler = Shell;

    fn new_client(&mut self, _: Option<std::net::SocketAddr>) -> Shell {
        Shell {
            root: self.root.clone(),
            stdin: None,
        }
    }
}

/// One connection's worth of shell.
struct Shell {
    root: PathBuf,
    /// Held so that what the client writes reaches the command's stdin, which
    /// is the whole protocol once a rung has answered.
    stdin: Option<ChildStdin>,
}

impl Handler for Shell {
    type Error = russh::Error;

    /// Any key gets in. Which key it was is the client's business, and testing
    /// sshd's authorisation would be testing sshd.
    async fn auth_publickey(
        &mut self,
        _: &str,
        _: &russh::keys::ssh_key::PublicKey,
    ) -> Result<Auth, Self::Error> {
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
        });

        Ok(())
    }
}
