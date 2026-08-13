//! Driving a real terminal from an attached session.
//!
//! The terminal-specific half of the client. A mobile app skips this entirely
//! and drives [`crate::client::Attached`] directly, feeding the bytes to its
//! own terminal widget.
//!
//! The client stays deliberately dumb: raw mode, forward keystrokes, paint what
//! arrives, watch for the detach key. All the state lives on the server, which
//! is what makes detaching free.

use anyhow::Result;

use crate::client::{Attached, SessionHalves, Update};

/// Detach prefix: Ctrl-\ (0x1c).
///
/// Not tmux's Ctrl-b or screen's Ctrl-a, because you are quite likely running
/// one of those *inside* a tiles session: tiles has no panes or tabs, so
/// splitting a window is still their job, and taking their prefix would mean
/// swallowing it before it ever reached them. Ctrl-\ is SIGQUIT, which almost
/// nobody sends deliberately, and `Ctrl-\ Ctrl-\` sends a literal one through.
pub const DEFAULT_PREFIX: u8 = 0x1c;

/// The detach prefix in force, from `TILES_PREFIX` if it is set and usable.
///
/// Accepts `C-b`, `^B` or `\x02`. An unusable value is a warning rather than a
/// failure: losing the ability to detach because of a typo in an environment
/// variable would be worse than ignoring it.
pub fn prefix() -> u8 {
    let Some(text) = std::env::var_os("TILES_PREFIX") else {
        return DEFAULT_PREFIX;
    };
    let text = text.to_string_lossy();
    match parse_prefix(&text) {
        Some(byte) => byte,
        None => {
            eprintln!("tiles: TILES_PREFIX={text:?} is not a control key; using Ctrl-\\");
            DEFAULT_PREFIX
        }
    }
}

/// Parse a control key: `C-b`, `c-B`, `^b`, a bare `b`, or the raw byte.
///
/// A bare letter is read as the control key, since a printable character could
/// not serve as a prefix anyway: it would arm on every one you typed.
fn parse_prefix(text: &str) -> Option<u8> {
    let key = text
        .strip_prefix("C-")
        .or_else(|| text.strip_prefix("c-"))
        .or_else(|| text.strip_prefix('^'))
        .unwrap_or(text);

    let mut chars = key.chars();
    let key = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    // A control character given literally, `$'\x02'` style.
    if key.is_control() {
        return Some(key as u8);
    }
    // `C-b` is 0x02: the letter with the top three bits cleared. The same
    // arithmetic covers `C-\`, `C-]` and friends, which sit just past `Z`.
    let byte = u8::try_from(key).ok()?.to_ascii_uppercase();
    (0x40..0x60).contains(&byte).then_some(byte & 0x1f)
}

/// How the attach ended.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Detached,
    /// The session's process exited with this code.
    Exited(i32),
    /// The host went away.
    Disconnected,
}

/// Watches the keystroke stream for the detach sequence.
///
/// `Ctrl-\ d` detaches. `Ctrl-\ Ctrl-\` sends a literal Ctrl-\ through, so the
/// key is still usable (it is SIGQUIT) inside the session. Any other key after
/// the prefix passes both bytes through unchanged.
pub struct KeyFilter {
    prefix: u8,
    armed: bool,
}

impl Default for KeyFilter {
    fn default() -> Self {
        Self::new(prefix())
    }
}

/// What a chunk of keystrokes amounts to once the detach sequence is taken out.
#[derive(Debug, PartialEq, Eq)]
pub struct Keystrokes {
    /// The bytes to send on to the session.
    pub forward: Vec<u8>,
    /// Whether the user asked to detach.
    pub detach: bool,
}

impl KeyFilter {
    pub fn new(prefix: u8) -> Self {
        Self {
            prefix,
            armed: false,
        }
    }

    pub fn filter(&mut self, input: &[u8]) -> Keystrokes {
        let mut forward = Vec::with_capacity(input.len());
        for &b in input {
            if self.armed {
                self.armed = false;
                match b {
                    b'd' | b'D' => {
                        return Keystrokes {
                            forward,
                            detach: true,
                        };
                    }
                    b if b == self.prefix => forward.push(self.prefix),
                    other => {
                        forward.push(self.prefix);
                        forward.push(other);
                    }
                }
            } else if b == self.prefix {
                self.armed = true;
            } else {
                forward.push(b);
            }
        }
        Keystrokes {
            forward,
            detach: false,
        }
    }
}

