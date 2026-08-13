//! Turning session events into desktop notifications.
//!
//! A bell from a session nobody is watching is the whole reason the daemon
//! exists: your build finished, or Claude wants a decision, and you are in a
//! different window on a different machine.
//!
//! Notifications go out through the platform's own tool (`osascript`,
//! `notify-send`) rather than a library, which keeps this dependency-free and
//! avoids needing a D-Bus connection just to run a session server.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tokio::process::Command;
use tracing::{debug, info};

use crate::proto::{EventKind, SessionEvent};

/// Minimum gap between notifications for the same session. A program in a loop
/// ringing the bell should interrupt you once, not fifty times.
const COOLDOWN: Duration = Duration::from_secs(30);

/// What a notification should say.
pub struct Notification {
    pub title: String,
    pub body: String,
}

pub struct Notifier {
    /// Last notification per `host/session`, for the cooldown.
    last: Mutex<HashMap<String, Instant>>,
    /// Set when notifications are switched off, so the daemon is still useful
    /// as a connection holder on a machine with no desktop.
    enabled: bool,
}

impl Notifier {
    pub fn new(enabled: bool) -> Self {
        Self {
            last: Mutex::new(HashMap::new()),
            enabled,
        }
    }

    /// Handle one event from a host.
    pub async fn handle(&self, host: &str, event: &SessionEvent) {
        let Some(notification) = worth_interrupting(host, event) else {
            return;
        };
        let key = format!("{host}/{}", event.session);
        if !self.allow(&key) {
            debug!(session = %key, "notification suppressed by cooldown");
            return;
        }
        info!(session = %key, "notifying: {}", notification.body);
        if self.enabled {
            deliver(&notification).await;
        }
    }

    /// Whether this session may notify again yet.
    fn allow(&self, key: &str) -> bool {
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
        .arg("--app-name=tiles")
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
    fn the_cooldown_stops_a_session_ringing_in_a_loop() {
        let notifier = Notifier::new(false);
        assert!(notifier.allow("gpu-box/api"));
        assert!(!notifier.allow("gpu-box/api"));
        // A different session is unaffected.
        assert!(notifier.allow("gpu-box/build"));
    }
}
