// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Markdown-to-Telegram conversion and UTF-8-safe message chunking.
//!
//! Telegram's `MarkdownV2` format differs from `CommonMark` in several ways:
//! bold uses a single `*`, italic uses `_`, and all 19 special characters
//! must be escaped with `\` in regular text.  This module handles both the
//! format conversion and the 4096-byte message-length limit.
//!
//! # Public API
//!
//! * [`markdown_to_telegram`] — convert `CommonMark` to Telegram `MarkdownV2` using the
//!   default expandable-blockquote threshold.
//! * [`markdown_to_telegram_with_config`] — same conversion with an explicit
//!   expandable-blockquote line-count threshold (Bot API 10.1 `expandable_blockquote`).
//! * [`utf8_chunks`] — split long strings at UTF-8 / newline boundaries.

use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};

const SPECIAL_CHARS: &[char] = &[
    '_', '*', '[', ']', '(', ')', '~', '`', '>', '#', '+', '-', '=', '|', '{', '}', '.', '!', '\\',
];

/// Default blockquote line-count threshold for the expandable form, matching
/// `TelegramConfig::expandable_blockquote_min_lines`'s default (Bot API 10.1).
pub(crate) const DEFAULT_EXPANDABLE_BLOCKQUOTE_MIN_LINES: u32 = 10;

/// Convert standard Markdown to Telegram `MarkdownV2` format.
///
/// Uses `pulldown-cmark` to parse the input into an event stream, then walks
/// those events to produce properly escaped Telegram `MarkdownV2` output.
///
/// # Formatting conversions
///
/// | Markdown | Telegram `MarkdownV2` | Note |
/// |----------|---------------------|------|
/// | `**bold**` | `*bold*` | single asterisk |
/// | `*italic*` | `_italic_` | underscore |
/// | `# Heading` | `*Heading*` | headings become bold |
/// | `` `code` `` | `` `code` `` | preserved verbatim |
/// | `~~strike~~` | `~strike~` | single tilde |
/// | `[text](url)` | `[text](url)` | links preserved |
/// | `- item` | `• item` | bullet list |
/// | `> quote` | `> quote` | blockquote |
///
/// # Escaping rules
///
/// * Regular text: all 19 Telegram special characters are escaped with `\`.
/// * Code blocks and inline code: only `\` and `` ` `` are escaped.
///
/// # Examples
///
/// ```rust
/// use zeph_channels::markdown::markdown_to_telegram;
///
/// assert_eq!(markdown_to_telegram("**bold**"), "*bold*");
/// assert_eq!(markdown_to_telegram("*italic*"), "_italic_");
/// assert_eq!(markdown_to_telegram(""), "");
/// ```
#[must_use]
pub fn markdown_to_telegram(input: &str) -> String {
    markdown_to_telegram_with_config(input, DEFAULT_EXPANDABLE_BLOCKQUOTE_MIN_LINES)
}

/// Convert standard Markdown to Telegram `MarkdownV2`, with an explicit expandable-blockquote
/// line-count threshold (Bot API 10.1 `expandable_blockquote`).
///
/// Identical to [`markdown_to_telegram`] except a blockquote spanning
/// `expandable_blockquote_min_lines` lines or more renders as the expandable
/// (collapsed-by-default) form — `**>` on the first line, `\|\|` appended to the
/// last line, `>` on every line in between. `expandable_blockquote_min_lines = 0`
/// disables the expandable form unconditionally, regardless of quote length.
///
/// # Examples
///
/// ```rust
/// use zeph_channels::markdown::markdown_to_telegram_with_config;
///
/// let quote = "> line 1\n> line 2\n> line 3";
///
/// // Threshold of 3 makes a 3-line quote expandable.
/// let expandable = markdown_to_telegram_with_config(quote, 3);
/// assert!(expandable.starts_with("**>line 1"));
/// assert!(expandable.trim_end().ends_with("||"));
///
/// // A threshold of 0 always disables the expandable form.
/// let never_expandable = markdown_to_telegram_with_config(quote, 0);
/// assert!(!never_expandable.contains("**>"));
/// ```
#[must_use]
pub fn markdown_to_telegram_with_config(
    input: &str,
    expandable_blockquote_min_lines: u32,
) -> String {
    let options = Options::ENABLE_STRIKETHROUGH;
    let parser = Parser::new_ext(input, options);
    let mut renderer = TelegramRenderer::new(input.len(), expandable_blockquote_min_lines);
    for event in parser {
        renderer.push_event(event);
    }
    renderer.finish()
}

