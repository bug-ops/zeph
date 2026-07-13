// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Language detection and tree-sitter grammar registry.
//!
//! The central type is [`Lang`], an enum of every language supported by the
//! indexing pipeline. Each variant carries its own:
//!
//! * tree-sitter grammar ([`Lang::grammar`])
//! * compiled symbol query ([`Lang::symbol_query`])
//! * compiled method query ([`Lang::method_query`])
//! * named entity node kinds used for chunk boundaries ([`Lang::entity_node_kinds`])
//!
//! Top-level helpers:
//!
//! * [`detect_language`] — map a file extension to a [`Lang`] variant.
//! * [`is_indexable`] — return `true` when a file has both a supported language
//!   and an available grammar (used by the directory walker to skip unsupported files).

use std::path::Path;
use std::sync::LazyLock;

use serde::{Deserialize, Serialize};

// ts-query source strings for symbol and method extraction.
// Shared symbol queries are sourced from zeph-common::treesitter.
use zeph_common::treesitter::{
    GO_SYM_Q, JS_SYM_Q, PYTHON_SYM_Q, RUST_SYM_Q, TS_SYM_Q, compile_query, lang_for_ext,
};

const RUST_METHOD_Q: &str = "
(impl_item body: (declaration_list
  (function_item (visibility_modifier)? @vis name: (identifier) @name) @def))
";

const PYTHON_METHOD_Q: &str = "
(class_definition body: (block
  (function_definition name: (identifier) @name) @def))
";

/// A programming language or file format supported by the indexing pipeline.
///
/// Each variant corresponds to a tree-sitter grammar bundled as a workspace
/// dependency. The variant is used throughout the pipeline to select the correct
/// grammar, query, and entity-node kinds.
///
/// # Serialization
///
/// Serializes to lowercase strings (`"rust"`, `"python"`, …) via `serde`.
///
/// # Examples
///
/// ```
/// use zeph_index::languages::{Lang, detect_language};
/// use std::path::Path;
///
/// let lang = detect_language(Path::new("src/main.rs")).unwrap();
/// assert_eq!(lang, Lang::Rust);
/// assert_eq!(lang.id(), "rust");
/// assert!(lang.grammar().is_some());
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum Lang {
    /// The Rust programming language (`*.rs`).
    Rust,
    /// Python 3 (`*.py`, `*.pyi`).
    Python,
    /// JavaScript including JSX (`*.js`, `*.jsx`, `*.mjs`, `*.cjs`).
    JavaScript,
    /// TypeScript including TSX (`*.ts`, `*.tsx`, `*.mts`, `*.cts`).
    TypeScript,
    /// Go (`*.go`).
    Go,
    /// Bash / shell scripts (`*.sh`, `*.bash`, `*.zsh`).
    Bash,
    /// TOML configuration files (`*.toml`).
    Toml,
    /// JSON and JSONC (`*.json`, `*.jsonc`).
    Json,
    /// Markdown documents (`*.md`, `*.markdown`).
    Markdown,
}

impl Lang {
    /// Parse a [`Lang`] from the short lowercase identifier used in Qdrant payloads.
    ///
    /// Returns `None` when the string does not match any known language id.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_index::languages::Lang;
    ///
    /// assert_eq!(Lang::from_id("rust"), Some(Lang::Rust));
    /// assert_eq!(Lang::from_id("typescript"), Some(Lang::TypeScript));
    /// assert_eq!(Lang::from_id("unknown"), None);
    /// ```
    #[must_use]
    pub fn from_id(s: &str) -> Option<Self> {
        match s {
            "rust" => Some(Self::Rust),
            "python" => Some(Self::Python),
            "javascript" => Some(Self::JavaScript),
            "typescript" => Some(Self::TypeScript),
            "go" => Some(Self::Go),
            "bash" => Some(Self::Bash),
            "toml" => Some(Self::Toml),
            "json" => Some(Self::Json),
            "markdown" => Some(Self::Markdown),
            _ => None,
        }
    }

