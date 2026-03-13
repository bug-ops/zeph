// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Hover-on-read hook: pre-fetches LSP hover info for top-level symbols.
//!
//! Symbol extraction uses tree-sitter for multi-language support, with
//! regex fallback (Rust-only) when tree-sitter cannot parse the file.

use std::sync::LazyLock;

use futures::StreamExt as _;
use regex::Regex;
use zeph_mcp::McpCaller;
use zeph_memory::TokenCounter;

use crate::config::LspConfig;
use crate::sanitizer::{ContentSanitizer, ContentSource, ContentSourceKind};

use super::{LspHookRunner, LspNote};

/// Maximum concurrent MCP hover calls per file to avoid connection saturation.
const MAX_CONCURRENT_HOVER_CALLS: usize = 3;

/// Matches Rust top-level definition lines: `fn`, `struct`, `enum`, `trait`, `impl`, `type`.
static SYMBOL_LINE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?(?:fn|struct|enum|trait|impl|type)\s+\w")
        .expect("valid regex")
});

/// Strip cat-n prefix from content and return `(clean_source, line_number_map)`.
///
/// `line_number_map[i]` = 0-based LSP line number for raw line `i`.
/// If the content does not have cat-n format, returns the content as-is.
fn strip_cat_n_prefix(content: &str) -> (String, Vec<u64>) {
    let mut clean = String::with_capacity(content.len());
    let mut map: Vec<u64> = Vec::new();

    for (raw_idx, raw_line) in content.lines().enumerate() {
        let raw_idx = raw_idx as u64;
        let (lsp_line, source_line) = if let Some(tab) = raw_line.find('\t') {
            let prefix = raw_line[..tab].trim();
            if !prefix.is_empty() && prefix.chars().all(|c| c.is_ascii_digit()) {
                let one_based: u64 = prefix.parse().unwrap_or(0);
                (one_based.saturating_sub(1), &raw_line[tab + 1..])
            } else {
                (raw_idx, raw_line)
            }
        } else {
            (raw_idx, raw_line)
        };
        clean.push_str(source_line);
        clean.push('\n');
        map.push(lsp_line);
    }

    (clean, map)
}

/// Extract (`line_number`, `character_offset`) pairs for symbol definitions.
/// Lines and characters are 0-indexed (LSP convention).
///
/// Handles both raw source content and `cat -n` formatted output (` N\t` prefix)
/// emitted by the native `read` tool.
///
/// Uses tree-sitter for multi-language support and semantic filtering.
/// Falls back to regex (Rust-only) when tree-sitter cannot parse the file.
fn extract_symbol_positions(content: &str, file_path: &str, max_symbols: usize) -> Vec<(u64, u64)> {
    if let Some(positions) = extract_symbol_positions_tsquery(content, file_path, max_symbols) {
        tracing::debug!(
            path = file_path,
            symbols = positions.len(),
            extractor = "tree-sitter",
            "LSP hover: extracted symbol positions"
        );
        return positions;
    }
    let positions = extract_symbol_positions_regex(content, file_path, max_symbols);
    tracing::debug!(
        path = file_path,
        symbols = positions.len(),
        extractor = "regex",
        "LSP hover: extracted symbol positions"
    );
    positions
}

/// Regex-based extraction: Rust-only, top-level definitions only.
fn extract_symbol_positions_regex(
    content: &str,
    file_path: &str,
    max_symbols: usize,
) -> Vec<(u64, u64)> {
    // Regex is Rust-only.
    if !std::path::Path::new(file_path)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("rs"))
    {
        return vec![];
    }

    let mut positions = Vec::new();
    for (raw_idx, raw_line) in content.lines().enumerate() {
        if positions.len() >= max_symbols {
            break;
        }
        let (lsp_line, source_line) = if let Some(tab) = raw_line.find('\t') {
            let prefix = raw_line[..tab].trim();
            if !prefix.is_empty() && prefix.chars().all(|c| c.is_ascii_digit()) {
                let one_based: u64 = prefix.parse().unwrap_or(0);
                (one_based.saturating_sub(1), &raw_line[tab + 1..])
            } else {
                (raw_idx as u64, raw_line)
            }
        } else {
            (raw_idx as u64, raw_line)
        };
        if let Some(m) = SYMBOL_LINE_RE.find(source_line) {
            positions.push((lsp_line, m.start() as u64));
        }
    }
    positions
}

