//! Rendering the session table.

use crate::hosts::is_this_machine;
use crate::proto::SessionInfo;
use crate::style;

/// One line of the session table: a session, and the machine it lives on.
pub struct SessionRow {
    pub host: String,
    pub session: SessionInfo,
}

impl SessionRow {
    /// What you would type to reach this session. Bare on this machine, since
    /// that is what you would type, and `host/name` anywhere else.
    pub fn target(&self) -> String {
        if is_this_machine(&self.host) {
            self.session.name.clone()
        } else {
            format!("{}/{}", self.host, self.session.name)
        }
    }
}

/// One column of one row. The two forms are kept side by side because colour
/// codes have width on the wire and none on the screen, so alignment has to be
/// measured on the plain text and printed with the styled.
struct Cell {
    plain: String,
    styled: String,
}

impl Cell {
    fn new(plain: String, styled: String) -> Self {
        Self { plain, styled }
    }

    fn width(&self) -> usize {
        self.plain.chars().count()
    }
}

const HEADER: [&str; 5] = ["TARGET", "TITLE", "ATTACHED", "IDLE", "BELL"];

/// Render `tiles ls` output.
///
/// One `TARGET` column rather than separate host and session columns, because
/// what you want from a listing is the thing to type next, and `host/name` is
/// the only handle that is unique across machines.
pub fn session_table(rows: &[SessionRow]) -> String {
    if rows.is_empty() {
        return format!("{}\n", style::faint("no sessions"));
    }

    let mut table = vec![HEADER.map(|head| Cell::new(head.to_string(), style::label(head)))];
    table.extend(rows.iter().map(|row| {
        let info = &row.session;
        let target = row.target();
        [
            Cell::new(target.clone(), paint_target(&row.host, &target)),
            Cell::new(
                truncate(&info.title, 40),
                style::value(&truncate(&info.title, 40)),
            ),
            attached(info.attached),
            idle(info.idle),
            bell(info.bells),
        ]
    }));

    let widths: Vec<usize> = (0..HEADER.len())
        .map(|i| table.iter().map(|row| row[i].width()).max().unwrap_or(0))
        .collect();

    let mut out = String::new();
    for row in &table {
        let mut line = String::new();
        for (cell, width) in row.iter().zip(&widths) {
            line.push_str(&cell.styled);
            line.push_str(&" ".repeat(width.saturating_sub(cell.width()) + 2));
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

/// The machine kept quieter than the session name, so a column of targets reads
/// as a list of sessions rather than a wall of hostnames.
fn paint_target(host: &str, target: &str) -> String {
    match target.strip_prefix(&format!("{host}/")) {
        Some(name) => format!("{}{}", style::host(&format!("{host}/")), style::bold(name)),
        None => style::bold(target),
    }
}

/// The dot is content rather than decoration, so it is part of the plain text
/// too: otherwise the column would be measured one way and printed another.
fn attached(clients: usize) -> Cell {
    match clients {
        0 => Cell::new(
            format!("{} -", DOT_IDLE),
            format!("{} {}", style::dot_idle(), style::faint("-")),
        ),
        n => Cell::new(
            format!("{DOT_ATTACHED} {n}"),
            format!("{} {}", style::dot_attached(), style::green(&n.to_string())),
        ),
    }
}

const DOT_ATTACHED: char = '●';
const DOT_IDLE: char = '○';

/// Idle time fades as it grows: a session that just did something is worth
/// noticing, one that has been quiet for a day is background.
fn idle(secs: u64) -> Cell {
    let text = duration(secs);
    let styled = if secs < 60 {
        style::value(&text)
    } else if secs < 3600 {
        style::label(&text)
    } else {
        style::faint(&text)
    };
    Cell::new(text, styled)
}

fn bell(bells: u64) -> Cell {
    if bells == 0 {
        return Cell::new(String::new(), String::new());
    }
    Cell::new("*".to_string(), style::amber("*"))
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let kept: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{kept}…")
}

/// Compact duration: `0s`, `45s`, `12m`, `3h`, `2d`.
pub fn duration(secs: u64) -> String {
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::Size;

    fn info(name: &str, title: &str, attached: usize, idle: u64, bells: u64) -> SessionInfo {
        SessionInfo {
            name: name.into(),
            title: title.into(),
            command: "zsh".into(),
            pid: 1,
            size: Size::default(),
            attached,
            idle,
            bells,
        }
    }

    fn row(host: &str, session: SessionInfo) -> SessionRow {
        SessionRow {
            host: host.to_string(),
            session,
        }
    }

    /// These tests run with stdout captured, so styling is off and the table is
    /// plain: exactly the alignment a pipe or a script would see.
    #[test]
    fn the_target_column_is_what_you_would_type() {
        let rows = vec![
            row("gpu-box", info("api", "claude", 0, 120, 1)),
            row(crate::hosts::this_machine(), info("notes", "vim", 1, 0, 0)),
        ];
        let out = session_table(&rows);
        let lines: Vec<_> = out.lines().collect();
        assert_eq!(lines[0], "TARGET       TITLE   ATTACHED  IDLE  BELL");
        assert_eq!(lines[1], "gpu-box/api  claude  ○ -       2m    *");
        // This machine's own sessions need no prefix, and typing one works.
        assert_eq!(lines[2], "notes        vim     ● 1       0s");
    }

    #[test]
    fn empty_is_not_a_bare_header() {
        assert_eq!(session_table(&[]), "no sessions\n");
    }

    #[test]
    fn durations_are_compact() {
        assert_eq!(duration(0), "0s");
        assert_eq!(duration(59), "59s");
        assert_eq!(duration(90), "1m");
        assert_eq!(duration(7200), "2h");
        assert_eq!(duration(200_000), "2d");
    }

    #[test]
    fn long_titles_are_truncated_with_an_ellipsis() {
        let long = "a".repeat(60);
        assert_eq!(truncate(&long, 10).chars().count(), 10);
        assert!(truncate(&long, 10).ends_with('…'));
        assert_eq!(truncate("short", 10), "short");
    }
}
