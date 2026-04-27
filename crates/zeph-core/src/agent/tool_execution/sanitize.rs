// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use zeph_sanitizer::{ContentSource, ContentSourceKind, MemorySourceHint};

use super::super::Agent;
use crate::channel::Channel;

#[cfg(feature = "classifiers")]
fn is_policy_blocked_output(body: &str) -> bool {
    body.contains("[tool_error]") && body.contains("category: policy_blocked")
}

/// Tools whose outputs are produced exclusively by Zeph's own code paths and cannot
/// carry attacker-controlled injection payloads. The `DeBERTa` ML classifier is bypassed
/// for these tools because innocuous internal error strings (e.g. "skill not found: exit")
/// trigger high-confidence false positives (#3384).
///
/// Safety invariant: only non-namespaced names are listed here. MCP tools use a
/// `server:tool` format and are routed to `ContentSourceKind::McpResponse` before this
/// check is reached, so they are never mistakenly matched.
///
/// `read_overflow` is intentionally excluded: its success path replays stored external
/// tool output and must remain subject to ML classification.
///
/// NOTE: if you add a new first-party tool, update this list. See also the overlapping
/// lists in `zeph-tools/src/config.rs::AdversarialPolicyConfig::default_exempt_tools`,
/// `zeph-config/src/vigil.rs`, and `zeph-common/src/quarantine.rs` — each serves a
/// different policy but must stay consistent.
#[cfg(feature = "classifiers")]
const INTERNAL_TOOLS: &[&str] = &[
    "invoke_skill",
    "load_skill",
    "memory_save",
    "memory_search",
    "compress_context",
    "complete_focus",
    "start_focus",
    "schedule_periodic",
    "schedule_deferred",
    "cancel_task",
];

/// Returns `true` only for non-MCP, Zeph-internal tools that cannot carry injection payloads.
///
/// The colon guard ensures a malicious MCP server cannot register a bare `invoke_skill`
/// tool name and bypass ML classification — MCP tools always use `server:tool` naming.
#[cfg(feature = "classifiers")]
fn is_internal_tool(tool_name: &str) -> bool {
    !tool_name.contains(':') && INTERNAL_TOOLS.contains(&tool_name)
}

/// Build the `ContentSource` that describes a tool's trust level for the sanitizer.
fn build_tool_output_source(tool_name: &str) -> ContentSource {
    if tool_name.contains(':') || tool_name == "mcp" {
        ContentSource::new(ContentSourceKind::McpResponse).with_identifier(tool_name)
    } else if tool_name == "web-scrape" || tool_name == "web_scrape" || tool_name == "fetch" {
        ContentSource::new(ContentSourceKind::WebScrape).with_identifier(tool_name)
    } else if tool_name == "memory_search" {
        ContentSource::new(ContentSourceKind::MemoryRetrieval)
            .with_identifier(tool_name)
            .with_memory_hint(MemorySourceHint::ConversationHistory)
    } else {
        ContentSource::new(ContentSourceKind::ToolResult).with_identifier(tool_name)
    }
}

impl<C: Channel> Agent<C> {
    /// Sanitize tool output body before inserting it into the LLM message history.
    ///
    /// Channel display (`send_tool_output`) still receives the raw body so the user
    /// sees unmodified output; spotlighting delimiters are added only for the LLM.
    ///
    /// This is the SOLE sanitization point for tool output data flows. Do not add
    /// redundant sanitization in leaf crates (zeph-tools, zeph-mcp).
    pub(super) async fn sanitize_tool_output(
        &mut self,
        body: &str,
        tool_name: &str,
    ) -> (String, bool) {
        let source = build_tool_output_source(tool_name);
        let kind = source.kind;
        #[cfg(feature = "classifiers")]
        let memory_hint = source.memory_hint;
        #[cfg(not(feature = "classifiers"))]
        let _ = source.memory_hint;
        let sanitized = self.services.security.sanitizer.sanitize(body, source);
        let has_injection_flags = !sanitized.injection_flags.is_empty();
        self.record_injection_flags(&sanitized, tool_name);
        if sanitized.was_truncated {
            self.update_metrics(|m| m.sanitizer_truncations += 1);
            self.push_security_event(
                zeph_common::SecurityEventCategory::Truncation,
                tool_name,
                "Content truncated to max_content_size",
            );
        }
        self.update_metrics(|m| m.sanitizer_runs += 1);

        #[cfg(feature = "classifiers")]
        if let Some(result) = self
            .apply_classifier_verdict(body, tool_name, memory_hint)
            .await
        {
            return result;
        }

        let is_cross_boundary = self.services.security.is_acp_session
            && self
                .runtime
                .config
                .security
                .content_isolation
                .mcp_to_acp_boundary
            && kind == ContentSourceKind::McpResponse;

        if is_cross_boundary
            && let Some(result) = self
                .handle_cross_boundary_quarantine(&sanitized, tool_name, has_injection_flags)
                .await
        {
            return result;
        }

        if !is_cross_boundary
            && let Some(result) = self
                .handle_quarantine_summary(&sanitized, tool_name, kind, has_injection_flags)
                .await
        {
            return result;
        }

        let body = self.scrub_pii_union(&sanitized.body, tool_name).await;
        let body = self.apply_guardrail_to_tool_output(body, tool_name).await;

        (body, has_injection_flags)
    }

