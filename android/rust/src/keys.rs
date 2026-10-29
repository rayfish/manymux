//! What this device is, and what it knows about the machines it reaches.
//!
//! Both halves are a departure from `src/hosts.rs`, which keeps no addresses
//! and no keys anywhere on the grounds that `~/.ssh/config` and sshd already
//! hold them. A phone has neither, so it carries its own: one key it generated
//! and a note of the host key each machine presented the first time. The
//! departure is the app's and stops here: nothing about it reaches the wire
//! or the library.
//! Writers lock a separate file across each read, change and rename, so
//! concurrent opens share one identity and cannot discard remembered host keys.

use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};

use getrandom::SysRng;
use russh::keys::ssh_key::LineEnding;
use russh::keys::ssh_key::rand_core::UnwrapErr;
use russh::keys::{Algorithm, PrivateKey, PublicKey};

use crate::agent::Agent;

/// A fresh ed25519 key.
///
/// ed25519 and nothing else: it is the one algorithm every sshd worth reaching
/// has accepted for a decade, and generating it costs nothing on a phone, where
/// an RSA key of a defensible size does not.
pub fn generate() -> Result<PrivateKey> {
    PrivateKey::random(&mut UnwrapErr(SysRng), Algorithm::Ed25519)
        .context("generating an ed25519 key")
}

/// This device's key, kept in app-private storage, and whatever else it has
/// been given to sign with.
///
/// The key is the whole of it on a phone. An agent is the desktop's half: see
/// [`crate::agent`] for why it arrives here rather than being looked up where
/// it is used.
#[derive(Clone)]
pub struct Identity {
    key: Arc<PrivateKey>,
    agent: Option<Agent>,
}

impl Identity {
    /// The key at `path`, generated and written down if there is none there.
    ///
    /// A file that exists and cannot be read is an error rather than a reason
    /// to generate: a device that quietly made itself a second key would be
    /// locked out of every machine the first one had been let into, with
    /// nothing anywhere saying why.
    pub fn kept_at(path: &Path) -> Result<Self> {
        let _lock = lock_file(path)?;
        match fs::read_to_string(path) {
            Ok(text) => {
                let key = PrivateKey::from_openssh(&text)
                    .with_context(|| format!("reading the key in {}", path.display()))?;
                Ok(Self::of(key))
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {
                let mut key = generate()?;
                key.set_comment("manymux");
                write_privately(path, key.to_openssh(LineEnding::LF)?.as_bytes())
                    .with_context(|| format!("writing the key to {}", path.display()))?;
                Ok(Self::of(key))
            }
            Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
        }
    }

    fn of(key: PrivateKey) -> Self {
        Self {
            key: Arc::new(key),
            agent: None,
        }
    }

    /// The same identity, also able to sign with what `agent` is holding.
    ///
    /// Takes an `Option` because the caller's question is "is there one", and
    /// making every caller write the `if` around this would be the same line
    /// four times.
    pub fn asking(self, agent: Option<Agent>) -> Self {
        Self { agent, ..self }
    }

    pub(crate) fn agent(&self) -> Option<&Agent> {
        self.agent.as_ref()
    }

    /// The line to paste into a machine's `authorized_keys`.
    pub fn authorized_line(&self) -> String {
        self.key
            .public_key()
            .to_openssh()
            .unwrap_or_else(|_| String::new())
    }

    /// What this key is, short enough to read out loud.
    pub fn fingerprint(&self) -> String {
        self.key
            .public_key()
            .fingerprint(Default::default())
            .to_string()
    }

    pub(crate) fn key(&self) -> Arc<PrivateKey> {
        Arc::clone(&self.key)
    }
}

/// What is known about a machine's host key.
#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Never seen. Trust on first use writes it down and carries on.
    New,
    /// The key that was written down.
    Known,
    /// A different key from the one written down, which is either a machine
    /// that was reinstalled or somebody in the middle, and only the person
    /// reading it can say which.
    Changed {
        /// The fingerprint of the key that was written down.
        had: String,
    },
}

/// The host keys this device has decided to trust.
///
/// One line per machine, in `known_hosts` order: where it was reached,
/// then the key it presented.
#[derive(Clone)]
pub struct KnownHosts {
    path: PathBuf,
}

