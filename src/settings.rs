//! The handful of preferences that are neither a flag nor ssh's business.
//!
//! Kept in `settings.toml` beside the host list, and read wherever the
//! preference is acted on rather than cached at startup: `mm config notify off`
//! has to reach a node that has been running for a week, and rereading a file
//! of two lines costs nothing next to spawning `notify-send`.
//!
//! An unknown key is an error rather than a line written to the file. A typo
//! that saves cleanly and changes nothing is the worst outcome available here.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::config;

/// Every key `mm config` takes, in the order it lists them.
pub const KEYS: &[&str] = &["notify", "screen"];

/// Whose screen an attached client paints on.
///
/// `alternate` takes the terminal's second screen buffer, which is what every
/// attach did before inline existed: the session gets a surface of its own and
/// detaching gives your shell's screen back untouched. `inline` paints on the
/// terminal's own screen instead, so what the session prints scrolls into the
/// terminal's scrollback and the wheel, find and select are the terminal's.
///
/// Alternate is the default because it behaves the same on every terminal.
/// Inline needs one that keeps what scrolls out of a scrolling region, since
/// the mark's row is fenced off with one.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(feature = "desktop", derive(clap::ValueEnum))]
#[serde(rename_all = "lowercase")]
pub enum Screen {
    #[default]
    Alternate,
    Inline,
}

impl Screen {
    fn word(self) -> &'static str {
        match self {
            Screen::Alternate => "alternate",
            Screen::Inline => "inline",
        }
    }
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
pub struct Settings {
    /// Whether anything may interrupt you: desktop notifications from the
    /// daemon, and the ones an attached client hands to your terminal.
    #[serde(default = "on")]
    pub notify: bool,
    /// Whose screen an attached client paints on.
    #[serde(default)]
    pub screen: Screen,
}

fn on() -> bool {
    true
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            notify: true,
            screen: Screen::default(),
        }
    }
}

impl Settings {
    /// What is on disk, or the defaults if there is nothing there yet.
    pub fn load() -> Result<Self> {
        let path = Self::path();
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }

    /// What is on disk, or the defaults if it cannot be read at all.
    ///
    /// For the places acting on a preference rather than showing it: a broken
    /// settings file must not stop a bell reaching you, and `mm config` is
    /// where it gets reported.
    pub fn or_default() -> Self {
        Self::load().unwrap_or_default()
    }

    pub fn save(&self) -> Result<()> {
        let text = toml::to_string_pretty(self).context("encoding the settings")?;
        config::write_private_file(&Self::path(), text.as_bytes())
    }

    pub fn path() -> PathBuf {
        config::config_dir().join("settings.toml")
    }

    pub fn get(&self, key: &str) -> Result<String> {
        match key {
            "notify" => Ok(word(self.notify).to_string()),
            "screen" => Ok(self.screen.word().to_string()),
            _ => bail!("no setting called `{key}`; known settings: {}", list()),
        }
    }

    pub fn set(&mut self, key: &str, value: &str) -> Result<()> {
        match key {
            "notify" => self.notify = flag(key, value)?,
            "screen" => self.screen = screen(key, value)?,
            _ => bail!("no setting called `{key}`; known settings: {}", list()),
        }
        Ok(())
    }

    /// Every setting and its value, for a bare `mm config`.
    pub fn all(&self) -> Vec<(&'static str, String)> {
        KEYS.iter()
            .map(|key| (*key, self.get(key).expect("KEYS are settings")))
            .collect()
    }
}

/// Whether notifications are wanted at all, from the environment first so a
/// single command can be silenced without touching the file.
pub fn notify_wanted() -> bool {
    if let Some(value) = std::env::var_os("MM_NOTIFY") {
        return flag("MM_NOTIFY", &value.to_string_lossy()).unwrap_or(true);
    }
    Settings::or_default().notify
}

/// Read a setting written the way a person writes one.
fn flag(key: &str, value: &str) -> Result<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "on" | "true" | "yes" | "1" => Ok(true),
        "off" | "false" | "no" | "0" => Ok(false),
        other => bail!("`{key}` is on or off, not {other:?}"),
    }
}

/// Read the screen setting the way a person writes one.
fn screen(key: &str, value: &str) -> Result<Screen> {
    match value.trim().to_ascii_lowercase().as_str() {
        "alternate" | "alt" => Ok(Screen::Alternate),
        "inline" => Ok(Screen::Inline),
        other => bail!("`{key}` is alternate or inline, not {other:?}"),
    }
}

fn word(on: bool) -> &'static str {
    if on { "on" } else { "off" }
}

fn list() -> String {
    KEYS.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_setting_is_written_the_way_people_write_one() {
        let mut settings = Settings::default();
        assert_eq!(settings.get("notify").unwrap(), "on");

        for off in ["off", "OFF", "false", "no", "0", " off "] {
            settings.set("notify", off).unwrap();
            assert_eq!(settings.get("notify").unwrap(), "off", "{off:?}");
        }
        for on in ["on", "true", "yes", "1"] {
            settings.set("notify", on).unwrap();
            assert_eq!(settings.get("notify").unwrap(), "on", "{on:?}");
        }
    }

    /// A typo that saves cleanly and changes nothing is worse than a refusal:
    /// you would go on believing the setting took.
    #[test]
    fn a_misspelled_key_or_value_is_refused() {
        let mut settings = Settings::default();
        assert!(settings.set("nofity", "off").is_err());
        assert!(settings.set("notify", "maybe").is_err());
        assert!(settings.get("nofity").is_err());
        assert_eq!(settings, Settings::default());
    }

    #[test]
    fn the_file_round_trips_and_an_empty_one_is_the_defaults() {
        let mut settings = Settings::default();
        settings.set("notify", "off").unwrap();
        let text = toml::to_string_pretty(&settings).unwrap();
        assert_eq!(toml::from_str::<Settings>(&text).unwrap(), settings);
        // A file written by a build that had other keys in it, or none.
        assert_eq!(toml::from_str::<Settings>("").unwrap(), Settings::default());
    }

    #[test]
    fn the_screen_is_named_the_way_people_name_it() {
        let mut settings = Settings::default();
        assert_eq!(settings.get("screen").unwrap(), "alternate");

        for inline in ["inline", "INLINE", " inline "] {
            settings.set("screen", inline).unwrap();
            assert_eq!(settings.get("screen").unwrap(), "inline", "{inline:?}");
        }
        settings.set("screen", "alternate").unwrap();
        assert_eq!(settings.screen, Screen::Alternate);

        assert!(settings.set("screen", "maybe").is_err());
    }

    #[test]
    fn every_key_can_be_listed() {
        let all = Settings::default().all();
        assert_eq!(
            all,
            vec![
                ("notify", "on".to_string()),
                ("screen", "alternate".to_string())
            ]
        );
    }
}
