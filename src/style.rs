//! Minimal, dependency-free ANSI styling.
//!
//! Colour only when stdout is a terminal and `NO_COLOR` is unset (the
//! <https://no-color.org> convention). `CLICOLOR_FORCE` overrides the terminal
//! check, so piped output can still be coloured when asked for.

use std::io::IsTerminal;
use std::sync::OnceLock;

fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        if std::env::var_os("NO_COLOR").is_some() {
            return false;
        }
        if let Some(force) = std::env::var_os("CLICOLOR_FORCE")
            && force != "0"
        {
            return true;
        }
        std::io::stdout().is_terminal()
    })
}

/// Whether anything written will be coloured at all.
///
/// For a drawing with more than one way of saying the same thing: the popup's
/// cursor is a band where there are colours to sit it on, and reverse video
/// where there are none.
pub fn coloured() -> bool {
    enabled()
}

fn paint(code: &str, text: &str) -> String {
    if enabled() {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

/// Live, attached, healthy.
pub fn green(text: &str) -> String {
    paint("38;5;42", text)
}

/// Wants your attention.
pub fn amber(text: &str) -> String {
    paint("38;5;221", text)
}

/// Unreachable, failed.
pub fn red(text: &str) -> String {
    paint("38;5;203", text)
}

/// Column headings and secondary labels.
pub fn label(text: &str) -> String {
    paint("38;5;245", text)
}

/// Easy-to-ignore text: hints, and values that are merely background.
pub fn faint(text: &str) -> String {
    paint("38;5;240", text)
}

/// The thing you are most likely to read or copy.
pub fn bold(text: &str) -> String {
    paint("1;38;5;255", text)
}

/// Ordinary readable text, brighter than the surrounding chrome.
pub fn value(text: &str) -> String {
    paint("38;5;252", text)
}

/// The machine part of a target, kept quieter than the session name.
pub fn host(text: &str) -> String {
    paint("38;5;110", text)
}

/// Someone is attached and watching.
pub fn dot_attached() -> String {
    green("●")
}

/// Nobody is watching this one.
pub fn dot_idle() -> String {
    faint("○")
}

/// Whether styling is on, for callers deciding whether to draw anything fancy.
pub fn is_enabled() -> bool {
    enabled()
}
