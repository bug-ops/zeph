// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! AST-based chunking via tree-sitter with greedy sibling merge.
//!
//! The main entry point is [`chunk_file`], which:
//!
//! 1. Parses the source with the appropriate tree-sitter grammar.
//! 2. Walks the AST and groups sibling nodes into [`CodeChunk`]s using a greedy
//!    merge strategy that tries to stay near [`ChunkerConfig::target_size`].
//! 3. Recursively splits nodes that exceed [`ChunkerConfig::max_size`].
//! 4. Merges adjacent chunks that are below [`ChunkerConfig::min_size`].
//! 5. For languages without named entity kinds (TOML, JSON, Markdown) a single
//!    file-level chunk is emitted.
//!
//! Each chunk carries a Blake3 content hash so unchanged chunks are skipped on
//! incremental re-indexing without any Qdrant round-trips.

use tree_sitter::{Node, Parser};
use zeph_common::hash::blake3_hex_str as blake3_hex;

use crate::error::{IndexError, Result};
use crate::languages::Lang;

/// One chunk of source code extracted from a file, with rich metadata for retrieval.
///
/// A `CodeChunk` represents a contiguous span of source text that was grouped by the
/// chunker — typically one or more top-level AST nodes (e.g. a single function, a
/// struct definition, or a small batch of sibling constants).
///
/// # Fields
///
/// * `code` — the raw source text of this chunk.
/// * `file_path` — relative path from the project root.
/// * `language` — detected language.
/// * `node_type` — tree-sitter node kind of the primary node (e.g. `"function_item"`).
///   For batches of siblings the format is `"<kind>x<count>"`.
/// * `entity_name` — extracted symbol name when available (e.g. `"my_function"`).
/// * `line_range` — 1-based inclusive `(start, end)` line numbers.
/// * `scope_chain` — `">"` separated nesting path (e.g. `"MyStruct > my_method"`).
/// * `imports` — up to 5 import declarations from the file, prepended to the embedding
///   text for better retrieval quality.
/// * `content_hash` — Blake3 hex digest of `code`, used for incremental deduplication.
#[derive(Debug, Clone)]
pub struct CodeChunk {
    /// Raw source text of this chunk.
    pub code: String,
    /// Relative path from the project root (e.g. `"src/lib.rs"`).
    pub file_path: String,
    /// Detected language for this file.
    pub language: Lang,
    /// Tree-sitter node kind of the primary AST node (e.g. `"function_item"`).
    pub node_type: String,
    /// Extracted symbol name, if the primary node has a `name` or `type` field.
    pub entity_name: Option<String>,
    /// 1-based inclusive `(start_line, end_line)` range within the source file.
    pub line_range: (usize, usize),
    /// `">"` separated scope nesting path (e.g. `"Outer > Inner > method"`).
    pub scope_chain: String,
    /// Import declarations extracted from the file header (up to 5 lines).
    pub imports: String,
    /// Blake3 hex digest of `code` for incremental deduplication.
    pub content_hash: String,
}

/// Configuration for the AST-based chunker.
///
/// All size thresholds are measured in **non-whitespace characters** rather than total
/// bytes to avoid counting indentation.
///
/// # Examples
///
/// ```no_run
/// use zeph_index::chunker::ChunkerConfig;
///
/// // Use defaults suitable for most code bases.
/// let config = ChunkerConfig::default();
/// assert_eq!(config.target_size, 600);
///
/// // Tighter chunks for a token-constrained context window.
/// let config = ChunkerConfig { target_size: 300, max_size: 600, min_size: 50 };
/// ```
#[derive(Debug, Clone)]
#[allow(clippy::struct_field_names)]
pub struct ChunkerConfig {
    /// Target chunk size in non-whitespace characters (default: 600).
    ///
    /// The chunker tries to fill batches up to this limit before starting a new chunk.
    pub target_size: usize,
    /// Maximum chunk size before forced recursive split (default: 1200).
    ///
    /// Nodes exceeding this limit are recursively descended into rather than emitted
    /// as a single oversized chunk.
    pub max_size: usize,
    /// Minimum chunk size — smaller pieces merge with adjacent siblings (default: 100).
    ///
    /// After the initial batch pass, chunks below this threshold are merged with their
    /// right-hand neighbour when the combined size stays within `target_size`.
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
    chunk_children(&ctx, &root, "", &mut chunks, 0);
    merge_small_chunks(&mut chunks, config);

