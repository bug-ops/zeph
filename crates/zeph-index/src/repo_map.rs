//! Lightweight structural map of a project (signatures only).
//!
//! Generates a compact `<repo_map>` showing file paths and top-level
//! symbols, suitable for permanent inclusion in the system prompt.

use std::fmt::Write;
use std::path::Path;

use tree_sitter::Parser;

use crate::languages::{Lang, detect_language};
use zeph_memory::estimate_tokens;

/// Generate a compact structural map of the project.
///
/// Output fits within `token_budget` tokens. Files sorted by symbol count
/// (more symbols = more important).
///
/// # Errors
///
/// Returns an error if the file walk fails.
pub fn generate_repo_map(root: &Path, token_budget: usize) -> anyhow::Result<String> {
    let walker = ignore::WalkBuilder::new(root)
        .hidden(true)
        .git_ignore(true)
        .build();

    let mut entries: Vec<(String, Vec<String>)> = Vec::new();

    for entry in walker.flatten() {
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }

        let Some(lang) = detect_language(entry.path()) else {
            continue;
        };
        let Some(grammar) = lang.grammar() else {
            continue;
        };

        let rel = entry
            .path()
            .strip_prefix(root)
            .unwrap_or(entry.path())
            .to_string_lossy()
            .to_string();

        if lang.entity_node_kinds().is_empty() {
            entries.push((rel, vec!["[config]".to_string()]));
            continue;
        }

        let Ok(source) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        let symbols = extract_top_level_symbols(&source, &grammar, lang);
        if symbols.is_empty() {
            continue;
        }

        entries.push((rel, symbols));
    }

    entries.sort_by(|a, b| b.1.len().cmp(&a.1.len()));

    let header = "<repo_map>\n";
    let footer = "</repo_map>";
    let mut map = String::from(header);
    let mut used = estimate_tokens(header) + estimate_tokens(footer);

    for (idx, (path, symbols)) in entries.iter().enumerate() {
        let line = format!("  {path} :: {}\n", symbols.join(", "));
        let cost = estimate_tokens(&line);
        if used + cost > token_budget {
            let remaining = entries.len() - idx;
            let _ = writeln!(map, "  ... and {remaining} more files");
            break;
        }
        map.push_str(&line);
        used += cost;
    }

    map.push_str(footer);
    Ok(map)
}

fn extract_top_level_symbols(
    source: &str,
    grammar: &tree_sitter::Language,
    lang: Lang,
) -> Vec<String> {
    let mut parser = Parser::new();
    if parser.set_language(grammar).is_err() {
        return vec![];
    }
    let Some(tree) = parser.parse(source, None) else {
        return vec![];
    };

    let root = tree.root_node();
    let entity_kinds = lang.entity_node_kinds();
    let mut symbols = Vec::new();
    let child_count = u32::try_from(root.named_child_count()).unwrap_or(u32::MAX);

    for i in 0..child_count {
        let Some(child) = root.named_child(i) else {
            continue;
        };
        if !entity_kinds.contains(&child.kind()) {
            continue;
        }

        let name = child
            .child_by_field_name("name")
            .or_else(|| child.child_by_field_name("type"))
            .map_or_else(
                || child.kind().to_string(),
                |n| source[n.byte_range()].to_string(),
            );

        let short_kind = shorten_kind(child.kind());
        symbols.push(format!("{short_kind}:{name}"));

        // For impl/class blocks, also extract methods
        if child.kind() == "impl_item" || child.kind() == "class_definition" {
            extract_body_methods(&child, source, &mut symbols);
        }
    }

    symbols
}

fn extract_body_methods(node: &tree_sitter::Node, source: &str, symbols: &mut Vec<String>) {
    let body = node.child_by_field_name("body").or_else(|| {
        let child_count = u32::try_from(node.named_child_count()).unwrap_or(u32::MAX);
        (0..child_count)
            .filter_map(|j| node.named_child(j))
            .find(|c| c.kind() == "declaration_list")
    });

    let Some(body) = body else { return };
    let child_count = u32::try_from(body.named_child_count()).unwrap_or(u32::MAX);

    for j in 0..child_count {
        let Some(method) = body.named_child(j) else {
            continue;
        };
        if let Some(method_name) = method.child_by_field_name("name") {
            let mn = source[method_name.byte_range()].to_string();
            let mk = shorten_kind(method.kind());
            symbols.push(format!("  {mk}:{mn}"));
        }
    }
}

fn shorten_kind(kind: &str) -> &str {
    match kind {
        "function_item" | "function_declaration" | "function_definition" | "method_definition" => {
            "fn"
        }
        "struct_item" => "struct",
        "enum_item" => "enum",
        "trait_item" => "trait",
        "impl_item" => "impl",
        "type_item" | "type_alias_declaration" => "type",
        "const_item" | "const_declaration" => "const",
        "static_item" => "static",
        "mod_item" => "mod",
        "class_definition" | "class_declaration" => "class",
        "macro_definition" => "macro",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shorten_known_kinds() {
        assert_eq!(shorten_kind("function_item"), "fn");
        assert_eq!(shorten_kind("struct_item"), "struct");
        assert_eq!(shorten_kind("enum_item"), "enum");
        assert_eq!(shorten_kind("trait_item"), "trait");
        assert_eq!(shorten_kind("impl_item"), "impl");
        assert_eq!(shorten_kind("mod_item"), "mod");
        assert_eq!(shorten_kind("class_definition"), "class");
        assert_eq!(shorten_kind("macro_definition"), "macro");
    }

    #[test]
    fn shorten_unknown_kind_passthrough() {
        assert_eq!(shorten_kind("custom_node"), "custom_node");
    }

    #[test]
    fn extract_rust_symbols() {
        let source = r#"
fn hello() {}
struct Foo;
impl Foo {
    fn bar(&self) {}
}
"#;
        let grammar = Lang::Rust.grammar().unwrap();
        let symbols = extract_top_level_symbols(source, &grammar, Lang::Rust);
        assert!(symbols.contains(&"fn:hello".to_string()));
        assert!(symbols.contains(&"struct:Foo".to_string()));
        assert!(symbols.contains(&"impl:Foo".to_string()));
        assert!(symbols.iter().any(|s| s.contains("fn:bar")));
    }

    #[test]
    fn extract_empty_source() {
        let grammar = Lang::Rust.grammar().unwrap();
        let symbols = extract_top_level_symbols("", &grammar, Lang::Rust);
        assert!(symbols.is_empty());
    }

    #[test]
    fn repo_map_with_tempdir() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("main.rs");
        std::fs::write(&file_path, "fn main() {}\nstruct App;\n").unwrap();

        let map = generate_repo_map(dir.path(), 1000).unwrap();
        assert!(map.contains("<repo_map>"));
        assert!(map.contains("</repo_map>"));
        assert!(map.contains("fn:main"));
        assert!(map.contains("struct:App"));
    }

    #[test]
    fn repo_map_budget_truncation() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..20 {
            let path = dir.path().join(format!("file_{i}.rs"));
            std::fs::write(&path, format!("fn func_{i}() {{}}\n")).unwrap();
        }

        let map = generate_repo_map(dir.path(), 30).unwrap();
        assert!(map.contains("... and"));
    }
}