    /// Short lowercase identifier stored in Qdrant payload and config fields.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_index::languages::Lang;
    ///
    /// assert_eq!(Lang::Rust.id(), "rust");
    /// assert_eq!(Lang::TypeScript.id(), "typescript");
    /// ```
    #[must_use]
    pub fn id(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::Python => "python",
            Self::JavaScript => "javascript",
            Self::TypeScript => "typescript",
            Self::Go => "go",
            Self::Bash => "bash",
            Self::Toml => "toml",
            Self::Json => "json",
            Self::Markdown => "markdown",
        }
    }

    /// Return the tree-sitter grammar for this language, if available.
    ///
    /// All current [`Lang`] variants have a grammar; this returns `Option` so
    /// callers can handle future variants gracefully without a compile-time break.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_index::languages::Lang;
    ///
    /// assert!(Lang::Rust.grammar().is_some());
    /// assert!(Lang::Markdown.grammar().is_some());
    /// ```
    #[must_use]
    pub fn grammar(self) -> Option<tree_sitter::Language> {
        // Delegate to the shared extension-to-grammar mapping via each variant's
        // canonical extension, instead of re-listing every `tree_sitter_*::LANGUAGE`
        // construction here (that list already lives in `lang_for_ext`).
        let ext = match self {
            Self::Rust => "rs",
            Self::Python => "py",
            Self::JavaScript => "js",
            Self::TypeScript => "ts",
            Self::Go => "go",
            Self::Bash => "sh",
            Self::Toml => "toml",
            Self::Json => "json",
            Self::Markdown => "md",
        };
        lang_for_ext(ext)
    }

    /// Compiled ts-query for extracting top-level symbols (name + visibility capture).
    ///
    /// Returns `None` when the query fails to compile (e.g. grammar version mismatch).
    /// Callers fall back to heuristic extraction.
    #[must_use]
    pub fn symbol_query(self) -> Option<&'static tree_sitter::Query> {
        match self {
            Self::Rust => {
                static Q: LazyLock<Option<tree_sitter::Query>> = LazyLock::new(|| {
                    let lang: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();
                    compile_query(&lang, RUST_SYM_Q, "rust symbol")
                });
                Q.as_ref()
            }
            Self::Python => {
                static Q: LazyLock<Option<tree_sitter::Query>> = LazyLock::new(|| {
                    let lang: tree_sitter::Language = tree_sitter_python::LANGUAGE.into();
                    compile_query(&lang, PYTHON_SYM_Q, "python symbol")
                });
                Q.as_ref()
            }
            Self::JavaScript => {
                static Q: LazyLock<Option<tree_sitter::Query>> = LazyLock::new(|| {
                    let lang: tree_sitter::Language = tree_sitter_javascript::LANGUAGE.into();
                    compile_query(&lang, JS_SYM_Q, "js symbol")
                });
                Q.as_ref()
            }
            Self::TypeScript => {
                static Q: LazyLock<Option<tree_sitter::Query>> = LazyLock::new(|| {
                    let lang: tree_sitter::Language =
                        tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into();
                    compile_query(&lang, TS_SYM_Q, "ts symbol")
                });
                Q.as_ref()
            }
            Self::Go => {
                static Q: LazyLock<Option<tree_sitter::Query>> = LazyLock::new(|| {
                    let lang: tree_sitter::Language = tree_sitter_go::LANGUAGE.into();
                    compile_query(&lang, GO_SYM_Q, "go symbol")
                });
                Q.as_ref()
            }
            _ => None,
        }
    }

    /// Compiled ts-query for extracting methods inside impl/class bodies.
    ///
    /// Returns `None` when query compilation fails.
    #[must_use]
    pub fn method_query(self) -> Option<&'static tree_sitter::Query> {
        match self {
            Self::Rust => {
                static Q: LazyLock<Option<tree_sitter::Query>> = LazyLock::new(|| {
                    let lang: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();
                    compile_query(&lang, RUST_METHOD_Q, "rust method")
                });
                Q.as_ref()
            }
            Self::Python => {
                static Q: LazyLock<Option<tree_sitter::Query>> = LazyLock::new(|| {
                    let lang: tree_sitter::Language = tree_sitter_python::LANGUAGE.into();
                    compile_query(&lang, PYTHON_METHOD_Q, "python method")
                });
                Q.as_ref()
            }
            _ => None,
        }
    }

    /// Return the tree-sitter node kinds that delimit named entities.
    ///
    /// The [`crate::chunker`] uses this list to decide chunk boundaries: only
    /// nodes whose kind appears in this list are considered "interesting" for
    /// chunk creation. Languages like TOML and JSON return an empty slice, which
    /// causes the chunker to emit a single file-level chunk instead.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_index::languages::Lang;
    ///
    /// let rust_kinds = Lang::Rust.entity_node_kinds();
    /// assert!(rust_kinds.contains(&"function_item"));
    /// assert!(rust_kinds.contains(&"impl_item"));
    ///
    /// // Config formats have no named entities — single file chunk.
    /// assert!(Lang::Toml.entity_node_kinds().is_empty());
    /// ```
    #[must_use]
    pub fn entity_node_kinds(self) -> &'static [&'static str] {
        match self {
            Self::Rust => &[
                "function_item",
                "struct_item",
                "enum_item",
                "trait_item",
                "impl_item",
                "type_item",
                "const_item",
                "static_item",
                "macro_definition",
                "mod_item",
            ],
            Self::Python => &[
                "function_definition",
                "class_definition",
                "decorated_definition",
            ],
            Self::JavaScript | Self::TypeScript => &[
                "function_declaration",
                "class_declaration",
                "method_definition",
                "arrow_function",
                "export_statement",
                "lexical_declaration",
            ],
            Self::Go => &[
                "function_declaration",
                "method_declaration",
                "type_declaration",
                "const_declaration",
            ],
            _ => &[],
        }
    }
}

