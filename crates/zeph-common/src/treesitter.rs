// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Shared tree-sitter query constants and helpers used by zeph-tools and zeph-index,
//! including [`lang_for_ext`] — the single source of truth both crates call to map a
//! file extension to a tree-sitter `Language`, instead of each maintaining its own
//! extension-to-language match arms.
//!
//! Only available with the `treesitter` feature.

use tree_sitter::{Language, Query};

// ---------------------------------------------------------------------------
// Shared symbol query constants
// ---------------------------------------------------------------------------

pub const RUST_SYM_Q: &str = "
(function_item (visibility_modifier)? @vis name: (identifier) @name) @def
(struct_item (visibility_modifier)? @vis name: (type_identifier) @name) @def
(enum_item (visibility_modifier)? @vis name: (type_identifier) @name) @def
(trait_item (visibility_modifier)? @vis name: (type_identifier) @name) @def
(impl_item type: (_) @name) @def
(type_item (visibility_modifier)? @vis name: (type_identifier) @name) @def
(const_item (visibility_modifier)? @vis name: (identifier) @name) @def
(static_item (visibility_modifier)? @vis name: (identifier) @name) @def
(mod_item (visibility_modifier)? @vis name: (identifier) @name) @def
(macro_definition name: (identifier) @name) @def
";

pub const PYTHON_SYM_Q: &str = "
(function_definition name: (identifier) @name) @def
(class_definition name: (identifier) @name) @def
";

pub const JS_SYM_Q: &str = "
(function_declaration name: (identifier) @name) @def
(class_declaration name: (identifier) @name) @def
(method_definition name: (property_identifier) @name) @def
(export_statement declaration: (function_declaration name: (identifier) @name)) @def
(export_statement declaration: (class_declaration name: (identifier) @name)) @def
(lexical_declaration (variable_declarator name: (identifier) @name)) @def
";

pub const TS_SYM_Q: &str = "
(function_declaration name: (identifier) @name) @def
(class_declaration name: (type_identifier) @name) @def
(method_definition name: (property_identifier) @name) @def
(interface_declaration name: (type_identifier) @name) @def
(type_alias_declaration name: (type_identifier) @name) @def
(export_statement declaration: (function_declaration name: (identifier) @name)) @def
(export_statement declaration: (class_declaration name: (type_identifier) @name)) @def
(lexical_declaration (variable_declarator name: (identifier) @name)) @def
";

pub const GO_SYM_Q: &str = "
(function_declaration name: (identifier) @name) @def
(method_declaration name: (field_identifier) @name) @def
(type_declaration (type_spec name: (type_identifier) @name)) @def
(const_declaration (const_spec name: (identifier) @name)) @def
";

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Compile a tree-sitter query, logging a warning on failure.
///
/// Returns `None` if the query string fails to compile (e.g. grammar version mismatch).
#[must_use]
pub fn compile_query(lang: &Language, source: &str, label: &str) -> Option<Query> {
    Query::new(lang, source)
        .map_err(|e| tracing::warn!("{label} query compile failed: {e}"))
        .ok()
}

/// Map a file extension to its tree-sitter `Language`.
///
/// This is the single source of truth for extension-to-language coverage;
/// `zeph-index` and `zeph-tools` both call this instead of hand-rolling their
/// own extension match arms, so the two crates cannot drift apart.
///
/// Returns `None` for unsupported extensions.
#[must_use]
pub fn lang_for_ext(ext: &str) -> Option<Language> {
    match ext {
        "rs" => Some(tree_sitter_rust::LANGUAGE.into()),
        "py" | "pyi" => Some(tree_sitter_python::LANGUAGE.into()),
        "js" | "jsx" | "mjs" | "cjs" => Some(tree_sitter_javascript::LANGUAGE.into()),
        "ts" | "tsx" | "mts" | "cts" => Some(tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()),
        "go" => Some(tree_sitter_go::LANGUAGE.into()),
        "sh" | "bash" | "zsh" => Some(tree_sitter_bash::LANGUAGE.into()),
        "toml" => Some(tree_sitter_toml_ng::LANGUAGE.into()),
        "json" | "jsonc" => Some(tree_sitter_json::LANGUAGE.into()),
        "md" | "markdown" => Some(tree_sitter_md::LANGUAGE.into()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every extension group `zeph-index` and `zeph-tools` route through
    /// `lang_for_ext` must resolve to a grammar (#5971): the five languages
    /// that already had call sites plus the four added when the two crates'
    /// hand-rolled mappings were consolidated into this function.
    #[test]
    fn lang_for_ext_covers_all_extension_groups() {
        let supported = [
            "rs", "py", "pyi", "js", "jsx", "mjs", "cjs", "ts", "tsx", "mts", "cts", "go", "sh",
            "bash", "zsh", "toml", "json", "jsonc", "md", "markdown",
        ];
        for ext in supported {
            assert!(
                lang_for_ext(ext).is_some(),
                "expected .{ext} to be supported"
            );
        }
    }

    #[test]
    fn lang_for_ext_unsupported_returns_none() {
        assert!(lang_for_ext("xyz").is_none());
        assert!(lang_for_ext("").is_none());
    }

    /// Aliases within one extension group must resolve to the identical
    /// `Language`, and distinct groups must not collide.
    #[test]
    fn lang_for_ext_aliases_match_within_group_and_differ_across_groups() {
        assert_eq!(lang_for_ext("sh"), lang_for_ext("bash"));
        assert_eq!(lang_for_ext("sh"), lang_for_ext("zsh"));
        assert_eq!(lang_for_ext("json"), lang_for_ext("jsonc"));
        assert_eq!(lang_for_ext("md"), lang_for_ext("markdown"));
        assert_ne!(lang_for_ext("sh"), lang_for_ext("toml"));
        assert_ne!(lang_for_ext("toml"), lang_for_ext("json"));
        assert_ne!(lang_for_ext("json"), lang_for_ext("md"));
    }
}
