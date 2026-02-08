use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};

const SPECIAL_CHARS: &[char] = &[
    '_', '*', '[', ']', '(', ')', '~', '`', '>', '#', '+', '-', '=', '|', '{', '}', '.', '!', '\\',
];

/// Converts standard Markdown to Telegram `MarkdownV2` format.
///
/// Uses `pulldown-cmark` to parse the input into AST events, then walks
/// those events to produce properly escaped Telegram `MarkdownV2` output.
///
/// Formatting conversions:
/// - `**bold**` → `*bold*` (Telegram uses single asterisk)
/// - `*italic*` → `_italic_` (Telegram uses underscore)
/// - `# Header` → `*Header*` (headers become bold text)
/// - Code blocks and inline code preserve content with minimal escaping
///
/// Escaping rules:
/// - Regular text: escape all 19 special characters
/// - Code blocks and inline code: escape only `\` and `` ` ``
#[must_use]
pub fn markdown_to_telegram(input: &str) -> String {
    let options = Options::ENABLE_STRIKETHROUGH;
    let parser = Parser::new_ext(input, options);
    let mut renderer = TelegramRenderer::new(input.len());
    for event in parser {
        renderer.push_event(event);
    }
    renderer.finish()
}

/// Splits text into chunks respecting UTF-8 character boundaries.
///
/// Prefers splitting at newline boundaries when possible for better readability.
/// Each chunk is guaranteed to be valid UTF-8 and at most `max_bytes` in length.
#[must_use]
pub fn utf8_chunks(text: &str, max_bytes: usize) -> Vec<&str> {
    if text.len() <= max_bytes {
        return vec![text];
    }

    let mut chunks = Vec::new();
    let mut offset = 0;

    while offset < text.len() {
        let remaining = text.len() - offset;
        if remaining <= max_bytes {
            chunks.push(&text[offset..]);
            break;
        }

        let mut split_at = offset + max_bytes;

        if split_at >= text.len() {
            chunks.push(&text[offset..]);
            break;
        }

        if !text.is_char_boundary(split_at) {
            while split_at > offset && !text.is_char_boundary(split_at) {
                split_at -= 1;
            }
        }

        let search_start = split_at.saturating_sub(256).max(offset);
        if let Some(newline_pos) = text[search_start..split_at].rfind('\n') {
            let potential_split = search_start + newline_pos + 1;
            if potential_split > offset {
                split_at = potential_split;
            }
        }

        chunks.push(&text[offset..split_at]);
        offset = split_at;
    }

    chunks
}

struct TelegramRenderer {
    output: String,
    in_code_block: bool,
    link_url: Option<String>,
}

impl TelegramRenderer {
    fn new(capacity: usize) -> Self {
        Self {
            output: String::with_capacity(capacity),
            in_code_block: false,
            link_url: None,
        }
    }

    fn push_event(&mut self, event: Event<'_>) {
        match event {
            Event::End(TagEnd::Heading { .. }) => {
                self.output.push_str("*\n");
            }
            Event::Start(Tag::Heading { .. } | Tag::Strong) | Event::End(TagEnd::Strong) => {
                self.output.push('*');
            }
            Event::Start(Tag::Emphasis) | Event::End(TagEnd::Emphasis) => {
                self.output.push('_');
            }
            Event::Start(Tag::Strikethrough) | Event::End(TagEnd::Strikethrough) => {
                self.output.push('~');
            }
            Event::Start(Tag::CodeBlock(_)) => {
                self.output.push_str("```\n");
                self.in_code_block = true;
            }
            Event::End(TagEnd::CodeBlock) => {
                self.output.push_str("```");
                self.in_code_block = false;
            }
            Event::Code(text) => {
                self.output.push('`');
                self.output.push_str(&Self::escape_code_text(&text));
                self.output.push('`');
            }
            Event::Text(text) => {
                let escaped = if self.in_code_block {
                    Self::escape_code_text(&text)
                } else {
                    Self::escape_text(&text)
                };
                self.output.push_str(&escaped);
            }
            Event::Start(Tag::Link { dest_url, .. }) => {
                self.output.push('[');
                self.link_url = Some(dest_url.to_string());
            }
            Event::End(TagEnd::Link) => {
                if let Some(url) = self.link_url.take() {
                    self.output.push_str("](");
                    self.output.push_str(&Self::escape_url(&url));
                    self.output.push(')');
                }
            }
            Event::Start(Tag::Item) => {
                self.output.push_str("• ");
            }
            Event::Start(Tag::BlockQuote(_)) => {
                self.output.push('>');
            }
            Event::End(TagEnd::Paragraph | TagEnd::Item | TagEnd::BlockQuote(_))
            | Event::SoftBreak
            | Event::HardBreak => {
                self.output.push('\n');
            }
            _ => {}
        }
    }

