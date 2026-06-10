// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::HashMap;
use std::sync::LazyLock;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use tree_sitter::Language;
use tree_sitter_highlight::{HighlightConfiguration, HighlightEvent, Highlighter};

use crate::theme::SyntaxTheme;

const CAPTURE_NAMES: &[&str] = &[
    "attribute",
    "boolean",
    "comment",
    "constant",
    "constant.builtin",
    "constructor",
    "escape",
    "function",
    "function.builtin",
    "function.call",
    "function.method",
    "keyword",
    "keyword.operator",
    "label",
    "number",
    "operator",
    "property",
    "punctuation",
    "punctuation.bracket",
    "punctuation.delimiter",
    "punctuation.special",
    "storageclass",
    "string",
    "string.escape",
    "string.special",
    "tag",
    "text.literal",
    "text.reference",
    "text.title",
    "text.uri",
    "type",
    "type.builtin",
    "type.qualifier",
    "variable",
    "variable.builtin",
    "variable.parameter",
];

// Custom bash query because tree-sitter-bash doesn't bundle a highlights.scm
// compatible with tree-sitter-highlight's capture convention.
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
        ("ts", "typescript"),
        ("tsx", "typescript"),
        ("mts", "typescript"),
        ("cts", "typescript"),
        ("golang", "go"),
        ("yml", "yaml"),
        ("md", "markdown"),
        ("mysql", "sql"),
        ("psql", "sql"),
        ("postgres", "sql"),
        ("sequel", "sql"),
    ])
});

/// Process-wide singleton [`SyntaxHighlighter`], initialised on first access.
///
/// Use this instead of constructing a new highlighter to avoid redundant
/// tree-sitter grammar compilation on each frame.
///
/// # Examples
///
/// ```rust
/// use zeph_tui::highlight::SYNTAX_HIGHLIGHTER;
/// use zeph_tui::theme::SyntaxTheme;
///
/// let theme = SyntaxTheme::default();
/// let spans = SYNTAX_HIGHLIGHTER.highlight("rust", "let x = 1;", &theme);
/// assert!(spans.is_some());
/// ```
pub static SYNTAX_HIGHLIGHTER: LazyLock<SyntaxHighlighter> = LazyLock::new(SyntaxHighlighter::new);

/// Tree-sitter-based syntax highlighter for TUI code blocks.
///
/// Supports Rust, Python, JavaScript, TypeScript, Go, JSON, TOML, YAML, SQL,
/// Markdown (block-level), and Bash out of the box. Language aliases
/// (`"rs"` → `"rust"`, `"ts"` → `"typescript"`, `"yml"` → `"yaml"`, etc.)
/// are resolved transparently.
///
/// Construct via the [`SYNTAX_HIGHLIGHTER`] static for process-level sharing,
/// or call the private `new` method directly in tests.
///
/// # Supported languages
///
/// | Identifier   | Aliases                          |
/// |--------------|----------------------------------|
/// | `rust`       | `rs`                             |
/// | `python`     | `py`                             |
/// | `javascript` | `js`                             |
/// | `typescript` | `ts`, `tsx`, `mts`, `cts`        |
/// | `go`         | `golang`                         |
/// | `bash`       | `sh`, `shell`                    |
/// | `json`       | —                                |
/// | `toml`       | —                                |
/// | `yaml`       | `yml`                            |
/// | `sql`        | `mysql`, `psql`, `postgres`, `sequel` |
/// | `markdown`   | `md`                             |
///
/// # Examples
///
/// ```rust
/// use zeph_tui::highlight::SYNTAX_HIGHLIGHTER;
/// use zeph_tui::theme::SyntaxTheme;
///
/// let theme = SyntaxTheme::default();
///
/// // Known language → styled spans
/// let spans = SYNTAX_HIGHLIGHTER.highlight("rust", "fn main() {}", &theme);
/// assert!(spans.is_some());
///
/// // Alias works the same way
/// let spans = SYNTAX_HIGHLIGHTER.highlight("rs", "let x = 1;", &theme);
/// assert!(spans.is_some());
///
/// // Unknown language → None
/// assert!(SYNTAX_HIGHLIGHTER.highlight("brainfuck", "+++", &theme).is_none());
/// ```
pub struct SyntaxHighlighter {
    configs: HashMap<&'static str, HighlightConfiguration>,
}

impl SyntaxHighlighter {
    #[allow(clippy::too_many_lines)]
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

        // TypeScript grammar only ships a delta query over JS — register TS with
        // the full JS query prepended so string/comment/function highlighting works.
        let ts_query = [
            tree_sitter_javascript::HIGHLIGHT_QUERY,
            "\n",
            tree_sitter_typescript::HIGHLIGHTS_QUERY,
        ]
        .concat();
        register(
            "typescript",
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            "typescript",
            &ts_query,
            "",
        );

        register(
            "go",
            tree_sitter_go::LANGUAGE.into(),
            "go",
            tree_sitter_go::HIGHLIGHTS_QUERY,
            "",
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
            "yaml",
            tree_sitter_yaml::LANGUAGE.into(),
            "yaml",
            tree_sitter_yaml::HIGHLIGHTS_QUERY,
            "",
        );

        register(
            "sql",
            tree_sitter_sequel::LANGUAGE.into(),
            "sql",
            tree_sitter_sequel::HIGHLIGHTS_QUERY,
            "",
        );

        register(
            "bash",
            tree_sitter_bash::LANGUAGE.into(),
            "bash",
            BASH_HIGHLIGHTS_QUERY,
            "",
        );