    /// Record injection-flag metrics and security events for a sanitized output.
    fn record_injection_flags(
        &mut self,
        sanitized: &zeph_sanitizer::SanitizedContent,
        tool_name: &str,
    ) {
        if sanitized.injection_flags.is_empty() {
            return;
        }
        tracing::warn!(
            tool = %tool_name,
            flags = sanitized.injection_flags.len(),
            "injection patterns detected in tool output"
        );
        self.update_metrics(|m| {
            let flag_count = sanitized.injection_flags.len() as u64;
            m.sanitizer_injection_flags += flag_count;
            if sanitized.source.kind == zeph_sanitizer::ContentSourceKind::ToolResult {
                m.sanitizer_injection_fp_local += flag_count;
            }
        });
        let detail = sanitized
            .injection_flags
            .first()
            .map_or_else(String::new, |f| {
                format!("Detected pattern: {}", f.pattern_name)
            });
        self.push_security_event(
            zeph_common::SecurityEventCategory::InjectionFlag,
            tool_name,
            detail,
        );
        let urls = zeph_sanitizer::exfiltration::extract_flagged_urls(&sanitized.body);
        self.services.security.flagged_urls.extend(urls);
    }

    /// Run the ML classifier on `body` and return an early result if the output is blocked
    /// or if the classification verdict warrants it. Returns `None` to continue normal flow.
    ///
    /// Synthetic outputs from the utility gate are trusted internal content and are never
    /// classified. Memory-hinted outputs and first-party tool outputs are also exempt.
    #[cfg(feature = "classifiers")]
    async fn apply_classifier_verdict(
        &mut self,
        body: &str,
        tool_name: &str,
        memory_hint: Option<zeph_sanitizer::MemorySourceHint>,
    ) -> Option<(String, bool)> {
        // Synthetic outputs from the utility gate are trusted internal content — never
        // classify them. Only real tool output from external sources needs ML inspection.
        let is_utility_gate_synthetic =
            body.starts_with("[skipped]") || body.starts_with("[stopped]");
        let skip_ml = matches!(
            memory_hint,
            Some(
                zeph_sanitizer::MemorySourceHint::ConversationHistory
                    | zeph_sanitizer::MemorySourceHint::LlmSummary
            )
        ) || is_policy_blocked_output(body)
            || is_utility_gate_synthetic
            || is_internal_tool(tool_name);
        if !skip_ml && self.services.security.sanitizer.has_classifier_backend() {
            let ml_verdict = self
                .services
                .security
                .sanitizer
                .classify_injection(body)
                .await;
            match ml_verdict {
                zeph_sanitizer::InjectionVerdict::Blocked => {
                    tracing::warn!(tool = %tool_name, "ML classifier blocked tool output");
                    self.update_metrics(|m| m.classifier_tool_blocks += 1);
                    self.push_security_event(
                        zeph_common::SecurityEventCategory::InjectionBlocked,
                        tool_name,
                        "ML classifier blocked tool output",
                    );
                    return Some((
                        "[tool output blocked: injection detected by classifier]".into(),
                        true,
                    ));
                }
                zeph_sanitizer::InjectionVerdict::Suspicious => {
                    tracing::warn!(
                        tool = %tool_name,
                        "ML classifier: suspicious tool output"
                    );
                    self.update_metrics(|m| m.classifier_tool_suspicious += 1);
                }
                zeph_sanitizer::InjectionVerdict::Clean => {}
            }
        }
        None
    }

