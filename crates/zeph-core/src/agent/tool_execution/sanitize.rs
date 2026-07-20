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
    "bash",
    "shell",
    "invoke_skill",
    "load_skill",
    "memory_save",
    "memory_search",
    "compress_context",
    "request_compaction",
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

/// Splits off the literal `"$ {command}\n"` echo line that `bash`/`shell` tool output is
/// prefixed with (`crates/zeph-tools/src/shell/mod.rs`'s `execute_block`/
/// `execute_block_with_context`, `format!("$ {command}\n{filtered}")`), so that this
/// Zeph-generated echo text is never subject to PII scanning (regex or NER) — only the
/// actual command output after it is.
///
/// Unlike [`is_internal_tool`], which exempts an internal tool's *entire* output from the
/// ML injection classifier (safe there because injection payloads must be attacker-authored
/// text), `bash`/`shell` output legitimately CAN contain real PII from the command's actual
/// output (e.g. `cat customer_data.csv`). So only the literal echo line — never real command
/// output — is exempt here; everything after the first line still goes through full PII
/// scanning unchanged (#5702).
///
/// Returns `(prefix, remainder)`: `prefix` must be reattached unscanned (includes the
/// trailing newline), and `remainder` is what the PII scrubber should run on. For any tool
/// other than `bash`/`shell`, or when the body doesn't start with the exact `"$ "` echo
/// marker (e.g. a snapshot-rollback warning line precedes it — a narrow, accepted gap since
/// that shape is rare), returns `("", body)` unchanged so full PII scanning applies as before.
fn split_bash_echo_prefix<'a>(body: &'a str, tool_name: &str) -> (&'a str, &'a str) {
    if !matches!(tool_name, "bash" | "shell") || !body.starts_with("$ ") {
        return ("", body);
    }
    match body.find('\n') {
        Some(idx) => body.split_at(idx + 1),
        None => ("", body),
    }
}

/// Worst-case content-trust tier a batch of about-to-be-dispatched tool calls could
/// introduce, computed purely from `tool_name` — no execution needed.
///
/// `build_tool_output_source(name).trust_level` never varies with a tool's actual output,
/// only with its name (see `sanitize_tool_output`, which computes `trust_level` from the
/// source before any sanitization happens), so this precomputation is exact, not merely a
/// conservative upper bound. Used to ratchet the write-time memory-consent gate slot BEFORE
/// tier dispatch starts (issue #6569: same-tier/cross-tier parallel-dispatch TOCTOU race).
///
/// `memory_save` itself is excluded: a `memory_save` call's own (not-yet-produced) result is
/// always trusted `ToolResult`-tier content and must never gate itself — only *other* tool
/// calls dispatched in the same batch (e.g. `web_scrape`, `memory_search`) contribute here.
/// Without this exclusion, a lone `memory_save` call would spuriously require confirmation
/// whenever `confirm_threshold` is configured at or below `local_untrusted`.
fn batch_dispatch_trust_level(
    tool_calls: &[zeph_llm::provider::ToolUseRequest],
) -> zeph_sanitizer::ContentTrustLevel {
    tool_calls
        .iter()
        .filter(|tc| tc.name.as_str() != "memory_save")
        .map(|tc| build_tool_output_source(tc.name.as_str()).trust_level)
        .max()
        .unwrap_or(zeph_sanitizer::ContentTrustLevel::Trusted)
}