#[cfg(feature = "desktop")]
pub use terminal::{run, terminal_size};

#[cfg(feature = "desktop")]
mod terminal {
    use std::io::IsTerminal;

    use anyhow::{Result, bail};
    use crossterm::terminal;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::signal::unix::{SignalKind, signal};

    use super::{KeyFilter, Outcome};
    use crate::client::{Attached, SessionHalves, Update};
    use crate::proto::Size;

    /// Sent before attaching.
    ///
    /// The alternate screen is the important part. A session's repaint places
    /// everything by absolute coordinates, because that is what a screen dump
    /// is, so painting it onto your shell's screen writes the session over your
    /// scrollback and leaves the cursor at the session's row 1 rather than
    /// where the session's prompt appears to be. Anything you then type lands
    /// at the top of the terminal. On a surface of its own the coordinates mean
    /// what they say, and detaching gives your shell's screen back untouched.
    ///
    /// The title is pushed for the same reason: detaching should give you back
    /// the tab name you had, not leave it named after a session you left.
    const SETUP: &str = concat!(
        "\x1b[22;2t",  // push the window title
        "\x1b[?1049h", // switch to the alternate screen
    );

    /// Terminal state a full-screen program may have left behind. Sent when we
    /// give the terminal back so a detach never leaves an invisible cursor or a
    /// stuck mouse-reporting mode.
    const RESET: &str = concat!(
        "\x1b[23;2t",                                   // pop the title pushed on attach
        "\x1b[?1047l", // leave the alternate screen (the older form, which vim uses)
        "\x1b[?1049l", // leave the alternate screen
        "\x1b[?25h",   // show the cursor
        "\x1b[0m",     // default attributes
        "\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l", // mouse reporting off
        "\x1b[?2004l", // bracketed paste off
        "\x1b[?7h",    // autowrap on
        "\r\n",
    );

    pub fn terminal_size() -> Size {
        terminal::size()
            .map(|(cols, rows)| Size::new(cols, rows))
            .unwrap_or_default()
            .sane()
    }

    /// Run the attach loop until the client detaches or the session ends.
    ///
    /// Puts the terminal in raw mode for the duration and always restores it,
    /// so even an error path hands back a usable shell.
    pub async fn run(session: Attached) -> Result<Outcome> {
        if !std::io::stdin().is_terminal() {
            bail!("attach needs a terminal on stdin");
        }

        terminal::enable_raw_mode()?;
        {
            use std::io::Write;
            let mut out = std::io::stdout();
            let _ = out.write_all(SETUP.as_bytes());
            let _ = out.flush();
        }
        let result = pump(session).await;
        let _ = terminal::disable_raw_mode();

        // Restore through the real stdout: the async handle may have buffered
        // writes we no longer own.
        use std::io::Write;
        let mut out = std::io::stdout();
        let _ = out.write_all(RESET.as_bytes());
        let _ = out.flush();

        result
    }

    async fn pump(session: Attached) -> Result<Outcome> {
        let SessionHalves {
            mut reader,
            mut writer,
        } = session.split();
        let mut stdin = tokio::io::stdin();
        let mut stdout = tokio::io::stdout();
        let mut winch = signal(SignalKind::window_change())?;
        let mut keys = KeyFilter::default();
        let mut buf = vec![0u8; 8192];

        loop {
            tokio::select! {
                n = stdin.read(&mut buf) => {
                    let n = match n {
                        Ok(0) | Err(_) => return Ok(Outcome::Detached),
                        Ok(n) => n,
                    };
                    let keystrokes = keys.filter(&buf[..n]);
                    if !keystrokes.forward.is_empty() {
                        writer.send_input(&keystrokes.forward).await?;
                    }
                    if keystrokes.detach {
                        writer.detach().await?;
                        return Ok(Outcome::Detached);
                    }
                }
                update = reader.next() => match update? {
                    Update::Output(bytes) => {
                        stdout.write_all(&bytes).await?;
                        stdout.flush().await?;
                    }
                    Update::Exited(code) => return Ok(Outcome::Exited(code)),
                    Update::Disconnected => return Ok(Outcome::Disconnected),
                },
                _ = winch.recv() => writer.resize(terminal_size()).await?,
            }
        }
    }
}

