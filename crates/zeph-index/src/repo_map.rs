// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Lightweight structural map of a project (signatures only).
//!
//! Generates a compact `<repo_map>` showing file paths and top-level
//! symbols, suitable for permanent inclusion in the system prompt.

use std::fmt::Write;
use std::path::Path;

use tree_sitter::{Parser, QueryCursor, StreamingIterator as _};

use crate::error::Result;
use crate::languages::{Lang, detect_language};
use zeph_memory::TokenCounter;

/// Structured symbol extracted by ts-query.
#[derive(Debug, Clone)]
pub struct SymbolInfo {
    pub name: String,
    pub kind: SymbolKind,
    pub visibility: Visibility,
    pub line: usize,
    /// Methods inside impl/class bodies.
    pub children: Vec<SymbolInfo>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymbolKind {
    Function,
    Struct,
    Enum,
    Trait,
    Impl,
    TypeAlias,
    Const,
    Static,
    Mod,
    Class,
    Macro,
    Interface,
    Method,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Visibility {
    Public,     // pub
    Crate,      // pub(crate)
    Restricted, // pub(super), pub(in path)
    Private,    // no modifier
}

impl Visibility {
    fn from_node_text(text: Option<&str>) -> Self {
        match text {
            Some("pub") => Self::Public,
            Some("pub(crate)") => Self::Crate,
            Some(s) if s.starts_with("pub(") => Self::Restricted,
            _ => Self::Private,
        }
    }

    fn prefix(self) -> &'static str {
        match self {
            Self::Public => "pub ",
            Self::Crate => "pub(crate) ",
            Self::Restricted => "pub(super) ",
            Self::Private => "",
        }
    }
}

impl SymbolKind {
    fn from_node_kind(kind: &str) -> Self {
        match kind {
            "function_item" | "function_declaration" | "function_definition" => Self::Function,
            "struct_item" => Self::Struct,
            "enum_item" => Self::Enum,
            "trait_item" => Self::Trait,
            "impl_item" => Self::Impl,
            "type_item" | "type_alias_declaration" => Self::TypeAlias,
            "const_item" | "const_declaration" | "const_spec" => Self::Const,
            "static_item" => Self::Static,
            "mod_item" => Self::Mod,
            "class_definition" | "class_declaration" => Self::Class,
            "macro_definition" => Self::Macro,
            "interface_declaration" => Self::Interface,
            "method_declaration" | "method_definition" => Self::Method,
            _ => Self::Other,
        }
    }

    fn short(self) -> &'static str {
        match self {
            Self::Function | Self::Method => "fn",
            Self::Struct => "struct",
            Self::Enum => "enum",
            Self::Trait => "trait",
            Self::Impl => "impl",
            Self::TypeAlias => "type",
            Self::Const => "const",
            Self::Static => "static",
            Self::Mod => "mod",
            Self::Class => "class",
            Self::Macro => "macro",
            Self::Interface => "iface",
            Self::Other => "?",
        }
    }
}

/// Format a symbol for compact repo map output.
fn format_symbol(sym: &SymbolInfo) -> String {
    let vis = sym.visibility.prefix();
    let kind = sym.kind.short();
    let name = &sym.name;
    let line = sym.line + 1; // 1-based for human readability

    if sym.children.is_empty() {
        format!("{vis}{kind}:{name}({line})")
    } else {
        let methods: Vec<String> = sym
            .children
            .iter()
            .map(|m| {
                let mv = m.visibility.prefix();
                format!("{mv}fn:{}", m.name)
            })
            .collect();
        format!("{vis}{kind}:{name}({line}){{{}}}", methods.join(","))
    }
}