impl std::fmt::Display for Lang {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.pad(self.id())
    }
}

/// Detect the language of a file based on its extension.
///
/// Returns `None` for extensions not supported by any tree-sitter grammar in this crate.
///
/// # Examples
///
/// ```
/// use std::path::Path;
/// use zeph_index::languages::{Lang, detect_language};
///
/// assert_eq!(detect_language(Path::new("main.rs")), Some(Lang::Rust));
/// assert_eq!(detect_language(Path::new("script.py")), Some(Lang::Python));
/// assert_eq!(detect_language(Path::new("unknown.xyz")), None);
/// assert_eq!(detect_language(Path::new("no_extension")), None);
/// ```
#[must_use]
pub fn detect_language(path: &Path) -> Option<Lang> {
    let ext = path.extension()?.to_str()?;
    // `lang_for_ext` is the single source of truth for which extensions have
    // tree-sitter support; gating on it here means this match can never accept
    // an extension the shared grammar lookup would reject.
    lang_for_ext(ext)?;
    match ext {
        "rs" => Some(Lang::Rust),
        "py" | "pyi" => Some(Lang::Python),
        "js" | "jsx" | "mjs" | "cjs" => Some(Lang::JavaScript),
        "ts" | "tsx" | "mts" | "cts" => Some(Lang::TypeScript),
        "go" => Some(Lang::Go),
        "sh" | "bash" | "zsh" => Some(Lang::Bash),
        "toml" => Some(Lang::Toml),
        "json" | "jsonc" => Some(Lang::Json),
        "md" | "markdown" => Some(Lang::Markdown),
        _ => None,
    }
}