/// Tree-sitter based extraction: multi-language, definition nodes at any depth.
///
/// Uses a hover-specific query strategy: extract all definition node positions
/// (fn, struct, enum, trait, impl, type, class, etc.) regardless of depth.
/// Filters out trivial nodes: local `let` bindings inside function bodies are
/// not definition nodes and won't appear in any definition query.
fn extract_symbol_positions_tsquery(
    content: &str,
    file_path: &str,
    max_symbols: usize,
) -> Option<Vec<(u64, u64)>> {
    use tree_sitter::{Parser, StreamingIterator as _};
    use zeph_index::languages::detect_language;

    let lang = detect_language(std::path::Path::new(file_path))?;
    let grammar = lang.grammar()?;
    let query = lang.symbol_query()?;

    let (clean_source, line_map) = strip_cat_n_prefix(content);
    let source_bytes = clean_source.as_bytes();

    let mut parser = Parser::new();
    parser.set_language(&grammar).ok()?;
    let tree = parser.parse(source_bytes, None)?;
    let root = tree.root_node();

    let name_idx = query.capture_index_for_name("name")?;
    let def_idx = query.capture_index_for_name("def")?;

    let mut cursor = tree_sitter::QueryCursor::new();
    // No depth restriction — capture definitions at all depths for hover.
    let mut matches = cursor.matches(query, root, source_bytes);

    let mut positions = Vec::new();

    while let Some(m) = matches.next() {
        if positions.len() >= max_symbols {
            break;
        }
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

        let (Some(def_node), Some(name_node)) = (def_node, name_node) else {
            continue;
        };

        let raw_row = def_node.start_position().row;
        let lsp_line = line_map.get(raw_row).copied().unwrap_or(raw_row as u64);
        let char_offset = name_node.start_position().column as u64;

        positions.push((lsp_line, char_offset));
    }

    Some(positions)
}

/// Fetch hover info for key symbols in a file that was just read.
///
/// Makes concurrent MCP `get_hover` calls (up to `MAX_CONCURRENT_HOVER_CALLS` at a time)
/// for each detected Rust symbol position. Returns `None` when no symbols are found,
/// the file is not a `.rs` file, or all calls fail.
pub(super) async fn fetch_hover(
    runner: &LspHookRunner,
    tool_params: &serde_json::Value,
    tool_output: &str,
    token_counter: &std::sync::Arc<TokenCounter>,
    sanitizer: &ContentSanitizer,
) -> Option<LspNote> {
    fetch_hover_inner(
        runner.manager.as_ref(),
        &runner.config,
        tool_params,
        tool_output,
        token_counter,
        sanitizer,
    )
    .await
}

