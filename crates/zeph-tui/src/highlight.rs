use std::collections::HashMap;
use std::sync::LazyLock;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use tree_sitter::Language;
use tree_sitter_highlight::{HighlightConfiguration, HighlightEvent, Highlighter};

use crate::theme::SyntaxTheme;

const CAPTURE_NAMES: &[&str] = &[
    "attribute",
    "comment",
    "constant",
    "constant.builtin",
    "constructor",
    "function",
    "function.builtin",
    "keyword",
    "number",
    "operator",
    "property",
    "punctuation",
    "punctuation.bracket",
    "punctuation.delimiter",
    "string",
    "string.escape",
    "type",
    "type.builtin",
    "variable",
    "variable.builtin",
    "variable.parameter",
];

const BASH_HIGHLIGHTS_QUERY: &str = r#"
[(string) (raw_string) (heredoc_body) (heredoc_start)] @string
(command_name) @function
(variable_name) @property
["case" "do" "done" "elif" "else" "esac" "export" "fi" "for" "function" "if" "in" "select" "then" "unset" "until" "while"] @keyword
(comment) @comment
(function_definition name: (word) @function)
(file_descriptor) @number
["$" "&&" ">" ">>" "<" "|"] @operator
((command (_) @constant) (#match? @constant "^-"))
"#;

static LANG_ALIASES: LazyLock<HashMap<&'static str, &'static str>> = LazyLock::new(|| {
    HashMap::from([
        ("rs", "rust"),
        ("py", "python"),
        ("js", "javascript"),
        ("sh", "bash"),
        ("shell", "bash"),
    ])
});

pub static SYNTAX_HIGHLIGHTER: LazyLock<SyntaxHighlighter> = LazyLock::new(SyntaxHighlighter::new);

pub struct SyntaxHighlighter {
    configs: HashMap<&'static str, HighlightConfiguration>,
}

impl SyntaxHighlighter {
    fn new() -> Self {
        let mut configs = HashMap::new();

        let mut register = |name: &'static str,
                            language: Language,
                            lang_name: &str,
                            highlights_query: &str,
                            injections_query: &str| {
            let Ok(mut config) = HighlightConfiguration::new(
                language,
                lang_name.to_string(),
                highlights_query,
                injections_query,
                "",
            ) else {
                return;
            };
            config.configure(CAPTURE_NAMES);
            configs.insert(name, config);
        };

        register(
            "rust",
            tree_sitter_rust::LANGUAGE.into(),
            "rust",
            tree_sitter_rust::HIGHLIGHTS_QUERY,
            tree_sitter_rust::INJECTIONS_QUERY,
        );

        register(
            "python",
            tree_sitter_python::LANGUAGE.into(),
            "python",
            tree_sitter_python::HIGHLIGHTS_QUERY,
            "",
        );

        register(
            "javascript",
            tree_sitter_javascript::LANGUAGE.into(),
            "javascript",
            tree_sitter_javascript::HIGHLIGHT_QUERY,
            tree_sitter_javascript::INJECTIONS_QUERY,
        );

        register(
            "json",
            tree_sitter_json::LANGUAGE.into(),
            "json",
            tree_sitter_json::HIGHLIGHTS_QUERY,
            "",
        );

        register(
            "toml",
            tree_sitter_toml_ng::LANGUAGE.into(),
            "toml",
            tree_sitter_toml_ng::HIGHLIGHTS_QUERY,
            "",
        );

        register(
            "bash",
            tree_sitter_bash::LANGUAGE.into(),
            "bash",
            BASH_HIGHLIGHTS_QUERY,
            "",
        );

        Self { configs }
    }

    pub fn highlight(
        &self,
        lang: &str,
        code: &str,
        theme: &SyntaxTheme,
    ) -> Option<Vec<Span<'static>>> {
        let lang_lower = lang.to_lowercase();
        let canonical = LANG_ALIASES
            .get(lang_lower.as_str())
            .copied()
            .unwrap_or(lang_lower.as_str());
        let config = self.configs.get(canonical)?;

        let mut highlighter = Highlighter::new();
        let events = highlighter
            .highlight(config, code.as_bytes(), None, |_| None)
            .ok()?;

        let mut spans = Vec::new();
        let mut style_stack: Vec<Style> = Vec::new();

        for event in events {
            match event.ok()? {
                HighlightEvent::Source { start, end } => {
                    let text = code.get(start..end).unwrap_or_default();
                    let style = style_stack.last().copied().unwrap_or(theme.default);
                    spans.push(Span::styled(text.to_string(), style));
                }
                HighlightEvent::HighlightStart(highlight) => {
                    let style = capture_to_style(highlight.0, theme);
                    style_stack.push(style);
                }
                HighlightEvent::HighlightEnd => {
                    style_stack.pop();
                }
            }
        }

        Some(spans)
    }
}

fn capture_to_style(index: usize, theme: &SyntaxTheme) -> Style {
    match CAPTURE_NAMES.get(index).copied().unwrap_or_default() {
        "attribute" => theme.attribute,
        "comment" => theme.comment,
        "constant" | "constant.builtin" => theme.constant,
        "constructor" | "type" | "type.builtin" => theme.r#type,
        "function" | "function.builtin" => theme.function,
        "keyword" => theme.keyword,
        "number" => theme.number,
        "operator" => theme.operator,
        "property" | "variable" | "variable.builtin" | "variable.parameter" => theme.variable,
        "punctuation" | "punctuation.bracket" | "punctuation.delimiter" => theme.punctuation,
        "string" | "string.escape" => theme.string,
        _ => theme.default,
    }
}

impl Default for SyntaxTheme {
    fn default() -> Self {
        Self {
            keyword: Style::default()
                .fg(Color::Rgb(198, 120, 221))
                .add_modifier(Modifier::BOLD),
            string: Style::default().fg(Color::Rgb(152, 195, 121)),
            comment: Style::default()
                .fg(Color::Rgb(92, 99, 112))
                .add_modifier(Modifier::ITALIC),
            function: Style::default().fg(Color::Rgb(97, 175, 239)),
            r#type: Style::default().fg(Color::Rgb(229, 192, 123)),
            number: Style::default().fg(Color::Rgb(209, 154, 102)),
            operator: Style::default().fg(Color::Rgb(171, 178, 191)),
            variable: Style::default().fg(Color::Rgb(224, 108, 117)),
            attribute: Style::default().fg(Color::Rgb(229, 192, 123)),
            punctuation: Style::default().fg(Color::Rgb(171, 178, 191)),
            constant: Style::default().fg(Color::Rgb(209, 154, 102)),
            default: Style::default().fg(Color::Rgb(190, 175, 145)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn highlight_rust_code() {
        let hl = &*SYNTAX_HIGHLIGHTER;
        let theme = SyntaxTheme::default();
        let spans = hl.highlight("rust", "let x = 42;", &theme);
        assert!(spans.is_some());
        let spans = spans.unwrap();
        assert!(!spans.is_empty());
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "let x = 42;");
    }

    #[test]
    fn highlight_python_code() {
        let hl = &*SYNTAX_HIGHLIGHTER;
        let theme = SyntaxTheme::default();
        let spans = hl.highlight("python", "def foo():\n    pass", &theme);
        assert!(spans.is_some());
    }

    #[test]
    fn highlight_unknown_lang_returns_none() {
        let hl = &*SYNTAX_HIGHLIGHTER;
        let theme = SyntaxTheme::default();
        assert!(hl.highlight("brainfuck", "+++", &theme).is_none());
    }

    #[test]
    fn highlight_json_code() {
        let hl = &*SYNTAX_HIGHLIGHTER;
        let theme = SyntaxTheme::default();
        let spans = hl.highlight("json", r#"{"key": "value"}"#, &theme);
        assert!(spans.is_some());
    }

    #[test]
    fn highlight_js_code() {
        let hl = &*SYNTAX_HIGHLIGHTER;
        let theme = SyntaxTheme::default();
        let spans = hl.highlight("js", "const x = 1;", &theme);
        assert!(spans.is_some());
    }

    #[test]
    fn highlight_alias_rs() {
        let hl = &*SYNTAX_HIGHLIGHTER;
        let theme = SyntaxTheme::default();
        assert!(hl.highlight("rs", "fn main() {}", &theme).is_some());
    }

    #[test]
    fn highlight_empty_string() {
        let hl = &*SYNTAX_HIGHLIGHTER;
        let theme = SyntaxTheme::default();
        let spans = hl.highlight("rust", "", &theme);
        assert!(spans.is_some());
        assert!(spans.unwrap().is_empty());
    }

    #[test]
    fn highlight_malformed_code_no_panic() {
        let hl = &*SYNTAX_HIGHLIGHTER;
        let theme = SyntaxTheme::default();
        // Malformed Rust — should not panic, tree-sitter is error-tolerant
        let spans = hl.highlight("rust", "fn {{{{ let !!!", &theme);
        assert!(spans.is_some());
    }

    #[test]
    fn highlight_toml_code() {
        let hl = &*SYNTAX_HIGHLIGHTER;
        let theme = SyntaxTheme::default();
        let spans = hl.highlight("toml", "[package]\nname = \"foo\"", &theme);
        assert!(spans.is_some());
    }

    #[test]
    fn highlight_bash_code() {
        let hl = &*SYNTAX_HIGHLIGHTER;
        let theme = SyntaxTheme::default();
        let spans = hl.highlight("bash", "echo \"hello\"", &theme);
        assert!(spans.is_some());
    }

    #[test]
    fn rust_keywords_get_keyword_style() {
        let hl = &*SYNTAX_HIGHLIGHTER;
        let theme = SyntaxTheme::default();
        let spans = hl.highlight("rust", "let x = 1;", &theme).unwrap();
        let let_span = spans.iter().find(|s| s.content.as_ref() == "let").unwrap();
        assert_eq!(let_span.style, theme.keyword);
    }
}
