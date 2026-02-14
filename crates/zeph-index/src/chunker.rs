//! AST-based chunking via tree-sitter with greedy sibling merge.

use tree_sitter::{Node, Parser};

use crate::error::{IndexError, Result};
use crate::languages::Lang;

/// One chunk of source code with rich metadata.
#[derive(Debug, Clone)]
pub struct CodeChunk {
    pub code: String,
    pub file_path: String,
    pub language: Lang,
    pub node_type: String,
    pub entity_name: Option<String>,
    pub line_range: (usize, usize),
    pub scope_chain: String,
    pub imports: String,
    pub content_hash: String,
}

/// Chunker configuration.
#[derive(Debug, Clone)]
pub struct ChunkerConfig {
    /// Target chunk size in non-whitespace characters (default: 600).
    pub target_size: usize,
    /// Maximum chunk size before forced recursive split (default: 1200).
    pub max_size: usize,
    /// Minimum chunk size — smaller pieces merge with adjacent siblings (default: 100).
    pub min_size: usize,
}

impl Default for ChunkerConfig {
    fn default() -> Self {
        Self {
            target_size: 600,
            max_size: 1200,
            min_size: 100,
        }
    }
}

/// Shared context passed through the recursive chunking process.
struct ChunkCtx<'a> {
    source: &'a str,
    file_path: &'a str,
    lang: Lang,
    imports: &'a str,
    config: &'a ChunkerConfig,
}

/// Parse and chunk a source file.
///
/// # Errors
///
/// Returns error if tree-sitter fails to parse or no grammar is available.
pub fn chunk_file(
    source: &str,
    file_path: &str,
    lang: Lang,
    config: &ChunkerConfig,
) -> Result<Vec<CodeChunk>> {
    let grammar = lang
        .grammar()
        .ok_or_else(|| IndexError::Parse(format!("no grammar for {}", lang.id())))?;

    let mut parser = Parser::new();
    parser
        .set_language(&grammar)
        .map_err(|e| IndexError::Parse(format!("set_language failed: {e}")))?;

    let tree = parser
        .parse(source, None)
        .ok_or_else(|| IndexError::Parse(format!("parse failed for {file_path}")))?;

    let root = tree.root_node();
    let imports = extract_imports(source, &root, lang);
    let mut chunks = Vec::new();

    if lang.entity_node_kinds().is_empty() {
        let nws = non_ws_len(source);
        if nws > 0 {
            chunks.push(make_chunk(source, file_path, lang, "", &imports));
        }
        return Ok(chunks);
    }

    let ctx = ChunkCtx {
        source,
        file_path,
        lang,
        imports: &imports,
        config,
    };
    chunk_children(&ctx, &root, "", &mut chunks);
    merge_small_chunks(&mut chunks, config);

    // Fallback: if AST chunking produced nothing but source has content,
    // emit a single file-level chunk so small files still get indexed.
    if chunks.is_empty() && non_ws_len(source) > 0 {
        chunks.push(make_chunk(source, file_path, lang, "", &imports));
    }

    Ok(chunks)
}

fn chunk_children(
    ctx: &ChunkCtx<'_>,
    parent: &Node,
    parent_scope: &str,
    output: &mut Vec<CodeChunk>,
) {
    let mut batch: Vec<Node> = Vec::new();
    let mut batch_size: usize = 0;
    let child_count = u32::try_from(parent.named_child_count()).unwrap_or(u32::MAX);

    for i in 0..child_count {
        let Some(child) = parent.named_child(i) else {
            continue;
        };
        let child_text = &ctx.source[child.byte_range()];
        let child_nws = non_ws_len(child_text);

        if child_nws > ctx.config.max_size {
            flush_batch(ctx, &batch, parent_scope, output);
            batch.clear();
            batch_size = 0;

            let scope = extend_scope(parent_scope, &child, ctx.source);
            chunk_children(ctx, &child, &scope, output);
            continue;
        }

        if batch_size + child_nws > ctx.config.target_size && !batch.is_empty() {
            flush_batch(ctx, &batch, parent_scope, output);
            batch.clear();
            batch_size = 0;
        }

        batch.push(child);
        batch_size += child_nws;
    }

    if !batch.is_empty() {
        flush_batch(ctx, &batch, parent_scope, output);
    }
}

