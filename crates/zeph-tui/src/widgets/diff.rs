use std::path::Path;

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use similar::ChangeTag;

use crate::highlight::SYNTAX_HIGHLIGHTER;
use crate::theme::{SyntaxTheme, Theme};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffLineKind {
    Added,
    Removed,
    Context,
}

#[derive(Debug, Clone)]
pub struct DiffLine<'a> {
    pub kind: DiffLineKind,
    pub content: &'a str,
}

#[must_use]
pub fn compute_diff<'a>(old: &'a str, new: &'a str) -> Vec<DiffLine<'a>> {
    let diff = similar::TextDiff::from_lines(old, new);
    diff.iter_all_changes()
        .map(|change| {
            let kind = match change.tag() {
                ChangeTag::Delete => DiffLineKind::Removed,
                ChangeTag::Insert => DiffLineKind::Added,
                ChangeTag::Equal => DiffLineKind::Context,
            };
            DiffLine {
                kind,
                content: change.value(),
            }
        })
        .collect()
}

#[must_use]
pub fn render_diff_lines(lines: &[DiffLine], file_path: &str, theme: &Theme) -> Vec<Line<'static>> {
    let lang = lang_from_path(file_path);
    let syntax_theme = SyntaxTheme::default();
    let mut result = Vec::new();

    // Header
    let added = lines
        .iter()
        .filter(|l| l.kind == DiffLineKind::Added)
        .count();
    let removed = lines
        .iter()
        .filter(|l| l.kind == DiffLineKind::Removed)
        .count();
    result.push(Line::from(Span::styled(
        format!("{file_path}: +{added} -{removed}"),
        theme.diff_header,
    )));

    for dl in lines {
        let (gutter, gutter_style, bg) = match dl.kind {
            DiffLineKind::Added => ("+", theme.diff_gutter_add, Some(theme.diff_added_bg)),
            DiffLineKind::Removed => ("-", theme.diff_gutter_remove, Some(theme.diff_removed_bg)),
            DiffLineKind::Context => (" ", Style::default(), None),
        };

        let content = dl.content.trim_end_matches('\n');
        let mut line_spans = vec![Span::styled(format!("{gutter} "), gutter_style)];

        let highlighted =
            lang.and_then(|l| SYNTAX_HIGHLIGHTER.highlight(l, content, &syntax_theme));

        if let Some(spans) = highlighted {
            for span in spans {
                let mut style = span.style;
                if let Some(bg_color) = bg {
                    style = style.bg(bg_color);
                }
                line_spans.push(Span::styled(span.content.to_string(), style));
            }
        } else {
            let mut style = Style::default();
            if let Some(bg_color) = bg {
                style = style.bg(bg_color);
            }
            line_spans.push(Span::styled(content.to_string(), style));
        }

        result.push(Line::from(line_spans));
    }

    result
}

/// Render a compact one-line diff summary.
#[must_use]
pub fn render_diff_compact(file_path: &str, lines: &[DiffLine], theme: &Theme) -> Line<'static> {
    let added = lines
        .iter()
        .filter(|l| l.kind == DiffLineKind::Added)
        .count();
    let removed = lines
        .iter()
        .filter(|l| l.kind == DiffLineKind::Removed)
        .count();
    Line::from(Span::styled(
        format!("  {file_path}: +{added} -{removed} lines"),
        theme.diff_header,
    ))
}

fn lang_from_path(path: &str) -> Option<&'static str> {
    match Path::new(path).extension()?.to_str()? {
        "rs" => Some("rust"),
        "py" => Some("python"),
        "js" => Some("javascript"),
        "json" => Some("json"),
        "toml" => Some("toml"),
        "sh" | "bash" => Some("bash"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compute_diff_empty_to_content() {
        let lines = compute_diff("", "hello\nworld\n");
        assert_eq!(lines.len(), 2);
        assert!(lines.iter().all(|l| l.kind == DiffLineKind::Added));
    }

    #[test]
    fn compute_diff_identical() {
        let lines = compute_diff("same\n", "same\n");
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].kind, DiffLineKind::Context);
    }

    #[test]
    fn compute_diff_edit() {
        let lines = compute_diff("foo\nbar\nbaz\n", "foo\nqux\nbaz\n");
        let removed: Vec<_> = lines
            .iter()
            .filter(|l| l.kind == DiffLineKind::Removed)
            .collect();
        let added: Vec<_> = lines
            .iter()
            .filter(|l| l.kind == DiffLineKind::Added)
            .collect();
        assert_eq!(removed.len(), 1);
        assert!(removed[0].content.contains("bar"));
        assert_eq!(added.len(), 1);
        assert!(added[0].content.contains("qux"));
    }

    #[test]
    fn compute_diff_content_to_empty() {
        let lines = compute_diff("hello\n", "");
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].kind, DiffLineKind::Removed);
    }

    #[test]
    fn render_diff_lines_has_header() {
        let diff_lines = compute_diff("", "line\n");
        let theme = Theme::default();
        let rendered = render_diff_lines(&diff_lines, "test.rs", &theme);
        assert!(!rendered.is_empty());
        let header: String = rendered[0]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(header.contains("test.rs"));
        assert!(header.contains("+1"));
    }

    #[test]
    fn render_diff_compact_format() {
        let diff_lines = compute_diff("old\n", "new\n");
        let theme = Theme::default();
        let line = render_diff_compact("file.rs", &diff_lines, &theme);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("file.rs"));
        assert!(text.contains("+1"));
        assert!(text.contains("-1"));
    }

    #[test]
    fn lang_from_path_known() {
        assert_eq!(lang_from_path("foo.rs"), Some("rust"));
        assert_eq!(lang_from_path("bar.py"), Some("python"));
        assert_eq!(lang_from_path("baz.js"), Some("javascript"));
        assert_eq!(lang_from_path("q.toml"), Some("toml"));
        assert_eq!(lang_from_path("s.sh"), Some("bash"));
    }

    #[test]
    fn lang_from_path_unknown() {
        assert_eq!(lang_from_path("file.xyz"), None);
        assert_eq!(lang_from_path("noext"), None);
    }
}
