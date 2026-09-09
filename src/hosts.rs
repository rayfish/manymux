//! The machines `mm ls` should look at.
//!
//! A host is an ssh destination and nothing more: a name from your ssh config,
//! or `user@host`. manymux stores no addresses, keys or credentials, because ssh
//! already has all of that and knows more about it than we do. This list only
//! answers "which machines am I interested in", so a listing knows where to
//! look.
//!
//! Which means two entries in it can be one machine, `box` and `me@box` being
//! two destinations that ssh lands in the same place. Nothing here can tell:
//! resolving a destination is ssh's business, and the strings say nothing. So
//! the node says instead, and [`node_id`] is what it says.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::fs::File;
use std::io::Read as _;
use std::path::PathBuf;
use std::sync::OnceLock;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::config;

/// Always accepted as a name for this machine, whatever it is really called.
/// Useful for typing, and as the way to say "here" unambiguously.
pub const LOCAL: &str = "local";

/// What this machine calls itself in a listing.
///
/// The short hostname, so every row of `mm ls` is a machine name rather than
/// one magic word among real ones. Falls back to [`LOCAL`] on a machine that
/// will not tell us.
pub fn this_machine() -> &'static str {
    static NAME: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    NAME.get_or_init(|| {
        let mut buf = [0u8; 256];
        // SAFETY: gethostname writes at most `len` bytes into a buffer we own,
        // and we only read up to the first NUL it wrote.
        let ok = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } == 0;
        let name = if ok {
            let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
            String::from_utf8_lossy(&buf[..end]).into_owned()
        } else {
            String::new()
        };
        // `dario-mbp.local` and `box.example.com` both read better short.
        let short = name.split('.').next().unwrap_or("").trim();
        if short.is_empty() {
            LOCAL.to_string()
        } else {
            short.to_string()
        }
    })
}

/// Whether `name` means this machine rather than one reached over ssh.
pub fn is_this_machine(name: &str) -> bool {
    name == LOCAL || name == this_machine()
}

/// Who is answering: an id for this node, made once and held for as long as the
/// process lives.
///
/// Two entries in the host list can be one machine. `box` and `me@box` are
/// different ssh destinations and the same node, so a listing asks both, gets
/// the same sessions back twice, and draws them under two names with nothing to
/// say they are one shell. The client cannot work this out for itself: an ssh
/// destination is opaque, and what it resolves to is ssh's business rather than
/// ours. So the node answers for it, and this is what it answers with, stamped
/// on every row of a listing ([`SessionInfo::node`]).
///
/// In memory and never written down, which is the whole of why it is correct.
/// The question it settles is "did these two answers come from the same node",
/// and two destinations reaching one node reach one *process*: nothing here has
/// to survive a restart, and a node that restarted between two halves of a
/// listing answers differently, which reads as two machines and leaves the
/// listing exactly as it was.
///
/// Written down, it would be wrong. The two obvious places to keep it are the
/// config directory and something derived from the machine, and both are shared
/// by machines that are not the same node: a fleet on an NFS home has one
/// config directory, a container fleet has one bind-mounted home, an image
/// snapshotted after `mm` ran once has one of everything, and two containers
/// both called `build` have one hostname between them. Every one of those folds
/// a running machine's sessions into another's rows and hides work somebody is
/// doing. Not knowing is always safe, and this cannot know anything it was not
/// told by the process that is answering.
///
/// `None` if the machine will not give us any randomness, which is the same
/// safe failure: nothing to compare, so nothing is called a duplicate.
///
/// [`SessionInfo::node`]: crate::proto::SessionInfo::node
pub fn node_id() -> Option<String> {
    static ID: OnceLock<Option<String>> = OnceLock::new();
    ID.get_or_init(|| match fresh_id() {
        Ok(id) => Some(id),
        Err(e) => {
            tracing::debug!("no identity for this node: {e:#}");
            None
        }
    })
    .clone()
}

