//! Rendering the session table.

use crate::proto::SessionInfo;

/// One line of the session table: a session, and the host it lives on.
pub struct SessionRow {
    pub host: String,
    pub session: SessionInfo,
}

const HEADER: [&str; 6] = ["HOST", "SESSION", "TITLE", "ATTACHED", "IDLE", "BELL"];

/// Render `tiles ls` output. Columns are sized to their contents so the table
/// stays readable whether you have one session or forty.
pub fn session_table(rows: &[SessionRow]) -> String {
    if rows.is_empty() {
        return "no sessions\n".to_string();
    }

    let mut table = vec![HEADER.map(String::from)];
    table.extend(rows.iter().map(|row| {
        let info = &row.session;
        [
            row.host.clone(),
            info.name.clone(),
            truncate(&info.title, 40),
            match info.attached {
                0 => "-".to_string(),
                n => n.to_string(),
            },
            duration(info.idle),
            if info.bells > 0 { "*" } else { "" }.to_string(),
        ]
    }));

    let widths: Vec<usize> = (0..HEADER.len())
        .map(|i| {
            table
                .iter()
                .map(|row| row[i].chars().count())
                .max()
                .unwrap_or(0)
        })
        .collect();

    let mut out = String::new();
    for row in &table {
        let mut line = String::new();
        for (cell, width) in row.iter().zip(&widths) {
            line.push_str(&format!("{cell:width$}  "));
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
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

    #[test]
    fn table_aligns_columns_and_marks_bells() {
        let rows = vec![
            SessionRow {
                host: "gpu-box".to_string(),
                session: info("api", "claude", 0, 120, 1),
            },
            SessionRow {
                host: "gpu-box".to_string(),
                session: info("build", "cargo watch", 1, 0, 0),
            },
        ];
        let out = session_table(&rows);
        let lines: Vec<_> = out.lines().collect();
        assert_eq!(
            lines[0],
            "HOST     SESSION  TITLE        ATTACHED  IDLE  BELL"
        );
        assert_eq!(lines[1], "gpu-box  api      claude       -         2m    *");
        assert_eq!(lines[2], "gpu-box  build    cargo watch  1         0s");
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
