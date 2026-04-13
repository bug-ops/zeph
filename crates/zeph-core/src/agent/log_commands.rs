// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::fmt::Write as _;
use std::io::{BufRead as _, BufReader, Seek, SeekFrom};

use crate::config::{LogRotation, LoggingConfig};

pub(crate) fn format_logging_status(logging: &LoggingConfig, out: &mut String) {
    let _ = writeln!(
        out,
        "Log file:  {}",
        if logging.file.is_empty() {
            "<disabled>"
        } else {
            &logging.file
        }
    );
    let _ = writeln!(out, "Level:     {}", logging.level);
    let rotation_str = match logging.rotation {
        LogRotation::Daily => "daily",
        LogRotation::Hourly => "hourly",
        LogRotation::Never => "never",
    };
    let _ = writeln!(out, "Rotation:  {rotation_str}");
    let _ = writeln!(out, "Max files: {}", logging.max_files);
}

/// Resolve the most recently modified log file in the log directory whose name starts with
/// the configured file's stem. `tracing-appender` appends a date suffix (e.g.
/// `zeph.2026-03-09.log`) for daily/hourly rotation, so opening the base path directly
/// would fail.
pub(crate) fn resolve_current_log_file(base: &std::path::Path) -> Option<std::path::PathBuf> {
    // Fast path: base path exists as-is (Never rotation).
    if base.exists() {
        return Some(base.to_path_buf());
    }

    let dir = base.parent()?;
    let stem = base.file_stem()?.to_string_lossy();

    let mut best: Option<(std::time::SystemTime, std::path::PathBuf)> = None;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if !name_str.starts_with(stem.as_ref()) {
            continue;
        }
        if let Ok(meta) = entry.metadata()
            && let Ok(modified) = meta.modified()
            && best.as_ref().is_none_or(|(t, _)| modified > *t)
        {
            best = Some((modified, entry.path()));
        }
    }
    best.map(|(_, p)| p)
}

pub(crate) const MAX_LINE_CHARS: usize = 512;
pub(crate) const MAX_TAIL_BYTES: usize = 4 * 1024;

pub(crate) fn read_log_tail(path: &std::path::Path, n: usize) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    let mut reader = BufReader::new(file);
    let size = reader.seek(SeekFrom::End(0)).ok()?;
    if size == 0 {
        return None;
    }

    let chunk = size.min(64 * 1024);
    reader.seek(SeekFrom::End(-chunk.cast_signed())).ok()?;
    let mut lines: Vec<String> = reader
        .lines()
        .map_while(Result::ok)
        .map(|l| {
            if l.chars().count() > MAX_LINE_CHARS {
                let mut s: String = l.chars().take(MAX_LINE_CHARS).collect();
                s.push('…');
                s
            } else {
                l
            }
        })
        .collect();
    lines.reverse();
    lines.truncate(n);
    lines.reverse();

    let mut out = String::new();
    for line in &lines {
        if out.len() + line.len() + 1 > MAX_TAIL_BYTES {
            break;
        }
        out.push_str(line);
        out.push('\n');
    }
    if out.is_empty() { None } else { Some(out) }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- format_logging_status ---

    #[test]
    fn format_logging_status_disabled() {
        let logging = LoggingConfig {
            file: String::new(),
            level: "info".into(),
            rotation: LogRotation::Daily,
            max_files: 7,
        };
        let mut out = String::new();
        format_logging_status(&logging, &mut out);
        assert!(
            out.contains("<disabled>"),
            "expected <disabled>, got: {out}"
        );
        assert!(out.contains("info"));
        assert!(out.contains("daily"));
        assert!(out.contains('7'));
    }

    #[test]
    fn format_logging_status_enabled() {
        let logging = LoggingConfig {
            file: "/var/log/zeph.log".into(),
            level: "debug".into(),
            rotation: LogRotation::Hourly,
            max_files: 3,
        };
        let mut out = String::new();
        format_logging_status(&logging, &mut out);
        assert!(out.contains("/var/log/zeph.log"), "path missing: {out}");
        assert!(out.contains("debug"));
        assert!(out.contains("hourly"));
        assert!(out.contains('3'));
    }

    // --- read_log_tail ---

    #[test]
    fn read_log_tail_missing_file_returns_none() {
        let result = read_log_tail(std::path::Path::new("/nonexistent/path/zeph.log"), 20);
        assert!(result.is_none());
    }

    #[test]
    fn read_log_tail_empty_file_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.log");
        std::fs::write(&path, b"").unwrap();
        let result = read_log_tail(&path, 20);
        assert!(result.is_none());
    }

    #[test]
    fn read_log_tail_returns_last_n_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("zeph.log");
        let content = (1u32..=30)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        std::fs::write(&path, content).unwrap();
        let result = read_log_tail(&path, 5).unwrap();
        let lines: Vec<&str> = result.trim_end().split('\n').collect();
        assert_eq!(lines.len(), 5);
        assert_eq!(lines[0], "line 26");
        assert_eq!(lines[4], "line 30");
    }

    #[test]
    fn read_log_tail_long_line_truncated() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("zeph.log");
        let long_line = "x".repeat(MAX_LINE_CHARS + 100);
        std::fs::write(&path, format!("{long_line}\n")).unwrap();
        let result = read_log_tail(&path, 5).unwrap();
        let line = result.trim_end();
        // char count: MAX_LINE_CHARS chars + 1 ellipsis char
        assert!(line.chars().count() <= MAX_LINE_CHARS + 1);
        assert!(line.ends_with('…'));
    }

    // --- resolve_current_log_file ---

    #[test]
    fn resolve_current_log_file_base_path_exists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("zeph.log");
        std::fs::write(&path, b"hello\n").unwrap();
        let result = resolve_current_log_file(&path);
        assert_eq!(result.as_deref(), Some(path.as_path()));
    }

    #[test]
    fn resolve_current_log_file_date_suffixed_file_found() {
        let dir = tempfile::tempdir().unwrap();
        // tracing-appender creates files like `zeph.2026-03-09.log`
        let rotated = dir.path().join("zeph.2026-03-09.log");
        std::fs::write(&rotated, b"rotated\n").unwrap();
        // base path does not exist
        let base = dir.path().join("zeph.log");
        let result = resolve_current_log_file(&base);
        assert_eq!(result.as_deref(), Some(rotated.as_path()));
    }

    #[test]
    fn resolve_current_log_file_no_matching_files_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        // write a file with a completely different stem
        std::fs::write(dir.path().join("other.log"), b"x\n").unwrap();
        let base = dir.path().join("zeph.log");
        let result = resolve_current_log_file(&base);
        assert!(result.is_none());
    }
}
