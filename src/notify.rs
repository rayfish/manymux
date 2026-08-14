//! Turning session events into notifications, and deciding which ones deserve
//! to interrupt someone.
//!
//! A bell from a session nobody is watching is the whole reason the daemon
//! exists: your build finished, or Claude wants a decision, and you are in a
//! different window on a different machine.
//!
//! There are two ways out of here, and they answer different questions. The
//! daemon has no terminal, so it goes through the platform's own tool
//! (`osascript`, `notify-send`), which keeps this dependency-free and avoids
//! needing a D-Bus connection just to run a session server. An attached client
//! does have one, so it hands the notification to the terminal it is sitting in
//! with [`escape`] and lets the emulator raise it. That second path is the only
//! one that works across ssh: `notify-send` on a machine you are logged into
//! draws on a desktop nobody is looking at, while an escape sequence comes back
//! down the same connection you are typing into.
//!
//! [`worth_interrupting`] is shared by both, so a bell means the same thing
//! wherever it is heard.

use std::collections::HashMap;
use std::io::IsTerminal;
use std::process::Stdio;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tokio::process::Command;
use tracing::{debug, info};

use crate::proto::{EventKind, SessionEvent};
use crate::settings;

/// The longest notification handed to a terminal. Titles come from the program
/// running in the session, and one that sets a title the size of a file is not
/// going to be read on a notification anyway.
const MAX_TEXT: usize = 256;

/// Minimum gap between notifications for the same session. A program in a loop
/// ringing the bell should interrupt you once, not fifty times.
const COOLDOWN: Duration = Duration::from_secs(30);

/// What a notification should say.
pub struct Notification {
    pub title: String,
    pub body: String,
}

/// How often a session is allowed to interrupt you, whichever way its
/// notifications go out.
#[derive(Default)]
pub struct Cooldown {
    /// Last notification per `host/session`.
    last: Mutex<HashMap<String, Instant>>,
}

impl Cooldown {
    /// Whether this session may notify again yet, counting the answer as one
    /// if it is yes.
    pub fn allow(&self, key: &str) -> bool {
        let mut last = self.last.lock().unwrap();
        let now = Instant::now();
        match last.get(key) {
            Some(previous) if now.duration_since(*previous) < COOLDOWN => false,
            _ => {
                last.insert(key.to_string(), now);
                true
            }
        }
    }
}

pub struct Notifier {
    cooldown: Cooldown,
    /// Set when notifications are switched off, so the daemon is still useful
    /// as a connection holder on a machine with no desktop.
    enabled: bool,
}

impl Notifier {
    pub fn new(enabled: bool) -> Self {
        Self {
            cooldown: Cooldown::default(),
            enabled,
        }
    }

    /// Handle one event from a host.
    pub async fn handle(&self, host: &str, event: &SessionEvent) {
        let Some(notification) = worth_interrupting(host, event) else {
            return;
        };
        if !for_desktop(event) {
            debug!(host = %host, "leaving this one to the terminal attached there");
            return;
        }
        let key = format!("{host}/{}", event.session);
        if !self.cooldown.allow(&key) {
            debug!(session = %key, "notification suppressed by cooldown");
            return;
        }
        info!(session = %key, "notifying: {}", notification.body);
        // Read now rather than at startup: a node runs for weeks, and
        // `mm config notify off` is typed by someone who wants it quiet now.
        if self.enabled && settings::notify_wanted() {
            deliver(&notification).await;
        }
    }
}

/// Decide whether an event deserves to interrupt someone, and what to say.
///
/// Kept as a free function so the policy is testable without a desktop.
pub fn worth_interrupting(host: &str, event: &SessionEvent) -> Option<Notification> {
    // Somebody is looking at this session already. Whatever it wants, they can
    // see it.
    if event.attached > 0 {
        return None;
    }

    let where_ = format!("{host}/{}", event.session);
    match &event.kind {
        EventKind::Bell => Some(Notification {
            title: where_,
            body: event.title.clone(),
        }),
        // The program said exactly what it wanted to say; don't second-guess it.
        EventKind::Notify { title, body } => Some(Notification {
            title: if title.is_empty() {
                where_
            } else {
                format!("{where_}: {title}")
            },
            body: body.clone(),
        }),
        // A session ending is worth knowing about when it failed; a clean exit
        // is usually just you closing a shell.
        EventKind::Exited(code) if *code != 0 => Some(Notification {
            title: where_,
            body: format!("exited with status {code}"),
        }),
        EventKind::Exited(_) | EventKind::TitleChanged(_) | EventKind::Started => None,
    }
}