impl KnownHosts {
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// What this device makes of the key `at` is presenting.
    pub fn verdict(&self, at: &str, offered: &PublicKey) -> Result<Verdict> {
        match self.stored(at)? {
            None => Ok(Verdict::New),
            Some(stored) if &stored == offered => Ok(Verdict::Known),
            Some(stored) => Ok(Verdict::Changed {
                had: stored.fingerprint(Default::default()).to_string(),
            }),
        }
    }

    /// Check and record first use in one transaction, leaving a changed key alone.
    pub(crate) fn trust(&self, at: &str, offered: &PublicKey) -> Result<Verdict> {
        let _lock = lock_file(&self.path)?;
        let verdict = self.verdict(at, offered)?;
        if verdict == Verdict::New {
            let line = format!("{at} {}", offered.to_openssh()?);
            self.rewrite_locked(at, Some(&line))?;
        }
        Ok(verdict)
    }

    /// Write down the key `at` presented, replacing anything already there.
    pub fn remember(&self, at: &str, key: &PublicKey) -> Result<()> {
        let line = format!("{at} {}", key.to_openssh()?);
        self.rewrite(at, Some(&line))
    }

    /// Forget what was written down about `at`, which is what somebody says
    /// after deciding a changed key is a machine they reinstalled.
    pub fn forget(&self, at: &str) -> Result<()> {
        self.rewrite(at, None)
    }

    fn stored(&self, at: &str) -> Result<Option<PublicKey>> {
        let text = match fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error).with_context(|| format!("reading {}", self.path.display()));
            }
        };
        for line in text.lines() {
            let Some((where_, key)) = line.split_once(' ') else {
                continue;
            };
            if where_ != at {
                continue;
            }
            let key = PublicKey::from_openssh(key.trim())
                .with_context(|| format!("reading the key written down for {at}"))?;
            return Ok(Some(key));
        }
        Ok(None)
    }

    /// Rewrite the file with `at`'s line replaced by `line`, or dropped.
    fn rewrite(&self, at: &str, line: Option<&str>) -> Result<()> {
        let _lock = lock_file(&self.path)?;
        self.rewrite_locked(at, line)
    }

    fn rewrite_locked(&self, at: &str, line: Option<&str>) -> Result<()> {
        let mut kept: Vec<String> = match fs::read_to_string(&self.path) {
            Ok(text) => text
                .lines()
                .filter(|line| line.split_once(' ').map(|(where_, _)| where_) != Some(at))
                .map(|line| line.to_string())
                .collect(),
            Err(error) if error.kind() == ErrorKind::NotFound => Vec::new(),
            Err(error) => {
                return Err(error).with_context(|| format!("reading {}", self.path.display()));
            }
        };
        if let Some(line) = line {
            kept.push(line.trim().to_string());
        }
        let mut text = kept.join("\n");
        text.push('\n');
        write_privately(&self.path, text.as_bytes())
            .with_context(|| format!("writing {}", self.path.display()))
    }
}

fn lock_file(path: &Path) -> Result<File> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let path = path.with_extension("lock");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    file.lock()
        .with_context(|| format!("locking {}", path.display()))?;
    Ok(file)
}

/// Write a file nobody else on the device can read, all at once or not at all.
///
/// Written beside and renamed over, rather than truncated in place. Both files
/// here are ones a torn write ruins outright: a zero-byte `id_ed25519` cannot
/// be read and cannot be regenerated either, since a key file that exists and
/// will not parse is deliberately an error rather than a reason to make a
/// second key, so the app would fail to start, every time, with nothing to do
/// about it but clear its storage. `known_hosts` is rewritten whole on every
/// first contact with a machine, so the same accident drops every host key
/// this device has ever recorded and quietly puts every machine back to trust
/// on first use.
///
/// App-private storage is already per-app, so the mode is the second lock
/// rather than the only one; it is here because the same code runs in the
/// host-target example, where the directory is an ordinary one in a home.
fn write_privately(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let beside = path.with_extension("writing");
    let mut file: File = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&beside)?;
    file.write_all(bytes)?;
    // Before the rename, or a crash in between leaves the name pointing at a
    // file whose contents never reached the disk.
    file.sync_all()?;
    drop(file);
    fs::rename(&beside, path)?;
    Ok(())
}

/// Guard against a key that cannot be spelled back.
///
/// Not a round trip through the disk, which the tests do: this is the one line
/// that has to keep working for a machine to be told what to let in.
#[cfg(test)]
mod tests {
    use super::{Identity, KnownHosts, Verdict, generate};
    use std::sync::{Arc, Barrier};
    use std::thread;

