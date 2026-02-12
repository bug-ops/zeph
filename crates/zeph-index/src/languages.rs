//! Language detection and tree-sitter grammar registry.

use std::path::Path;

use serde::{Deserialize, Serialize};

/// Supported language with its tree-sitter grammar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Lang {
    Rust,
    Python,
    JavaScript,
    TypeScript,
    Go,
    Bash,
    Toml,
    Json,
    Markdown,
}

impl Lang {
    /// Identifier used in Qdrant payload and config.
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

    /// Get the tree-sitter grammar. Returns `None` if the
    /// corresponding feature is not enabled.
    #[must_use]
    pub fn grammar(self) -> Option<tree_sitter::Language> {
        match self {
            #[cfg(feature = "lang-rust")]
            Self::Rust => Some(tree_sitter_rust::LANGUAGE.into()),
            #[cfg(feature = "lang-python")]
            Self::Python => Some(tree_sitter_python::LANGUAGE.into()),
            #[cfg(feature = "lang-js")]
            Self::JavaScript => Some(tree_sitter_javascript::LANGUAGE.into()),
            #[cfg(feature = "lang-js")]
            Self::TypeScript => Some(tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()),
            #[cfg(feature = "lang-go")]
            Self::Go => Some(tree_sitter_go::LANGUAGE.into()),
            #[cfg(feature = "lang-config")]
            Self::Bash => Some(tree_sitter_bash::LANGUAGE.into()),
            #[cfg(feature = "lang-config")]
            Self::Toml => Some(tree_sitter_toml_ng::LANGUAGE.into()),
            #[cfg(feature = "lang-config")]
            Self::Json => Some(tree_sitter_json::LANGUAGE.into()),
            #[cfg(feature = "lang-config")]
            Self::Markdown => Some(tree_sitter_md::LANGUAGE.into()),
            #[allow(unreachable_patterns)]
            _ => None,
        }
    }

    /// Top-level AST node kinds that represent named entities.
    /// Used by the chunker to decide chunk boundaries.
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
        f.write_str(self.id())
    }
}

/// Detect language from file extension.
#[must_use]
pub fn detect_language(path: &Path) -> Option<Lang> {
    let ext = path.extension()?.to_str()?;
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

/// Check if a file should be indexed (has a supported language with grammar).
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
    fn grammar_returns_some_for_enabled_features() {
        #[cfg(feature = "lang-rust")]
        assert!(Lang::Rust.grammar().is_some());
        #[cfg(feature = "lang-python")]
        assert!(Lang::Python.grammar().is_some());
        #[cfg(feature = "lang-js")]
        {
            assert!(Lang::JavaScript.grammar().is_some());
            assert!(Lang::TypeScript.grammar().is_some());
        }
        #[cfg(feature = "lang-go")]
        assert!(Lang::Go.grammar().is_some());
        #[cfg(feature = "lang-config")]
        {
            assert!(Lang::Bash.grammar().is_some());
            assert!(Lang::Toml.grammar().is_some());
            assert!(Lang::Json.grammar().is_some());
            assert!(Lang::Markdown.grammar().is_some());
        }
    }

    #[test]
    fn is_indexable_known_extension() {
        #[cfg(feature = "lang-rust")]
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
}
