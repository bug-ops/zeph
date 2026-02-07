/// Converts standard Markdown to Telegram `MarkdownV2` format.
///
/// Telegram `MarkdownV2` has different syntax:
/// - `*bold*` instead of `**bold**`
/// - Headers must be converted to bold text
/// - Horizontal rules are removed
/// - Special characters must be escaped
/// - Code blocks escape only backtick and backslash
pub fn markdown_to_telegram(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let lines = input.lines();
    let mut in_code_block = false;

    for line in lines {
        let trimmed = line.trim();

        // Check for code block delimiter
        if trimmed.starts_with("```") {
            // Add empty line before code block for separation
            if !in_code_block && !output.is_empty() {
                output.push('\n');
            }
            output.push_str("```");
            // Note: Don't include language specifier - Telegram MarkdownV2 may not support it
            output.push('\n');
            in_code_block = !in_code_block;
            continue;
        }

        // Inside code block: escape only ` and \
        if in_code_block {
            let escaped = escape_code_block(line);
            output.push_str(&escaped);
            output.push('\n');
            continue;
        }

        // Skip horizontal rules
        if trimmed.starts_with("---") || trimmed.starts_with("***") || trimmed.starts_with("___") {
            continue;
        }

        // Convert headers to bold text
        if let Some(header) = trimmed.strip_prefix("# ") {
            output.push('*');
            output.push_str(&escape_telegram(header));
            output.push_str("*\n");
            continue;
        }
        if let Some(header) = trimmed.strip_prefix("## ") {
            output.push('*');
            output.push_str(&escape_telegram(header));
            output.push_str("*\n");
            continue;
        }
        if let Some(header) = trimmed.strip_prefix("### ") {
            output.push('*');
            output.push_str(&escape_telegram(header));
            output.push_str("*\n");
            continue;
        }

        // Convert **bold** to *bold* and escape special characters
        let converted = convert_bold_and_escape(line);
        output.push_str(&converted);
        output.push('\n');
    }

    // Remove trailing newlines
    output.trim_end().to_string()
}

/// Escapes text inside code blocks.
///
/// In Telegram `MarkdownV2` code blocks, backtick and backslash must be escaped.
fn escape_code_block(text: &str) -> String {
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

/// Converts **bold** to *bold* and escapes special characters for Telegram `MarkdownV2`.
fn convert_bold_and_escape(text: &str) -> String {
    let mut result = String::with_capacity(text.len() * 2);
    let mut chars = text.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '*' {
            if let Some(&next) = chars.peek()
                && next == '*'
            {
                // Found ** - convert to single * for Telegram bold (don't escape)
                chars.next(); // consume second *
                result.push('*');
                continue;
            }
            // Single * should be escaped
            result.push('\\');
            result.push('*');
        } else if needs_escape(c) {
            // Escape special characters
            result.push('\\');
            result.push(c);
        } else {
            result.push(c);
        }
    }

    result
}

/// Checks if a character needs escaping in Telegram `MarkdownV2`.
///
/// Special characters that need escaping (per Telegram documentation).
fn needs_escape(c: char) -> bool {
    matches!(
        c,
        '_' | '['
            | ']'
            | '('
            | ')'
            | '~'
            | '`'
            | '>'
            | '#'
            | '+'
            | '-'
            | '='
            | '|'
            | '{'
            | '}'
            | '.'
            | '!'
    )
}

/// Escapes special characters for Telegram `MarkdownV2`.
///
/// Escapes all reserved characters per Telegram documentation.
fn escape_telegram(text: &str) -> String {
    let mut result = String::with_capacity(text.len() * 2);

    for c in text.chars() {
        match c {
            '_' | '[' | ']' | '(' | ')' | '~' | '`' | '>' | '#' | '+' | '-' | '=' | '|' | '{'
            | '}' | '.' | '!' => {
                result.push('\\');
                result.push(c);
            }
            _ => result.push(c),
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_convert_headers() {
        let input = "# Header 1\n## Header 2\n### Header 3";
        let output = markdown_to_telegram(input);
        assert!(output.contains("*Header 1*"));
        assert!(output.contains("*Header 2*"));
        assert!(output.contains("*Header 3*"));
    }

    #[test]
    fn test_remove_horizontal_rules() {
        let input = "Text\n---\nMore text\n***\nEnd";
        let output = markdown_to_telegram(input);
        assert!(!output.contains("---"));
        assert!(!output.contains("***"));
    }

    #[test]
    fn test_convert_bold_and_escape() {
        let input = "This is **bold** text with - and .";
        let output = convert_bold_and_escape(input);
        assert_eq!(output, "This is *bold* text with \\- and \\.");
    }

    #[test]
    fn test_escape_parentheses() {
        let input = "Text with (parentheses) and **bold**";
        let output = convert_bold_and_escape(input);
        assert_eq!(output, "Text with \\(parentheses\\) and *bold*");
    }

    #[test]
    fn test_escape_pipes() {
        let input = "Table | with | pipes";
        let output = convert_bold_and_escape(input);
        assert_eq!(output, "Table \\| with \\| pipes");
    }

    #[test]
    fn test_code_block_escaping() {
        let input = "```bash\nfind ~/Documents -type f \\( -iname \"*.jpg\" \\)\n```";
        let output = markdown_to_telegram(input);
        // Inside code blocks, only backtick and backslash are escaped
        // Language specifier is removed for Telegram compatibility
        assert!(output.starts_with("```\n"));
        assert!(output.contains("find ~/Documents -type f \\\\( -iname"));
        assert!(output.ends_with("\n```"));
    }

    #[test]
    fn test_escape_special_chars() {
        let input = "Text with . and ! and -";
        let output = escape_telegram(input);
        assert_eq!(output, "Text with \\. and \\! and \\-");
    }
}