    // Fallback: if AST chunking produced nothing but source has content,
    // emit a single file-level chunk so small files still get indexed.
    if chunks.is_empty() && non_ws_len(source) > 0 {
        chunks.push(make_chunk(source, file_path, lang, "", &imports));
    }

    Ok(chunks)
}

/// Maximum AST descent depth for [`chunk_children`].
///
/// Guards against stack overflow on adversarially deep source files (e.g. thousands of
/// nested brackets). Chosen well below typical thread stack sizes while comfortably
/// exceeding any legitimate nesting depth seen in real-world code.
const MAX_CHUNK_DEPTH: usize = 512;

fn chunk_children(
    ctx: &ChunkCtx<'_>,
    parent: &Node,
    parent_scope: &str,
    output: &mut Vec<CodeChunk>,
    depth: usize,
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
            if depth >= MAX_CHUNK_DEPTH {
                // Depth limit reached: emit the oversized node as a single leaf chunk
                // instead of recursing further.
                tracing::warn!(
                    file_path = ctx.file_path,
                    scope = %scope,
                    depth,
                    "chunk_children: max AST depth reached, emitting oversized node as a leaf chunk"
                );
                flush_batch(ctx, &[child], &scope, output);
            } else {
                chunk_children(ctx, &child, &scope, output, depth + 1);
            }
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

/// Merge adjacent chunks below `min_size` into their left neighbour, in a single forward pass.
///
/// Builds a new `Vec` instead of mutating in place: `Vec::remove` on the original approach
/// shifts every trailing element on each merge, and rescanning `non_ws_len` over the
/// already-merged, growing string on every iteration made the whole function O(n * merge-run
/// length) for a file with many small consecutive items. Tracking each accumulator's
/// non-whitespace length incrementally (simple addition per merge) avoids the rescan; only the
/// `blake3_hex` recompute remains per-merge, since it must reflect the final merged content.
#[tracing::instrument(name = "index.chunker.merge_small_chunks", skip_all)]
fn merge_small_chunks(chunks: &mut Vec<CodeChunk>, config: &ChunkerConfig) {
    if chunks.len() < 2 {
        return;
    }

    let mut merged: Vec<CodeChunk> = Vec::with_capacity(chunks.len());
    let mut merged_nws: Vec<usize> = Vec::with_capacity(chunks.len());

    for chunk in chunks.drain(..) {
        let chunk_nws = non_ws_len(&chunk.code);

        if let (Some(last), Some(last_nws)) = (merged.last_mut(), merged_nws.last_mut())
            && *last_nws < config.min_size
            && *last_nws + chunk_nws <= config.target_size
            && last.file_path == chunk.file_path
        {
            last.code.push('\n');
            last.code.push_str(&chunk.code);
            last.line_range.1 = chunk.line_range.1;
            if last.entity_name.is_none() {
                last.entity_name = chunk.entity_name;
            }
            last.content_hash = blake3_hex(&last.code);
            *last_nws += chunk_nws;
            continue;
        }

        merged_nws.push(chunk_nws);
        merged.push(chunk);
    }

    *chunks = merged;
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
        let source = r"
fn my_function() {
    let x = 1;
}
";
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
        let source = r"
fn a() { 1 }
fn b() { 2 }
fn c() { 3 }
";
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
            use std::fmt::Write as _;
            let _ = writeln!(body, "    let var{i} = {i};");
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
        let source = r"
impl MyStruct {
    fn method_one(&self) {
        let a = 1;
        let b = 2;
        let c = 3;
        let d = 4;
    }
}
";
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

    /// Pins `merge_small_chunks`'s output for a long run of small consecutive chunks (the
    /// pattern that made the old `Vec::remove` + per-iteration `non_ws_len` rescan quadratic,
    /// common in files with many small `const`/`enum` items). Rather than asserting on timing,
    /// this checks the correctness contract that must hold before and after the refactor: every
    /// merged chunk is an exact, ordered, gap-free, newline-joined concatenation of the original
    /// chunks, and no merged chunk exceeds `target_size`.
    #[test]
    fn merge_small_chunks_large_run_partitions_correctly() {
        let config = ChunkerConfig::default();
        let n = 60;
        let originals: Vec<CodeChunk> = (0..n)
            .map(|i| {
                let code = format!("const A{i}: i32 = {i};");
                CodeChunk {
                    content_hash: blake3_hex(&code),
                    code,
                    file_path: "src/consts.rs".to_string(),
                    language: Lang::Rust,
                    node_type: "const_item".to_string(),
                    entity_name: Some(format!("A{i}")),
                    line_range: (i + 1, i + 1),
                    scope_chain: String::new(),
                    imports: String::new(),
                }
            })
            .collect();

        let mut chunks = originals.clone();
        merge_small_chunks(&mut chunks, &config);

        assert!(
            chunks.len() < originals.len(),
            "expected many small items to merge"
        );

        // Every merged chunk must reconstruct a contiguous, ordered, gap-free subsequence
        // of `originals`, joined by '\n' exactly as `merge_small_chunks` does.
        let mut cursor = 0usize;
        for merged in &chunks {
            let parts: Vec<&str> = merged.code.split('\n').collect();
            for part in &parts {
                assert_eq!(
                    *part, originals[cursor].code,
                    "chunk boundary mismatch at original index {cursor}"
                );
                cursor += 1;
            }
            assert_eq!(
                merged.line_range.0,
                originals[cursor - parts.len()].line_range.0
            );
            assert_eq!(merged.line_range.1, originals[cursor - 1].line_range.1);
            assert!(non_ws_len(&merged.code) <= config.target_size);
        }
        assert_eq!(
            cursor,
            originals.len(),
            "every original chunk must appear exactly once, in order"
        );
    }

    /// Builds `fn f() { {{{...}}} let x = 1; ...}` with `nesting` levels of nested
    /// block expressions wrapping a single statement — a real tree-sitter-parseable
    /// fixture (not a naive repeated-char string) that produces `nesting` genuine
    /// `block` AST nodes chained via single-child recursion.
    fn nested_blocks_source(nesting: usize) -> String {
        let mut source = String::from("fn f() {\n");
        for _ in 0..nesting {
            source.push_str("{\n");
        }
        source.push_str("let x = 1;\n");
        for _ in 0..nesting {
            source.push_str("}\n");
        }
        source.push_str("}\n");
        source
    }

    #[test]
    fn flush_batch_singular_node_type_has_no_count_suffix() {
        let config = ChunkerConfig {
            target_size: 600,
            max_size: 1200,
            min_size: 5,
        };
        let chunks = chunk_file("fn solo() { 1 }", "src/solo.rs", Lang::Rust, &config).unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].node_type, "function_item");
    }

    // Each nested-block level in `nested_blocks_source` parses as
    // `expression_statement(block(...))` (confirmed via `to_sexp()`), i.e. each level
    // costs 2 AST-descent levels, not 1. With `function_item` + the body `block` as
    // fixed overhead (2 more descent levels), the depth guard — checked against the
    // *parent* frame's own depth before it would recurse into a MAX_CHUNK_DEPTH-deep
    // child — always fires while processing the frame at nesting level
    // `MAX_CHUNK_DEPTH / 2 - 1`, flushing that frame's child (nesting level
    // `MAX_CHUNK_DEPTH / 2`, an `expression_statement` node) as a single leaf chunk.
    const FALLBACK_LEVEL: usize = MAX_CHUNK_DEPTH / 2;

    #[test]
    fn chunk_deeply_nested_blocks_triggers_depth_guard_without_crashing() {
        let nesting = MAX_CHUNK_DEPTH + 100; // comfortably past the guard
        let source = nested_blocks_source(nesting);
        let config = ChunkerConfig {
            target_size: 600,
            max_size: 10,
            min_size: 0,
        };

        let chunks = chunk_file(&source, "src/deep.rs", Lang::Rust, &config)
            .expect("deeply nested source must parse and chunk without panicking");
        assert!(!chunks.is_empty());

        // Once the depth guard fires, the remaining oversized subtree is emitted as
        // a single leaf chunk whose node_type is the child's own kind (not the
        // "<kind>xN" batch form) instead of recursing further.
        let fallback = chunks
            .iter()
            .find(|c| c.node_type == "expression_statement" && c.code.contains("let x = 1;"))
            .expect("expected a leaf-fallback chunk covering the depth-limited subtree");

        // The fallback chunk must still hold a large slice of the leftover nesting,
        // proving consolidation happened rather than continued per-level recursion.
        let remaining_levels = nesting - FALLBACK_LEVEL + 1;
        assert_eq!(fallback.code.matches('{').count(), remaining_levels);
        assert!(remaining_levels > 50);
    }

    #[test]
    fn chunk_shallow_nested_blocks_unaffected_by_depth_guard() {
        let nesting = 20; // far below MAX_CHUNK_DEPTH — guard must never engage
        let source = nested_blocks_source(nesting);
        let config = ChunkerConfig {
            target_size: 600,
            max_size: 10,
            min_size: 0,
        };

        let chunks = chunk_file(&source, "src/shallow.rs", Lang::Rust, &config).unwrap();
        assert!(!chunks.is_empty());
        assert!(
            chunks.iter().any(|c| c.code.contains("let x = 1;")),
            "innermost statement must still be reachable via normal (non-fallback) recursion"
        );
    }

    #[test]
    fn depth_guard_fallback_respects_min_size_and_can_silently_drop_content() {
        // Nesting depth exactly at FALLBACK_LEVEL: the leaf-fallback fires on the
        // innermost expression_statement/block pair itself, so the leftover subtree
        // is tiny (just that one block wrapping the statement). flush_batch's
        // existing min_size check applies here too — this is pre-existing
        // flush_batch behavior, now reachable via the new depth-limited path, and it
        // can silently drop the whole leftover subtree with no chunk emitted for it
        // and no panic.
        //
        // 20 leading sibling statements are included so the function body produces
        // other, normal-sized surviving chunks — otherwise chunk_children would
        // yield an empty chunk list and chunk_file's own whole-file fallback (for
        // files with no AST chunks at all) would re-introduce the dropped content
        // via a different, coarser path, masking the behavior under test.
        use std::fmt::Write as _;

        let nesting = FALLBACK_LEVEL;
        let mut source = String::from("fn f() {\n");
        for i in 0..20 {
            let _ = writeln!(source, "let v{i} = {i};");
        }
        for _ in 0..nesting {
            source.push_str("{\n");
        }
        source.push_str("let x = 1;\n");
        for _ in 0..nesting {
            source.push_str("}\n");
        }
        source.push_str("}\n");

        let config = ChunkerConfig {
            target_size: 600,
            max_size: 10,
            min_size: 100, // comfortably larger than the tiny leftover subtree
        };

        let chunks = chunk_file(&source, "src/tiny-leftover.rs", Lang::Rust, &config).unwrap();
        assert!(
            chunks.iter().any(|c| c.code.contains("let v0 = 0;")),
            "the 20 leading statements must survive as their own batch"
        );
        assert!(
            chunks.iter().all(|c| !c.code.contains("let x = 1;")),
            "tiny leftover subtree below min_size must be silently dropped by flush_batch, \
             same as any other undersized batch"
        );
    }
}
