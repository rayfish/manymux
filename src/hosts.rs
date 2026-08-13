//! The machines `mm ls` should look at.
//!
//! A host is an ssh destination and nothing more: a name from your ssh config,
//! or `user@host`. manymux stores no addresses, keys or credentials, because ssh
//! already has all of that and knows more about it than we do. This list only
//! answers "which machines am I interested in", so a listing knows where to
//! look.

use std::collections::BTreeSet;
use std::path::PathBuf;

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

/// A session address: `name` on this machine, or `host/name` elsewhere.
#[derive(Debug, PartialEq, Eq)]
pub struct Target {
    pub host: Option<String>,
    pub session: String,
}

impl Target {
    pub fn parse(text: &str) -> Result<Self> {
        match text.split_once('/') {
            None => Ok(Self {
                host: None,
                session: text.to_string(),
            }),
            Some((host, session)) if !host.is_empty() && !session.is_empty() => Ok(Self {
                host: Some(host.to_string()),
                session: session.to_string(),
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
        assert_eq!(t.session, "build");
    }

    #[test]
    fn a_slash_splits_host_from_session() {
        let t = Target::parse("gpu-box/api").unwrap();
        assert_eq!(t.host.as_deref(), Some("gpu-box"));
        assert_eq!(t.session, "api");
    }

    #[test]
    fn a_half_written_target_is_an_error_not_a_guess() {
        assert!(Target::parse("gpu-box/").is_err());
        assert!(Target::parse("/api").is_err());
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