/// 128 bits of urandom, in hex.
///
/// Straight from the device rather than through a crate: this is the only
/// randomness in the library, and it does not have to be unguessable, only
/// unique. The id is not a secret, and nothing is authorised by knowing it.
fn fresh_id() -> Result<String> {
    let mut bytes = [0u8; 16];
    File::open("/dev/urandom")
        .and_then(|mut urandom| urandom.read_exact(&mut bytes))
        .context("reading /dev/urandom")?;
    Ok(bytes.iter().fold(String::new(), |mut id, byte| {
        // Writing to a String cannot fail.
        let _ = write!(id, "{byte:02x}");
        id
    }))
}

#[derive(Serialize, Deserialize, Default, Debug)]
pub struct Hosts {
    /// ssh destinations. A `BTreeSet` so the file stays in a stable order and
    /// adding the same host twice is not an error.
    #[serde(default)]
    hosts: BTreeSet<String>,
}

impl Hosts {
    pub fn load() -> Result<Self> {
        let path = Self::path();
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn save(&self) -> Result<()> {
        let text = toml::to_string_pretty(self).context("encoding the host list")?;
        config::write_private_file(&Self::path(), text.as_bytes())
    }

    pub fn path() -> PathBuf {
        config::config_dir().join("hosts.toml")
    }

    pub fn add(&mut self, host: &str) -> Result<()> {
        let host = host.trim();
        if is_this_machine(host) {
            bail!("`{host}` names this machine, which is always listed");
        }
        // `/` separates host from session everywhere else, and a blank name
        // would silently become an ssh invocation with no destination.
        if host.is_empty() || host.contains('/') || host.contains(char::is_whitespace) {
            bail!("a host must be an ssh destination, with no spaces or `/`");
        }
        self.hosts.insert(host.to_string());
        Ok(())
    }

    pub fn remove(&mut self, host: &str) -> bool {
        self.hosts.remove(host)
    }

    pub fn has(&self, host: &str) -> bool {
        self.hosts.contains(host)
    }

    pub fn names(&self) -> Vec<String> {
        self.hosts.iter().cloned().collect()
    }

    pub fn is_empty(&self) -> bool {
        self.hosts.is_empty()
    }
}

/// One half of a target: an ordinary name, or a group marked with `@`.
///
/// A sigil rather than trying a name as a session and then as a group.
/// `gpu-box/pi` cannot say which `pi` is, and resolving it by trying one and
/// then the other means that making a group named after a session you already
/// have silently changes where a command you have typed for weeks goes. This
/// file has ruled against that kind of guess twice already: a bare word is only
/// a machine if it is one you have added, and a name found on two machines is
/// an error naming both rather than a pick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Named {
    Session(String),
    Group(String),
}

impl Named {
    fn parse(text: &str) -> Result<Self> {
        match text.strip_prefix('@') {
            Some("") => bail!("`@` needs a group name after it"),
            Some(group) => Ok(Named::Group(group.to_string())),
            None => Ok(Named::Session(text.to_string())),
        }
    }

    /// The name, whichever kind it is.
    pub fn name(&self) -> &str {
        match self {
            Named::Session(name) | Named::Group(name) => name,
        }
    }

    pub fn group(&self) -> Option<&str> {
        match self {
            Named::Group(name) => Some(name),
            Named::Session(_) => None,
        }
    }
}

/// A session address: `name` on this machine, `host/name` elsewhere, and either
/// half spelled `@group` to mean a group instead.
#[derive(Debug, PartialEq, Eq)]
pub struct Target {
    pub host: Option<Named>,
    pub session: Named,
}