fn flush_batch(ctx: &ChunkCtx<'_>, batch: &[Node], scope: &str, output: &mut Vec<CodeChunk>) {
    if batch.is_empty() {
        return;
    }

    let start = batch[0].start_byte();
    let end = batch[batch.len() - 1].end_byte();
    let code = &ctx.source[start..end];
    let nws = non_ws_len(code);

    if nws < ctx.config.min_size {
        return;
    }

    let entity_name = batch
        .iter()
        .find_map(|n| extract_entity_name(n, ctx.source));
    let node_type = if batch.len() == 1 {
        batch[0].kind().to_string()
    } else {
        format!("{}x{}", batch[0].kind(), batch.len())
    };

    output.push(CodeChunk {
        content_hash: blake3_hex(code),
        line_range: (
            batch[0].start_position().row + 1,
            batch[batch.len() - 1].end_position().row + 1,
        ),
        entity_name,
        node_type,
        scope_chain: scope.to_string(),
        imports: ctx.imports.to_string(),
        file_path: ctx.file_path.to_string(),
        language: ctx.lang,
        code: code.to_string(),
    });
}

fn make_chunk(source: &str, file_path: &str, lang: Lang, scope: &str, imports: &str) -> CodeChunk {
    let lines = source.lines().count();
    CodeChunk {
        content_hash: blake3_hex(source),
        line_range: (1, lines.max(1)),
        entity_name: None,
        node_type: "file".to_string(),
        scope_chain: scope.to_string(),
        imports: imports.to_string(),
        file_path: file_path.to_string(),
        language: lang,
        code: source.to_string(),
    }
}

fn non_ws_len(text: &str) -> usize {
    text.chars().filter(|c| !c.is_whitespace()).count()
}

fn extract_imports(source: &str, root: &Node, lang: Lang) -> String {
    let import_kinds: &[&str] = match lang {
        Lang::Rust => &["use_declaration"],
        Lang::Python => &["import_statement", "import_from_statement"],
        Lang::JavaScript | Lang::TypeScript => &["import_statement"],
        Lang::Go => &["import_declaration"],
        _ => return String::new(),
    };

    let mut imports = String::new();
    let child_count = u32::try_from(root.named_child_count()).unwrap_or(u32::MAX);
    for i in 0..child_count {
        let Some(child) = root.named_child(i) else {
            continue;
        };
        if import_kinds.contains(&child.kind()) {
            imports.push_str(&source[child.byte_range()]);
            imports.push('\n');
        }
    }
    imports
}

fn extract_entity_name(node: &Node, source: &str) -> Option<String> {
    // tree-sitter-rust: impl_item uses "type" field, most others use "name"
    node.child_by_field_name("name")
        .or_else(|| node.child_by_field_name("type"))
        .map(|n| source[n.byte_range()].to_string())
}

fn extend_scope(parent_scope: &str, node: &Node, source: &str) -> String {
    let name = extract_entity_name(node, source).unwrap_or_else(|| node.kind().to_string());
    if parent_scope.is_empty() {
        name
    } else {
        format!("{parent_scope} > {name}")
    }
}

fn merge_small_chunks(chunks: &mut Vec<CodeChunk>, config: &ChunkerConfig) {
    if chunks.len() < 2 {
        return;
    }

    let mut i = 0;
    while i < chunks.len() - 1 {
        let cur_nws = non_ws_len(&chunks[i].code);
        let next_nws = non_ws_len(&chunks[i + 1].code);

        if cur_nws < config.min_size
            && cur_nws + next_nws <= config.target_size
            && chunks[i].file_path == chunks[i + 1].file_path
        {
            let next = chunks.remove(i + 1);
            let cur = &mut chunks[i];
            cur.code.push('\n');
            cur.code.push_str(&next.code);
            cur.line_range.1 = next.line_range.1;
            cur.content_hash = blake3_hex(&cur.code);
            if cur.entity_name.is_none() {
                cur.entity_name = next.entity_name;
            }
        } else {
            i += 1;
        }
    }
}