    fn escape_text(text: &str) -> String {
        let mut result = String::with_capacity(text.len() * 2);
        for c in text.chars() {
            if SPECIAL_CHARS.contains(&c) {
                result.push('\\');
            }
            result.push(c);
        }
        result
    }

    fn escape_code_text(text: &str) -> String {
        let mut result = String::with_capacity(text.len() * 2);
        for c in text.chars() {
            match c {
                '`' | '\\' => {
                    result.push('\\');
                    result.push(c);
                }
                _ => result.push(c),
            }
        }
        result
    }

    fn escape_url(text: &str) -> String {
        let mut result = String::with_capacity(text.len());
        for c in text.chars() {
            if c == ')' || c == '\\' {
                result.push('\\');
            }
            result.push(c);
        }
        result
    }

    fn finish(mut self) -> String {
        if self.output.ends_with('\n') {
            self.output.pop();
        }
        self.output
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bold_conversion() {
        let input = "**bold**";
        let output = markdown_to_telegram(input);
        assert_eq!(output, "*bold*");
    }

    #[test]
    fn test_italic_conversion() {
        let input = "*italic*";
        let output = markdown_to_telegram(input);
        assert_eq!(output, "_italic_");
    }

    #[test]
    fn test_strikethrough_conversion() {
        let input = "~~strikethrough~~";
        let output = markdown_to_telegram(input);
        assert_eq!(output, "~strikethrough~");
    }

    #[test]
    fn test_header_to_bold() {
        let input = "# Header 1\n## Header 2";
        let output = markdown_to_telegram(input);
        assert!(output.contains("*Header 1*"));
        assert!(output.contains("*Header 2*"));
    }

    #[test]
    fn test_nested_formatting() {
        let input = "**bold _italic_**";
        let output = markdown_to_telegram(input);
        assert_eq!(output, "*bold _italic_*");
    }

    #[test]
    fn test_inline_code() {
        let input = "text `code` text";
        let output = markdown_to_telegram(input);
        assert!(output.contains("`code`"));
    }

    #[test]
    fn test_code_block() {
        let input = "```\ncode block\n```";
        let output = markdown_to_telegram(input);
        assert!(output.starts_with("```\n"));
        assert!(output.contains("code block"));
        assert!(output.ends_with("```"));
    }

    #[test]
    fn test_links() {
        let input = "[text](https://example.com)";
        let output = markdown_to_telegram(input);
        assert_eq!(output, "[text](https://example.com)");
    }

    #[test]
    fn test_blockquote() {
        let input = "> quote";
        let output = markdown_to_telegram(input);
        assert!(output.starts_with('>'));
    }

    #[test]
    fn test_lists() {
        let input = "- item 1\n- item 2";
        let output = markdown_to_telegram(input);
        assert!(output.contains("• item 1"));
        assert!(output.contains("• item 2"));
    }

    #[test]
    fn test_escape_special_chars() {
        let input = "Special: . ! - + = | { }";
        let output = markdown_to_telegram(input);
        assert_eq!(output, "Special: \\. \\! \\- \\+ \\= \\| \\{ \\}");
    }

    #[test]
    fn test_code_block_minimal_escape() {
        let input = "```\nbackslash \\ and backtick `\n```";
        let output = markdown_to_telegram(input);
        assert!(output.contains("backslash \\\\"));
        assert!(output.contains("backtick \\`"));
    }

    #[test]
    fn test_no_double_escape() {
        let input = "already escaped: \\*";
        let output = markdown_to_telegram(input);
        assert_eq!(output, "already escaped: \\*");
    }

    #[test]
    fn test_mixed_code_and_text() {
        let input = "text with `code` and **bold**";
        let output = markdown_to_telegram(input);
        assert!(output.contains("`code`"));
        assert!(output.contains("*bold*"));
    }

    #[test]
    fn test_empty_input() {
        let input = "";
        let output = markdown_to_telegram(input);
        assert_eq!(output, "");
    }

    #[test]
    fn test_plain_text() {
        let input = "Plain text with special chars: -";
        let output = markdown_to_telegram(input);
        assert!(output.contains("\\-"));
    }

    #[test]
    fn test_unclosed_bold() {
        let input = "**unclosed bold";
        let output = markdown_to_telegram(input);
        assert!(!output.is_empty());
    }

    #[test]
    fn test_unclosed_code_block() {
        let input = "```\nunclosed";
        let output = markdown_to_telegram(input);
        assert!(!output.is_empty());
    }

    #[test]
    fn test_horizontal_rule() {
        let input = "Text\n---\nMore";
        let output = markdown_to_telegram(input);
        assert!(output.contains("Text"));
        assert!(output.contains("More"));
    }

    #[test]
    fn test_unicode_text() {
        let input = "emoji 🎉 and CJK 中文";
        let output = markdown_to_telegram(input);
        assert!(output.contains("🎉"));
        assert!(output.contains("中文"));
    }

    #[test]
    fn test_multiline() {
        let input = "# Title\n\nParagraph 1.\n\nParagraph 2 with **bold**.";
        let output = markdown_to_telegram(input);
        assert!(output.contains("*Title*"));
        assert!(output.contains("Paragraph 1"));
        assert!(output.contains("*bold*"));
    }

    #[test]
    fn test_no_split_needed() {
        let text = "short text";
        let chunks = utf8_chunks(text, 100);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0], text);
    }

    #[test]
    fn test_split_at_newline() {
        let text = "line 1\nline 2\nline 3";
        let chunks = utf8_chunks(text, 10);
        assert!(chunks.len() > 1);
        for chunk in &chunks {
            assert!(chunk.len() <= 10);
        }
    }

    #[test]
    fn test_split_respects_utf8() {
        let text = "日本語";
        let chunks = utf8_chunks(text, 5);
        for chunk in &chunks {
            assert!(std::str::from_utf8(chunk.as_bytes()).is_ok());
        }
    }

    #[test]
    fn test_split_emoji() {
        let text = "🎉🎊🎈🎁";
        let chunks = utf8_chunks(text, 8);
        for chunk in &chunks {
            assert!(std::str::from_utf8(chunk.as_bytes()).is_ok());
            assert!(chunk.len() <= 8);
        }
    }

    #[test]
    fn test_chunks_concatenate() {
        let text = "The quick brown fox jumps over the lazy dog";
        let chunks = utf8_chunks(text, 10);
        let rejoined = chunks.join("");
        assert_eq!(rejoined, text);
    }

    #[test]
    fn test_each_chunk_within_limit() {
        let text = "a".repeat(1000);
        let max_bytes = 100;
        let chunks = utf8_chunks(&text, max_bytes);
        for chunk in &chunks {
            assert!(chunk.len() <= max_bytes);
        }
    }

    #[test]
    fn test_code_block_with_special_chars() {
        let input = "```bash\nfind . -name \"*.txt\"\n```";
        let output = markdown_to_telegram(input);
        assert!(output.contains("find . -name"));
    }

    #[test]
    fn test_escaping_backslash() {
        let input = "backslash \\";
        let output = markdown_to_telegram(input);
        assert!(output.contains("\\\\"));
    }

    #[test]
    fn test_link_with_special_chars() {
        let input = "[link](https://example.com/path?param=value)";
        let output = markdown_to_telegram(input);
        assert!(output.contains("[link]"));
        assert!(output.contains("example.com"));
    }

    #[test]
    fn test_utf8_chunks_no_infinite_loop() {
        let text = format!("{}\n{}{}", "A".repeat(7), "X".repeat(90), "Y".repeat(50));
        let chunks = utf8_chunks(&text, 50);
        let rejoined: String = chunks.concat();
        assert_eq!(rejoined, text);
        assert!(chunks.len() >= 2, "Should produce at least 2 chunks");
        for chunk in &chunks {
            assert!(
                chunk.len() <= 50,
                "Chunk exceeds max_bytes: {}",
                chunk.len()
            );
            assert!(
                !chunk.is_empty(),
                "Empty chunk detected - infinite loop bug"
            );
        }
    }
}