    #[test]
    fn concurrent_first_contacts_cannot_replace_each_others_host_key() {
        let dir = std::env::temp_dir().join(format!("mm-first-contact-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let known = KnownHosts::at(dir.join("known_hosts"));
        let keys: Vec<_> = (0..2).map(|_| generate().unwrap()).collect();
        let ready = Arc::new(Barrier::new(keys.len()));
        let answers = thread::scope(|scope| {
            let threads: Vec<_> = keys
                .iter()
                .map(|key| {
                    let ready = Arc::clone(&ready);
                    let known = &known;
                    scope.spawn(move || {
                        ready.wait();
                        known.trust("host", key.public_key()).unwrap()
                    })
                })
                .collect();
            threads
                .into_iter()
                .map(|thread| thread.join().unwrap())
                .collect::<Vec<_>>()
        });
        assert_eq!(
            answers
                .iter()
                .filter(|answer| **answer == Verdict::New)
                .count(),
            1
        );
        assert_eq!(
            answers
                .iter()
                .filter(|answer| matches!(answer, Verdict::Changed { .. }))
                .count(),
            1
        );
        for (key, answer) in keys.iter().zip(answers) {
            let stored = known.verdict("host", key.public_key()).unwrap();
            if answer == Verdict::New {
                assert_eq!(stored, Verdict::Known);
            } else {
                assert!(matches!(stored, Verdict::Changed { .. }));
            }
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn concurrent_first_opens_share_one_device_identity() {
        let dir = std::env::temp_dir().join(format!("mm-identity-race-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("id_ed25519");
        let ready = Arc::new(Barrier::new(8));
        let identities = thread::scope(|scope| {
            let threads: Vec<_> = (0..8)
                .map(|_| {
                    let ready = Arc::clone(&ready);
                    let path = &path;
                    scope.spawn(move || {
                        ready.wait();
                        Identity::kept_at(path).map(|identity| identity.authorized_line())
                    })
                })
                .collect();
            threads
                .into_iter()
                .map(|thread| thread.join().unwrap().unwrap())
                .collect::<Vec<_>>()
        });
        let stored = Identity::kept_at(&path).unwrap().authorized_line();
        assert!(identities.iter().all(|identity| identity == &stored));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn concurrent_host_key_writes_preserve_every_machine() {
        let dir = std::env::temp_dir().join(format!("mm-host-key-race-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let known = KnownHosts::at(dir.join("known_hosts"));
        let keys: Vec<_> = (0..8).map(|_| generate().unwrap()).collect();
        let ready = Arc::new(Barrier::new(keys.len()));
        thread::scope(|scope| {
            let threads: Vec<_> = keys
                .iter()
                .enumerate()
                .map(|(index, key)| {
                    let ready = Arc::clone(&ready);
                    let known = &known;
                    scope.spawn(move || {
                        ready.wait();
                        known.remember(&format!("host-{index}"), key.public_key())
                    })
                })
                .collect();
            for thread in threads {
                thread.join().unwrap().unwrap();
            }
        });
        for (index, key) in keys.iter().enumerate() {
            assert_eq!(
                known
                    .verdict(&format!("host-{index}"), key.public_key())
                    .unwrap(),
                Verdict::Known
            );
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_key_written_down_is_the_key_read_back() {
        let dir = std::env::temp_dir().join(format!("mm-known-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let known = KnownHosts::at(dir.join("known_hosts"));
        let key = generate().unwrap();

        known.remember("host:22", key.public_key()).unwrap();

        assert_eq!(
            known.verdict("host:22", key.public_key()).unwrap(),
            Verdict::Known
        );
    }

    #[test]
    fn forgetting_one_machine_leaves_the_others_alone() {
        let dir = std::env::temp_dir().join(format!("mm-forget-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let known = KnownHosts::at(dir.join("known_hosts"));
        let one = generate().unwrap();
        let other = generate().unwrap();
        known.remember("one:22", one.public_key()).unwrap();
        known.remember("other:22", other.public_key()).unwrap();

        known.forget("one:22").unwrap();

        assert_eq!(
            known.verdict("one:22", one.public_key()).unwrap(),
            Verdict::New
        );
        assert_eq!(
            known.verdict("other:22", other.public_key()).unwrap(),
            Verdict::Known
        );
    }
}