fn blake3_hex(input: &str) -> String {
    blake3::hash(input.as_bytes()).to_hex().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_config() -> ChunkerConfig {
        ChunkerConfig::default()
    }

    #[test]
    fn chunk_rust_single_function() {
        let source = r#"
fn hello() {
    println!("hello world");
}
"#;
        let chunks = chunk_file(source, "src/main.rs", Lang::Rust, &default_config()).unwrap();
        assert!(!chunks.is_empty());
        assert!(chunks[0].code.contains("fn hello"));
    }

    #[test]
    fn chunk_rust_impl_with_methods() {
        let source = r#"
struct Foo;

impl Foo {
    fn bar(&self) -> i32 {
        42
    }
    fn baz(&self) -> String {
        String::new()
    }
    fn qux(&self) {
        println!("qux");
    }
}
"#;
        let chunks = chunk_file(source, "src/foo.rs", Lang::Rust, &default_config()).unwrap();
        assert!(!chunks.is_empty());
    }

    #[test]
    fn chunk_toml_file_level() {
        let source = r#"
[package]
name = "test"
version = "0.1.0"
"#;
        let chunks = chunk_file(source, "Cargo.toml", Lang::Toml, &default_config()).unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].node_type, "file");
    }

    #[test]
    fn imports_extracted_for_rust() {
        let source = r#"
use std::io;
use std::path::Path;

fn main() {
    println!("hello");
}
"#;
        let chunks = chunk_file(source, "src/main.rs", Lang::Rust, &default_config()).unwrap();
        assert!(!chunks.is_empty());
        assert!(chunks[0].imports.contains("use std::io"));
        assert!(chunks[0].imports.contains("use std::path::Path"));
    }

    #[test]
    fn entity_name_extracted() {
        let config = ChunkerConfig {
            target_size: 600,
            max_size: 1200,
            min_size: 5,
        };
        let source = r#"
fn my_function() {
    let x = 1;
}
"#;
        let chunks = chunk_file(source, "src/main.rs", Lang::Rust, &config).unwrap();
        assert!(!chunks.is_empty());
        assert_eq!(chunks[0].entity_name.as_deref(), Some("my_function"));
    }

    #[test]
    fn content_hash_deterministic() {
        let source = "fn test() { 42 }";
        let c1 = chunk_file(source, "a.rs", Lang::Rust, &default_config()).unwrap();
        let c2 = chunk_file(source, "a.rs", Lang::Rust, &default_config()).unwrap();
        assert!(!c1.is_empty());
        assert_eq!(c1[0].content_hash, c2[0].content_hash);
    }

    #[test]
    fn non_ws_len_counts_correctly() {
        assert_eq!(non_ws_len("fn  foo () { }"), 9);
        assert_eq!(non_ws_len(""), 0);
        assert_eq!(non_ws_len("   "), 0);
    }

    #[test]
    fn chunk_small_fns_merge() {
        let config = ChunkerConfig {
            target_size: 600,
            max_size: 1200,
            min_size: 50,
        };
        let source = r#"
fn a() { 1 }
fn b() { 2 }
fn c() { 3 }
"#;
        let chunks = chunk_file(source, "src/main.rs", Lang::Rust, &config).unwrap();
        assert_eq!(chunks.len(), 1);
    }

    #[test]
    fn chunk_rust_large_function_splits() {
        let config = ChunkerConfig {
            target_size: 50,
            max_size: 100,
            min_size: 10,
        };
        let mut body = String::from("fn big() {\n");
        for i in 0..30 {
            body.push_str(&format!("    let var{i} = {i};\n"));
        }
        body.push_str("}\n");

        let chunks = chunk_file(&body, "src/big.rs", Lang::Rust, &config).unwrap();
        assert!(
            chunks.len() > 1,
            "expected split but got {} chunks",
            chunks.len()
        );
    }

    #[test]
    fn scope_chain_nested_impl() {
        let config = ChunkerConfig {
            target_size: 30,
            max_size: 60,
            min_size: 5,
        };
        let source = r#"
impl MyStruct {
    fn method_one(&self) {
        let a = 1;
        let b = 2;
        let c = 3;
        let d = 4;
    }
}
"#;
        let chunks = chunk_file(source, "src/lib.rs", Lang::Rust, &config).unwrap();
        let has_scope = chunks.iter().any(|c| c.scope_chain.contains("MyStruct"));
        assert!(has_scope, "expected scope chain with MyStruct");
    }

    #[test]
    fn python_class_chunked() {
        let source = r#"
class Greeter:
    def hello(self):
        print("hello")

    def goodbye(self):
        print("bye")
"#;
        let chunks = chunk_file(source, "app.py", Lang::Python, &default_config()).unwrap();
        assert!(!chunks.is_empty());
    }

    #[test]
    fn blake3_hex_consistent() {
        let h1 = blake3_hex("test input");
        let h2 = blake3_hex("test input");
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 64);
    }
}
