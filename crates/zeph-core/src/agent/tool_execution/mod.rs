// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

mod legacy;
mod native;
mod tool_call_dag;

use zeph_llm::provider::{LlmProvider, Message, MessageMetadata, Role, ToolDefinition};

use super::Agent;
use crate::channel::Channel;
use crate::redact::redact_secrets;
use crate::sanitizer::{ContentSource, ContentSourceKind};
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
    async fn sanitize_tool_output(&mut self, body: &str, tool_name: &str) -> (String, bool) {
        // MCP tools use "server:tool" format (contains ':') or legacy "mcp" name.
        // Web scrape tools use "web-scrape" (hyphenated) or "fetch".
        // Everything else is local shell/file output.
        let kind = if tool_name.contains(':') || tool_name == "mcp" || tool_name == "search_code" {
            ContentSourceKind::McpResponse
        } else if tool_name == "web-scrape" || tool_name == "web_scrape" || tool_name == "fetch" {
            ContentSourceKind::WebScrape
        } else {
            ContentSourceKind::ToolResult
        };
        let source = ContentSource::new(kind).with_identifier(tool_name);
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
            let urls = crate::sanitizer::exfiltration::extract_flagged_urls(&sanitized.body);
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
                    let escaped = crate::sanitizer::ContentSanitizer::escape_delimiter_tags(&facts);
                    return (
                        crate::sanitizer::ContentSanitizer::apply_spotlight(
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
        let body = if self.security.pii_filter.is_enabled() {
            let scrubbed = self.security.pii_filter.scrub(&sanitized.body);
            if matches!(scrubbed, std::borrow::Cow::Owned(_)) {
                self.update_metrics(|m| m.pii_scrub_count += 1);
                tracing::debug!(tool = %tool_name, "PII scrubbed from tool output");
            }
            scrubbed.into_owned()
        } else {
            sanitized.body
        };

        // Guardrail: opt-in tool output scanning for indirect prompt injection (scan_tool_output=true).
        #[cfg(feature = "guardrail")]
        let body = self.apply_guardrail_to_tool_output(body, tool_name).await;

        (body, has_injection_flags)
    }

    #[cfg(feature = "guardrail")]
    async fn apply_guardrail_to_tool_output(&self, mut body: String, tool_name: &str) -> String {
        use crate::sanitizer::guardrail::GuardrailVerdict;
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

    async fn check_response_cache(&mut self) -> Result<Option<String>, super::error::AgentError> {
        if let Some(ref cache) = self.session.response_cache {
            let Some(content) = self.last_user_content() else {
                return Ok(None);
            };
            let key = zeph_memory::ResponseCache::compute_key(content, &self.runtime.model_name);
            if let Ok(Some(cached)) = cache.get(&key).await {
                tracing::debug!("response cache hit");
                // M4: scan cached responses before sending to channel.
                let cleaned = self.scan_output_and_warn(&cached);
                if !cleaned.is_empty() {
                    let display = self.maybe_redact(&cleaned);
                    self.channel.send(&display).await?;
                }
                return Ok(Some(cleaned));
            }
        }
        Ok(None)
    }

    async fn store_response_in_cache(&self, response: &str) {
        if let Some(ref cache) = self.session.response_cache {
            let Some(content) = self.last_user_content() else {
                return;
            };
            let key = zeph_memory::ResponseCache::compute_key(content, &self.runtime.model_name);
            if let Err(e) = cache.put(&key, response, &self.runtime.model_name).await {
                tracing::warn!("failed to store response in cache: {e:#}");
            }
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
/// Formula: `base_ms * 2^attempt`, nominally capped at 5000ms.
/// Full jitter in `[0, cap]` is applied using `rand` for cryptographically
/// seeded randomness — avoids predictable timing that an adversary could exploit
/// to align retry windows.
fn retry_backoff_ms(attempt: usize) -> u64 {
    use rand::RngExt as _;
    const BASE_MS: u64 = 500;
    const MAX_MS: u64 = 5000;
    let base = BASE_MS.saturating_mul(1_u64 << attempt.min(10));
    let capped = base.min(MAX_MS);
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

#[cfg(test)]
mod tests;