impl Target {
    pub fn parse(text: &str) -> Result<Self> {
        match text.split_once('/') {
            None => Ok(Self {
                host: None,
                session: Named::parse(text)?,
            }),
            Some((host, session)) if !host.is_empty() && !session.is_empty() => Ok(Self {
                host: Some(Named::parse(host)?),
                session: Named::parse(session)?,
            }),
            Some(_) => bail!("expected `session` or `host/session`, got {text:?}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_name_is_a_local_session() {
        let t = Target::parse("build").unwrap();
        assert_eq!(t.host, None);
        assert_eq!(t.session, Named::Session("build".into()));
    }

    #[test]
    fn a_slash_splits_host_from_session() {
        let t = Target::parse("gpu-box/api").unwrap();
        assert_eq!(t.host, Some(Named::Session("gpu-box".into())));
        assert_eq!(t.session, Named::Session("api".into()));
    }

    #[test]
    fn a_half_written_target_is_an_error_not_a_guess() {
        assert!(Target::parse("gpu-box/").is_err());
        assert!(Target::parse("/api").is_err());
    }

    #[test]
    fn an_at_sign_marks_a_group_rather_than_a_session() {
        let t = Target::parse("@pi").unwrap();
        assert_eq!(t.host, None);
        assert_eq!(t.session, Named::Group("pi".into()));
    }

    #[test]
    fn a_group_can_be_narrowed_to_a_session_in_it() {
        let t = Target::parse("@pi/build").unwrap();
        assert_eq!(t.host, Some(Named::Group("pi".into())));
        assert_eq!(t.session, Named::Session("build".into()));
    }

    #[test]
    fn a_group_can_be_narrowed_to_a_machine() {
        let t = Target::parse("gpu-box/@pi").unwrap();
        assert_eq!(t.host, Some(Named::Session("gpu-box".into())));
        assert_eq!(t.session, Named::Group("pi".into()));
    }

    /// The whole point of the sigil: nothing about the existing spellings
    /// moves, so a command you have typed for weeks keeps going where it went.
    #[test]
    fn every_spelling_without_an_at_sign_means_what_it_always_did() {
        let bare = Target::parse("build").unwrap();
        assert_eq!(bare.host, None);
        assert_eq!(bare.session, Named::Session("build".into()));
        let qualified = Target::parse("gpu-box/api").unwrap();
        assert_eq!(qualified.host, Some(Named::Session("gpu-box".into())));
        assert_eq!(qualified.session, Named::Session("api".into()));
    }

    #[test]
    fn a_bare_at_sign_is_an_error_not_an_empty_group() {
        assert!(Target::parse("@").is_err());
        assert!(Target::parse("@pi/").is_err());
        assert!(Target::parse("@/build").is_err());
    }

    #[test]
    fn a_host_is_an_ssh_destination() {
        let mut hosts = Hosts::default();
        hosts.add("gpu-box").unwrap();
        // ssh understands this, so manymux does too, with no parsing of its own.
        hosts.add("dario@gpu-box").unwrap();
        assert!(hosts.has("dario@gpu-box"));

        assert!(hosts.add("local").is_err(), "`local` is reserved");
        assert!(hosts.add(this_machine()).is_err());
        assert!(hosts.add("a/b").is_err(), "`/` separates host from session");
        assert!(hosts.add("two words").is_err());
        assert!(hosts.add("").is_err());
    }

    #[test]
    fn the_host_file_round_trips() {
        let mut hosts = Hosts::default();
        hosts.add("box").unwrap();
        let text = toml::to_string_pretty(&hosts).unwrap();
        let back: Hosts = toml::from_str(&text).unwrap();
        assert!(back.has("box"));
    }

    #[test]
    fn this_machine_has_a_short_usable_name() {
        let name = this_machine();
        assert!(!name.is_empty());
        assert!(
            !name.contains('.'),
            "a listing wants the short name: {name}"
        );
        assert!(!name.contains('/'), "it has to be safe in `host/session`");
        assert!(is_this_machine(name));
        assert!(is_this_machine(LOCAL));
        assert!(!is_this_machine("gpu-box"));
    }
}