/// Build the `ContentSource` that describes a tool's trust level for the sanitizer.
fn build_tool_output_source(tool_name: &str) -> ContentSource {
    if tool_name.contains(':') || tool_name == "mcp" {
        ContentSource::new(ContentSourceKind::McpResponse).with_identifier(tool_name)
    } else if tool_name == "web-scrape"
        || tool_name == "web_scrape"
        || tool_name == "fetch"
        || tool_name == "web_search"
    {
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
    ) -> (String, bool, zeph_sanitizer::ContentTrustLevel) {
        let source = build_tool_output_source(tool_name);
        let kind = source.kind;
        let trust_level = source.trust_level;
        // Ratchet the turn-scoped memory-consent trust tracker (issue #6490, MemGhost) so
        // `MemoryToolExecutor::memory_save` can require confirmation when the LLM tries to
        // save content derived from this turn's untrusted tool output. Never lowers the
        // value — reset only happens at turn boundaries (`process_response`/history clear).
        {
            let mut slot = self.services.security.memory_consent_trust.write();
            *slot = (*slot).max(trust_level as u8);
        }
        #[cfg(feature = "classifiers")]
        let memory_hint = source.memory_hint;
        #[cfg(not(feature = "classifiers"))]
        let _ = source.memory_hint;

        // Scrub PII on the raw payload BEFORE `ContentSanitizer::sanitize` wraps it in the
        // <tool-output>/<external-data> spotlight XML. Scanning after wrapping (the previous
        // order) let the regex/NER scan redact structural wrapper text — including the tool
        // identifier in the `name` attribute (#5647) — instead of only the actual tool output
        // content, which is also how ordinary numeric output got misredacted (#5702).
        //
        // For bash/shell, the leading "$ {command}\n" echo line is Zeph-generated text (not
        // real command output) and is split off first so it's never PII-scanned either —
        // command-echo tokens like `+%s.%N` were otherwise misclassified by the NER model as
        // e.g. `[PII:PASSWORD]` (#5702). Everything after that line is real command output and
        // still goes through full PII scanning, since it can legitimately contain real PII.
        let (echo_prefix, scrub_target) = split_bash_echo_prefix(body, tool_name);
        let scrubbed_remainder = self.scrub_pii_union(scrub_target, tool_name).await;
        let body = if echo_prefix.is_empty() {
            scrubbed_remainder
        } else {
            format!("{echo_prefix}{scrubbed_remainder}")
        };

        let sanitized = self.services.security.sanitizer.sanitize(&body, source);
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
        if let Some((b, f)) = self
            .apply_classifier_verdict(&body, tool_name, memory_hint)
            .await
        {
            return (b, f, trust_level);
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
            && let Some((b, f)) = self
                .handle_cross_boundary_quarantine(&sanitized, tool_name, has_injection_flags)
                .await
        {
            return (b, f, trust_level);
        }

        if !is_cross_boundary
            && let Some((b, f)) = self
                .handle_quarantine_summary(&sanitized, tool_name, kind, has_injection_flags)
                .await
        {
            return (b, f, trust_level);
        }

        let body = sanitized.body;
        self.record_nli_verdict(&body, tool_name).await;
        let body = self.apply_guardrail_to_tool_output(body, tool_name).await;

        (body, has_injection_flags, trust_level)
    }

    /// Maximum content-trust tier tagged on any message still present in the live
    /// conversation context (issue #6558: cross-turn memory-consent-gate deferral bypass).
    ///
    /// Scans `self.msg.messages` for the trust tag `process_tool_result_batch` writes onto
    /// each tool-result batch message (`MessageMetadata::trust_level`, set to the batch's
    /// worst-case trust tier). Unlike the turn-scoped ratchet-and-reset slot alone, this
    /// reflects untrusted content for as long as it remains in the model's context — including
    /// across a user-turn boundary — and only stops once `/clear` wipes `self.msg.messages`, or
    /// the tagged message is fully evicted by hard compaction/token-budget trimming (pruning
    /// alone does NOT clear it — pruning blanks a message's body in place but leaves the
    /// `Message`/its tag intact, which is intentionally conservative, never a false negative).
    /// LLM-summarization compaction also does NOT clear it: `compact_context`
    /// (`zeph-agent-context/src/summarization/compaction.rs`) propagates `max(trust_level)`
    /// of the compacted-away messages onto the new synthetic summary message, so a condensed
    /// untrusted tool result keeps gating `memory_save` through the summary. A plain `Vec`
    /// scan: no locks, no I/O, same cost class as `recompute_prompt_tokens`.
    ///
    /// Note: `memory_search` results are tagged `ExternalUntrusted` like any other untrusted
    /// tool output, which broadens `memory_save`'s gate footprint beyond the original per-turn
    /// design — this is intentional; see `MemoryToolExecutor::current_trust_level`'s doc
    /// comment for the full rationale.
    pub(super) fn context_max_trust_level(&self) -> zeph_sanitizer::ContentTrustLevel {
        self.msg
            .messages
            .iter()
            .filter_map(|m| m.metadata.trust_level)
            .map(zeph_sanitizer::ContentTrustLevel::from_ordinal)
            .max()
            .unwrap_or(zeph_sanitizer::ContentTrustLevel::Trusted)
    }

    /// Ratchet the write-time memory-consent trust slot to the worst case this dispatch batch
    /// could introduce, combined with whatever untrusted content is still present in the live
    /// context — BEFORE any tool call in `tool_calls` (including `memory_save` itself) starts
    /// executing.
    ///
    /// `MemoryToolExecutor`'s confirm-gate check only ever reads this slot — it has no handle
    /// to `Agent`/`self.msg` (the `ToolExecutor` trait is deliberately object-safe with no
    /// `&Agent` parameter) — so the slot must already hold the correct value by the time any
    /// tool in the batch is dispatched, not merely by the time `sanitize_tool_output` gets
    /// around to processing that tool's *own* result sequentially afterward. Closes both the
    /// cross-turn deferral bypass (#6558, via `context_max_trust_level`) and the same-tier /
    /// cross-tier parallel-dispatch race (#6569, via `batch_dispatch_trust_level` — computed
    /// before any tier's concurrent `join_all` starts, so it cannot lose a race regardless of
    /// tokio scheduling).
    pub(super) fn ratchet_memory_consent_trust_for_dispatch(
        &mut self,
        tool_calls: &[zeph_llm::provider::ToolUseRequest],
    ) {
        let effective = batch_dispatch_trust_level(tool_calls).max(self.context_max_trust_level());
        let mut slot = self.services.security.memory_consent_trust.write();
        *slot = (*slot).max(effective as u8);
    }

    /// Run the SONAR NLI entailment check on tool output and record a flagged verdict.
    ///
    /// Observe-only: unlike the ML classifier (which can block on `enforcement_mode=block`),
    /// a flagged NLI verdict only raises a `SecurityEventCategory::InjectionFlag` event and
    /// increments metrics — the body returned by `sanitize_tool_output` is never altered here.
    /// No-op when the NLI stage is disabled, inactive (no provider), or the circuit breaker
    /// has tripped (in which case `check` returns `None`).
    ///
    /// Also called from `pre_process_security` (user input boundary) with `tool_name` set to
    /// a synthetic source label — this is the shared observe-only NLI entry point.
    pub(crate) async fn record_nli_verdict(&mut self, body: &str, tool_name: &str) {
        let verdict = match self.services.security.nli_sanitizer.as_ref() {
            Some(nli) if nli.is_active() => nli.check(body).await,
            _ => None,
        };
        let Some(verdict) = verdict else {
            return;
        };
        self.update_metrics(|m| m.nli_checks += 1);
        if !verdict.flagged {
            return;
        }
        tracing::warn!(
            tool = %tool_name,
            score = verdict.injection_score,
            "NLI entailment check flagged tool output as likely injection"
        );
        self.update_metrics(|m| m.nli_flags += 1);
        self.push_security_event(
            zeph_common::SecurityEventCategory::InjectionFlag,
            tool_name,
            format!(
                "NLI entailment score {:.2} exceeded threshold",
                verdict.injection_score
            ),
        );
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
        // `flagged_urls` does exact-string membership checks in `ExfiltrationGuard::
        // validate_tool_call`/`scan_raw_args`, so entries must be normalized here — an
        // explicit-scheme and scheme-relative occurrence of the same URL must compare equal.
        // `extract_flagged_urls` itself returns raw text (other consumers, e.g.
        // `user_provided_urls` in `agent/mod.rs`, need exact fidelity), so normalization is
        // this call site's responsibility. See `normalize_url_for_matching`'s doc comment.
        let urls = zeph_sanitizer::exfiltration::extract_flagged_urls(&sanitized.body);
        self.services.security.flagged_urls.extend(
            urls.iter()
                .map(|u| zeph_sanitizer::exfiltration::normalize_url_for_matching(u).to_owned()),
        );
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
                _ => {}
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
        // `tool_name` here is always the qualified `server:tool` form (`ToolOutput.tool_name` is
        // deliberately set to `qualified_name()`, not `sanitized_id()` — see the invariant note
        // in `crates/zeph-mcp/src/executor.rs`), since this function only runs for
        // `ContentSourceKind::McpResponse`, which `build_tool_output_source` only assigns when
        // `tool_name.contains(':')`. Mirrors the reference split in
        // `crates/zeph-tools/src/policy_gate.rs`.
        let mcp_server_id = tool_name
            .split_once(':')
            .map(|(server, _)| server.to_owned());
        tracing::warn!(
            tool = %tool_name,
            mcp_server_id = mcp_server_id.as_deref().unwrap_or("unknown"),
            "MCP tool result crossing ACP trust boundary"
        );
        self.push_security_event(
            zeph_common::SecurityEventCategory::CrossBoundaryMcpToAcp,
            tool_name,
            "MCP result force-quarantined for ACP session",
        );
        if let Some(ref logger) = self.tool_orchestrator.audit_logger {
            let entry = zeph_tools::AuditEntry {
                source_kind: None,
                trust_level: None,
                timestamp: zeph_tools::chrono_now(),
                tool: tool_name.into(),
                command: String::new(),
                result: zeph_tools::AuditResult::Success,
                duration_ms: 0,
                error_category: None,
                error_domain: Some("security".to_owned()),
                error_phase: None,
                claim_source: None,
                mcp_server_id,
                injection_flagged: has_injection_flags,
                embedding_anomalous: false,
                cross_boundary_mcp_to_acp: true,
                adversarial_policy_decision: None,
                exit_code: None,
                truncated: false,
                caller_id: None,
                skill_name: None,
                policy_match: None,
                correlation_id: None,
                vigil_risk: None,
                execution_env: None,
                resolved_cwd: None,
                scope_at_definition: None,
                scope_at_dispatch: None,
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
                    // Resolve qs's fail-closed fallback before the `&mut self` calls below —
                    // `qs` borrows `self.services.security.quarantine_summarizer` and cannot
                    // stay live across a `self.update_metrics`/`push_security_event` call.
                    let blocked_body = qs
                        .error_should_block()
                        .then(|| qs.blocked_fallback(sanitized));
                    self.update_metrics(|m| m.quarantine_failures += 1);
                    if let Some(body) = blocked_body {
                        tracing::warn!(
                            tool = %tool_name,
                            error = %e,
                            "cross-boundary quarantine failed, fail_strategy=closed: blocking content"
                        );
                        self.push_security_event(
                            zeph_common::SecurityEventCategory::Quarantine,
                            tool_name,
                            format!("Cross-boundary quarantine failed (fail-closed): {e}"),
                        );
                        return Some((body, has_injection_flags));
                    }
                    tracing::warn!(
                        tool = %tool_name,
                        error = %e,
                        "cross-boundary quarantine failed, fail_strategy=open: using spotlighted output"
                    );
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
                // Resolve qs's fail-closed fallback before the `&mut self` calls below — `qs`
                // borrows `self.services.security.quarantine_summarizer` and cannot stay live
                // across a `self.update_metrics`/`push_security_event` call.
                let blocked_body = qs
                    .error_should_block()
                    .then(|| qs.blocked_fallback(sanitized));
                self.update_metrics(|m| m.quarantine_failures += 1);
                if let Some(body) = blocked_body {
                    tracing::warn!(
                        tool = %tool_name,
                        error = %e,
                        "quarantine failed, fail_strategy=closed: blocking content"
                    );
                    self.push_security_event(
                        zeph_common::SecurityEventCategory::Quarantine,
                        tool_name,
                        format!("Quarantine failed (fail-closed): {e}"),
                    );
                    return Some((body, has_injection_flags));
                }
                tracing::warn!(
                    tool = %tool_name,
                    error = %e,
                    "quarantine failed, fail_strategy=open: using original sanitized output"
                );
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

#[cfg(test)]
mod split_bash_echo_prefix_tests {
    use super::split_bash_echo_prefix;

    #[test]
    fn splits_bash_echo_line() {
        let body = "$ date +%s.%N\n1783259155.445901000\n";
        let (prefix, remainder) = split_bash_echo_prefix(body, "bash");
        assert_eq!(prefix, "$ date +%s.%N\n");
        assert_eq!(remainder, "1783259155.445901000\n");
    }

    #[test]
    fn splits_shell_echo_line() {
        let body = "$ ls -la\ntotal 0\n";
        let (prefix, remainder) = split_bash_echo_prefix(body, "shell");
        assert_eq!(prefix, "$ ls -la\n");
        assert_eq!(remainder, "total 0\n");
    }

    #[test]
    fn non_bash_shell_tool_not_split() {
        let body = "$ date +%s.%N\n1783259155.445901000\n";
        let (prefix, remainder) = split_bash_echo_prefix(body, "web-scrape");
        assert_eq!(prefix, "");
        assert_eq!(remainder, body);
    }

    #[test]
    fn body_without_dollar_prefix_not_split() {
        let body = "no echo line here\nsome output\n";
        let (prefix, remainder) = split_bash_echo_prefix(body, "bash");
        assert_eq!(prefix, "");
        assert_eq!(remainder, body);
    }

    #[test]
    fn body_without_newline_not_split() {
        // No trailing newline after the echo line — nothing to safely split off.
        let body = "$ date +%s.%N";
        let (prefix, remainder) = split_bash_echo_prefix(body, "bash");
        assert_eq!(prefix, "");
        assert_eq!(remainder, body);
    }
}

#[cfg(test)]
mod build_tool_output_source_tests {
    use super::build_tool_output_source;
    use zeph_sanitizer::ContentSourceKind;

    /// INVARIANT-1 (spec 006-1-web-search §4): `web_search` output must be classified
    /// identically to `web_scrape`/`fetch` — `ContentSourceKind::WebScrape`
    /// (`ExternalUntrusted` + quarantine) — via the tool-name string branch, not
    /// `ClaimSource`. Regression guard: dropping `web_search` from this branch would
    /// silently fall through to `ContentSourceKind::ToolResult`, a strictly weaker trust
    /// class for equivalent attacker-controllable content.
    #[test]
    fn web_search_classified_as_web_scrape_source() {
        let source = build_tool_output_source("web_search");
        assert_eq!(source.kind, ContentSourceKind::WebScrape);
    }

    #[test]
    fn web_scrape_and_fetch_classified_as_web_scrape_source() {
        for name in ["web-scrape", "web_scrape", "fetch"] {
            let source = build_tool_output_source(name);
            assert_eq!(
                source.kind,
                ContentSourceKind::WebScrape,
                "{name} must classify as WebScrape"
            );
        }
    }

    #[test]
    fn unrelated_tool_classified_as_tool_result() {
        let source = build_tool_output_source("some_other_tool");
        assert_eq!(source.kind, ContentSourceKind::ToolResult);
    }
}

/// Unit tests for the #6558/#6569 TOCTOU fix's building blocks: `context_max_trust_level`,
/// `batch_dispatch_trust_level`, and `ratchet_memory_consent_trust_for_dispatch`. These test
/// the pure/local logic directly; `tests/consent_gate_toctou_tests.rs` covers the end-to-end
/// behavior through `handle_native_tool_calls` and a real `MemoryToolExecutor`.
#[cfg(test)]
mod consent_gate_dispatch_tests {
    use super::batch_dispatch_trust_level;
    use crate::agent::agent_tests::{
        MockChannel, MockToolExecutor, create_test_registry, mock_provider,
    };
    use zeph_llm::provider::{Message, MessageMetadata, Role, ToolUseRequest};

    fn make_agent() -> crate::agent::Agent<MockChannel> {
        crate::agent::Agent::new(
            mock_provider(vec![]),
            MockChannel::new(vec![]),
            create_test_registry(),
            None,
            5,
            MockToolExecutor::no_tools(),
        )
    }

    fn tagged_message(trust: Option<u8>) -> Message {
        Message {
            role: Role::User,
            content: String::new(),
            parts: vec![],
            metadata: MessageMetadata {
                trust_level: trust,
                ..MessageMetadata::default()
            },
        }
    }

    fn call(name: &str) -> ToolUseRequest {
        ToolUseRequest {
            id: format!("id-{name}"),
            name: name.to_owned().into(),
            input: serde_json::json!({}),
        }
    }

    #[test]
    fn context_max_trust_level_defaults_to_trusted_on_empty_context() {
        let agent = make_agent();
        assert_eq!(
            agent.context_max_trust_level(),
            zeph_sanitizer::ContentTrustLevel::Trusted
        );
    }

    #[test]
    fn context_max_trust_level_ignores_untagged_messages() {
        let mut agent = make_agent();
        agent.msg.messages.push(tagged_message(None));
        assert_eq!(
            agent.context_max_trust_level(),
            zeph_sanitizer::ContentTrustLevel::Trusted
        );
    }

    /// Core #6558 building block: a tagged message from an EARLIER turn (simulated by just
    /// pushing it directly, with no dispatch involved) must still be found by the scan — this
    /// is what lets the gate survive a `begin_turn` reset.
    #[test]
    fn context_max_trust_level_finds_tagged_message_from_prior_turn() {
        let mut agent = make_agent();
        agent.msg.messages.push(tagged_message(Some(
            zeph_sanitizer::ContentTrustLevel::ExternalUntrusted as u8,
        )));
        assert_eq!(
            agent.context_max_trust_level(),
            zeph_sanitizer::ContentTrustLevel::ExternalUntrusted
        );
    }

    #[test]
    fn context_max_trust_level_takes_max_across_messages() {
        let mut agent = make_agent();
        agent.msg.messages.push(tagged_message(Some(
            zeph_sanitizer::ContentTrustLevel::LocalUntrusted as u8,
        )));
        agent.msg.messages.push(tagged_message(Some(
            zeph_sanitizer::ContentTrustLevel::ExternalUntrusted as u8,
        )));
        agent.msg.messages.push(tagged_message(None));
        assert_eq!(
            agent.context_max_trust_level(),
            zeph_sanitizer::ContentTrustLevel::ExternalUntrusted
        );
    }

    #[test]
    fn batch_dispatch_trust_level_reflects_web_scrape() {
        let tool_calls = vec![call("web_scrape")];
        assert_eq!(
            batch_dispatch_trust_level(&tool_calls),
            zeph_sanitizer::ContentTrustLevel::ExternalUntrusted
        );
    }

    /// Core #6569 building block: `memory_search` (`ContentSourceKind::MemoryRetrieval`) is
    /// itself an untrusted source — a `memory_save` racing a `memory_search` in the same batch
    /// must be covered too, not just the `web_scrape` case explicitly named in the issue.
    #[test]
    fn batch_dispatch_trust_level_reflects_memory_search() {
        let tool_calls = vec![call("memory_search")];
        assert_eq!(
            batch_dispatch_trust_level(&tool_calls),
            zeph_sanitizer::ContentTrustLevel::ExternalUntrusted
        );
    }

    /// Regression guard: `memory_save`'s own (not-yet-produced) result must never contribute to
    /// the batch trust it is itself gated against — otherwise a lone `memory_save` call would
    /// spuriously self-gate whenever `confirm_threshold` is `local_untrusted` or lower.
    #[test]
    fn batch_dispatch_trust_level_excludes_memory_save_itself() {
        let tool_calls = vec![call("memory_save")];
        assert_eq!(
            batch_dispatch_trust_level(&tool_calls),
            zeph_sanitizer::ContentTrustLevel::Trusted
        );
    }

    #[test]
    fn batch_dispatch_trust_level_other_tool_alongside_memory_save_still_counts() {
        let tool_calls = vec![call("web_scrape"), call("memory_save")];
        assert_eq!(
            batch_dispatch_trust_level(&tool_calls),
            zeph_sanitizer::ContentTrustLevel::ExternalUntrusted
        );
    }

    /// End-to-end for the pure helper: combines a stale-but-still-tagged context message with
    /// this batch's own tool-name trust, and asserts the slot is ratcheted to the max of both
    /// BEFORE any tool in the batch would execute (this method is called at the very top of
    /// `handle_native_tool_calls`, ahead of tier dispatch).
    #[test]
    fn ratchet_combines_context_and_batch_trust() {
        let mut agent = make_agent();
        agent.msg.messages.push(tagged_message(Some(
            zeph_sanitizer::ContentTrustLevel::LocalUntrusted as u8,
        )));
        let tool_calls = vec![call("web_scrape")];
        agent.ratchet_memory_consent_trust_for_dispatch(&tool_calls);
        assert_eq!(
            *agent.services.security.memory_consent_trust.read(),
            zeph_sanitizer::ContentTrustLevel::ExternalUntrusted as u8
        );
    }

    /// A bare `memory_save` call in an otherwise-clean context must not ratchet the slot at
    /// all — preserves pre-#6558 behavior for the common case (no over-triggering).
    #[test]
    fn ratchet_leaves_slot_trusted_for_bare_memory_save_in_clean_context() {
        let mut agent = make_agent();
        let tool_calls = vec![call("memory_save")];
        agent.ratchet_memory_consent_trust_for_dispatch(&tool_calls);
        assert_eq!(*agent.services.security.memory_consent_trust.read(), 0);
    }
}

#[cfg(all(test, feature = "classifiers"))]
mod tests {
    use super::is_internal_tool;

    #[test]
    fn internal_tool_allowlist_covers_all_zeph_tools() {
        for name in [
            "bash",
            "shell",
            "invoke_skill",
            "load_skill",
            "memory_save",
            "memory_search",
            "compress_context",
            "request_compaction",
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
