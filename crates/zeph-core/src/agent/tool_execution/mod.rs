// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

mod legacy;
mod native;
mod tool_call_dag;

use zeph_llm::provider::{LlmProvider, Message, MessageMetadata, Role, ToolDefinition};

use super::Agent;
use crate::channel::Channel;
use crate::redact::redact_secrets;
use zeph_sanitizer::{ContentSource, ContentSourceKind, MemorySourceHint};
use zeph_skills::loader::Skill;

/// Prefix used in the overflow notice appended to tool outputs that exceed the size threshold.
/// Shared with the pruning logic so both sides stay in sync if the format changes.
///
/// Current format: `[full output stored — ID: {uuid} — {bytes} bytes, use read_overflow tool to retrieve]`
pub(crate) const OVERFLOW_NOTICE_PREFIX: &str = "[full output stored — ID: ";

enum AnomalyOutcome {
    Success,
    Error,
    Blocked,
    /// Quality failure (`ToolNotFound`, `InvalidParameters`, `TypeMismatch`) from a
    /// reasoning-enhanced model. Triggers `record_reasoning_quality_failure` which both
    /// counts the error in the sliding window and emits a `reasoning_amplification` warning.
    ReasoningQualityFailure {
        model: String,
        tool: String,
    },
}

/// Result of a response cache lookup.
///
/// On `Hit`, the caller should return the cached response.
/// On `Miss`, the `query_embedding` field contains the pre-computed embedding (if semantic
/// caching is enabled and embedding succeeded) — pass it to `store_response_in_cache` to
/// avoid recomputing the embedding on the store path.
pub(super) enum CacheCheckResult {
    Hit(String),
    Miss { query_embedding: Option<Vec<f32>> },
}

/// Hash message content for doom-loop detection, skipping volatile IDs in-place.
/// Normalizes `[tool_result: <id>]` → `[tool_result]` and `[tool_use: <name>(<id>)]` → `[tool_use: <name>]`
/// by feeding only stable segments into the hasher without materializing the normalized string.
// DefaultHasher output is not stable across Rust versions — do not persist or serialize
// these hashes. They are used only for within-session equality comparison.
fn doom_loop_hash(content: &str) -> u64 {
    use std::hash::{DefaultHasher, Hasher};
    let mut hasher = DefaultHasher::new();
    let mut rest = content;
    while !rest.is_empty() {
        let r_pos = rest.find("[tool_result: ");
        let u_pos = rest.find("[tool_use: ");
        match (r_pos, u_pos) {
            (Some(r), Some(u)) if u < r => hash_tool_use_in_place(&mut hasher, &mut rest, u),
            (Some(r), _) => hash_tool_result_in_place(&mut hasher, &mut rest, r),
            (_, Some(u)) => hash_tool_use_in_place(&mut hasher, &mut rest, u),
            _ => {
                hasher.write(rest.as_bytes());
                break;
            }
        }
    }
    hasher.finish()
}

/// Extracts the language identifier from the first fenced code block in `response`
/// (e.g. "bash" from ` ```bash `). Returns "tool" as fallback.
fn first_tool_name(response: &str) -> &str {
    if let Some(pos) = response.find("```") {
        let after = &response[pos + 3..];
        let line = after.split_once('\n').map_or(after, |(l, _)| l).trim();
        let lang = line.split_whitespace().next().unwrap_or("");
        if !lang.is_empty() {
            return lang;
        }
    }
    "tool"
}

fn hash_tool_result_in_place(hasher: &mut impl std::hash::Hasher, rest: &mut &str, start: usize) {
    hasher.write(&rest.as_bytes()[..start]);
    if let Some(end) = rest[start..].find(']') {
        hasher.write(b"[tool_result]");
        *rest = &rest[start + end + 1..];
    } else {
        hasher.write(&rest.as_bytes()[start..]);
        *rest = "";
    }
}

fn hash_tool_use_in_place(hasher: &mut impl std::hash::Hasher, rest: &mut &str, start: usize) {
    hasher.write(&rest.as_bytes()[..start]);
    let tag = &rest[start..];
    if let (Some(paren), Some(end)) = (tag.find('('), tag.find(']')) {
        hasher.write(&tag.as_bytes()[..paren]);
        hasher.write(b"]");
        *rest = &rest[start + end + 1..];
    } else {
        hasher.write(tag.as_bytes());
        *rest = "";
    }
}

#[cfg(test)]
fn normalize_for_doom_loop(content: &str) -> String {
    let mut out = String::with_capacity(content.len());
    let mut rest = content;
    while !rest.is_empty() {
        let r_pos = rest.find("[tool_result: ");
        let u_pos = rest.find("[tool_use: ");
        match (r_pos, u_pos) {
            (Some(r), Some(u)) if u < r => {
                handle_tool_use(&mut out, &mut rest, u);
            }
            (Some(r), _) => {
                handle_tool_result(&mut out, &mut rest, r);
            }
            (_, Some(u)) => {
                handle_tool_use(&mut out, &mut rest, u);
            }
            _ => {
                out.push_str(rest);
                break;
            }
        }
    }
    out
}

