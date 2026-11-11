// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Tool name to [`ContentSource`] mapping shared by the main agent and sub-agent loops.

use crate::types::{ContentSource, ContentSourceKind, MemorySourceHint};

/// Build the [`ContentSource`] that describes a tool's trust level for the sanitizer.
///
/// The mapping keys on the tool name only: `server:tool` ids and `mcp` map to
/// [`ContentSourceKind::McpResponse`], web tools to [`ContentSourceKind::WebScrape`],
/// `memory_search` to [`ContentSourceKind::MemoryRetrieval`], everything else to
/// [`ContentSourceKind::ToolResult`]. Both agent loops call this so a tool is never trusted
/// more in one loop than in the other.
///
/// # Examples
///
/// ```
/// use zeph_sanitizer::{ContentSourceKind, tool_output_source};
///
/// assert_eq!(tool_output_source("web_search").kind, ContentSourceKind::WebScrape);
/// assert_eq!(tool_output_source("srv:tool").kind, ContentSourceKind::McpResponse);
/// assert_eq!(tool_output_source("bash").kind, ContentSourceKind::ToolResult);
/// ```
#[must_use]
pub fn tool_output_source(tool_name: &str) -> ContentSource {
    if tool_name.contains(':') || tool_name == "mcp" {
        ContentSource::new(ContentSourceKind::McpResponse).with_identifier(tool_name)
    } else if matches!(
        tool_name,
        "web-scrape" | "web_scrape" | "fetch" | "web_search"
    ) {
        ContentSource::new(ContentSourceKind::WebScrape).with_identifier(tool_name)
    } else if tool_name == "memory_search" {
        ContentSource::new(ContentSourceKind::MemoryRetrieval)
            .with_identifier(tool_name)
            .with_memory_hint(MemorySourceHint::ConversationHistory)
    } else {
        ContentSource::new(ContentSourceKind::ToolResult).with_identifier(tool_name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// INVARIANT-1 (spec 006-1-web-search §4): `web_search` output must be classified
    /// identically to `web_scrape`/`fetch` through the tool-name branch, not `ClaimSource`.
    #[test]
    fn web_tools_classify_as_web_scrape() {
        for name in ["web_search", "web-scrape", "web_scrape", "fetch"] {
            assert_eq!(
                tool_output_source(name).kind,
                ContentSourceKind::WebScrape,
                "{name}"
            );
        }
    }

    #[test]
    fn unrelated_tool_classifies_as_tool_result() {
        assert_eq!(
            tool_output_source("some_other_tool").kind,
            ContentSourceKind::ToolResult
        );
    }

    #[test]
    fn namespaced_and_mcp_tools_classify_as_mcp_response() {
        for name in ["server:tool", "mcp"] {
            assert_eq!(
                tool_output_source(name).kind,
                ContentSourceKind::McpResponse,
                "{name}"
            );
        }
    }
}
