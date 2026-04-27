// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

mod native;
mod sanitize;
mod tool_call_dag;

use zeph_llm::provider::{LlmProvider, Message, MessageMetadata, Role, ToolDefinition};

use super::Agent;
use crate::channel::Channel;
use crate::redact::redact_secrets;
use zeph_skills::loader::Skill;

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

/// Internal outcome of `run_vigil_gate` — not exposed outside `tool_execution`.
pub(crate) enum VigilOutcome {
    Clean,
    /// Advisory: body was truncated+annotated; `ContentSanitizer` continues downstream.
    Sanitized {
        #[allow(dead_code)]
        risk: zeph_tools::VigilRiskLevel,
    },
    /// Hard-block: sentinel replaces the body; `ContentSanitizer` is skipped.
    Blocked {
        #[allow(dead_code)]
        risk: zeph_tools::VigilRiskLevel,
        sentinel: String,
    },
}

impl VigilOutcome {
    pub(crate) fn is_blocked(&self) -> bool {
        matches!(self, Self::Blocked { .. })
    }
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

// pub(super) for tool_execution::tests — see issue #3497
#[cfg(test)]
pub(super) fn normalize_for_doom_loop(content: &str) -> String {
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

        let llm_timeout = std::time::Duration::from_secs(self.runtime.config.timeouts.llm_seconds);
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
                    timeout_secs = self.runtime.config.timeouts.llm_seconds,
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
        } else if let (Some(memory), Some(conv_id)) = (
            &self.services.memory.persistence.memory,
            self.services.memory.persistence.conversation_id,
        ) {
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
        let Some(ref mut det) = self.runtime.debug.anomaly_detector else {
            return Ok(());
        };
        match outcome {
            AnomalyOutcome::Success => det.record_success(),
            AnomalyOutcome::Error => det.record_error(),
            AnomalyOutcome::Blocked => det.record_blocked(),
            AnomalyOutcome::ReasoningQualityFailure { model, tool } => {
                if self.runtime.debug.reasoning_model_warning {
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

    /// Run regex PII filter and (optionally) NER classifier, merge spans, and redact in one pass.
    ///
    /// Thin wrapper: delegates to [`SecurityState::scrub_pii`] and applies metrics side-effects.
    async fn scrub_pii_union(&mut self, text: &str, tool_name: &str) -> String {
        let result = self.services.security.scrub_pii(text, tool_name).await;
        if result.ner_timeouts > 0 {
            self.update_metrics(|m| m.pii_ner_timeouts += u64::from(result.ner_timeouts));
        }
        if result.circuit_breaker_tripped {
            self.update_metrics(|m| m.pii_ner_circuit_breaker_trips += 1);
        }
        if result.scrubbed {
            self.update_metrics(|m| m.pii_scrub_count += 1);
            self.push_classifier_metrics();
        }
        result.text
    }

    /// Delegate guardrail check to [`SecurityState::check_guardrail`].
    async fn apply_guardrail_to_tool_output(&self, body: String, tool_name: &str) -> String {
        self.services
            .security
            .check_guardrail(body, tool_name)
            .await
    }

    fn scan_output_and_warn(&mut self, text: &str) -> String {
        let (cleaned, events) = self.services.security.exfiltration_guard.scan_output(text);
        if !events.is_empty() {
            tracing::warn!(
                count = events.len(),
                "exfiltration guard: markdown images blocked"
            );
            self.update_metrics(|m| {
                m.exfiltration_images_blocked += events.len() as u64;
            });
            self.push_security_event(
                zeph_common::SecurityEventCategory::ExfiltrationBlock,
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

        if !self.services.security.response_verifier.is_enabled() {
            return false;
        }

        let ctx = VerificationContext { response_text };
        let result = self.services.security.response_verifier.verify(&ctx);

        match result {
            ResponseVerificationResult::Clean => false,
            ResponseVerificationResult::Flagged { matched } => {
                let detail = matched.join(", ");
                tracing::warn!(patterns = %detail, "response verification: injection patterns in LLM output");
                self.push_security_event(
                    zeph_common::SecurityEventCategory::ResponseVerification,
                    "llm_response",
                    format!("flagged: {detail}"),
                );
                false
            }
            ResponseVerificationResult::Blocked { matched } => {
                let detail = matched.join(", ");
                tracing::error!(patterns = %detail, "response verification: blocking LLM response");
                self.push_security_event(
                    zeph_common::SecurityEventCategory::ResponseVerification,
                    "llm_response",
                    format!("blocked: {detail}"),
                );
                true
            }
        }
    }

    pub(super) fn maybe_redact<'a>(&self, text: &'a str) -> std::borrow::Cow<'a, str> {
        if self.runtime.config.security.redact_secrets {
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

    fn last_user_content(&self) -> Option<&str> {
        self.msg
            .messages
            .iter()
            .rev()
            .find(|m| m.role == zeph_llm::provider::Role::User)
            .map(|m| m.content.as_str())
    }

    async fn check_response_cache(&mut self) -> Result<CacheCheckResult, super::error::AgentError> {
        let Some(ref cache) = self.services.session.response_cache else {
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
        let key =
            zeph_memory::ResponseCache::compute_key(&content, &self.runtime.config.model_name);

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
        if self.runtime.config.semantic_cache_enabled && self.provider.supports_embeddings() {
            use zeph_llm::provider::LlmProvider as _;
            let threshold = self.runtime.config.semantic_cache_threshold;
            let max_candidates = self.runtime.config.semantic_cache_max_candidates;
            tracing::debug!(
                max_candidates,
                threshold,
                "semantic cache lookup: examining up to {max_candidates} candidates",
            );
            match self.embedding_provider.embed(&content).await {
                Ok(embedding) => {
                    let embed_model = self.services.skill.embedding_model.clone();
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
        let Some(ref cache) = self.services.session.response_cache else {
            return;
        };
        let Some(content) = self.last_user_content() else {
            return;
        };
        let key = zeph_memory::ResponseCache::compute_key(content, &self.runtime.config.model_name);

        // If we have a pre-computed embedding (semantic cache enabled + embed succeeded) and the
        // response is not tool-call output, use put_with_embedding — it uses INSERT OR REPLACE so
        // it handles the exact-match write too, avoiding a redundant SQL round-trip.
        // Otherwise fall back to exact-match-only put().
        if let Some(embedding) = query_embedding
            && !response.contains("[tool_use:")
        {
            let embed_model = &self.services.skill.embedding_model;
            if let Err(e) = cache
                .put_with_embedding(
                    &key,
                    response,
                    &self.runtime.config.model_name,
                    &embedding,
                    embed_model,
                )
                .await
            {
                tracing::warn!("failed to store semantic cache entry: {e:#}");
                // Fallback: at least persist exact-match entry.
                if let Err(e2) = cache
                    .put(&key, response, &self.runtime.config.model_name)
                    .await
                {
                    tracing::warn!("failed to store response in cache: {e2:#}");
                }
            }
        } else if let Err(e) = cache
            .put(&key, response, &self.runtime.config.model_name)
            .await
        {
            tracing::warn!("failed to store response in cache: {e:#}");
        }
    }

    fn inject_active_skill_env(&self) {
        if self.services.skill.active_skill_names.is_empty()
            || self.services.skill.available_custom_secrets.is_empty()
        {
            return;
        }
        let active_skills: Vec<Skill> = {
            let reg = self.services.skill.registry.read();
            self.services
                .skill
                .active_skill_names
                .iter()
                .filter_map(|name| reg.skill(name).ok())
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
                        self.services
                            .skill
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
// pub(super) for tool_execution::tests — see issue #3497
pub(super) fn tool_args_hash(params: &serde_json::Map<String, serde_json::Value>) -> u64 {
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
// pub(super) for tool_execution::tests — see issue #3497
pub(super) fn retry_backoff_ms(attempt: usize, base_ms: u64, max_ms: u64) -> u64 {
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
        // NOTE: ToolDef.id uses Cow<'static, str> for zero-copy registration.
        // Converted to ToolName at the LLM boundary for type safety downstream.
        name: zeph_common::ToolName::new(def.id.as_ref()),
        description: def.description.to_string(),
        parameters: params,
        output_schema: def.output_schema.clone(),
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

/// VIGIL pre-sanitizer gate integration for the agent tool-execution pipeline.
impl<C: Channel> Agent<C> {
    /// Run the VIGIL gate on `body` and return `(body_after, outcome)`.
    ///
    /// Returns `(body, None)` immediately for subagent sessions or when the gate is absent.
    /// On `Sanitize`: emits `VigilFlag` event, bumps `vigil_flags_total`.
    /// On `Block`: emits `VigilFlag` event, bumps both counters; caller skips `ContentSanitizer`.
    pub(super) fn run_vigil_gate(
        &mut self,
        tool_name: &str,
        body: String,
    ) -> (String, Option<VigilOutcome>) {
        // Subagent exemption (FR-009): skip VIGIL entirely.
        if self.services.session.parent_tool_use_id.is_some() {
            return (body, None);
        }
        let Some(ref gate) = self.services.security.vigil else {
            return (body, None);
        };

        let intent = self
            .services
            .session
            .current_turn_intent
            .as_deref()
            .unwrap_or_default();

        let verdict = gate.verify(intent, tool_name, &body);

        let crate::agent::vigil::VigilVerdict::Flagged {
            ref reason, action, ..
        } = verdict
        else {
            return (body, Some(VigilOutcome::Clean));
        };

        let (body_after, risk) = gate.apply(body, &verdict);

        self.push_security_event(
            zeph_common::SecurityEventCategory::VigilFlag,
            tool_name,
            reason,
        );

        let is_block = matches!(action, crate::agent::vigil::VigilAction::Block);
        self.update_metrics(|m| {
            m.vigil_flags_total += 1;
            if is_block {
                m.vigil_blocks_total += 1;
            }
        });

        let outcome = if is_block {
            tracing::warn!(tool = %tool_name, reason = %reason, "VIGIL blocked tool output");
            VigilOutcome::Blocked {
                risk,
                sentinel: body_after.clone(),
            }
        } else {
            tracing::debug!(tool = %tool_name, reason = %reason, "VIGIL sanitized tool output");
            VigilOutcome::Sanitized { risk }
        };

        (body_after, Some(outcome))
    }
}

#[cfg(test)]
mod tests;