#[cfg(test)]
fn handle_tool_result(out: &mut String, rest: &mut &str, start: usize) {
    out.push_str(&rest[..start]);
    if let Some(end) = rest[start..].find(']') {
        out.push_str("[tool_result]");
        *rest = &rest[start + end + 1..];
    } else {
        out.push_str(&rest[start..]);
        *rest = "";
    }
}

#[cfg(test)]
fn handle_tool_use(out: &mut String, rest: &mut &str, start: usize) {
    out.push_str(&rest[..start]);
    let tag = &rest[start..];
    if let (Some(paren), Some(end)) = (tag.find('('), tag.find(']')) {
        out.push_str(&tag[..paren]);
        out.push(']');
        *rest = &rest[start + end + 1..];
    } else {
        out.push_str(tag);
        *rest = "";
    }
}

impl<C: Channel> Agent<C> {
    pub(super) fn last_user_query(&self) -> &str {
        self.msg
            .messages
            .iter()
            .rev()
            .find(|m| m.role == Role::User && !m.content.starts_with("[tool output"))
            .map_or("", |m| m.content.as_str())
    }

    pub(super) async fn summarize_tool_output(&self, output: &str, threshold: usize) -> String {
        let truncated = zeph_tools::truncate_tool_output_at(output, threshold);
        let query = self.last_user_query();
        let prompt = format!(
            "The user asked: {query}\n\n\
             A tool produced output ({len} chars, truncated to fit).\n\
             Summarize the key information relevant to the user's question.\n\
             Preserve exact: file paths, error messages, numeric values, exit codes.\n\n\
             {truncated}",
            len = output.len(),
        );

        let messages = vec![Message {
            role: Role::User,
            content: prompt,
            parts: vec![],
            metadata: MessageMetadata::default(),
        }];

        let llm_timeout = std::time::Duration::from_secs(self.runtime.timeouts.llm_seconds);
        let result = tokio::time::timeout(
            llm_timeout,
            self.summary_or_primary_provider().chat(&messages),
        )
        .await;
        match result {
            Ok(Ok(summary)) => format!("[tool output summary]\n```\n{summary}\n```"),
            Ok(Err(e)) => {
                tracing::warn!(
                    "tool output summarization failed, falling back to truncation: {e:#}"
                );
                truncated
            }
            Err(_elapsed) => {
                tracing::warn!(
                    timeout_secs = self.runtime.timeouts.llm_seconds,
                    "tool output summarization timed out, falling back to truncation"
                );
                truncated
            }
        }
    }

    pub(super) async fn maybe_summarize_tool_output(&self, output: &str) -> String {
        let threshold = self.tool_orchestrator.overflow_config.threshold;
        if output.len() <= threshold {
            return output.to_string();
        }

        let max_bytes = self.tool_orchestrator.overflow_config.max_overflow_bytes;
        let overflow_notice = if max_bytes > 0 && output.len() > max_bytes {
            format!(
                "\n[warning: full output ({} bytes) exceeds max_overflow_bytes ({max_bytes}) — \
                 not saved]",
                output.len()
            )
        } else if let (Some(memory), Some(conv_id)) =
            (&self.memory_state.memory, self.memory_state.conversation_id)
        {
            match memory
                .sqlite()
                .save_overflow(conv_id.0, output.as_bytes())
                .await
            {
                Ok(uuid) => format!(
                    "\n[full output stored — ID: {uuid} — {} bytes, use read_overflow \
                         tool to retrieve]",
                    output.len()
                ),
                Err(e) => {
                    tracing::warn!("failed to save overflow to SQLite: {e}");
                    format!(
                        "\n[warning: full output ({} bytes) could not be saved to overflow store]",
                        output.len()
                    )
                }
            }
        } else {
            format!(
                "\n[warning: full output ({} bytes) could not be saved — no memory backend or \
                 conversation available]",
                output.len()
            )
        };

        let truncated = if self.tool_orchestrator.summarize_tool_output_enabled {
            self.summarize_tool_output(output, threshold).await
        } else {
            zeph_tools::truncate_tool_output_at(output, threshold)
        };
        format!("{truncated}{overflow_notice}")
    }

    async fn record_anomaly_outcome(
        &mut self,
        outcome: AnomalyOutcome,
    ) -> Result<(), super::error::AgentError> {
        let Some(ref mut det) = self.debug_state.anomaly_detector else {
            return Ok(());
        };
        match outcome {
            AnomalyOutcome::Success => det.record_success(),
            AnomalyOutcome::Error => det.record_error(),
            AnomalyOutcome::Blocked => det.record_blocked(),
            AnomalyOutcome::ReasoningQualityFailure { model, tool } => {
                if self.debug_state.reasoning_model_warning {
                    det.record_reasoning_quality_failure(&model, &tool);
                } else {
                    det.record_error();
                }
            }
        }
        if let Some(anomaly) = det.check() {
            tracing::warn!(severity = ?anomaly.severity, "{}", anomaly.description);
            self.channel
                .send(&format!("[anomaly] {}", anomaly.description))
                .await?;
        }
        Ok(())
    }

