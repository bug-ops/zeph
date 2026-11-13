// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Single resolution of a tool result's classification, shared by every agent loop.

use zeph_common::trust_level::{ToolProvenance, ToolResultSource, TurnContextTrust};
use zeph_common::types::McpServerId;
use zeph_sanitizer::{ContentSource, content_source, native_kind};

use crate::executor::ClaimSource;
use crate::registry::McpToolIds;

/// Who wrote the body being classified.
///
/// # Examples
///
/// ```
/// use zeph_tools::BodyAuthor;
///
/// assert_ne!(BodyAuthor::Tool, BodyAuthor::Harness);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyAuthor {
    /// The tool (or MCP server) produced the body, possibly replayed from storage.
    Tool,
    /// Zeph wrote the body itself (gate refusal, utility skip, local policy denial).
    Harness,
}

/// A tool result classification that has been through the registry/claim join.
///
/// Only [`ResolvedToolSource::resolve`] builds one, so a consumer that takes it (the sanitizer,
/// the overflow save, the reflection path) cannot receive a source that skipped the join.
///
/// # Examples
///
/// ```
/// use zeph_common::trust_level::{ToolProvenance, TurnContextTrust};
/// use zeph_common::types::McpServerId;
/// use zeph_tools::{BodyAuthor, McpToolIds, ResolvedToolSource};
///
/// let mut ids = McpToolIds::default();
/// ids.insert("github_create_issue".into(), "github".into());
/// let resolved = ResolvedToolSource::resolve("github_create_issue", None, None, &ids, BodyAuthor::Tool);
/// assert_eq!(resolved.provenance(), ToolProvenance::Mcp);
/// assert_eq!(resolved.trust(), TurnContextTrust::ExternalUntrusted);
/// assert_eq!(resolved.mcp_server_id().map(McpServerId::as_str), Some("github"));
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedToolSource(ToolResultSource);

impl ResolvedToolSource {
    /// Resolve the classification of one tool result.
    ///
    /// The base is the stored source for a replayed body, otherwise the native kind selected by
    /// `tool_name`. Provenance is the join of the registry, the base and the executor claim, so
    /// any `Mcp` observation wins. A [`BodyAuthor::Harness`] body (gate refusal, local policy denial)
    /// is zeph-written text and ignores all three.
    #[must_use]
    pub fn resolve(
        tool_name: &str,
        stored: Option<&ToolResultSource>,
        claim: Option<ClaimSource>,
        mcp_ids: &McpToolIds,
        author: BodyAuthor,
    ) -> Self {
        let native = || ToolResultSource::Native(native_kind(tool_name));
        if author == BodyAuthor::Harness {
            return Self(native());
        }
        let base = stored.cloned().unwrap_or_else(native);
        let provenance = mcp_ids
            .provenance(tool_name)
            .join(base.provenance())
            .join(claim.map_or(ToolProvenance::Native, ToolProvenance::from));
        Self(match (provenance, base) {
            (ToolProvenance::Native, base) => base,
            (ToolProvenance::Mcp, base) => ToolResultSource::Mcp {
                server_id: mcp_ids.server_id(tool_name).cloned().or(match base {
                    ToolResultSource::Mcp { server_id } => server_id,
                    ToolResultSource::Native(_) | ToolResultSource::Archived(_) => None,
                }),
            },
        })
    }

    /// The resolved classification, as stored with an overflow row.
    #[must_use]
    pub fn source(&self) -> &ToolResultSource {
        &self.0
    }

    /// Provenance of the resolved source.
    #[must_use]
    pub fn provenance(&self) -> ToolProvenance {
        self.0.provenance()
    }

    /// Content-trust tier of the resolved source.
    #[must_use]
    pub fn trust(&self) -> TurnContextTrust {
        self.0.trust()
    }

    /// Serving MCP server id, when the source is MCP and the server is known.
    #[must_use]
    pub fn mcp_server_id(&self) -> Option<&McpServerId> {
        match &self.0 {
            ToolResultSource::Mcp { server_id } => server_id.as_ref(),
            ToolResultSource::Native(_) | ToolResultSource::Archived(_) => None,
        }
    }