pub(crate) async fn fetch_hover_inner(
    manager: &impl McpCaller,
    config: &LspConfig,
    tool_params: &serde_json::Value,
    tool_output: &str,
    token_counter: &std::sync::Arc<TokenCounter>,
    sanitizer: &ContentSanitizer,
) -> Option<LspNote> {
    let Some(file_path) = tool_params
        .get("path")
        .and_then(|v| v.as_str())
        .map(ToOwned::to_owned)
    else {
        tracing::debug!(
            tool = "read",
            "LSP hook: skipped hover fetch (missing path)"
        );
        return None;
    };

    // Extract symbol positions from the file content returned by the read tool.
    let positions = extract_symbol_positions(tool_output, &file_path, config.hover.max_symbols);
    if positions.is_empty() {
        tracing::debug!(path = %file_path, "LSP hover: no symbols found in file");
        return None;
    }

    tracing::debug!(
        path = %file_path,
        symbols = positions.len(),
        concurrency = MAX_CONCURRENT_HOVER_CALLS,
        timeout_secs = config.call_timeout_secs,
        "LSP hook: queuing hover fetch"
    );

    let timeout = std::time::Duration::from_secs(config.call_timeout_secs);
    let server_id = &config.mcp_server_id;

    // Use buffer_unordered to cap concurrent MCP connections.
    let mut entries: Vec<String> =
        futures::stream::iter(positions.iter().map(|(line, character)| {
            let args = serde_json::json!({
                "file_path": file_path,
                "line": line,
                "character": character,
            });
            tokio::time::timeout(timeout, manager.call_tool(server_id, "get_hover", args))
        }))
        .buffer_unordered(MAX_CONCURRENT_HOVER_CALLS)
        .filter_map(|r| async move {
            match r {
                Ok(Ok(result)) => {
                    // Extract text from first content item.
                    let text = result
                        .content
                        .iter()
                        .find_map(|c| c.as_text().map(|t| t.text.trim().to_owned()))?;
                    if text.is_empty() { None } else { Some(text) }
                }
                _ => None,
            }
        })
        .collect()
        .await;

    if entries.is_empty() {
        tracing::debug!(path = %file_path, "LSP hover: no hover entries returned");
        return None;
    }

    // Sort before dedup to catch non-consecutive duplicates.
    entries.sort_unstable();
    entries.dedup();

    let raw_content = entries.join("\n---\n");

    // Sanitize hover content via ContentSanitizer before injecting into LLM context.
    let clean = sanitizer.sanitize(
        &raw_content,
        ContentSource::new(ContentSourceKind::McpResponse).with_identifier("mcpls/hover"),
    );
    if !clean.injection_flags.is_empty() {
        tracing::warn!(
            path = file_path,
            flags = ?clean.injection_flags.iter().map(|f| f.pattern_name).collect::<Vec<_>>(),
            "LSP hover content contains injection patterns"
        );
    }

    let estimated_tokens = token_counter.count_tokens(&clean.body);

    tracing::debug!(
        path = %file_path,
        entries = entries.len(),
        estimated_tokens,
        "LSP hover: injecting hover note"
    );

    Some(LspNote {
        kind: "hover",
        content: clean.body,
        estimated_tokens,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_cat_n_basic() {
        let src = "   1\tpub fn foo() {}\n   2\tstruct Bar;\n";
        let (clean, map) = strip_cat_n_prefix(src);
        assert!(clean.contains("pub fn foo()"));
        assert!(!clean.contains('\t'));
        assert_eq!(map[0], 0); // cat-n line 1 → LSP 0
        assert_eq!(map[1], 1); // cat-n line 2 → LSP 1
    }

    #[test]
    fn strip_cat_n_high_line_numbers() {
        let src = "  30\tpub struct Foo {\n  31\t    x: u32,\n  40\tpub fn bar() {}";
        let (clean, map) = strip_cat_n_prefix(src);
        assert_eq!(map[0], 29); // cat-n 30 → LSP 29
        assert_eq!(map[2], 39); // cat-n 40 → LSP 39
        assert!(clean.contains("pub struct Foo"));
        assert!(clean.contains("pub fn bar"));
    }

    #[test]
    fn strip_cat_n_raw_source_passthrough() {
        let src = "pub fn foo() {}\nstruct Bar;\n";
        let (clean, map) = strip_cat_n_prefix(src);
        // No cat-n prefix: raw_idx used as lsp_line
        assert_eq!(map[0], 0);
        assert_eq!(map[1], 1);
        assert!(clean.contains("pub fn foo()"));
    }

    #[test]
    fn extracts_rust_symbols_regex() {
        let src = "pub fn foo() {}\npub struct Bar;\npub enum Baz {}";
        let positions = extract_symbol_positions_regex(src, "foo.rs", 10);
        assert_eq!(positions.len(), 3);
        assert_eq!(positions[0].0, 0);
        assert_eq!(positions[1].0, 1);
        assert_eq!(positions[2].0, 2);
    }

    #[test]
    fn regex_skips_non_rust_files() {
        let src = "def foo(): pass\nclass Bar: pass";
        let positions = extract_symbol_positions_regex(src, "foo.py", 10);
        assert!(positions.is_empty());
    }

    #[test]
    fn respects_max_symbols() {
        let src = "pub fn a() {}\npub fn b() {}\npub fn c() {}";
        let positions = extract_symbol_positions_regex(src, "a.rs", 2);
        assert_eq!(positions.len(), 2);
    }

    #[test]
    fn no_symbols_empty_file() {
        let positions = extract_symbol_positions_regex("", "a.rs", 10);
        assert!(positions.is_empty());
    }

    #[test]
    fn handles_cat_n_prefix_regex() {
        let src = "   1\t// comment\n   2\tuse std::fmt;\n  30\tpub struct Foo {\n  31\t    x: u32,\n  32\t}\n  40\tpub fn bar() {}";
        let positions = extract_symbol_positions_regex(src, "a.rs", 10);
        assert_eq!(positions.len(), 2);
        assert_eq!(positions[0].0, 29);
        assert_eq!(positions[1].0, 39);
    }

    #[test]
    fn cat_n_character_offset_starts_at_zero() {
        let src = "   1\tpub fn top() {}";
        let positions = extract_symbol_positions_regex(src, "a.rs", 10);
        assert_eq!(positions.len(), 1);
        assert_eq!(positions[0].0, 0);
        assert_eq!(positions[0].1, 0);
    }

    #[test]
    fn non_digit_tab_prefix_no_symbol_match() {
        let src = "  abc\tpub fn foo() {}";
        let positions = extract_symbol_positions_regex(src, "a.rs", 10);
        assert!(positions.is_empty());
    }

    #[test]
    fn empty_prefix_before_tab_no_symbol_match() {
        let src = "\tpub fn foo() {}";
        let positions = extract_symbol_positions_regex(src, "a.rs", 10);
        assert!(positions.is_empty());
    }

    #[test]
    fn max_symbols_zero_returns_empty() {
        let src = "pub fn a() {}\npub fn b() {}";
        let positions = extract_symbol_positions_regex(src, "a.rs", 0);
        assert!(positions.is_empty());
    }

    #[test]
    fn mixed_cat_n_and_raw_lines() {
        let src = "   5\tpub struct Baz;\npub fn raw() {}";
        let positions = extract_symbol_positions_regex(src, "a.rs", 10);
        assert_eq!(positions.len(), 2);
        assert_eq!(positions[0].0, 4);
        assert_eq!(positions[1].0, 1);
    }

    #[test]
    fn tsquery_extracts_rust_symbols() {
        let src = "pub fn foo() {}\npub struct Bar;\npub enum Baz {}";
        let positions = extract_symbol_positions_tsquery(src, "a.rs", 10).unwrap();
        assert!(!positions.is_empty());
        // All three symbols at lines 0,1,2
        let lines: Vec<u64> = positions.iter().map(|(l, _)| *l).collect();
        assert!(lines.contains(&0));
        assert!(lines.contains(&1));
        assert!(lines.contains(&2));
    }

    /// GAP-1553-A: tsquery path works for Python source.
    #[test]
    fn tsquery_extracts_python_symbols() {
        let src = "def greet(name):\n    pass\n\nclass Animal:\n    pass\n";
        let positions = extract_symbol_positions_tsquery(src, "module.py", 10).unwrap();
        assert!(
            !positions.is_empty(),
            "should extract at least one Python symbol"
        );
        let lines: Vec<u64> = positions.iter().map(|(l, _)| *l).collect();
        assert!(lines.contains(&0), "greet() starts at line 0");
        assert!(lines.contains(&3), "Animal starts at line 3");
    }

    /// GAP-1553-A: tsquery path works for JavaScript source.
    #[test]
    fn tsquery_extracts_javascript_symbols() {
        let src = "function hello() {}\nclass Greeter {}\n";
        let positions = extract_symbol_positions_tsquery(src, "app.js", 10).unwrap();
        assert!(
            !positions.is_empty(),
            "should extract at least one JS symbol"
        );
        let lines: Vec<u64> = positions.iter().map(|(l, _)| *l).collect();
        assert!(lines.contains(&0), "hello() starts at line 0");
        assert!(lines.contains(&1), "Greeter starts at line 1");
    }

    #[test]
    fn tsquery_returns_none_for_unsupported_lang() {
        let src = "hello: world\n";
        let result = extract_symbol_positions_tsquery(src, "config.toml", 10);
        // toml has no symbol_query → None
        assert!(result.is_none());
    }

    #[test]
    fn tsquery_respects_max_symbols() {
        let src = "pub fn a() {}\npub fn b() {}\npub fn c() {}\npub fn d() {}";
        let positions = extract_symbol_positions_tsquery(src, "a.rs", 2).unwrap();
        assert!(positions.len() <= 2);
    }

    // Tests that verify fetch_hover_inner passes the correct argument keys to McpManager.call_tool.
    // Regression tests for issue #1538: wrong "path" key was used instead of "file_path".

    use std::sync::Arc;

    use crate::lsp_hooks::test_helpers::RecordingCaller;

    /// Rust source with one top-level symbol so extract_symbol_positions finds it.
    const RUST_SOURCE_ONE_FN: &str = "pub fn my_function() {}";

    #[tokio::test]
    async fn fetch_hover_passes_file_path_key() {
        use zeph_memory::TokenCounter;

        use crate::config::{HoverConfig, LspConfig};
        use crate::sanitizer::{ContentIsolationConfig, ContentSanitizer};

        let mock = RecordingCaller::new().with_text("hover info for my_function");
        let config = LspConfig {
            hover: HoverConfig {
                enabled: true,
                max_symbols: 1,
            },
            ..LspConfig::default()
        };
        let tc = Arc::new(TokenCounter::default());
        let sanitizer = ContentSanitizer::new(&ContentIsolationConfig::default());
        let params = serde_json::json!({ "path": "src/lib.rs" });

        fetch_hover_inner(&mock, &config, &params, RUST_SOURCE_ONE_FN, &tc, &sanitizer).await;

        let calls = mock.calls.lock().unwrap();
        assert!(
            !calls.is_empty(),
            "expected at least one call_tool invocation"
        );
        let args = &calls[0].2;
        assert!(
            args.get("file_path").is_some(),
            "call_tool args must contain 'file_path' key, got: {args}"
        );
        assert!(
            args.get("path").is_none(),
            "call_tool args must NOT contain old 'path' key, got: {args}"
        );
        assert_eq!(calls[0].1, "get_hover");
    }

    #[tokio::test]
    async fn fetch_hover_passes_line_and_character_keys() {
        use zeph_memory::TokenCounter;

        use crate::config::{HoverConfig, LspConfig};
        use crate::sanitizer::{ContentIsolationConfig, ContentSanitizer};

        let mock = RecordingCaller::new().with_text("hover info");
        let config = LspConfig {
            hover: HoverConfig {
                enabled: true,
                max_symbols: 1,
            },
            ..LspConfig::default()
        };
        let tc = Arc::new(TokenCounter::default());
        let sanitizer = ContentSanitizer::new(&ContentIsolationConfig::default());
        let params = serde_json::json!({ "path": "src/lib.rs" });

        fetch_hover_inner(&mock, &config, &params, RUST_SOURCE_ONE_FN, &tc, &sanitizer).await;

        let calls = mock.calls.lock().unwrap();
        assert!(
            !calls.is_empty(),
            "expected at least one call_tool invocation"
        );
        let args = &calls[0].2;
        assert!(
            args.get("line").is_some(),
            "call_tool args must contain 'line' key, got: {args}"
        );
        assert!(
            args.get("character").is_some(),
            "call_tool args must contain 'character' key, got: {args}"
        );
    }
}