    /// Sanitize tool output body before inserting it into the LLM message history.
    ///
    /// Channel display (`send_tool_output`) still receives the raw body so the user
    /// sees unmodified output; spotlighting delimiters are added only for the LLM.
    ///
    /// This is the SOLE sanitization point for tool output data flows. Do not add
    /// redundant sanitization in leaf crates (zeph-tools, zeph-mcp).
    #[allow(clippy::too_many_lines)]
    async fn sanitize_tool_output(&mut self, body: &str, tool_name: &str) -> (String, bool) {
        // MCP tools use "server:tool" format (contains ':') or legacy "mcp" name.
        // Web scrape tools use "web-scrape" (hyphenated) or "fetch".
        // Everything else is local shell/file output.
        let source = if tool_name.contains(':') || tool_name == "mcp" || tool_name == "search_code"
        {
            ContentSource::new(ContentSourceKind::McpResponse).with_identifier(tool_name)
        } else if tool_name == "web-scrape" || tool_name == "web_scrape" || tool_name == "fetch" {
            ContentSource::new(ContentSourceKind::WebScrape).with_identifier(tool_name)
        } else if tool_name == "memory_search" {
            // Issue #2057: memory_search output is conversation history from SQLite/Qdrant.
            // Without this classification, benign recalled content (e.g. user discussing
            // "system prompt") triggers injection false positives → Qdrant embedding skipped
            // for the entire turn. ConversationHistory hint matches assembly.rs:698 usage.
            ContentSource::new(ContentSourceKind::MemoryRetrieval)
                .with_identifier(tool_name)
                .with_memory_hint(MemorySourceHint::ConversationHistory)
        } else {
            ContentSource::new(ContentSourceKind::ToolResult).with_identifier(tool_name)
        };
        let kind = source.kind;
        #[cfg(feature = "classifiers")]
        let memory_hint = source.memory_hint;
        #[cfg(not(feature = "classifiers"))]
        let _ = source.memory_hint;
        let sanitized = self.security.sanitizer.sanitize(body, source);
        let has_injection_flags = !sanitized.injection_flags.is_empty();
        if has_injection_flags {
            tracing::warn!(
                tool = %tool_name,
                flags = sanitized.injection_flags.len(),
                "injection patterns detected in tool output"
            );
            self.update_metrics(|m| {
                m.sanitizer_injection_flags += sanitized.injection_flags.len() as u64;
            });
            let detail = sanitized
                .injection_flags
                .first()
                .map_or_else(String::new, |f| {
                    format!("Detected pattern: {}", f.pattern_name)
                });
            self.push_security_event(
                crate::metrics::SecurityEventCategory::InjectionFlag,
                tool_name,
                detail,
            );
            // Collect URLs from the SANITIZED content (not raw body) for validate_tool_call.
            // Using sanitized.body ensures only URLs the LLM actually sees are tracked,
            // avoiding false-positive SuspiciousToolUrl warnings for truncated/stripped content.
            let urls = zeph_sanitizer::exfiltration::extract_flagged_urls(&sanitized.body);
            self.security.flagged_urls.extend(urls);
        }
        if sanitized.was_truncated {
            self.update_metrics(|m| m.sanitizer_truncations += 1);
            self.push_security_event(
                crate::metrics::SecurityEventCategory::Truncation,
                tool_name,
                "Content truncated to max_content_size",
            );
        }
        self.update_metrics(|m| m.sanitizer_runs += 1);

        // ML injection classifier: runs on tool output after regex sanitization.
        // Skip for memory_search (ConversationHistory hint) — same rationale as regex skip:
        // the user's own prior messages legitimately contain terms like "system prompt".
        #[cfg(feature = "classifiers")]
        {
            let skip_ml = matches!(
                memory_hint,
                Some(
                    zeph_sanitizer::MemorySourceHint::ConversationHistory
                        | zeph_sanitizer::MemorySourceHint::LlmSummary
                )
            );
            if !skip_ml && self.security.sanitizer.has_classifier_backend() {
                // Classify the original body, not sanitized.body: the spotlight wrapper
                // (<external-data ...>) would trigger the delimiter_escape pattern in
                // the regex fallback path, causing false positives for all tool outputs.
                let ml_verdict = self.security.sanitizer.classify_injection(body).await;
                match ml_verdict {
                    zeph_sanitizer::InjectionVerdict::Blocked => {
                        tracing::warn!(tool = %tool_name, "ML classifier blocked tool output");
                        self.update_metrics(|m| m.classifier_tool_blocks += 1);
                        self.push_security_event(
                            crate::metrics::SecurityEventCategory::InjectionBlocked,
                            tool_name,
                            "ML classifier blocked tool output",
                        );
                        return (
                            "[tool output blocked: injection detected by classifier]".into(),
                            true,
                        );
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
        }

        // Quarantine step: route high-risk sources through an isolated LLM (defense-in-depth).
        if self.security.sanitizer.is_enabled()
            && let Some(ref qs) = self.security.quarantine_summarizer
            && qs.should_quarantine(kind)
        {
            match qs.extract_facts(&sanitized, &self.security.sanitizer).await {
                Ok((facts, flags)) => {
                    self.update_metrics(|m| m.quarantine_invocations += 1);
                    self.push_security_event(
                        crate::metrics::SecurityEventCategory::Quarantine,
                        tool_name,
                        "Content quarantined, facts extracted",
                    );
                    let escaped = zeph_sanitizer::ContentSanitizer::escape_delimiter_tags(&facts);
                    return (
                        zeph_sanitizer::ContentSanitizer::apply_spotlight(
                            &escaped,
                            &sanitized.source,
                            &flags,
                        ),
                        has_injection_flags,
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        tool = %tool_name,
                        error = %e,
                        "quarantine failed, using original sanitized output"
                    );
                    self.update_metrics(|m| m.quarantine_failures += 1);
                    self.push_security_event(
                        crate::metrics::SecurityEventCategory::Quarantine,
                        tool_name,
                        format!("Quarantine failed: {e}"),
                    );
                }
            }
        }

        // PII scrub: apply after sanitization so the filter processes the same content
        // that will enter the LLM context (post-truncation, post-spotlight delimiters).
        let body = self.scrub_pii_union(&sanitized.body, tool_name).await;

        // Guardrail: opt-in tool output scanning for indirect prompt injection (scan_tool_output=true).
        #[cfg(feature = "guardrail")]
        let body = self.apply_guardrail_to_tool_output(body, tool_name).await;

        (body, has_injection_flags)
    }

    /// Run regex PII filter and (optionally) NER classifier, merge spans, and redact in one pass.
    ///
    /// When `pii_ner_backend` is configured, both sources are combined so neither regex-only
    /// nor NER-only detections are missed. Falls back to regex-only when NER is unavailable.
    #[cfg_attr(not(feature = "classifiers"), allow(clippy::unused_async))]
    async fn scrub_pii_union(&mut self, text: &str, tool_name: &str) -> String {
        use zeph_sanitizer::pii::{merge_spans, redact_spans};

        if !self.security.pii_filter.is_enabled() {
            // NER alone does not activate PII scrubbing — the regex filter must be enabled.
            return text.to_owned();
        }

        // Step 1: regex spans (byte offsets).
        #[cfg_attr(not(feature = "classifiers"), allow(unused_mut))]
        let mut spans = self.security.pii_filter.detect_spans(text);

        // Step 2: NER spans (char offsets → convert to byte offsets, then append).
        #[cfg(feature = "classifiers")]
        if let Some(ref backend) = self.security.pii_ner_backend {
            use zeph_sanitizer::pii::build_char_to_byte_map;
            let timeout_ms = self.security.pii_ner_timeout_ms;
            match tokio::time::timeout(
                std::time::Duration::from_millis(timeout_ms),
                backend.classify(text),
            )
            .await
            {
                Ok(Ok(result)) if result.is_positive => {
                    // Precompute char→byte map once for all NER spans (C2).
                    let char_to_byte = build_char_to_byte_map(text);
                    for ner_span in &result.spans {
                        let byte_start = char_to_byte
                            .get(ner_span.start)
                            .copied()
                            .unwrap_or(text.len());
                        let byte_end = char_to_byte
                            .get(ner_span.end)
                            .copied()
                            .unwrap_or(text.len());
                        if byte_end > byte_start {
                            spans.push(zeph_sanitizer::pii::PiiSpan {
                                label: ner_span.label.clone(),
                                start: byte_start,
                                end: byte_end,
                            });
                        }
                    }
                }
                Ok(Ok(_)) => {} // no positive detection
                Ok(Err(e)) => {
                    tracing::warn!(error = %e, tool = %tool_name, "PII NER failed, regex only");
                }
                Err(_) => {
                    tracing::warn!(
                        timeout_ms = timeout_ms,
                        tool = %tool_name,
                        "PII NER timed out, regex only"
                    );
                }
            }
        }

        // Step 3: merge overlapping/adjacent spans.
        let merged = merge_spans(spans);
        if merged.is_empty() {
            return text.to_owned();
        }

        // Step 4: single-pass redaction.
        self.update_metrics(|m| m.pii_scrub_count += 1);
        self.push_classifier_metrics();
        tracing::debug!(tool = %tool_name, span_count = merged.len(), "PII scrubbed from tool output");
        redact_spans(text, &merged)
    }

    #[cfg(feature = "guardrail")]
    async fn apply_guardrail_to_tool_output(&self, mut body: String, tool_name: &str) -> String {
        use zeph_sanitizer::guardrail::GuardrailVerdict;
        let Some(ref guardrail) = self.security.guardrail else {
            return body;
        };
        if !guardrail.scan_tool_output() {
            return body;
        }
        let verdict = guardrail.check(&body).await;
        if let GuardrailVerdict::Flagged { reason, .. } = &verdict {
            tracing::warn!(
                tool = %tool_name,
                reason = %reason,
                should_block = verdict.should_block(),
                "guardrail flagged tool output"
            );
            if verdict.should_block() {
                body = format!("[guardrail blocked] Tool output flagged: {reason}");
            }
            // Warn mode: log only, no user-channel notification. Tool output warn is intentionally
            // silent to avoid flooding the user with warnings for every suspicious tool result —
            // unlike user-input warn mode which notifies the user because it is interactive.
        } else if let GuardrailVerdict::Error { error } = &verdict {
            if guardrail.error_should_block() {
                tracing::warn!(
                    tool = %tool_name,
                    %error,
                    "guardrail check failed (fail_strategy=closed), blocking tool output"
                );
                "[guardrail blocked] Tool output check failed (see logs)".clone_into(&mut body);
            } else {
                tracing::warn!(
                    tool = %tool_name,
                    %error,
                    "guardrail check failed (fail_strategy=open), allowing tool output"
                );
            }
        }
        body
    }

    fn scan_output_and_warn(&mut self, text: &str) -> String {
        let (cleaned, events) = self.security.exfiltration_guard.scan_output(text);
        if !events.is_empty() {
            tracing::warn!(
                count = events.len(),
                "exfiltration guard: markdown images blocked"
            );
            self.update_metrics(|m| {
                m.exfiltration_images_blocked += events.len() as u64;
            });
            self.push_security_event(
                crate::metrics::SecurityEventCategory::ExfiltrationBlock,
                "llm_output",
                format!("{} markdown image(s) blocked", events.len()),
            );
        }
        cleaned
    }

    /// Scan the LLM response for injection patterns (response verification layer).
    ///
    /// Returns `true` when the response was blocked and the caller should return early.
    pub(super) fn run_response_verification(&mut self, response_text: &str) -> bool {
        use zeph_sanitizer::response_verifier::{ResponseVerificationResult, VerificationContext};

        if !self.security.response_verifier.is_enabled() {
            return false;
        }

        let ctx = VerificationContext { response_text };
        let result = self.security.response_verifier.verify(&ctx);

        match result {
            ResponseVerificationResult::Clean => false,
            ResponseVerificationResult::Flagged { matched } => {
                let detail = matched.join(", ");
                tracing::warn!(patterns = %detail, "response verification: injection patterns in LLM output");
                self.push_security_event(
                    crate::metrics::SecurityEventCategory::ResponseVerification,
                    "llm_response",
                    format!("flagged: {detail}"),
                );
                false
            }
            ResponseVerificationResult::Blocked { matched } => {
                let detail = matched.join(", ");
                tracing::error!(patterns = %detail, "response verification: blocking LLM response");
                self.push_security_event(
                    crate::metrics::SecurityEventCategory::ResponseVerification,
                    "llm_response",
                    format!("blocked: {detail}"),
                );
                true
            }
        }
    }

    pub(super) fn maybe_redact<'a>(&self, text: &'a str) -> std::borrow::Cow<'a, str> {
        if self.runtime.security.redact_secrets {
            let redacted = redact_secrets(text);
            let sanitized = crate::redact::sanitize_paths(&redacted);
            match sanitized {
                std::borrow::Cow::Owned(s) => std::borrow::Cow::Owned(s),
                std::borrow::Cow::Borrowed(_) => redacted,
            }
        } else {
            std::borrow::Cow::Borrowed(text)
        }
    }

    /// Walk a JSON value and apply `maybe_redact` to every string leaf.
    ///
    /// Used to sanitize `raw_response` before it is forwarded to `claudeCode.toolResponse`
    /// in the ACP notification. Without this, file content and shell stdout would bypass
    /// the `redact_secrets` pipeline even when it is enabled.
    pub(super) fn redact_json(&self, value: serde_json::Value) -> serde_json::Value {
        match value {
            serde_json::Value::String(s) => {
                serde_json::Value::String(self.maybe_redact(&s).into_owned())
            }
            serde_json::Value::Array(arr) => {
                serde_json::Value::Array(arr.into_iter().map(|v| self.redact_json(v)).collect())
            }
            serde_json::Value::Object(map) => serde_json::Value::Object(
                map.into_iter()
                    .map(|(k, v)| (k, self.redact_json(v)))
                    .collect(),
            ),
            other => other,
        }
    }

    fn last_user_content(&self) -> Option<&str> {
        self.msg
            .messages
            .iter()
            .rev()
            .find(|m| m.role == zeph_llm::provider::Role::User)
            .map(|m| m.content.as_str())
    }

    async fn check_response_cache(&mut self) -> Result<CacheCheckResult, super::error::AgentError> {
        let Some(ref cache) = self.session.response_cache else {
            return Ok(CacheCheckResult::Miss {
                query_embedding: None,
            });
        };
        let Some(content) = self.last_user_content() else {
            return Ok(CacheCheckResult::Miss {
                query_embedding: None,
            });
        };
        // Clone content to avoid borrow conflict when calling self methods below.
        let content = content.to_owned();
        let key = zeph_memory::ResponseCache::compute_key(&content, &self.runtime.model_name);

        // Fast path: exact-match lookup (sub-ms).
        if let Ok(Some(cached)) = cache.get(&key).await {
            tracing::debug!("response cache hit (exact match)");
            let cleaned = self.scan_output_and_warn(&cached);
            if !cleaned.is_empty() {
                let display = self.maybe_redact(&cleaned);
                self.channel.send(&display).await?;
            }
            return Ok(CacheCheckResult::Hit(cleaned));
        }

        // Semantic fallback: embed once, search by similarity.
        if self.runtime.semantic_cache_enabled && self.provider.supports_embeddings() {
            use zeph_llm::provider::LlmProvider as _;
            let threshold = self.runtime.semantic_cache_threshold;
            let max_candidates = self.runtime.semantic_cache_max_candidates;
            tracing::debug!(
                max_candidates,
                threshold,
                "semantic cache lookup: examining up to {max_candidates} candidates",
            );
            match self.embedding_provider.embed(&content).await {
                Ok(embedding) => {
                    let embed_model = self.skill_state.embedding_model.clone();
                    match cache
                        .get_semantic(&embedding, &embed_model, threshold, max_candidates)
                        .await
                    {
                        Ok(Some((response, score))) => {
                            tracing::debug!(score, max_candidates, "response cache hit (semantic)",);
                            let cleaned = self.scan_output_and_warn(&response);
                            if !cleaned.is_empty() {
                                let display = self.maybe_redact(&cleaned);
                                self.channel.send(&display).await?;
                            }
                            return Ok(CacheCheckResult::Hit(cleaned));
                        }
                        Ok(None) => {
                            tracing::debug!(
                                max_candidates,
                                threshold,
                                "semantic cache miss: no candidate met threshold",
                            );
                            // Semantic miss — pass embedding through to store path.
                            return Ok(CacheCheckResult::Miss {
                                query_embedding: Some(embedding),
                            });
                        }
                        Err(e) => {
                            tracing::warn!("semantic cache lookup failed: {e:#}");
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!("embedding generation failed, skipping semantic cache: {e:#}");
                }
            }
        }

        Ok(CacheCheckResult::Miss {
            query_embedding: None,
        })
    }

    async fn store_response_in_cache(&self, response: &str, query_embedding: Option<Vec<f32>>) {
        let Some(ref cache) = self.session.response_cache else {
            return;
        };
        let Some(content) = self.last_user_content() else {
            return;
        };
        let key = zeph_memory::ResponseCache::compute_key(content, &self.runtime.model_name);

        // If we have a pre-computed embedding (semantic cache enabled + embed succeeded) and the
        // response is not tool-call output, use put_with_embedding — it uses INSERT OR REPLACE so
        // it handles the exact-match write too, avoiding a redundant SQL round-trip.
        // Otherwise fall back to exact-match-only put().
        if let Some(embedding) = query_embedding
            && !response.contains("[tool_use:")
        {
            let embed_model = &self.skill_state.embedding_model;
            if let Err(e) = cache
                .put_with_embedding(
                    &key,
                    response,
                    &self.runtime.model_name,
                    &embedding,
                    embed_model,
                )
                .await
            {
                tracing::warn!("failed to store semantic cache entry: {e:#}");
                // Fallback: at least persist exact-match entry.
                if let Err(e2) = cache.put(&key, response, &self.runtime.model_name).await {
                    tracing::warn!("failed to store response in cache: {e2:#}");
                }
            }
        } else if let Err(e) = cache.put(&key, response, &self.runtime.model_name).await {
            tracing::warn!("failed to store response in cache: {e:#}");
        }
    }

    fn inject_active_skill_env(&self) {
        if self.skill_state.active_skill_names.is_empty()
            || self.skill_state.available_custom_secrets.is_empty()
        {
            return;
        }
        let active_skills: Vec<Skill> = {
            let reg = self
                .skill_state
                .registry
                .read()
                .expect("registry read lock");
            self.skill_state
                .active_skill_names
                .iter()
                .filter_map(|name| reg.get_skill(name).ok())
                .collect()
        };
        let env: std::collections::HashMap<String, String> = active_skills
            .into_iter()
            .flat_map(|skill| {
                skill
                    .meta
                    .requires_secrets
                    .into_iter()
                    .filter_map(|secret_name| {
                        self.skill_state
                            .available_custom_secrets
                            .get(&secret_name)
                            .map(|secret| {
                                let env_key = secret_name.to_uppercase();
                                // Secret is intentionally exposed here for subprocess
                                // env injection, not for logging.
                                let value = secret.expose().to_owned(); // lgtm[rust/cleartext-logging]
                                (env_key, value)
                            })
                    })
            })
            .collect();
        if !env.is_empty() {
            self.tool_executor.set_skill_env(Some(env));
        }
    }

    /// Build a compact context summary for causal IPI probes.
    ///
    /// Takes the last user message and last assistant message, each truncated to 500 chars.
    /// Never exposes the full message history to the probe provider.
    pub(super) fn build_causal_context_summary(&self) -> String {
        const MAX_CHARS: usize = 500;

        let mut last_user: Option<&str> = None;
        let mut last_assistant: Option<&str> = None;

        for msg in self.msg.messages.iter().rev() {
            match msg.role {
                Role::User if last_user.is_none() => {
                    if let Some(zeph_llm::provider::MessagePart::Text { text }) = msg.parts.first()
                    {
                        last_user = Some(text.as_str());
                    }
                }
                Role::Assistant if last_assistant.is_none() => {
                    if let Some(zeph_llm::provider::MessagePart::Text { text }) = msg.parts.first()
                    {
                        last_assistant = Some(text.as_str());
                    }
                }
                _ => {}
            }
            if last_user.is_some() && last_assistant.is_some() {
                break;
            }
        }

        let truncate = |s: &str| {
            if s.len() <= MAX_CHARS {
                s.to_owned()
            } else {
                s[..s.floor_char_boundary(MAX_CHARS)].to_owned()
            }
        };

        let user_part = last_user.map_or_else(String::new, truncate);
        let assistant_part = last_assistant.map_or_else(String::new, truncate);
        format!("User: {user_part}\nAssistant: {assistant_part}")
    }
}

/// Process-wide randomized `BuildHasher` for `tool_args_hash`.
///
/// Initialized once at first use. Consistent within a session (repeat-detection
/// works correctly) but unpredictable across sessions (prevents adversarial
/// hash collision bypasses).
static TOOL_ARGS_HASHER: std::sync::OnceLock<std::collections::hash_map::RandomState> =
    std::sync::OnceLock::new();

/// Compute a stable hash for tool arguments for repeat-detection.
///
/// Keys are sorted before hashing to normalize key ordering differences between
/// LLM tool calls that have the same logical parameters. Uses a process-scoped
/// randomized `RandomState` (seeded once at startup) to prevent adversarial hash
/// collision bypasses of the repeat-detection window.
fn tool_args_hash(params: &serde_json::Map<String, serde_json::Value>) -> u64 {
    use std::hash::{BuildHasher, Hash, Hasher};
    let state = TOOL_ARGS_HASHER.get_or_init(std::collections::hash_map::RandomState::new);
    let mut hasher = state.build_hasher();
    let mut keys: Vec<&String> = params.keys().collect();
    keys.sort_unstable();
    for k in keys {
        k.hash(&mut hasher);
        params[k].to_string().hash(&mut hasher);
    }
    hasher.finish()
}

/// Compute exponential backoff delay for retry attempt (0-indexed).
///
/// Formula: `base_ms * 2^attempt`, capped at `max_ms`.
/// Full jitter in `[0, cap]` is applied using `rand` for cryptographically
/// seeded randomness — avoids predictable timing that an adversary could exploit
/// to align retry windows.
fn retry_backoff_ms(attempt: usize, base_ms: u64, max_ms: u64) -> u64 {
    use rand::RngExt as _;
    let base = base_ms.saturating_mul(1_u64 << attempt.min(10));
    let capped = base.min(max_ms);
    rand::rng().random_range(0..=capped)
}

pub(crate) fn tool_def_to_definition(def: &zeph_tools::registry::ToolDef) -> ToolDefinition {
    let mut params = serde_json::to_value(&def.schema).unwrap_or_default();
    if let serde_json::Value::Object(ref mut map) = params {
        map.remove("$schema");
        map.remove("title");
    }
    ToolDefinition {
        name: def.id.to_string(),
        description: def.description.to_string(),
        parameters: params,
    }
}

/// Compute structural complexity of a JSON Schema in [0.0, 1.0].
///
/// The score is based on:
/// - Nesting depth (deeply nested schemas require more reasoning)
/// - Presence of combinators (`anyOf`, `oneOf`, `allOf`)
/// - Number of enum variants (more choices = more reasoning)
/// - Number of top-level properties (many flat params require parameter selection reasoning)
///
/// Scores above `tau` (default 0.6) are considered complex enough to benefit
/// from TAFC augmentation.
pub(crate) fn schema_complexity(schema: &serde_json::Value) -> f64 {
    #[allow(clippy::cast_precision_loss)]
    let depth = schema_depth(schema).min(8) as f64 / 8.0;
    let combinator_score = f64::from(u8::from(has_combinators(schema))) * 0.4;
    #[allow(clippy::cast_precision_loss)]
    let enum_score = (enum_variant_count(schema).min(10) as f64 / 10.0) * 0.3;
    #[allow(clippy::cast_precision_loss)]
    let flat_params_score = (top_level_property_count(schema).min(8) as f64 / 8.0) * 0.2;
    (depth * 0.3 + combinator_score + enum_score + flat_params_score).min(1.0)
}

fn schema_depth(v: &serde_json::Value) -> usize {
    match v {
        serde_json::Value::Object(map) => {
            let child_depth = map.values().map(schema_depth).max().unwrap_or(0);
            1 + child_depth
        }
        serde_json::Value::Array(arr) => {
            let child_depth = arr.iter().map(schema_depth).max().unwrap_or(0);
            1 + child_depth
        }
        _ => 1,
    }
}

fn has_combinators(v: &serde_json::Value) -> bool {
    match v {
        serde_json::Value::Object(map) => {
            if map.contains_key("anyOf") || map.contains_key("oneOf") || map.contains_key("allOf") {
                return true;
            }
            map.values().any(has_combinators)
        }
        serde_json::Value::Array(arr) => arr.iter().any(has_combinators),
        _ => false,
    }
}

fn enum_variant_count(v: &serde_json::Value) -> usize {
    match v {
        serde_json::Value::Object(map) => {
            let local = map
                .get("enum")
                .and_then(|e| e.as_array())
                .map_or(0, Vec::len);
            let child = map.values().map(enum_variant_count).max().unwrap_or(0);
            local.max(child)
        }
        serde_json::Value::Array(arr) => arr.iter().map(enum_variant_count).max().unwrap_or(0),
        _ => 0,
    }
}

/// Count top-level properties in the schema's `properties` object.
/// A high number of flat params indicates that the model must reason about which
/// arguments are applicable and how to fill them correctly.
fn top_level_property_count(schema: &serde_json::Value) -> usize {
    schema
        .as_object()
        .and_then(|m| m.get("properties"))
        .and_then(|p| p.as_object())
        .map_or(0, serde_json::Map::len)
}

/// Augment a `ToolDefinition` with the TAFC `_tafc_think` field.
///
/// Injects a top-level `_tafc_think` string property into the parameters schema,
/// prompting the model to reason before filling actual parameters.
/// Only applied when schema complexity >= `complexity_threshold`.
pub(crate) fn augment_with_tafc(
    mut def: ToolDefinition,
    complexity_threshold: f64,
) -> ToolDefinition {
    let complexity = schema_complexity(&def.parameters);
    if complexity < complexity_threshold {
        return def;
    }
    if let serde_json::Value::Object(ref mut map) = def.parameters {
        let properties = map
            .entry("properties")
            .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
        if let serde_json::Value::Object(props) = properties {
            props.insert(
                "_tafc_think".to_owned(),
                serde_json::json!({
                    "type": "string",
                    "description": "Think step-by-step before filling the parameters. \
                        Reason about the task, constraints, and which parameter values \
                        are most appropriate. This field is stripped before execution."
                }),
            );
        }
    }
    def
}

/// Prefix used to identify TAFC think fields in tool call inputs.
pub(crate) const TAFC_FIELD_PREFIX: &str = "_tafc_think";

/// Strip all TAFC think fields (`_tafc_think*`) from a tool input map.
///
/// Returns `true` if any fields were stripped, `false` otherwise.
/// Logs a WARN and returns `Err` if only think fields were present (no actual params).
///
/// # Audit trail note
///
/// `_tafc_think` reasoning content is intentionally dropped here and is never written
/// to the audit log, memory backend, or conversation history. The reasoning is ephemeral
/// scaffolding that improves parameter quality but must not inflate token budgets or
/// storage. If audit of reasoning is required in the future, a dedicated opt-in flag
/// should be added rather than persisting by default.
/// Returns true when `key` matches the TAFC field prefix, case-insensitively.
/// Case-insensitive matching prevents bypass via `_TAFC_THINK` or mixed-case variants (SEC-01).
fn is_tafc_key(key: &str) -> bool {
    key.len() >= TAFC_FIELD_PREFIX.len()
        && key[..TAFC_FIELD_PREFIX.len()].eq_ignore_ascii_case(TAFC_FIELD_PREFIX)
}

pub(crate) fn strip_tafc_fields(
    input: &mut serde_json::Map<String, serde_json::Value>,
    tool_name: &str,
) -> Result<bool, ()> {
    let tafc_keys: Vec<String> = input.keys().filter(|k| is_tafc_key(k)).cloned().collect();
    if tafc_keys.is_empty() {
        return Ok(false);
    }
    let has_real_params = input.keys().any(|k| !is_tafc_key(k));
    for k in &tafc_keys {
        input.remove(k);
    }
    if !has_real_params {
        tracing::warn!(
            tool = %tool_name,
            "TAFC: model produced only think fields with no actual parameters — treating as failure"
        );
        return Err(());
    }
    Ok(true)
}

pub(crate) fn tool_def_to_definition_with_tafc(
    def: &zeph_tools::registry::ToolDef,
    tafc: &zeph_tools::TafcConfig,
) -> ToolDefinition {
    let base = tool_def_to_definition(def);
    if tafc.enabled {
        augment_with_tafc(base, tafc.complexity_threshold)
    } else {
        base
    }
}

#[cfg(test)]
mod tests;