    /// The sanitizer source for this classification, labelled with `identifier`.
    #[must_use]
    pub fn content_source(&self, identifier: &str) -> ContentSource {
        content_source(&self.0, identifier)
    }
}

#[cfg(test)]
mod tests {
    use zeph_common::trust_level::{NativeToolKind, UntrustedTier};

    use super::*;

    fn github_ids() -> McpToolIds {
        let mut ids = McpToolIds::default();
        ids.insert("github_create_issue".into(), "github".into());
        ids
    }

    #[test]
    fn unregistered_tool_resolves_to_its_native_kind() {
        let none = McpToolIds::default();
        let kind = |name| ResolvedToolSource::resolve(name, None, None, &none, BodyAuthor::Tool);
        assert_eq!(
            kind("bash").source(),
            &ToolResultSource::Native(NativeToolKind::Local)
        );
        assert_eq!(
            kind("web_search").source(),
            &ToolResultSource::Native(NativeToolKind::Web)
        );
        assert_eq!(
            kind("memory_search").source(),
            &ToolResultSource::Native(NativeToolKind::Memory)
        );
    }

    #[test]
    fn registry_claim_and_stored_each_make_the_result_mcp() {
        let ids = github_ids();
        let none = McpToolIds::default();
        let by_registry =
            ResolvedToolSource::resolve("github_create_issue", None, None, &ids, BodyAuthor::Tool);
        let by_claim =
            ResolvedToolSource::resolve("x", None, Some(ClaimSource::Mcp), &none, BodyAuthor::Tool);
        let stored = ToolResultSource::Mcp {
            server_id: Some("github".into()),
        };
        let by_stored = ResolvedToolSource::resolve(
            "read_overflow",
            Some(&stored),
            None,
            &none,
            BodyAuthor::Tool,
        );
        for resolved in [by_registry, by_claim, by_stored] {
            assert_eq!(resolved.provenance(), ToolProvenance::Mcp);
        }
    }

    #[test]
    fn stored_native_source_is_kept_and_claim_can_only_raise_it() {
        let none = McpToolIds::default();
        let stored = ToolResultSource::Native(NativeToolKind::Web);
        let kept = ResolvedToolSource::resolve(
            "read_overflow",
            Some(&stored),
            None,
            &none,
            BodyAuthor::Tool,
        );
        assert_eq!(kept.source(), &stored);
        let raised = ResolvedToolSource::resolve(
            "read_overflow",
            Some(&stored),
            Some(ClaimSource::Mcp),
            &none,
            BodyAuthor::Tool,
        );
        assert_eq!(raised.provenance(), ToolProvenance::Mcp);
    }

    #[test]
    fn server_id_prefers_registry_then_stored() {
        let stored = ToolResultSource::Mcp {
            server_id: Some("old".into()),
        };
        let live = ResolvedToolSource::resolve(
            "github_create_issue",
            Some(&stored),
            None,
            &github_ids(),
            BodyAuthor::Tool,
        );
        assert_eq!(
            live.mcp_server_id().map(McpServerId::as_str),
            Some("github")
        );
        let none = McpToolIds::default();
        let fallback = ResolvedToolSource::resolve(
            "read_overflow",
            Some(&stored),
            None,
            &none,
            BodyAuthor::Tool,
        );
        assert_eq!(
            fallback.mcp_server_id().map(McpServerId::as_str),
            Some("old")
        );
    }

    #[test]
    fn harness_authored_text_is_native_whatever_the_registry_says() {
        let resolved = ResolvedToolSource::resolve(
            "github_create_issue",
            None,
            Some(ClaimSource::Mcp),
            &github_ids(),
            BodyAuthor::Harness,
        );
        assert_eq!(resolved.provenance(), ToolProvenance::Native);
    }

    #[test]
    fn archived_source_is_kept_for_native_replay() {
        let none = McpToolIds::default();
        let stored = ToolResultSource::Archived(UntrustedTier::External);
        let resolved = ResolvedToolSource::resolve(
            "read_overflow",
            Some(&stored),
            None,
            &none,
            BodyAuthor::Tool,
        );
        assert_eq!(resolved.trust(), TurnContextTrust::ExternalUntrusted);
        assert_eq!(resolved.source(), &stored);
    }
}