    /// Handle the cross-ACP-boundary quarantine path for MCP tool results.
    ///
    /// Logs a trust-boundary warning, fires an audit entry, and attempts fact extraction.
    /// Returns `Some((body, has_injection_flags))` if quarantine produced an early result,
    /// or `None` to continue normal processing.
    async fn handle_cross_boundary_quarantine(
        &mut self,
        sanitized: &zeph_sanitizer::SanitizedContent,
        tool_name: &str,
        has_injection_flags: bool,
    ) -> Option<(String, bool)> {
        tracing::warn!(
            tool = %tool_name,
            mcp_server_id = tool_name.split(':').next().unwrap_or("unknown"),
            "MCP tool result crossing ACP trust boundary"
        );
        self.push_security_event(
            zeph_common::SecurityEventCategory::CrossBoundaryMcpToAcp,
            tool_name,
            "MCP result force-quarantined for ACP session",
        );
        if let Some(ref logger) = self.tool_orchestrator.audit_logger {
            let entry = zeph_tools::AuditEntry {
                timestamp: zeph_tools::chrono_now(),
                tool: tool_name.into(),
                command: String::new(),
                result: zeph_tools::AuditResult::Success,
                duration_ms: 0,
                error_category: None,
                error_domain: Some("security".to_owned()),
                error_phase: None,
                claim_source: None,
                mcp_server_id: tool_name.split(':').next().map(ToOwned::to_owned),
                injection_flagged: has_injection_flags,
                embedding_anomalous: false,
                cross_boundary_mcp_to_acp: true,
                adversarial_policy_decision: None,
                exit_code: None,
                truncated: false,
                caller_id: None,
                policy_match: None,
                correlation_id: None,
                vigil_risk: None,
            };
            let logger = std::sync::Arc::clone(logger);
            self.runtime.lifecycle.supervisor.spawn(
                super::super::agent_supervisor::TaskClass::Telemetry,
                "audit-log-sanitize",
                async move { logger.log(&entry).await },
            );
        }
        if let Some(ref qs) = self.services.security.quarantine_summarizer {
            match qs
                .extract_facts(sanitized, &self.services.security.sanitizer)
                .await
            {
                Ok((facts, flags)) => {
                    self.update_metrics(|m| m.quarantine_invocations += 1);
                    let escaped = zeph_sanitizer::ContentSanitizer::escape_delimiter_tags(&facts);
                    return Some((
                        zeph_sanitizer::ContentSanitizer::apply_spotlight(
                            &escaped,
                            &sanitized.source,
                            &flags,
                        ),
                        has_injection_flags,
                    ));
                }
                Err(e) => {
                    tracing::warn!(
                        tool = %tool_name,
                        error = %e,
                        "cross-boundary quarantine failed, using spotlighted output"
                    );
                    self.update_metrics(|m| m.quarantine_failures += 1);
                }
            }
        }
        None
    }

    /// Handle standard quarantine summarization for non-cross-boundary tool outputs.
    ///
    /// Returns `Some((body, has_injection_flags))` if quarantine produced an early result,
    /// or `None` to continue normal processing.
    async fn handle_quarantine_summary(
        &mut self,
        sanitized: &zeph_sanitizer::SanitizedContent,
        tool_name: &str,
        kind: ContentSourceKind,
        has_injection_flags: bool,
    ) -> Option<(String, bool)> {
        if !(self.services.security.sanitizer.is_enabled()
            && self
                .services
                .security
                .quarantine_summarizer
                .as_ref()
                .is_some_and(|qs| qs.should_quarantine(kind)))
        {
            return None;
        }
        let qs = self.services.security.quarantine_summarizer.as_ref()?;
        match qs
            .extract_facts(sanitized, &self.services.security.sanitizer)
            .await
        {
            Ok((facts, flags)) => {
                self.update_metrics(|m| m.quarantine_invocations += 1);
                self.push_security_event(
                    zeph_common::SecurityEventCategory::Quarantine,
                    tool_name,
                    "Content quarantined, facts extracted",
                );
                let escaped = zeph_sanitizer::ContentSanitizer::escape_delimiter_tags(&facts);
                Some((
                    zeph_sanitizer::ContentSanitizer::apply_spotlight(
                        &escaped,
                        &sanitized.source,
                        &flags,
                    ),
                    has_injection_flags,
                ))
            }
            Err(e) => {
                tracing::warn!(
                    tool = %tool_name,
                    error = %e,
                    "quarantine failed, using original sanitized output"
                );
                self.update_metrics(|m| m.quarantine_failures += 1);
                self.push_security_event(
                    zeph_common::SecurityEventCategory::Quarantine,
                    tool_name,
                    format!("Quarantine failed: {e}"),
                );
                None
            }
        }
    }
}

#[cfg(all(test, feature = "classifiers"))]
mod tests {
    use super::is_internal_tool;

    #[test]
    fn internal_tool_allowlist_covers_all_zeph_tools() {
        for name in [
            "invoke_skill",
            "load_skill",
            "memory_save",
            "memory_search",
            "compress_context",
            "complete_focus",
            "start_focus",
            "schedule_periodic",
            "schedule_deferred",
            "cancel_task",
        ] {
            assert!(
                is_internal_tool(name),
                "{name} must be in internal allowlist"
            );
        }
    }

    #[test]
    fn external_and_mcp_tools_not_in_allowlist() {
        for name in [
            "shell",
            "web-scrape",
            "fetch",
            "read_overflow",
            "github:list_issues",
            "my-server:invoke_skill",
            "mcp:invoke_skill",
        ] {
            assert!(
                !is_internal_tool(name),
                "{name} must NOT be in internal allowlist"
            );
        }
    }

    #[test]
    fn colon_namespaced_names_always_excluded() {
        // An adversarial MCP server cannot bypass classification by registering a tool
        // with the same bare name as an internal tool.
        assert!(!is_internal_tool("server:invoke_skill"));
        assert!(!is_internal_tool("attacker:memory_save"));
        assert!(!is_internal_tool("x:cancel_task"));
    }
}
