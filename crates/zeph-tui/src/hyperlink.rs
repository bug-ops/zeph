use std::io::Write;
use std::sync::LazyLock;

use crossterm::cursor::MoveTo;
use crossterm::queue;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use regex::Regex;

static URL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"https?://[^\s<>\[\]()\x22'`]+").unwrap());

pub struct HyperlinkSpan {
    pub url: String,
    pub row: u16,
    pub start_col: u16,
    pub end_col: u16,
}

pub fn detect_urls_in_text(text: &str) -> Vec<(std::ops::Range<usize>, String)> {
    URL_RE
        .find_iter(text)
        .map(|m| (m.start()..m.end(), m.as_str().to_string()))
        .collect()
}

#[must_use]
pub fn collect_from_buffer(buffer: &Buffer, area: Rect) -> Vec<HyperlinkSpan> {
    let mut spans = Vec::new();
    for row in area.y..area.y + area.height {
        let mut row_text = String::new();
        let mut col_offsets: Vec<u16> = Vec::new();
        for col in area.x..area.x + area.width {
            let sym = buffer[(col, row)].symbol();
            for _ in sym.chars() {
                col_offsets.push(col);
            }
            row_text.push_str(sym);
        }
        for (range, url) in detect_urls_in_text(&row_text) {
            let Some(&start_col) = col_offsets.get(range.start) else {
                continue;
            };
            let end_col = col_offsets
                .get(range.end.saturating_sub(1))
                .map_or(start_col + 1, |c| c + 1);
            spans.push(HyperlinkSpan {
                url,
                row,
                start_col,
                end_col,
            });
        }
    }
    spans
}

/// Write OSC 8 escape sequences directly to the terminal writer.
/// Cursor is repositioned for each hyperlink; the visible text is untouched.
///
/// # Errors
///
/// Returns an error if writing to the terminal fails.
pub fn write_osc8(writer: &mut impl Write, spans: &[HyperlinkSpan]) -> std::io::Result<()> {
    for span in spans {
        queue!(writer, MoveTo(span.start_col, span.row))?;
        write!(writer, "\x1b]8;;{}\x1b\\", span.url)?;
        queue!(writer, MoveTo(span.end_col, span.row))?;
        write!(writer, "\x1b]8;;\x1b\\")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_urls_basic() {
        let urls = detect_urls_in_text("visit https://example.com for info");
        assert_eq!(urls.len(), 1);
        assert_eq!(urls[0].1, "https://example.com");
    }

    #[test]
    fn detect_urls_multiple() {
        let text = "see http://a.com and https://b.org/path?q=1";
        let urls = detect_urls_in_text(text);
        assert_eq!(urls.len(), 2);
        assert_eq!(urls[0].1, "http://a.com");
        assert_eq!(urls[1].1, "https://b.org/path?q=1");
    }

    #[test]
    fn detect_urls_none() {
        let urls = detect_urls_in_text("no links here");
        assert!(urls.is_empty());
    }

    #[test]
    fn detect_urls_in_markdown_brackets() {
        let urls = detect_urls_in_text("[text](https://example.com)");
        assert_eq!(urls.len(), 1);
        assert_eq!(urls[0].1, "https://example.com");
    }

    #[test]
    fn collect_from_buffer_finds_urls() {
        let area = Rect::new(0, 0, 40, 2);
        let mut buf = Buffer::empty(area);
        buf.set_string(
            0,
            0,
            "visit https://example.com now",
            ratatui::style::Style::default(),
        );
        buf.set_string(0, 1, "no links here", ratatui::style::Style::default());

        let spans = collect_from_buffer(&buf, area);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].url, "https://example.com");
        assert_eq!(spans[0].row, 0);
        assert_eq!(spans[0].start_col, 6);
        assert_eq!(spans[0].end_col, 25);
    }

    #[test]
    fn write_osc8_produces_escape_sequences() {
        let spans = vec![HyperlinkSpan {
            url: "https://x.com".to_string(),
            row: 0,
            start_col: 0,
            end_col: 5,
        }];
        let mut buf = Vec::new();
        write_osc8(&mut buf, &spans).unwrap();
        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("\x1b]8;;https://x.com\x1b\\"));
        assert!(output.contains("\x1b]8;;\x1b\\"));
    }
}