/// Split `text` into chunks that each fit within `max_bytes`.
///
/// All chunks are valid UTF-8 slices of the original string.  The function
/// prefers to split on newline boundaries within the last 256 bytes of the
/// window so that Telegram messages break at natural paragraph boundaries
/// rather than mid-sentence.
///
/// When no text exceeds `max_bytes` the original string is returned as a
/// single-element slice without any allocation.
///
/// # Panics
///
/// Does not panic; the loop terminates because every iteration either emits a
/// non-empty chunk or exits.
///
/// # Examples
///
/// ```rust
/// use zeph_channels::markdown::utf8_chunks;
///
/// let text = "Hello, world!";
/// let chunks = utf8_chunks(text, 100);
/// assert_eq!(chunks, vec!["Hello, world!"]);
///
/// // Chunks are joined back to the original string.
/// let long = "a".repeat(200);
/// let pieces = utf8_chunks(&long, 50);
/// assert_eq!(pieces.concat(), long);
/// for piece in &pieces {
///     assert!(piece.len() <= 50);
/// }
/// ```
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

        let mut split_at = text.floor_char_boundary(offset + max_bytes);

        if split_at >= text.len() {
            chunks.push(&text[offset..]);
            break;
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

/// Maximum tracked blockquote nesting depth. Levels beyond this are still parsed
/// (pulldown-cmark emits balanced `Start`/`End` events regardless) but no `output`
/// mark is recorded for them, so [`TelegramRenderer::blockquote_marks`] cannot grow
/// past this bound even under adversarially deep nested input. Same defence-in-depth
/// pattern as `MAX_CHUNK_DEPTH` (`crates/zeph-index/src/chunker.rs`, #6595) — this
/// path has no native-recursion stack-overflow risk (pulldown-cmark's event stream
/// and this renderer are both flat iterators), but the cap still bounds memory and
/// gives a documented, tested ceiling instead of an unbounded one.
const MAX_BLOCKQUOTE_NESTING_DEPTH: usize = 512;

struct TelegramRenderer {
    output: String,
    in_code_block: bool,
    link_url: Option<String>,
    /// Blockquote line-count threshold for the expandable form (0 = always disabled).
    expandable_blockquote_min_lines: u32,
    /// Stack of `output` byte offsets recorded at each `BlockQuote` start, up to
    /// [`MAX_BLOCKQUOTE_NESTING_DEPTH`] entries. Only the outermost mark (stack empty
    /// after popping) triggers the `>`-per-line prefix rewrite in
    /// [`Self::end_blockquote`] — nested blockquotes are flattened to that single
    /// level, since Telegram `MarkdownV2` has no nested-blockquote grammar.
    blockquote_marks: Vec<usize>,
    /// True `BlockQuote` nesting depth, incremented/decremented on every `Start`/`End`
    /// regardless of the [`MAX_BLOCKQUOTE_NESTING_DEPTH`] cap — kept separate from
    /// `blockquote_marks.len()` so `end_blockquote` can tell whether the level it is
    /// closing had a mark recorded (within the cap) or not (beyond it), keeping
    /// push/pop balanced either way.
    blockquote_depth: usize,
}

impl TelegramRenderer {
    fn new(capacity: usize, expandable_blockquote_min_lines: u32) -> Self {
        Self {
            output: String::with_capacity(capacity),
            in_code_block: false,
            link_url: None,
            expandable_blockquote_min_lines,
            blockquote_marks: Vec::new(),
            blockquote_depth: 0,
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
                self.blockquote_depth += 1;
                if self.blockquote_marks.len() < MAX_BLOCKQUOTE_NESTING_DEPTH {
                    self.blockquote_marks.push(self.output.len());
                }
            }
            Event::End(TagEnd::BlockQuote(_)) => {
                self.end_blockquote();
            }
            Event::End(TagEnd::Paragraph | TagEnd::Item) | Event::SoftBreak | Event::HardBreak => {
                self.output.push('\n');
            }
            _ => {}
        }
    }

    /// Close the innermost open blockquote: take everything emitted since its `Start`
    /// event, split it into lines, and re-emit with a `>` prefix on every line (FR-005),
    /// switching to the expandable form (`**>` … `\|\|`) when the line count meets
    /// `expandable_blockquote_min_lines` (FR-006/FR-007).
    ///
    /// Always appends two trailing newlines after the prefixed block, mirroring the
    /// pre-fix behaviour of the `TagEnd::Paragraph`/`TagEnd::BlockQuote` shared arm
    /// (one newline from the inner paragraph's end, one from the blockquote's own end)
    /// so that block-level spacing is unchanged for callers (NFR-002/NFR-003).
    ///
    /// Telegram `MarkdownV2` has no nested-blockquote grammar: a blockquote is exactly
    /// one leading `>` per line, and a second `>` on the same line is an unescaped
    /// reserved character that Telegram's parser rejects outright (`400 Bad Request`,
    /// dropping the whole message). A `BlockQuote` end that is still nested inside
    /// another open blockquote is therefore a no-op here: its content is left exactly
    /// as emitted, so it stays part of the enclosing blockquote's captured span and
    /// gets the single `>` prefix exactly once, when the outermost call below runs —
    /// nested quotes are flattened to one level, never accumulated as `>>`. This also
    /// keeps cost linear in the total content size regardless of nesting depth: only
    /// the outermost call ever splits/rescans, instead of once per nesting level.
    fn end_blockquote(&mut self) {
        let Some(closing_level) = self.blockquote_depth.checked_sub(1) else {
            // Unbalanced `BlockQuote` end with no matching start — pulldown-cmark never
            // emits this for well-formed input, but fail safe rather than panic.
            return;
        };
        self.blockquote_depth = closing_level;

        if closing_level >= MAX_BLOCKQUOTE_NESTING_DEPTH {
            // Beyond MAX_BLOCKQUOTE_NESTING_DEPTH: no mark was recorded for this level
            // (see the `Start` handler), so there is nothing to pop or process — its
            // content already flows straight into the nearest tracked ancestor.
            return;
        }

        let Some(mark) = self.blockquote_marks.pop() else {
            // Unreachable given the invariant above (marks.len() == min(depth, cap)
            // at all times), but fail safe rather than panic.
            return;
        };
        if !self.blockquote_marks.is_empty() {
            // Still inside an enclosing blockquote — flatten (see doc comment above).
            return;
        }
        let content = self.output.split_off(mark);
        let trimmed = content.trim_end_matches('\n');
        let lines: Vec<&str> = if trimmed.is_empty() {
            vec![""]
        } else {
            trimmed.split('\n').collect()
        };
        let line_count = lines.len();
        let line_count_u32 = u32::try_from(line_count).unwrap_or(u32::MAX);
        let expandable = self.expandable_blockquote_min_lines > 0
            && line_count_u32 >= self.expandable_blockquote_min_lines;

        for (i, line) in lines.iter().enumerate() {
            if i > 0 {
                self.output.push('\n');
            }
            if expandable && i == 0 {
                self.output.push_str("**>");
            } else {
                self.output.push('>');
            }
            self.output.push_str(line);
            if expandable && i == line_count - 1 {
                self.output.push_str("||");
            }
        }
        self.output.push_str("\n\n");
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
    fn test_multiline_blockquote_prefixes_every_line() {
        // 3 lines — below the default 10-line expandable threshold.
        let input = "> line 1\n> line 2\n> line 3";
        let output = markdown_to_telegram(input);
        for line in ["line 1", "line 2", "line 3"] {
            assert!(
                output.contains(&format!(">{line}")),
                "expected '>{line}' in output: {output:?}"
            );
        }
        assert!(
            !output.contains("**>"),
            "must not be expandable: {output:?}"
        );
        assert!(!output.contains("||"), "must not be expandable: {output:?}");
    }

    #[test]
    fn test_blockquote_line_counts_two_to_nine_below_default_threshold() {
        for n in 2..=9 {
            let lines: Vec<String> = (1..=n).map(|i| format!("> line {i}")).collect();
            let input = lines.join("\n");
            let output = markdown_to_telegram(&input);
            for i in 1..=n {
                assert!(
                    output.contains(&format!(">line {i}")),
                    "n={n}: expected '>line {i}' in output: {output:?}"
                );
            }
            assert!(!output.contains("**>"), "n={n}: must not be expandable");
        }
    }

    #[test]
    fn test_blockquote_expandable_at_threshold_boundary() {
        let lines: Vec<String> = (1..=5).map(|i| format!("> line {i}")).collect();
        let input = lines.join("\n");

        // Exactly at the threshold — must render expandable (>= comparison, FR-006).
        let expandable = markdown_to_telegram_with_config(&input, 5);
        assert!(
            expandable.starts_with("**>line 1"),
            "output: {expandable:?}"
        );
        assert!(
            expandable.trim_end().ends_with("||"),
            "output: {expandable:?}"
        );

        // One line short of the threshold — must render the regular form (FR-006/FR-007).
        let regular = markdown_to_telegram_with_config(&input, 6);
        assert!(!regular.contains("**>"), "output: {regular:?}");
        assert!(!regular.ends_with("||"), "output: {regular:?}");
        assert!(regular.starts_with(">line 1"), "output: {regular:?}");
    }

    #[test]
    fn test_expandable_blockquote_min_lines_zero_disables_expandable_unconditionally() {
        let lines: Vec<String> = (1..=50).map(|i| format!("> line {i}")).collect();
        let input = lines.join("\n");
        let output = markdown_to_telegram_with_config(&input, 0);
        assert!(!output.contains("**>"), "output: {output:?}");
        assert!(!output.ends_with("||"), "output: {output:?}");
        for i in 1..=50 {
            assert!(output.contains(&format!(">line {i}")));
        }
    }

    #[test]
    fn test_nested_blockquote_flattens_to_single_level() {
        // Telegram MarkdownV2 has no nested-blockquote grammar: a blockquote is
        // exactly one leading '>' per line. A second '>' is an unescaped reserved
        // character that Telegram's parser rejects outright (400 Bad Request,
        // dropping the whole message) — so nested quotes must flatten to one level,
        // never accumulate as '>>'.
        let input = "> outer\n> > inner";
        let output = markdown_to_telegram(input);
        assert_eq!(output, ">outer\n>inner\n");
        assert!(
            !output.contains(">>"),
            "nested blockquote must never emit a doubled '>' prefix: {output:?}"
        );
    }

    #[test]
    fn test_blockquote_nesting_beyond_depth_cap_stays_bounded() {
        // Regression for the depth-cap guard (same defence-in-depth pattern as
        // MAX_CHUNK_DEPTH, #6595): nesting far past MAX_BLOCKQUOTE_NESTING_DEPTH must
        // not panic, hang, or produce a doubled '>' — it flattens exactly like a
        // shallow nested quote, just with all levels merged into the one tracked
        // (outermost) blockquote.
        let depth = 600; // comfortably past MAX_BLOCKQUOTE_NESTING_DEPTH (512)
        let input = format!("{}deep", "> ".repeat(depth));
        let output = markdown_to_telegram(&input);
        assert_eq!(output, ">deep\n");
        assert!(!output.contains(">>"), "output: {output:?}");
    }

    #[test]
    fn test_blockquote_nesting_within_depth_cap_still_flattens() {
        // A depth comfortably below the cap must behave identically to the
        // beyond-cap case above — the cap must never change *correctness*, only bound
        // the tracked-mark memory for pathological input.
        let depth = 20; // far below MAX_BLOCKQUOTE_NESTING_DEPTH — guard must never engage
        let input = format!("{}shallow", "> ".repeat(depth));
        let output = markdown_to_telegram(&input);
        assert_eq!(output, ">shallow\n");
        assert!(!output.contains(">>"), "output: {output:?}");
    }

    #[test]
    fn test_short_blockquote_unaffected_by_expandable_config() {
        // A single-line blockquote must render identically regardless of the configured
        // threshold — parity with pre-fix output for quotes below any reasonable threshold.
        let input = "> quote";
        let default_output = markdown_to_telegram(input);
        let custom_output = markdown_to_telegram_with_config(input, 1);
        assert_eq!(default_output, ">quote\n");
        // threshold=1 with a single-line quote meets the expandable condition (1 >= 1).
        assert!(custom_output.starts_with("**>quote"));
        assert!(custom_output.trim_end().ends_with("||"));
    }

    #[test]
    fn test_blockquote_line_with_special_chars_escaped_exactly_as_outside_blockquote() {
        // Spec 007-3 §10: a blockquote line containing MarkdownV2 special characters is
        // escaped exactly as it would be outside a blockquote — the '>' prefix is
        // prepended to the already-escaped content, not interleaved with escaping.
        let input = "> Special: . ! - + = | { }";
        let output = markdown_to_telegram(input);
        assert_eq!(output, ">Special: \\. \\! \\- \\+ \\= \\| \\{ \\}\n");
    }

    #[test]
    fn test_fenced_code_block_inside_blockquote_pins_current_behavior() {
        // Security/critic finding M1: whether Telegram's real MarkdownV2 parser wants
        // the code-fence delimiters ("```") prefixed with '>' like every other quoted
        // line, or wants code content excluded from per-line prefixing, is NOT
        // verified here — that needs a live Telegram client (spec 007-3 SC-006/SC-007
        // are still unrun). This test pins the CURRENT behavior (every captured line,
        // fences included, gets a single '>' prefix) so a future change to it is a
        // deliberate, reviewed decision instead of a silent regression either way.
        let input = "> ```\n> code line\n> ```";
        let output = markdown_to_telegram(input);
        assert_eq!(output, ">```\n>code line\n>```\n");
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