/// Return `true` if `path` has a supported language **and** an available tree-sitter grammar.
///
/// Used by the directory walker to quickly filter out files that cannot be indexed.
/// Returns `false` for unrecognized extensions and for any language whose grammar
/// fails to load (which should not happen in practice with the bundled grammars).
///
/// # Examples
///
/// ```
/// use std::path::Path;
/// use zeph_index::languages::is_indexable;
///
/// assert!(is_indexable(Path::new("src/lib.rs")));
/// assert!(is_indexable(Path::new("config.toml")));
/// assert!(!is_indexable(Path::new("image.png")));
/// assert!(!is_indexable(Path::new("Makefile")));
/// ```
#[must_use]
pub fn is_indexable(path: &Path) -> bool {
    detect_language(path).and_then(Lang::grammar).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_language_rs() {
        assert_eq!(detect_language(Path::new("src/main.rs")), Some(Lang::Rust));
    }

    #[test]
    fn detect_language_py() {
        assert_eq!(detect_language(Path::new("script.py")), Some(Lang::Python));
    }

    #[test]
    fn detect_language_js_variants() {
        for ext in &["js", "jsx", "mjs", "cjs"] {
            let path = format!("file.{ext}");
            assert_eq!(
                detect_language(Path::new(&path)),
                Some(Lang::JavaScript),
                "failed for .{ext}"
            );
        }
    }

    #[test]
    fn detect_language_ts_variants() {
        for ext in &["ts", "tsx", "mts", "cts"] {
            let path = format!("file.{ext}");
            assert_eq!(
                detect_language(Path::new(&path)),
                Some(Lang::TypeScript),
                "failed for .{ext}"
            );
        }
    }

    #[test]
    fn detect_language_unknown_ext_returns_none() {
        assert_eq!(detect_language(Path::new("file.xyz")), None);
        assert_eq!(detect_language(Path::new("file")), None);
    }

    #[test]
    fn detect_language_go() {
        assert_eq!(detect_language(Path::new("main.go")), Some(Lang::Go));
    }

    /// Covers the four extension groups added to `lang_for_ext` by #5971
    /// (bash, toml, json, markdown) — previously only exercised indirectly
    /// via `grammar_returns_some_for_all_langs`, never through `detect_language`
    /// itself, leaving the extension-string match arms without direct coverage.
    #[test]
    fn detect_language_bash_variants() {
        for ext in &["sh", "bash", "zsh"] {
            let path = format!("file.{ext}");
            assert_eq!(
                detect_language(Path::new(&path)),
                Some(Lang::Bash),
                "failed for .{ext}"
            );
        }
    }

    #[test]
    fn detect_language_toml() {
        assert_eq!(detect_language(Path::new("Cargo.toml")), Some(Lang::Toml));
    }

    #[test]
    fn detect_language_json_variants() {
        for ext in &["json", "jsonc"] {
            let path = format!("file.{ext}");
            assert_eq!(
                detect_language(Path::new(&path)),
                Some(Lang::Json),
                "failed for .{ext}"
            );
        }
    }

    #[test]
    fn detect_language_markdown_variants() {
        for ext in &["md", "markdown"] {
            let path = format!("file.{ext}");
            assert_eq!(
                detect_language(Path::new(&path)),
                Some(Lang::Markdown),
                "failed for .{ext}"
            );
        }
    }

    #[test]
    fn entity_node_kinds_rust_includes_function_item() {
        let kinds = Lang::Rust.entity_node_kinds();
        assert!(kinds.contains(&"function_item"));
        assert!(kinds.contains(&"impl_item"));
        assert!(kinds.contains(&"struct_item"));
    }

    #[test]
    fn entity_node_kinds_config_empty() {
        assert!(Lang::Toml.entity_node_kinds().is_empty());
        assert!(Lang::Json.entity_node_kinds().is_empty());
        assert!(Lang::Markdown.entity_node_kinds().is_empty());
    }

    #[test]
    fn grammar_returns_some_for_all_langs() {
        assert!(Lang::Rust.grammar().is_some());
        assert!(Lang::Python.grammar().is_some());
        assert!(Lang::JavaScript.grammar().is_some());
        assert!(Lang::TypeScript.grammar().is_some());
        assert!(Lang::Go.grammar().is_some());
        assert!(Lang::Bash.grammar().is_some());
        assert!(Lang::Toml.grammar().is_some());
        assert!(Lang::Json.grammar().is_some());
        assert!(Lang::Markdown.grammar().is_some());
    }

    /// `Lang::grammar()` was rewritten (#5971) to delegate to `lang_for_ext` via
    /// each variant's canonical extension instead of constructing the
    /// `tree_sitter_*::LANGUAGE` directly. `is_some()` alone cannot catch a
    /// wrong-but-non-empty mapping (e.g. a variant accidentally wired to a
    /// sibling extension's grammar); this asserts each variant's grammar is
    /// identical to what `detect_language` + `lang_for_ext` resolve for that
    /// variant's own extension, i.e. the delegation is wired correctly, not
    /// just non-empty.
    #[test]
    fn grammar_matches_detect_language_for_canonical_extension() {
        let cases = [
            (Lang::Rust, "main.rs"),
            (Lang::Python, "script.py"),
            (Lang::JavaScript, "app.js"),
            (Lang::TypeScript, "app.ts"),
            (Lang::Go, "main.go"),
            (Lang::Bash, "script.sh"),
            (Lang::Toml, "Cargo.toml"),
            (Lang::Json, "data.json"),
            (Lang::Markdown, "README.md"),
        ];
        for (lang, path) in cases {
            assert_eq!(detect_language(Path::new(path)), Some(lang));
            assert_eq!(
                lang.grammar(),
                lang_for_ext(Path::new(path).extension().unwrap().to_str().unwrap()),
                "grammar() mismatch for {lang:?}"
            );
        }
    }

    #[test]
    fn is_indexable_known_extension() {
        assert!(is_indexable(Path::new("src/main.rs")));
    }

    #[test]
    fn is_indexable_unknown_extension() {
        assert!(!is_indexable(Path::new("file.xyz")));
    }

    #[test]
    fn lang_id_roundtrip() {
        let langs = [
            Lang::Rust,
            Lang::Python,
            Lang::JavaScript,
            Lang::TypeScript,
            Lang::Go,
            Lang::Bash,
            Lang::Toml,
            Lang::Json,
            Lang::Markdown,
        ];
        for lang in langs {
            assert!(!lang.id().is_empty());
            assert_eq!(lang.to_string(), lang.id());
        }
    }

    /// Locks in the `f.pad` fix (#6066): `f.write_str` ignores width/fill/align flags.
    /// `f.pad` must reproduce the same padding a plain `&str` would get under an
    /// identical width specifier.
    #[test]
    fn lang_display_respects_width() {
        assert_eq!(format!("{:<12}", Lang::Rust), format!("{:<12}", "rust"));
        assert_eq!(
            format!("{:>12}", Lang::TypeScript),
            format!("{:>12}", "typescript")
        );
    }
}