/// Whether a desktop notifier should raise this one, or leave it to whoever is
/// sitting attached on the machine it came from.
///
/// Somebody attached there has their terminal told directly, which is both
/// closer to them and the only way that works when the machine is one they ssh
/// into. One bell interrupting twice is worse than either way of hearing it.
pub fn for_desktop(event: &SessionEvent) -> bool {
    event.host_attached == 0
}

/// What the client's own status row says about an event, where the host is
/// already named on the row and the session is not.
pub fn summary(session: &str, notification: &Notification) -> String {
    let session = printable(session);
    match printable(&notification.body) {
        body if body.is_empty() => session,
        body => format!("{session}: {body}"),
    }
}

/// The notification as a sequence to write to a terminal: OSC 9, which iTerm2,
/// kitty, ghostty, WezTerm, foot, VS Code and Windows Terminal turn into a
/// desktop notification, and which everything else parses and throws away.
///
/// The text is scrubbed of control characters first. Both halves of it were
/// chosen by whatever is running in the session, and a title holding a BEL
/// would end the sequence early and leave the rest of itself being read as
/// commands by the terminal of whoever attached.
pub fn escape(notification: &Notification) -> String {
    let text = if notification.body.is_empty() {
        notification.title.clone()
    } else {
        format!("{}: {}", notification.title, notification.body)
    };
    format!("\x1b]9;{}\x07", printable(&text))
}

/// `text` with anything a terminal would act on taken out, and cut to a length
/// a notification can show.
fn printable(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(MAX_TEXT)
        .collect::<String>()
        .trim()
        .to_string()
}

/// Whether an attached client should hand its notifications to the terminal.
///
/// Nothing here asks which emulator it is. One that does not implement OSC 9
/// consumes the sequence and prints nothing, so an allowlist of names would
/// only mean silence on whichever terminal shipped after it was written.
pub fn to_terminal() -> bool {
    settings::notify_wanted()
        && std::io::stdout().is_terminal()
        && std::env::var_os("TERM").is_none_or(|term| term != "dumb")
}

#[cfg(target_os = "macos")]
async fn deliver(notification: &Notification) {
    // AppleScript string literals take double quotes and backslashes literally,
    // so both have to be escaped or a title could end the string early.
    let escape = |s: &str| s.replace('\\', r"\\").replace('"', "\\\"");
    let script = format!(
        r#"display notification "{}" with title "{}""#,
        escape(&notification.body),
        escape(&notification.title),
    );
    run(Command::new("osascript").arg("-e").arg(script)).await;
}

#[cfg(not(target_os = "macos"))]
async fn deliver(notification: &Notification) {
    run(Command::new("notify-send")
        .arg("--app-name=manymux")
        .arg(&notification.title)
        .arg(&notification.body))
    .await;
}