/// Generate a compact structural map of the project.
///
/// Output fits within `token_budget` tokens. Files sorted by symbol count
/// (more symbols = more important).
///
/// # Errors
///
/// Returns an error if the file walk fails.
pub fn generate_repo_map(root: &Path, token_budget: usize, tc: &TokenCounter) -> Result<String> {
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
        let symbols = extract_symbols(&source, &grammar, lang);
        if symbols.is_empty() {
            continue;
        }

        let formatted: Vec<String> = symbols.iter().map(format_symbol).collect();
        entries.push((rel, formatted));
    }

    entries.sort_by(|a, b| b.1.len().cmp(&a.1.len()));

    let header = "<repo_map>\n";
    let footer = "</repo_map>";
    let mut map = String::from(header);
    let mut used = tc.count_tokens(header) + tc.count_tokens(footer);

    for (idx, (path, symbols)) in entries.iter().enumerate() {
        let line = format!("  {path} :: {}\n", symbols.join(", "));
        let cost = tc.count_tokens(&line);
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

/// Extract top-level symbols using ts-query. Falls back to heuristic extraction
/// when the query is unavailable.
#[must_use]
pub fn extract_symbols(
    source: &str,
    grammar: &tree_sitter::Language,
    lang: Lang,
) -> Vec<SymbolInfo> {
    // Try ts-query path first.
    if let Some(query) = lang.symbol_query() {
        return extract_via_query(source, grammar, lang, query);
    }
    // Fallback: heuristic extraction compatible with old behaviour.
    extract_heuristic(source, grammar, lang)
}

fn extract_via_query(
    source: &str,
    grammar: &tree_sitter::Language,
    lang: Lang,
    query: &tree_sitter::Query,
) -> Vec<SymbolInfo> {
    let mut parser = Parser::new();
    if parser.set_language(grammar).is_err() {
        return vec![];
    }
    let Some(tree) = parser.parse(source, None) else {
        return vec![];
    };
    let source_bytes = source.as_bytes();
    let root = tree.root_node();
    let root_id = root.id();

    let name_idx = query.capture_index_for_name("name");
    let vis_idx = query.capture_index_for_name("vis");
    let def_idx = query.capture_index_for_name("def");

    let (Some(name_idx), Some(def_idx)) = (name_idx, def_idx) else {
        return vec![];
    };

    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(query, root, source_bytes);
    let mut symbols: Vec<SymbolInfo> = Vec::new();

    while let Some(m) = matches.next() {
        let def_node = m
            .captures
            .iter()
            .find(|c| c.index == def_idx)
            .map(|c| c.node);
        let name_node = m
            .captures
            .iter()
            .find(|c| c.index == name_idx)
            .map(|c| c.node);
        let vis_text: Option<&str> = vis_idx.and_then(|vi| {
            m.captures
                .iter()
                .find(|c| c.index == vi)
                .map(|c| &source[c.node.byte_range()])
        });

        let Some(def_node) = def_node else { continue };
        let Some(name_node) = name_node else { continue };

        // Skip symbols not directly under root (nested inside fn bodies, etc.).
        if def_node.parent().map(|p: tree_sitter::Node<'_>| p.id()) != Some(root_id) {
            continue;
        }

        let name = source[name_node.byte_range()].to_string();
        let kind = SymbolKind::from_node_kind(def_node.kind());
        let visibility = Visibility::from_node_text(vis_text);
        let line = def_node.start_position().row;

        let children = if matches!(kind, SymbolKind::Impl | SymbolKind::Class) {
            extract_methods(source, lang, &def_node)
        } else {
            vec![]
        };

        symbols.push(SymbolInfo {
            name,
            kind,
            visibility,
            line,
            children,
        });
    }

    symbols
}

fn extract_methods(source: &str, lang: Lang, parent: &tree_sitter::Node<'_>) -> Vec<SymbolInfo> {
    let Some(method_query) = lang.method_query() else {
        return vec![];
    };

    let source_bytes = source.as_bytes();
    let name_idx = method_query.capture_index_for_name("name");
    let vis_idx = method_query.capture_index_for_name("vis");
    let def_idx = method_query.capture_index_for_name("def");

    let (Some(name_idx), Some(def_idx)) = (name_idx, def_idx) else {
        return vec![];
    };

    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(method_query, *parent, source_bytes);
    let mut methods = Vec::new();

    while let Some(m) = matches.next() {
        let def_node = m
            .captures
            .iter()
            .find(|c| c.index == def_idx)
            .map(|c| c.node);
        let name_node = m
            .captures
            .iter()
            .find(|c| c.index == name_idx)
            .map(|c| c.node);
        let vis_text: Option<&str> = vis_idx.and_then(|vi| {
            m.captures
                .iter()
                .find(|c| c.index == vi)
                .map(|c| &source[c.node.byte_range()])
        });

        let Some(def_node) = def_node else { continue };
        let Some(name_node) = name_node else { continue };

        let name = source[name_node.byte_range()].to_string();
        let visibility = Visibility::from_node_text(vis_text);
        let line = def_node.start_position().row;

        methods.push(SymbolInfo {
            name,
            kind: SymbolKind::Method,
            visibility,
            line,
            children: vec![],
        });
    }

    methods
}

/// Heuristic fallback (original AST walking logic, kept for languages without ts-query).
fn extract_heuristic(source: &str, grammar: &tree_sitter::Language, lang: Lang) -> Vec<SymbolInfo> {
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

        let kind = SymbolKind::from_node_kind(child.kind());
        let line = child.start_position().row;

        let children = if child.kind() == "impl_item" || child.kind() == "class_definition" {
            extract_heuristic_methods(&child, source)
        } else {
            vec![]
        };

        symbols.push(SymbolInfo {
            name,
            kind,
            visibility: Visibility::Private,
            line,
            children,
        });
    }

    symbols
}

