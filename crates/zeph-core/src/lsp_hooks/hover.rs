// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Hover-on-read hook: pre-fetches LSP hover info for top-level Rust symbols.
//!
//! Symbol extraction uses simple regex patterns (Rust-only for MVP).
//! Future phases can upgrade to tree-sitter via the `index` feature.

use std::sync::LazyLock;

use futures::StreamExt as _;
use regex::Regex;
use zeph_memory::TokenCounter;

use crate::sanitizer::{ContentSanitizer, ContentSource, ContentSourceKind};

use super::{LspHookRunner, LspNote};

/// Maximum concurrent MCP hover calls per file to avoid connection saturation.
const MAX_CONCURRENT_HOVER_CALLS: usize = 3;

/// Matches Rust top-level definition lines: `fn`, `struct`, `enum`, `trait`, `impl`, `type`.
static SYMBOL_LINE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?m)^(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?(?:fn|struct|enum|trait|impl|type)\s+\w",
    )
    .expect("valid regex")
});

/// Extract (`line_number`, `character_offset`) pairs for symbol definitions.
/// Lines and characters are 0-indexed (LSP convention).
fn extract_symbol_positions(content: &str, max_symbols: usize) -> Vec<(u64, u64)> {
    let mut positions = Vec::new();
    for m in SYMBOL_LINE_RE.find_iter(content) {
        if positions.len() >= max_symbols {
            break;
        }
        let line = content[..m.start()].chars().filter(|c| *c == '\n').count() as u64;
        let line_start = content[..m.start()].rfind('\n').map_or(0, |p| p + 1);
        let character = (m.start() - line_start) as u64;
        positions.push((line, character));
    }
    positions
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
    let file_path = tool_params.get("path").and_then(|v| v.as_str())?.to_owned();

    // Hover regex is Rust-only; skip other file types to avoid false positives.
    if !std::path::Path::new(&file_path)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("rs"))
    {
        return None;
    }

    // Extract symbol positions from the file content returned by the read tool.
    let positions = extract_symbol_positions(tool_output, runner.config.hover.max_symbols);
    if positions.is_empty() {
        return None;
    }

    let timeout = std::time::Duration::from_secs(runner.config.call_timeout_secs);
    let manager = &runner.manager;
    let server_id = &runner.config.mcp_server_id;

    // Use buffer_unordered to cap concurrent MCP connections.
    let mut entries: Vec<String> =
        futures::stream::iter(positions.iter().map(|(line, character)| {
            let args = serde_json::json!({
                "path": file_path,
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
    fn extracts_rust_symbols() {
        let src = "pub fn foo() {}\npub struct Bar;\npub enum Baz {}";
        let positions = extract_symbol_positions(src, 10);
        assert_eq!(positions.len(), 3);
        assert_eq!(positions[0].0, 0);
        assert_eq!(positions[1].0, 1);
        assert_eq!(positions[2].0, 2);
    }

    #[test]
    fn respects_max_symbols() {
        let src = "pub fn a() {}\npub fn b() {}\npub fn c() {}";
        let positions = extract_symbol_positions(src, 2);
        assert_eq!(positions.len(), 2);
    }

    #[test]
    fn no_symbols_empty_file() {
        let positions = extract_symbol_positions("", 10);
        assert!(positions.is_empty());
    }
}