async fn run(command: &mut Command) {
    let result = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
    if let Err(e) = result {
        debug!("could not send a notification: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(kind: EventKind, attached: usize) -> SessionEvent {
        SessionEvent {
            session: "api".into(),
            title: "fixing the parser".into(),
            kind,
            attached,
            host_attached: attached,
        }
    }

    #[test]
    fn a_bell_nobody_is_watching_interrupts_you() {
        let notification = worth_interrupting("gpu-box", &event(EventKind::Bell, 0)).unwrap();
        assert_eq!(notification.title, "gpu-box/api");
        assert_eq!(notification.body, "fixing the parser");
    }

    #[test]
    fn a_bell_in_a_session_you_are_watching_does_not() {
        assert!(worth_interrupting("gpu-box", &event(EventKind::Bell, 1)).is_none());
    }

    #[test]
    fn a_programs_own_notification_is_used_verbatim() {
        let kind = EventKind::Notify {
            title: "Claude".into(),
            body: "needs a decision".into(),
        };
        let notification = worth_interrupting("gpu-box", &event(kind, 0)).unwrap();
        assert_eq!(notification.title, "gpu-box/api: Claude");
        assert_eq!(notification.body, "needs a decision");
    }

    #[test]
    fn only_a_failed_exit_is_worth_saying() {
        assert!(worth_interrupting("box", &event(EventKind::Exited(0), 0)).is_none());
        let notification = worth_interrupting("box", &event(EventKind::Exited(2), 0)).unwrap();
        assert_eq!(notification.body, "exited with status 2");
    }

    #[test]
    fn noise_is_not_a_notification() {
        assert!(worth_interrupting("box", &event(EventKind::Started, 0)).is_none());
        assert!(
            worth_interrupting("box", &event(EventKind::TitleChanged("x".into()), 0)).is_none()
        );
    }

    #[test]
    fn a_notification_goes_to_a_terminal_as_one_osc_9() {
        let notification = worth_interrupting("gpu-box", &event(EventKind::Bell, 0)).unwrap();
        assert_eq!(
            escape(&notification),
            "\x1b]9;gpu-box/api: fixing the parser\x07"
        );
    }

    /// The title is whatever the program in the session set, and it is written
    /// back into the terminal of whoever attached. A BEL in it would end the
    /// sequence early and leave the rest to be read as commands.
    #[test]
    fn a_title_cannot_smuggle_a_sequence_into_the_terminal_it_is_shown_on() {
        let notification = Notification {
            title: "box/api".into(),
            body: "done\x07\x1b]0;pwned\x07".into(),
        };
        let escaped = escape(&notification);
        assert_eq!(escaped, "\x1b]9;box/api: done  ]0;pwned\x07");
        assert_eq!(
            escaped.matches('\x07').count(),
            1,
            "one sequence: {escaped:?}"
        );
        assert!(!escaped[2..].contains('\x1b'));
    }

    #[test]
    fn a_notification_with_nothing_to_say_is_just_the_session() {
        let notification = Notification {
            title: "box/api".into(),
            body: String::new(),
        };
        assert_eq!(escape(&notification), "\x1b]9;box/api\x07");
    }

    #[test]
    fn a_title_long_enough_to_be_a_denial_of_service_is_cut() {
        let notification = Notification {
            title: "box/api".into(),
            body: "x".repeat(10_000),
        };
        assert!(escape(&notification).chars().count() <= MAX_TEXT + 5);
    }

    #[test]
    fn the_cooldown_stops_a_session_ringing_in_a_loop() {
        let cooldown = Cooldown::default();
        assert!(cooldown.allow("gpu-box/api"));
        assert!(!cooldown.allow("gpu-box/api"));
        // A different session is unaffected.
        assert!(cooldown.allow("gpu-box/build"));
    }

    /// Two notifications for one bell is what makes people turn them off: the
    /// terminal of whoever is attached there gets it, and the desktop notifier
    /// keeps out of the way.
    #[test]
    fn a_desktop_leaves_a_machine_somebody_is_sitting_at_to_its_own_terminal() {
        let mut event = event(EventKind::Bell, 0);
        assert!(for_desktop(&event), "nobody there: the desktop says it");

        event.host_attached = 1;
        assert!(!for_desktop(&event));
        // Still worth saying, just not by this route: the terminal attached
        // over there is the one saying it.
        assert!(worth_interrupting("gpu-box", &event).is_some());
    }

    #[test]
    fn the_status_row_names_the_session_rather_than_the_host_it_is_already_on() {
        let notification = worth_interrupting("gpu-box", &event(EventKind::Bell, 0)).unwrap();
        assert_eq!(summary("build", &notification), "build: fixing the parser");

        let bare = Notification {
            title: "gpu-box/build".into(),
            body: String::new(),
        };
        assert_eq!(summary("build", &bare), "build");
    }
}
