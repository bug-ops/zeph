// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Tool name to [`ContentSource`] mapping shared by the main agent and sub-agent loops.

use zeph_common::trust_level::{NativeToolKind, ToolProvenance, ToolResultSource};

use crate::types::{ContentSource, ContentSourceKind, MemorySourceHint};

/// Build the [`ContentSource`] that describes a tool's trust level for the sanitizer.
///
/// [`ToolProvenance::Mcp`] maps to [`ContentSourceKind::McpResponse`] whatever the name is.
/// For [`ToolProvenance::Native`] the name selects the kind: web tools map to
/// [`ContentSourceKind::WebScrape`], `memory_search` to [`ContentSourceKind::MemoryRetrieval`],
/// everything else to [`ContentSourceKind::ToolResult`]. Every agent loop calls this so a tool
/// is never trusted more in one loop than in another.
///
/// # Examples
///
/// ```
/// use zeph_sanitizer::{ContentSourceKind, ToolProvenance, tool_output_source};
///
/// let native = |name| tool_output_source(name, ToolProvenance::Native).kind;
/// assert_eq!(native("web_search"), ContentSourceKind::WebScrape);
/// assert_eq!(native("bash"), ContentSourceKind::ToolResult);
/// assert_eq!(
///     tool_output_source("github_create_issue", ToolProvenance::Mcp).kind,
///     ContentSourceKind::McpResponse
/// );
/// ```
#[must_use]
pub fn tool_output_source(tool_name: &str, provenance: ToolProvenance) -> ContentSource {
    let source = match provenance {
        ToolProvenance::Mcp => ToolResultSource::Mcp { server_id: None },
        ToolProvenance::Native => ToolResultSource::Native(native_kind(tool_name)),
    };
    content_source(&source, tool_name)
}

/// Kind of a built-in tool, selected by name: web tools, `memory_search`, otherwise local.
///
/// # Examples
///
/// ```
/// use zeph_sanitizer::native_kind;
/// use zeph_common::trust_level::NativeToolKind;
///
/// assert_eq!(native_kind("web_search"), NativeToolKind::Web);
/// assert_eq!(native_kind("bash"), NativeToolKind::Local);
/// ```
#[must_use]
pub fn native_kind(tool_name: &str) -> NativeToolKind {
    match tool_name {
        "web-scrape" | "web_scrape" | "fetch" | "web_search" => NativeToolKind::Web,
        "memory_search" => NativeToolKind::Memory,
        _ => NativeToolKind::Local,
    }
}

/// Build the [`ContentSource`] for a classified tool result, live or replayed from storage.
///
/// Archived bodies are post-sanitize context text: they get the generic tool-result kind at
/// their recorded tier, never a kind they do not have.
///
/// # Examples
///
/// ```
/// use zeph_common::trust_level::{NativeToolKind, ToolResultSource};
/// use zeph_sanitizer::{ContentSourceKind, content_source};
///
/// let web = ToolResultSource::Native(NativeToolKind::Web);
/// assert_eq!(content_source(&web, "web_scrape").kind, ContentSourceKind::WebScrape);
/// ```
#[must_use]
pub fn content_source(source: &ToolResultSource, identifier: &str) -> ContentSource {
    match source {
        ToolResultSource::Mcp { .. } => {
            ContentSource::new(ContentSourceKind::McpResponse).with_identifier(identifier)
        }
        ToolResultSource::Native(NativeToolKind::Web) => {
            ContentSource::new(ContentSourceKind::WebScrape).with_identifier(identifier)
        }
        ToolResultSource::Native(NativeToolKind::Memory) => {
            ContentSource::new(ContentSourceKind::MemoryRetrieval)
                .with_identifier(identifier)
                .with_memory_hint(MemorySourceHint::ConversationHistory)
        }
        ToolResultSource::Native(NativeToolKind::Local) => {
            ContentSource::new(ContentSourceKind::ToolResult).with_identifier(identifier)
        }
        ToolResultSource::Archived(tier) => ContentSource::new(ContentSourceKind::ToolResult)
            .with_identifier(identifier)
            .with_trust_level(tier.trust()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// INVARIANT-1 (spec 006-1-web-search §4): `web_search` output must be classified
    /// identically to `web_scrape`/`fetch` through the native tool-name branch.
    #[test]
    fn native_web_tools_classify_as_web_scrape() {
        for name in ["web_search", "web-scrape", "web_scrape", "fetch"] {
            assert_eq!(
                tool_output_source(name, ToolProvenance::Native).kind,
                ContentSourceKind::WebScrape,
                "{name}"
            );
        }
    }

    #[test]
    fn native_memory_search_classifies_as_memory_retrieval() {
        assert_eq!(
            tool_output_source("memory_search", ToolProvenance::Native).kind,
            ContentSourceKind::MemoryRetrieval
        );
    }

    #[test]
    fn unrelated_native_tool_classifies_as_tool_result() {
        assert_eq!(
            tool_output_source("some_other_tool", ToolProvenance::Native).kind,
            ContentSourceKind::ToolResult
        );
    }

    #[test]
    fn name_shape_does_not_imply_mcp() {
        for name in ["server:tool", "mcp", "github_create_issue"] {
            assert_eq!(
                tool_output_source(name, ToolProvenance::Native).kind,
                ContentSourceKind::ToolResult,
                "{name}"
            );
        }
    }

    #[test]
    fn mcp_provenance_overrides_every_name() {
        for name in [
            "github_create_issue",
            "web_search",
            "memory_search",
            "fetch",
            "bash",
        ] {
            assert_eq!(
                tool_output_source(name, ToolProvenance::Mcp).kind,
                ContentSourceKind::McpResponse,
                "{name}"
            );
        }
    }

    #[test]
    fn content_source_trust_matches_result_source_trust() {
        use zeph_common::trust_level::UntrustedTier;
        let sources = [
            ToolResultSource::Native(NativeToolKind::Local),
            ToolResultSource::Native(NativeToolKind::Web),
            ToolResultSource::Native(NativeToolKind::Memory),
            ToolResultSource::Mcp { server_id: None },
            ToolResultSource::Archived(UntrustedTier::Local),
            ToolResultSource::Archived(UntrustedTier::External),
        ];
        for source in sources {
            assert_eq!(
                content_source(&source, "t").trust_level,
                source.trust(),
                "{source:?}"
            );
        }
    }

    #[test]
    fn archived_source_keeps_recorded_tier_on_generic_kind() {
        use zeph_common::trust_level::UntrustedTier;
        let source = content_source(&ToolResultSource::Archived(UntrustedTier::External), "t");
        assert_eq!(source.kind, ContentSourceKind::ToolResult);
        assert_eq!(
            source.trust_level,
            zeph_common::trust_level::TurnContextTrust::ExternalUntrusted
        );
    }

    #[test]
    fn memory_replay_keeps_conversation_history_hint() {
        let source = content_source(
            &ToolResultSource::Native(NativeToolKind::Memory),
            "memory_search",
        );
        assert_eq!(
            source.memory_hint,
            Some(MemorySourceHint::ConversationHistory)
        );
    }
}