fn extract_heuristic_methods(node: &tree_sitter::Node, source: &str) -> Vec<SymbolInfo> {
    let body = node.child_by_field_name("body").or_else(|| {
        let child_count = u32::try_from(node.named_child_count()).unwrap_or(u32::MAX);
        (0..child_count)
            .filter_map(|j| node.named_child(j))
            .find(|c| c.kind() == "declaration_list")
    });

    let Some(body) = body else { return vec![] };
    let child_count = u32::try_from(body.named_child_count()).unwrap_or(u32::MAX);
    let mut methods = Vec::new();

    for j in 0..child_count {
        let Some(method) = body.named_child(j) else {
            continue;
        };
        if let Some(method_name) = method.child_by_field_name("name") {
            let name = source[method_name.byte_range()].to_string();
            let line = method.start_position().row;
            methods.push(SymbolInfo {
                name,
                kind: SymbolKind::Method,
                visibility: Visibility::Private,
                line,
                children: vec![],
            });
        }
    }
    methods
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visibility_from_node_text() {
        assert_eq!(Visibility::from_node_text(None), Visibility::Private);
        assert_eq!(Visibility::from_node_text(Some("pub")), Visibility::Public);
        assert_eq!(
            Visibility::from_node_text(Some("pub(crate)")),
            Visibility::Crate
        );
        assert_eq!(
            Visibility::from_node_text(Some("pub(super)")),
            Visibility::Restricted
        );
        assert_eq!(
            Visibility::from_node_text(Some("pub(in crate::foo)")),
            Visibility::Restricted
        );
        assert_eq!(
            Visibility::from_node_text(Some("other")),
            Visibility::Private
        );
    }

    #[test]
    fn visibility_prefix() {
        assert_eq!(Visibility::Public.prefix(), "pub ");
        assert_eq!(Visibility::Crate.prefix(), "pub(crate) ");
        assert_eq!(Visibility::Restricted.prefix(), "pub(super) ");
        assert_eq!(Visibility::Private.prefix(), "");
    }

    #[test]
    fn symbol_kind_short() {
        assert_eq!(SymbolKind::Function.short(), "fn");
        assert_eq!(SymbolKind::Struct.short(), "struct");
        assert_eq!(SymbolKind::Impl.short(), "impl");
        assert_eq!(SymbolKind::Class.short(), "class");
    }

    #[test]
    fn extract_rust_symbols_pub_visibility() {
        let source = r#"
pub fn hello() {}
pub(crate) struct Foo;
enum Bar {}
impl Foo {
    pub fn bar(&self) {}
    fn private_method(&self) {}
}
"#;
        let grammar = Lang::Rust.grammar().unwrap();
        let symbols = extract_symbols(source, &grammar, Lang::Rust);
        let hello = symbols.iter().find(|s| s.name == "hello").unwrap();
        assert_eq!(hello.visibility, Visibility::Public);
        assert_eq!(hello.kind, SymbolKind::Function);

        let foo = symbols.iter().find(|s| s.name == "Foo").unwrap();
        assert_eq!(foo.visibility, Visibility::Crate);
        assert_eq!(foo.kind, SymbolKind::Struct);

        let bar = symbols.iter().find(|s| s.name == "Bar").unwrap();
        assert_eq!(bar.visibility, Visibility::Private);

        let impl_sym = symbols.iter().find(|s| s.kind == SymbolKind::Impl).unwrap();
        assert!(!impl_sym.children.is_empty());
        let pub_method = impl_sym.children.iter().find(|m| m.name == "bar").unwrap();
        assert_eq!(pub_method.visibility, Visibility::Public);
    }

    #[test]
    fn extract_rust_symbols_line_numbers() {
        let source = "pub fn first() {}\n\npub fn second() {}\n";
        let grammar = Lang::Rust.grammar().unwrap();
        let symbols = extract_symbols(source, &grammar, Lang::Rust);
        let first = symbols.iter().find(|s| s.name == "first").unwrap();
        assert_eq!(first.line, 0);
        let second = symbols.iter().find(|s| s.name == "second").unwrap();
        assert_eq!(second.line, 2);
    }

    #[test]
    fn extract_empty_source() {
        let grammar = Lang::Rust.grammar().unwrap();
        let symbols = extract_symbols("", &grammar, Lang::Rust);
        assert!(symbols.is_empty());
    }

    #[test]
    fn format_symbol_output() {
        let sym = SymbolInfo {
            name: "Foo".to_string(),
            kind: SymbolKind::Struct,
            visibility: Visibility::Public,
            line: 4,
            children: vec![],
        };
        let out = format_symbol(&sym);
        assert_eq!(out, "pub struct:Foo(5)");
    }

    #[test]
    fn format_symbol_with_methods() {
        let sym = SymbolInfo {
            name: "Foo".to_string(),
            kind: SymbolKind::Impl,
            visibility: Visibility::Private,
            line: 0,
            children: vec![SymbolInfo {
                name: "bar".to_string(),
                kind: SymbolKind::Method,
                visibility: Visibility::Public,
                line: 1,
                children: vec![],
            }],
        };
        let out = format_symbol(&sym);
        assert_eq!(out, "impl:Foo(1){pub fn:bar}");
    }

    #[test]
    fn extract_python_symbols() {
        let source = "def greet(name):\n    pass\n\nclass Animal:\n    pass\n";
        let grammar = Lang::Python.grammar().unwrap();
        let symbols = extract_symbols(source, &grammar, Lang::Python);
        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"greet"), "should extract function 'greet'");
        assert!(names.contains(&"Animal"), "should extract class 'Animal'");
        let greet = symbols.iter().find(|s| s.name == "greet").unwrap();
        assert_eq!(greet.kind, SymbolKind::Function);
        let animal = symbols.iter().find(|s| s.name == "Animal").unwrap();
        assert_eq!(animal.kind, SymbolKind::Class);
    }

    #[test]
    fn extract_javascript_symbols() {
        let source = "function hello() {}\nclass Greeter {}\nconst PI = 3.14;\n";
        let grammar = Lang::JavaScript.grammar().unwrap();
        let symbols = extract_symbols(source, &grammar, Lang::JavaScript);
        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"hello"), "should extract function 'hello'");
        assert!(names.contains(&"Greeter"), "should extract class 'Greeter'");
        let hello = symbols.iter().find(|s| s.name == "hello").unwrap();
        assert_eq!(hello.kind, SymbolKind::Function);
        let greeter = symbols.iter().find(|s| s.name == "Greeter").unwrap();
        assert_eq!(greeter.kind, SymbolKind::Class);
    }

    /// GAP-1552-B: heuristic fallback runs when symbol_query is None.
    /// Bash has a grammar but no symbol_query, so extract_symbols falls through to
    /// extract_heuristic. Bash has no entity_node_kinds so the result is an empty
    /// Vec — the important thing is no panic and correct code path.
    #[test]
    fn extract_heuristic_fallback_no_symbol_query() {
        // Bash has grammar but symbol_query() returns None.
        assert!(
            Lang::Bash.symbol_query().is_none(),
            "test precondition: Bash has no symbol_query"
        );
        let grammar = Lang::Bash.grammar().unwrap();
        // Bash entity_node_kinds is empty, so heuristic produces nothing — but must not panic.
        let source = "echo hello\nif true; then\n  echo yes\nfi\n";
        let symbols = extract_symbols(source, &grammar, Lang::Bash);
        // No entity_node_kinds for Bash → heuristic returns empty.
        assert!(
            symbols.is_empty(),
            "heuristic fallback for Bash must return empty (no entity kinds)"
        );
    }

    #[test]
    fn repo_map_with_tempdir() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("main.rs");
        std::fs::write(&file_path, "fn main() {}\npub struct App;\n").unwrap();

        let tc = zeph_memory::TokenCounter::new();
        let map = generate_repo_map(dir.path(), 1000, &tc).unwrap();
        assert!(map.contains("<repo_map>"));
        assert!(map.contains("</repo_map>"));
        assert!(map.contains("fn:main") || map.contains("fn:main("));
        assert!(map.contains("struct:App") || map.contains("struct:App("));
    }

    #[test]
    fn repo_map_budget_truncation() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..20 {
            let path = dir.path().join(format!("file_{i}.rs"));
            std::fs::write(&path, format!("fn func_{i}() {{}}\n")).unwrap();
        }

        let tc = zeph_memory::TokenCounter::new();
        let map = generate_repo_map(dir.path(), 30, &tc).unwrap();
        assert!(map.contains("... and"));
    }
}