/// Everything an attached session produced before it stopped.
#[derive(Debug)]
pub struct Collected {
    pub output: Vec<u8>,
    pub outcome: Outcome,
}

/// Drive an attached session without a terminal, for tests and for clients that
/// render the output themselves.
pub async fn collect_until(
    session: Attached,
    mut stop: impl FnMut(&[u8]) -> bool,
) -> Result<Collected> {
    let SessionHalves { mut reader, .. } = session.split();
    let mut output = Vec::new();
    loop {
        let outcome = match reader.next().await? {
            Update::Output(bytes) => {
                output.extend_from_slice(&bytes);
                if stop(&output) {
                    Outcome::Detached
                } else {
                    continue;
                }
            }
            Update::Exited(code) => Outcome::Exited(code),
            Update::Disconnected => Outcome::Disconnected,
        };
        return Ok(Collected { output, outcome });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn forwarded(bytes: &[u8]) -> Keystrokes {
        Keystrokes {
            forward: bytes.to_vec(),
            detach: false,
        }
    }

    #[test]
    fn a_control_key_can_be_named_several_ways() {
        assert_eq!(parse_prefix("C-b"), Some(0x02));
        assert_eq!(parse_prefix("c-B"), Some(0x02));
        assert_eq!(parse_prefix("^b"), Some(0x02));
        assert_eq!(parse_prefix("\u{2}"), Some(0x02));
        // The default, and the other keys past `Z` that people pick.
        assert_eq!(parse_prefix("C-\\"), Some(DEFAULT_PREFIX));
        assert_eq!(parse_prefix("C-a"), Some(0x01));
        assert_eq!(parse_prefix("C-]"), Some(0x1d));
    }

    #[test]
    fn a_bare_letter_means_the_control_key() {
        // The only reading that works: a printable prefix would arm on every
        // one of those characters you typed.
        assert_eq!(parse_prefix("b"), Some(0x02));
    }

    #[test]
    fn a_prefix_that_is_not_a_key_at_all_is_refused() {
        // Refused rather than silently mangled, and the caller then warns and
        // keeps the default, since a typo here must not cost you the ability
        // to detach.
        assert_eq!(parse_prefix("C-bb"), None);
        assert_eq!(parse_prefix(""), None);
        assert_eq!(parse_prefix("C-"), None);
        assert_eq!(parse_prefix("1"), None);
    }

    #[test]
    fn the_prefix_can_be_tmuxs() {
        // `TILES_PREFIX=C-b` for muscle memory, at the price of tmux inside a
        // session no longer seeing its own prefix.
        let mut f = KeyFilter::new(0x02);
        assert_eq!(
            f.filter(b"ls\x02d"),
            Keystrokes {
                forward: b"ls".to_vec(),
                detach: true
            }
        );
        // And Ctrl-\ is then just an ordinary keystroke again.
        let mut f = KeyFilter::new(0x02);
        assert_eq!(f.filter(b"\x1c"), forwarded(b"\x1c"));
    }

    #[test]
    fn ordinary_input_passes_through() {
        let mut f = KeyFilter::default();
        assert_eq!(f.filter(b"ls -la\r"), forwarded(b"ls -la\r"));
    }

    #[test]
    fn prefix_then_d_detaches_without_forwarding_it() {
        let mut f = KeyFilter::default();
        assert_eq!(
            f.filter(b"abc\x1cd"),
            Keystrokes {
                forward: b"abc".to_vec(),
                detach: true
            }
        );
    }

    #[test]
    fn doubled_prefix_sends_one_through() {
        let mut f = KeyFilter::default();
        assert_eq!(f.filter(b"\x1c\x1c"), forwarded(&[DEFAULT_PREFIX]));
    }

    #[test]
    fn prefix_then_other_key_forwards_both() {
        let mut f = KeyFilter::default();
        assert_eq!(f.filter(b"\x1cx"), forwarded(&[DEFAULT_PREFIX, b'x']));
    }

    #[test]
    fn prefix_state_carries_across_reads() {
        // A slow typist splits the sequence across two reads.
        let mut f = KeyFilter::default();
        assert_eq!(f.filter(b"\x1c"), forwarded(b""));
        assert_eq!(
            f.filter(b"d"),
            Keystrokes {
                forward: vec![],
                detach: true
            }
        );
    }
}
