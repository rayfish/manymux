//! State shared by the node that replays a program's keyboard protocol and
//! the client that keeps late key releases out of the program that follows.
//!
//! Kitty's flags and xterm's `modifyOtherKeys` change how the terminal spells
//! a keystroke. Reattaching has to restore them, and seeing the release-event
//! flag turn off tells the client that later releases belong to the program
//! that just stopped reading.

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Keyboard {
    /// Flags set without pushing, with `CSI = flags ; mode u`.
    base: u16,
    /// Flags pushed with `CSI > flags u`, outermost first.
    stack: Vec<u16>,
    /// The level asked for with xterm's `CSI > 4 ; Pv m`.
    #[cfg(feature = "desktop")]
    modify_other_keys: Option<u16>,
}

/// How deep kitty's stack goes. A program that pushes past it loses the oldest
/// entry rather than growing memory on an untrusted output stream.
const KEYBOARD_STACK: usize = 16;

impl Keyboard {
    fn push(&mut self, flags: u16) {
        if self.stack.len() == KEYBOARD_STACK {
            self.stack.remove(0);
        }
        self.stack.push(flags);
    }

    fn pop(&mut self, count: usize) {
        self.stack.truncate(self.stack.len().saturating_sub(count));
    }

    fn set(&mut self, flags: u16, mode: u16) {
        let current = self.stack.last_mut().unwrap_or(&mut self.base);
        *current = match mode {
            2 => *current | flags,
            3 => *current & !flags,
            _ => flags,
        };
    }

    pub(crate) fn note(&mut self, params: &[u8]) {
        let Some((&lead, rest)) = params.split_first() else {
            return;
        };
        let Ok(text) = std::str::from_utf8(rest) else {
            return;
        };
        let mut fields = text.split(';');
        let first = fields.next().unwrap_or_default().parse::<u16>();
        match lead {
            b'>' => self.push(first.unwrap_or(0)),
            b'<' => self.pop(first.unwrap_or(1).into()),
            b'=' => {
                if let Ok(flags) = first {
                    let mode = fields.next().and_then(|m| m.parse().ok()).unwrap_or(1);
                    self.set(flags, mode);
                }
            }
            _ => {}
        }
    }

    pub(crate) fn reports_releases(&self) -> bool {
        const REPORT_EVENT_TYPES: u16 = 2;
        self.stack.last().copied().unwrap_or(self.base) & REPORT_EVENT_TYPES != 0
    }

    #[cfg(feature = "desktop")]
    pub(crate) fn set_modify_other_keys(&mut self, level: Option<u16>) {
        self.modify_other_keys = level;
    }

    #[cfg(feature = "desktop")]
    pub(crate) fn replay(&self, out: &mut String) {
        use std::fmt::Write as _;

        if self.base != 0 {
            let _ = write!(out, "\x1b[={};1u", self.base);
        }
        for flags in &self.stack {
            let _ = write!(out, "\x1b[>{flags}u");
        }
        if let Some(level) = self.modify_other_keys.filter(|level| *level != 0) {
            let _ = write!(out, "\x1b[>4;{level}m");
        }
    }
}