        // Markdown block-level highlighting only. The block grammar covers
        // headings, markers, links, and code fence delimiters.
        register(
            "markdown",
            tree_sitter_md::LANGUAGE.into(),
            "markdown",
            tree_sitter_md::HIGHLIGHT_QUERY_BLOCK,
            tree_sitter_md::INJECTION_QUERY_BLOCK,
        );

        Self { configs }
    }

    /// Highlight `code` for the given `lang` using `theme`.
    ///
    /// Returns `None` if the language is unsupported or if tree-sitter fails
    /// to parse the input. The returned spans concatenate to the original
    /// source text unchanged.
    ///
    /// # Arguments
    ///
    /// * `lang` — language identifier or alias (case-insensitive).
    /// * `code` — source code to highlight.
    /// * `theme` — style mapping for each token class.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_tui::highlight::SYNTAX_HIGHLIGHTER;
    /// use zeph_tui::theme::SyntaxTheme;
    ///
    /// let theme = SyntaxTheme::default();
    /// let spans = SYNTAX_HIGHLIGHTER.highlight("rust", "let x = 42;", &theme).unwrap();
    /// let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
    /// assert_eq!(text, "let x = 42;");
    /// ```
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
        "attribute" | "storageclass" => theme.attribute,
        "boolean" | "constant" | "constant.builtin" => theme.constant,
        "comment" => theme.comment,
        "constructor" | "type" | "type.builtin" | "type.qualifier" | "tag" => theme.r#type,
        "escape" | "string" | "string.escape" | "string.special" | "text.literal" => theme.string,
        "function" | "function.builtin" | "function.call" | "function.method"
        | "text.reference" | "text.uri" => theme.function,
        "keyword" | "keyword.operator" | "text.title" => theme.keyword,
        "label" | "property" | "variable" | "variable.builtin" | "variable.parameter" => {
            theme.variable
        }
        "number" => theme.number,
        "operator" => theme.operator,
        "punctuation" | "punctuation.bracket" | "punctuation.delimiter" | "punctuation.special" => {
            theme.punctuation
        }
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
    use std::collections::HashSet;

    use super::*;

    // ---- existing tests ----

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

    // ---- new language tests ----

    fn assert_multi_style(lang: &str, code: &str) {
        let hl = &*SYNTAX_HIGHLIGHTER;
        let theme = SyntaxTheme::default();
        let spans = hl.highlight(lang, code, &theme).unwrap_or_else(|| {
            panic!("highlight returned None for language '{lang}'");
        });
        let styles: HashSet<_> = spans.iter().map(|s| s.style).collect();
        assert!(
            styles.len() > 1,
            "expected >1 distinct style for '{lang}', got {}: {:?}",
            styles.len(),
            spans.iter().map(|s| s.content.as_ref()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn highlight_typescript_multi_style() {
        assert_multi_style(
            "typescript",
            "const greet = (name: string): void => {\n  console.log(name);\n};",
        );
    }

    #[test]
    fn highlight_typescript_alias_ts() {
        let hl = &*SYNTAX_HIGHLIGHTER;
        let theme = SyntaxTheme::default();
        assert!(hl.highlight("ts", "let x: number = 1;", &theme).is_some());
    }

    #[test]
    fn highlight_typescript_alias_tsx() {
        let hl = &*SYNTAX_HIGHLIGHTER;
        let theme = SyntaxTheme::default();
        assert!(
            hl.highlight("tsx", "const x: string = 'hello';", &theme)
                .is_some()
        );
    }

    #[test]
    fn highlight_go_multi_style() {
        assert_multi_style(
            "go",
            "package main\n\nimport \"fmt\"\n\nfunc main() {\n    fmt.Println(\"hello\")\n}",
        );
    }

    #[test]
    fn highlight_go_alias_golang() {
        let hl = &*SYNTAX_HIGHLIGHTER;
        let theme = SyntaxTheme::default();
        assert!(hl.highlight("golang", "func f() {}", &theme).is_some());
    }

    #[test]
    fn highlight_yaml_multi_style() {
        assert_multi_style("yaml", "name: foo\nversion: \"1.0\"\nenabled: true\n");
    }

    #[test]
    fn highlight_yaml_alias_yml() {
        let hl = &*SYNTAX_HIGHLIGHTER;
        let theme = SyntaxTheme::default();
        assert!(
            hl.highlight("yml", "key: value\nlist:\n  - a\n  - b", &theme)
                .is_some()
        );
    }

    #[test]
    fn highlight_sql_multi_style() {
        assert_multi_style("sql", "SELECT id, name FROM users WHERE active = true;");
    }

    #[test]
    fn highlight_sql_alias_mysql() {
        let hl = &*SYNTAX_HIGHLIGHTER;
        let theme = SyntaxTheme::default();
        assert!(hl.highlight("mysql", "SELECT * FROM t;", &theme).is_some());
    }

    #[test]
    fn highlight_markdown_headings_and_links() {
        assert_multi_style(
            "markdown",
            "# Hello World\n\n[link text](https://example.com)\n",
        );
    }

    #[test]
    fn highlight_markdown_alias_md() {
        let hl = &*SYNTAX_HIGHLIGHTER;
        let theme = SyntaxTheme::default();
        assert!(hl.highlight("md", "# Title\nSome text.", &theme).is_some());
    }

    #[test]
    fn highlight_unknown_still_none_after_new_langs() {
        let hl = &*SYNTAX_HIGHLIGHTER;
        let theme = SyntaxTheme::default();
        assert!(hl.highlight("brainfuck", "+++", &theme).is_none());
        assert!(hl.highlight("cobol", "DISPLAY 'hi'", &theme).is_none());
    }
}
