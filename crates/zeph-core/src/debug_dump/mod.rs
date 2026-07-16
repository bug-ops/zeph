// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Debug dump writer for a single agent session.
//!
//! When active, every LLM request/response pair and raw tool output is written to
//! numbered files in a timestamped subdirectory of the configured output directory.
//! Intended for context debugging only — do not use in production.

pub mod trace;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use base64::Engine as _;
use zeph_llm::provider::{Message, MessagePart, Role, ToolDefinition};

use crate::redact::scrub_content;

pub use zeph_config::DumpFormat;

/// Cloneable debug dump writer; clones share the same atomic counter.
#[derive(Clone)]
pub struct DebugDumper {
    dir: PathBuf,
    counter: Arc<AtomicU32>,
    format: DumpFormat,
    include_raw_images: bool,
}

pub struct RequestDebugDump<'a> {
    pub model_name: &'a str,
    pub messages: &'a [Message],
    pub tools: &'a [ToolDefinition],
    pub provider_request: serde_json::Value,
    /// Current `MemCoT` semantic state buffer at the time of this request, if any.
    ///
    /// `Some` when `memory.memcot.enabled = true` and at least one distillation has run.
    /// Written to the dump so offline analysis can correlate state with LLM payloads.
    pub memcot_state: Option<&'a str>,
}

impl DebugDumper {
    /// Create a new dumper, creating a timestamped subdirectory under `base_dir`.
    ///
    /// # Errors
    ///
    /// Returns an error if the directory cannot be created.
    pub fn new(base_dir: &Path, format: DumpFormat) -> std::io::Result<Self> {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let dir = base_dir.join(ts.to_string());
        std::fs::create_dir_all(&dir)?;
        tracing::info!(path = %dir.display(), format = ?format, "debug dump directory created");
        Ok(Self {
            dir,
            counter: Arc::new(AtomicU32::new(0)),
            format,
            include_raw_images: false,
        })
    }

    /// Sets whether `MessagePart::Image` payloads are written to debug dumps as full raw
    /// base64 bytes instead of a redacted `<redacted image: ...>` marker.
    ///
    /// Default: `false` (redacted). Mirrors [`zeph_config::DebugConfig::include_raw_images`];
    /// enable only when a developer explicitly needs full wire-payload fidelity for
    /// image-related debugging (#6306). Logs a warning when enabled so operators have a
    /// visible signal that this security tradeoff is active.
    #[must_use]
    pub fn with_include_raw_images(mut self, include: bool) -> Self {
        if include {
            tracing::warn!(
                "debug dumps: include_raw_images=true — image payloads will be written to disk unredacted"
            );
        }
        self.include_raw_images = include;
        self
    }

    /// Return the session dump directory.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Returns `true` when the dump format is [`DumpFormat::Trace`].
    ///
    /// In Trace mode `dump_request` returns early without using `provider_request`, so callers
    /// can skip the expensive `debug_request_json` serialization.
    #[must_use]
    pub fn is_trace_format(&self) -> bool {
        self.format == DumpFormat::Trace
    }

    fn next_id(&self) -> u32 {
        self.counter.fetch_add(1, Ordering::Relaxed)
    }

    /// Offload the write to the blocking thread pool. Fire-and-forget: the returned
    /// `JoinHandle` is intentionally dropped — callers never wait on debug dump I/O, and a
    /// failed write is only logged, never propagated (this is an opt-in debugging aid, not a
    /// correctness-critical path). Dropping the handle does not cancel the task; it still runs
    /// to completion on the blocking pool.
    ///
    /// Requires an active Tokio runtime. Used only by dump methods reachable from the async
    /// per-turn hot path (#6029); call sites reachable from synchronous contexts (e.g. a plain
    /// `Fn` callback) must use [`Self::write_sync`] instead.
    fn write(&self, filename: &str, content: &[u8]) {
        let path = self.dir.join(filename);
        let content = content.to_vec();
        // Ask-First exception to rust-code.md's Await Discipline rule #2 ("fire-and-forget
        // tasks MUST be tracked, never drop a JoinHandle"): this handle is deliberately
        // dropped, untracked. Rationale: this is a debug-only diagnostic path (opt-in,
        // disabled by default), write failures are already logged via `tracing::warn!` below
        // and never need to surface to a caller, and routing it through `BackgroundSupervisor`
        // would require threading a `&mut BackgroundSupervisor` through 5 otherwise-unrelated
        // hot-path call sites (llm_dispatch.rs, tool_result.rs, tier_loop.rs, focus.rs,
        // state/mod.rs) for a debug feature most users never enable — judged disproportionate
        // for this fix. Recorded here per the exception clause in
        // .claude/rules/continuous-improvement.md rather than left as a silent deviation.
        tokio::task::spawn_blocking(move || {
            if let Err(e) = zeph_common::fs_secure::write_private(&path, &content) {
                tracing::warn!(path = %path.display(), error = %e, "debug dump write failed");
            }
        });
    }

    /// Synchronous write, used by dump methods invoked from non-async call sites (a plain
    /// `Fn` callback, or test-only code paths not reachable from the agent turn loop). These are
    /// out of scope for the #6029 hot-path fix — see the issue for the tracked follow-up.
    fn write_sync(&self, filename: &str, content: &[u8]) {
        let path = self.dir.join(filename);
        if let Err(e) = zeph_common::fs_secure::write_private(&path, content) {
            tracing::warn!(path = %path.display(), error = %e, "debug dump write failed");
        }
    }

    /// Dump the messages about to be sent to the LLM.
    ///
    /// Returns an ID that must be passed to `dump_response` to correlate request and response.
    /// When `format = Trace`, no file is written (spans are collected by `trace::TracingCollector`).
    #[must_use]
    pub fn dump_request(&self, request: &RequestDebugDump<'_>) -> u32 {
        let id = self.next_id();
        // In Trace format, skip legacy numbered files — span data lives in TracingCollector.
        if self.format == DumpFormat::Trace {
            return id;
        }
        let json = match self.format {
            DumpFormat::Raw => raw_dump(request, self.include_raw_images),
            DumpFormat::Trace => unreachable!("handled above"),
            _ => json_dump(request, self.include_raw_images),
        };
        self.write(&format!("{id:04}-request.json"), json.as_bytes());
        id
    }

    /// Dump the LLM response corresponding to a prior `dump_request` call.
    /// When `format = Trace`, this is a no-op.
    pub fn dump_response(&self, id: u32, response: &str) {
        if self.format == DumpFormat::Trace {
            return;
        }
        self.write(&format!("{id:04}-response.txt"), response.as_bytes());
    }

    /// Dump raw tool output before any truncation or summarization.
    /// When `format = Trace`, this is a no-op (tool output is recorded via `TracingCollector`).
    pub fn dump_tool_output(&self, tool_name: &str, output: &str) {
        if self.format == DumpFormat::Trace {
            return;
        }
        let id = self.next_id();
        let safe_name = sanitize_dump_name(tool_name);
        self.write(&format!("{id:04}-tool-{safe_name}.txt"), output.as_bytes());
    }

    /// Dump pruning scores computed by task-aware or MIG scoring.
    /// When `format = Trace`, this is a no-op.
    #[cfg(test)]
    pub(crate) fn dump_pruning_scores(&self, scores: &[zeph_agent_context::BlockScore]) {
        if self.format == DumpFormat::Trace {
            return;
        }
        let id = self.next_id();
        let payload: Vec<serde_json::Value> = scores
            .iter()
            .map(|s| {
                serde_json::json!({
                    "msg_index": s.msg_index,
                    "relevance": s.relevance,
                    "redundancy": s.redundancy,
                    "mig": s.mig,
                })
            })
            .collect();
        match serde_json::to_string_pretty(&serde_json::json!({ "scores": payload })) {
            Ok(json) => self.write_sync(&format!("{id:04}-pruning-scores.json"), json.as_bytes()),
            Err(e) => tracing::warn!("dump_pruning_scores: serialize failed: {e}"),
        }
    }

    /// Dump an `AnchoredSummary` produced during structured compaction.
    ///
    /// Includes completeness metrics and a fallback flag.
    /// When `format = Trace`, this is a no-op.
    pub(crate) fn dump_anchored_summary(
        &self,
        summary: &zeph_memory::AnchoredSummary,
        fallback: bool,
        token_counter: &zeph_memory::TokenCounter,
    ) {
        if self.format == DumpFormat::Trace {
            return;
        }
        let id = self.next_id();
        let section_completeness = serde_json::json!({
            "session_intent": !summary.session_intent.trim().is_empty(),
            "files_modified": !summary.files_modified.is_empty(),
            "decisions_made": !summary.decisions_made.is_empty(),
            "open_questions": !summary.open_questions.is_empty(),
            "next_steps": !summary.next_steps.is_empty(),
        });
        let total_items = summary.files_modified.len()
            + summary.decisions_made.len()
            + summary.open_questions.len()
            + summary.next_steps.len();
        let markdown = summary.to_markdown();
        let token_estimate = token_counter.count_tokens(&markdown);
        let payload = serde_json::json!({
            "summary": summary,
            "section_completeness": section_completeness,
            "total_items": total_items,
            "token_estimate": token_estimate,
            "fallback": fallback,
        });
        match serde_json::to_string_pretty(&payload) {
            Ok(json) => self.write_sync(&format!("{id:04}-anchored-summary.json"), json.as_bytes()),
            Err(e) => tracing::warn!("dump_anchored_summary: serialize failed: {e}"),
        }
    }

    /// Dump the compaction probe result for a hard compaction event (#1609).
    /// When `format = Trace`, this is a no-op.
    pub(crate) fn dump_compaction_probe(&self, result: &zeph_memory::CompactionProbeResult) {
        if self.format == DumpFormat::Trace {
            return;
        }
        let id = self.next_id();
        let questions: Vec<serde_json::Value> = result
            .questions
            .iter()
            .zip(
                result
                    .answers
                    .iter()
                    .chain(std::iter::repeat(&String::new())),
            )
            .zip(
                result
                    .per_question_scores
                    .iter()
                    .chain(std::iter::repeat(&0.0_f32)),
            )
            .map(|((q, a), &s)| {
                serde_json::json!({
                    "question": scrub_content(&q.question),
                    "expected": scrub_content(&q.expected_answer),
                    "actual": scrub_content(a),
                    "score": s,
                    "category": format!("{:?}", q.category),
                })
            })
            .collect();
        let category_scores: Vec<serde_json::Value> = result
            .category_scores
            .iter()
            .map(|cs| {
                serde_json::json!({
                    "category": format!("{:?}", cs.category),
                    "score": cs.score,
                    "probes_run": cs.probes_run,
                })
            })
            .collect();
        let payload = serde_json::json!({
            "score": result.score,
            "category_scores": category_scores,
            "threshold": result.threshold,
            "hard_fail_threshold": result.hard_fail_threshold,
            "verdict": format!("{:?}", result.verdict),
            "model": result.model,
            "duration_ms": result.duration_ms,
            "questions": questions,
        });
        match serde_json::to_string_pretty(&payload) {
            Ok(json) => {
                self.write_sync(&format!("{id:04}-compaction-probe.json"), json.as_bytes());
            }
            Err(e) => tracing::warn!("dump_compaction_probe: serialize failed: {e}"),
        }
    }

    /// Dump the accumulated Focus Agent knowledge blocks.
    /// When `format = Trace`, this is a no-op.
    pub fn dump_focus_knowledge(&self, knowledge: &str) {
        if self.format == DumpFormat::Trace {
            return;
        }
        let id = self.next_id();
        self.write(
            &format!("{id:04}-focus-knowledge.txt"),
            knowledge.as_bytes(),
        );
    }

    /// Dump `SideQuest` eviction state: cursor list with eviction flags and freed token count.
    /// When `format = Trace`, this is a no-op.
    pub(crate) fn dump_sidequest_eviction(
        &self,
        cursors: &[crate::agent::sidequest::ToolOutputCursor],
        evicted_indices: &[usize],
        freed_tokens: usize,
    ) {
        if self.format == DumpFormat::Trace {
            return;
        }
        let id = self.next_id();
        let cursor_info: Vec<serde_json::Value> = cursors
            .iter()
            .enumerate()
            .map(|(i, c)| {
                serde_json::json!({
                    "cursor_id": i,
                    "msg_index": c.msg_index,
                    "part_index": c.part_index,
                    "tool_name": c.tool_name,
                    "token_count": c.token_count,
                    "evicted": evicted_indices.contains(&i),
                })
            })
            .collect();
        let payload = serde_json::json!({
            "cursors": cursor_info,
            "evicted_indices": evicted_indices,
            "freed_tokens": freed_tokens,
        });
        match serde_json::to_string_pretty(&payload) {
            Ok(json) => {
                self.write_sync(&format!("{id:04}-sidequest-eviction.json"), json.as_bytes());
            }
            Err(e) => tracing::warn!("dump_sidequest_eviction: serialize failed: {e}"),
        }
    }

    /// Dump the subgoal registry state alongside a compaction event (#2022).
    ///
    /// Writes a human-readable text file listing each subgoal with its state and message span.
    /// When `format = Trace`, this is a no-op.
    #[cfg(test)]
    pub(crate) fn dump_subgoal_registry(&self, registry: &zeph_agent_context::SubgoalRegistry) {
        if self.format == DumpFormat::Trace {
            return;
        }
        let id = self.next_id();
        let mut output = String::from("=== Subgoal Registry ===\n");
        if registry.subgoals.is_empty() {
            output.push_str("(no subgoals tracked yet)\n");
        } else {
            for sg in &registry.subgoals {
                let state_str = match sg.state {
                    zeph_agent_context::SubgoalState::Active => "Active   ",
                    zeph_agent_context::SubgoalState::Completed => "Completed",
                    _ => "Unknown  ",
                };
                let _ = std::fmt::write(
                    &mut output,
                    format_args!(
                        "[{}] {state_str}: \"{}\" (msgs {}-{})\n",
                        sg.id.0, sg.description, sg.start_msg_index, sg.end_msg_index,
                    ),
                );
            }
        }
        self.write_sync(&format!("{id:04}-subgoal-registry.txt"), output.as_bytes());
    }

    /// Dump a tool error with error classification for debugging transient/permanent failures.
    /// When `format = Trace`, this is a no-op.
    pub fn dump_tool_error(&self, tool_name: &str, error: &zeph_tools::ToolError) {
        if self.format == DumpFormat::Trace {
            return;
        }
        let id = self.next_id();
        let safe_name = sanitize_dump_name(tool_name);
        let payload = serde_json::json!({
            "tool": tool_name,
            "error": error.to_string(),
            "kind": error.kind().to_string(),
        });
        match serde_json::to_string_pretty(&payload) {
            Ok(json) => {
                self.write(
                    &format!("{id:04}-tool-error-{safe_name}.json"),
                    json.as_bytes(),
                );
            }
            Err(e) => {
                tracing::warn!("dump_tool_error: failed to serialize error payload: {e}");
            }
        }
    }
}

fn json_dump(request: &RequestDebugDump<'_>, include_raw_images: bool) -> String {
    let mut messages =
        serde_json::to_value(request.messages).unwrap_or(serde_json::Value::Array(vec![]));
    if !include_raw_images {
        redact_image_payloads(&mut messages);
    }
    let payload = serde_json::json!({
        "model": extract_model(&request.provider_request, request.model_name),
        "max_tokens": extract_max_tokens(&request.provider_request),
        "messages": messages,
        "tools": extract_tools(&request.provider_request, request.tools),
        "temperature": request
            .provider_request
            .get("temperature")
            .cloned()
            .unwrap_or(serde_json::Value::Null),
        "cache_control": request
            .provider_request
            .get("cache_control")
            .cloned()
            .unwrap_or(serde_json::Value::Null),
        "memcot_state": request.memcot_state,
    });
    serde_json::to_string_pretty(&payload).unwrap_or_else(|e| format!("serialization error: {e}"))
}

fn raw_dump(request: &RequestDebugDump<'_>, include_raw_images: bool) -> String {
    let mut payload = if request.provider_request.is_object() {
        request.provider_request.clone()
    } else {
        serde_json::json!({})
    };
    if let Some(obj) = payload.as_object_mut() {
        obj.entry("model")
            .or_insert_with(|| extract_model(&request.provider_request, request.model_name));
        obj.entry("max_tokens")
            .or_insert_with(|| extract_max_tokens(&request.provider_request));
        obj.entry("tools")
            .or_insert_with(|| extract_tools(&request.provider_request, request.tools));
        obj.entry("temperature").or_insert_with(|| {
            request
                .provider_request
                .get("temperature")
                .cloned()
                .unwrap_or(serde_json::Value::Null)
        });
        obj.entry("cache_control").or_insert_with(|| {
            request
                .provider_request
                .get("cache_control")
                .cloned()
                .unwrap_or(serde_json::Value::Null)
        });
        obj.insert(
            "memcot_state".to_owned(),
            match request.memcot_state {
                Some(s) => serde_json::Value::String(s.to_owned()),
                None => serde_json::Value::Null,
            },
        );
        if !obj.contains_key("messages") && !obj.contains_key("system") {
            let generic = messages_to_api_value(request.messages);
            if let Some(generic_obj) = generic.as_object() {
                for (key, value) in generic_obj {
                    obj.insert(key.clone(), value.clone());
                }
            }
        }
    }
    if !include_raw_images {
        redact_image_payloads(&mut payload);
    }
    serde_json::to_string_pretty(&payload).unwrap_or_else(|e| format!("serialization error: {e}"))
}

/// Minimum base64-encoded length treated as plausible image data under an `"images"` key
/// (Ollama's per-message image array). Guards against clobbering unrelated short strings
/// that happen to occupy a key named `images` (e.g. a tool argument) with a redaction marker.
const MIN_IMAGE_BASE64_LEN: usize = 64;

/// Recursively redacts base64-encoded image payloads from a dumped JSON tree, replacing
/// them with a size/format marker (see [`image_marker`]).
///
/// `MessagePart::Image` bytes reach the dump under different key shapes depending on the
/// dump format and target LLM provider, so the whole tree is walked rather than assuming
/// one shape:
/// - This crate's internal `MessagePart` serde form (`json_dump`, `part_to_block`) and
///   Claude/Anthropic content blocks: `{"type":"image","source":{"data":...,"media_type":...}}`
///   or `{"kind":"image","data":...,"mime_type":...}`.
/// - `OpenAI`: `{"image_url":{"url":"data:<mime>;base64,<data>"}}`.
/// - Gemini: `{"inlineData":{"mimeType":...,"data":...}}`.
/// - Ollama: `{"images":["<base64>", ...]}`.
fn redact_image_payloads(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            let is_image_kind =
                map.get("kind").and_then(serde_json::Value::as_str) == Some("image");
            let is_image_type =
                map.get("type").and_then(serde_json::Value::as_str) == Some("image");

            if is_image_kind {
                redact_base64_field(map, "data", "mime_type");
            }
            if is_image_type
                && let Some(source) = map
                    .get_mut("source")
                    .and_then(serde_json::Value::as_object_mut)
            {
                redact_base64_field(source, "data", "media_type");
            }
            if let Some(image_url) = map
                .get_mut("image_url")
                .and_then(serde_json::Value::as_object_mut)
                && let Some(marker) = image_url
                    .get("url")
                    .and_then(serde_json::Value::as_str)
                    .and_then(redact_data_url)
            {
                image_url.insert("url".to_owned(), serde_json::Value::String(marker));
            }
            if let Some(inline) = map
                .get_mut("inlineData")
                .and_then(serde_json::Value::as_object_mut)
            {
                redact_base64_field(inline, "data", "mimeType");
            }
            if let Some(images) = map
                .get_mut("images")
                .and_then(serde_json::Value::as_array_mut)
            {
                for img in images.iter_mut() {
                    if let Some(encoded) = img.as_str().filter(|s| s.len() >= MIN_IMAGE_BASE64_LEN)
                    {
                        *img =
                            serde_json::Value::String(image_marker_from_base64("image", encoded));
                    }
                }
            }
            for v in map.values_mut() {
                redact_image_payloads(v);
            }
        }
        serde_json::Value::Array(arr) => {
            for v in arr.iter_mut() {
                redact_image_payloads(v);
            }
        }
        _ => {}
    }
}

fn redact_base64_field(
    obj: &mut serde_json::Map<String, serde_json::Value>,
    data_key: &str,
    mime_key: &str,
) {
    let mime_type = obj
        .get(mime_key)
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown")
        .to_owned();
    if let Some(marker) = obj
        .get(data_key)
        .and_then(serde_json::Value::as_str)
        .map(|encoded| image_marker_from_base64(&mime_type, encoded))
    {
        obj.insert(data_key.to_owned(), serde_json::Value::String(marker));
    }
}

fn redact_data_url(url: &str) -> Option<String> {
    let rest = url.strip_prefix("data:")?;
    let (mime_type, encoded) = rest.split_once(";base64,")?;
    Some(image_marker_from_base64(mime_type, encoded))
}

fn image_marker_from_base64(mime_type: &str, encoded: &str) -> String {
    base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_or_else(
            |_| {
                format!(
                    "<redacted image: {mime_type}, undecodable base64 ({} chars)>",
                    encoded.len()
                )
            },
            |bytes| image_marker(mime_type, &bytes),
        )
}

/// Builds a placeholder for a redacted image, retaining enough context (MIME type, exact
/// byte size, content fingerprint) to correlate or diff dumps without ever persisting the
/// raw bytes to disk.
fn image_marker(mime_type: &str, data: &[u8]) -> String {
    let hash = blake3::hash(data).to_hex();
    format!(
        "<redacted image: {mime_type}, {} bytes, blake3:{}>",
        data.len(),
        &hash[..16]
    )
}

fn extract_model(payload: &serde_json::Value, fallback: &str) -> serde_json::Value {
    payload
        .get("model")
        .cloned()
        .unwrap_or_else(|| serde_json::json!(fallback))
}

fn extract_max_tokens(payload: &serde_json::Value) -> serde_json::Value {
    payload
        .get("max_tokens")
        .cloned()
        .or_else(|| payload.get("max_completion_tokens").cloned())
        .unwrap_or(serde_json::Value::Null)
}

fn extract_tools(payload: &serde_json::Value, fallback: &[ToolDefinition]) -> serde_json::Value {
    payload.get("tools").cloned().unwrap_or_else(|| {
        serde_json::to_value(fallback).unwrap_or(serde_json::Value::Array(vec![]))
    })
}

fn sanitize_dump_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Render messages as the API payload format (mirrors `split_messages_structured` in the
/// Claude provider): system extracted, `agent_visible = false` messages filtered out,
/// parts converted to typed content blocks (`text`, `tool_use`, `tool_result`, etc.).
fn messages_to_api_value(messages: &[Message]) -> serde_json::Value {
    let system: String = messages
        .iter()
        .filter(|m| m.metadata.visibility.is_agent_visible() && m.role == Role::System)
        .map(zeph_llm::provider::Message::to_llm_content)
        .collect::<Vec<_>>()
        .join("\n\n");

    let chat: Vec<serde_json::Value> = messages
        .iter()
        .filter(|m| m.metadata.visibility.is_agent_visible() && m.role != Role::System)
        .filter_map(|m| {
            let role = match m.role {
                Role::User => "user",
                Role::Assistant => "assistant",
                Role::System | _ => return None,
            };
            let is_assistant = m.role == Role::Assistant;
            let has_structured = m.parts.iter().any(|p| {
                matches!(
                    p,
                    MessagePart::ToolUse { .. }
                        | MessagePart::ToolResult { .. }
                        | MessagePart::Image(_)
                        | MessagePart::ThinkingBlock { .. }
                        | MessagePart::RedactedThinkingBlock { .. }
                )
            });
            let content: serde_json::Value = if !has_structured || m.parts.is_empty() {
                let text = m.to_llm_content();
                if text.trim().is_empty() {
                    return None;
                }
                serde_json::json!(text)
            } else {
                let blocks: Vec<serde_json::Value> = m
                    .parts
                    .iter()
                    .filter_map(|p| part_to_block(p, is_assistant))
                    .collect();
                if blocks.is_empty() {
                    return None;
                }
                serde_json::Value::Array(blocks)
            };
            Some(serde_json::json!({ "role": role, "content": content }))
        })
        .collect();

    serde_json::json!({ "system": system, "messages": chat })
}

fn part_to_block(part: &MessagePart, is_assistant: bool) -> Option<serde_json::Value> {
    match part {
        MessagePart::Text { text }
        | MessagePart::Recall { text }
        | MessagePart::CodeContext { text }
        | MessagePart::Summary { text }
        | MessagePart::CrossSession { text } => {
            if text.trim().is_empty() {
                None
            } else {
                Some(serde_json::json!({ "type": "text", "text": text }))
            }
        }
        MessagePart::ToolOutput {
            tool_name,
            body,
            compacted_at,
        } => {
            let text = if compacted_at.is_some() {
                if body.is_empty() {
                    format!("[tool output: {tool_name}] (pruned)")
                } else {
                    format!("[tool output: {tool_name}] {body}")
                }
            } else {
                format!("[tool output: {tool_name}]\n{body}")
            };
            Some(serde_json::json!({ "type": "text", "text": text }))
        }
        MessagePart::ToolUse { id, name, input } if is_assistant => {
            Some(serde_json::json!({ "type": "tool_use", "id": id, "name": name, "input": input }))
        }
        MessagePart::ToolUse { name, input, .. } => Some(
            serde_json::json!({ "type": "text", "text": format!("[tool_use: {name}] {input}") }),
        ),
        MessagePart::ToolResult {
            tool_use_id,
            content,
            is_error,
        } if !is_assistant => Some(
            serde_json::json!({ "type": "tool_result", "tool_use_id": tool_use_id, "content": content, "is_error": is_error }),
        ),
        MessagePart::ToolResult { content, .. } => {
            if content.trim().is_empty() {
                None
            } else {
                Some(serde_json::json!({ "type": "text", "text": content }))
            }
        }
        MessagePart::ThinkingBlock {
            thinking,
            signature,
        } if is_assistant => Some(
            serde_json::json!({ "type": "thinking", "thinking": thinking, "signature": signature }),
        ),
        MessagePart::RedactedThinkingBlock { data } if is_assistant => {
            Some(serde_json::json!({ "type": "redacted_thinking", "data": data }))
        }
        MessagePart::ThinkingBlock { .. }
        | MessagePart::RedactedThinkingBlock { .. }
        | MessagePart::Compaction { .. }
            if !is_assistant =>
        {
            None
        }
        MessagePart::Compaction { summary } => {
            Some(serde_json::json!({ "type": "compaction", "summary": summary }))
        }
        MessagePart::Image(img) => Some(serde_json::json!({
            "type": "image",
            "source": {
                "type": "base64",
                "media_type": img.mime_type,
                "data": base64::engine::general_purpose::STANDARD.encode(&img.data),
            },
        })),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn dump_format_from_str_valid() {
        assert_eq!("json".parse::<DumpFormat>().unwrap(), DumpFormat::Json);
        assert_eq!("raw".parse::<DumpFormat>().unwrap(), DumpFormat::Raw);
        assert_eq!("trace".parse::<DumpFormat>().unwrap(), DumpFormat::Trace);
    }

    #[test]
    fn dump_format_from_str_invalid_returns_error() {
        let err = "binary".parse::<DumpFormat>().unwrap_err();
        assert!(
            err.contains("unknown dump format"),
            "error must mention unknown dump format: {err}"
        );
    }

    fn sample_messages() -> Vec<Message> {
        vec![
            Message::from_legacy(Role::System, "system prompt"),
            Message::from_legacy(Role::User, "hello"),
        ]
    }

    fn sample_tools() -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "read_file".into(),
            description: "Read a file".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
            }),
            output_schema: None,
        }]
    }

    const SAMPLE_IMAGE_BYTES: &[u8] = b"not-a-real-png-but-long-enough-to-look-like-image-bytes";
    const SAMPLE_IMAGE_MIME: &str = "image/png";

    fn sample_image_messages() -> Vec<Message> {
        vec![
            Message::from_legacy(Role::System, "system prompt"),
            Message::from_parts(
                Role::User,
                vec![
                    MessagePart::Text {
                        text: "describe this".to_owned(),
                    },
                    MessagePart::Image(Box::new(zeph_llm::provider::ImageData {
                        data: SAMPLE_IMAGE_BYTES.to_vec(),
                        mime_type: SAMPLE_IMAGE_MIME.to_owned(),
                    })),
                ],
            ),
        ]
    }

    fn sample_image_base64() -> String {
        base64::engine::general_purpose::STANDARD.encode(SAMPLE_IMAGE_BYTES)
    }

    /// Polls for the dump file rather than reading it immediately: `write()` now offloads to
    /// `spawn_blocking` and is fire-and-forget (#6029), so the file may not exist yet the
    /// instant `dump_request` returns.
    async fn read_request_dump(dir: &Path) -> serde_json::Value {
        let session = std::fs::read_dir(dir)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let path = session.join("0000-request.json");
        for _ in 0..200 {
            if let Ok(content) = std::fs::read_to_string(&path) {
                return serde_json::from_str(&content).unwrap();
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        panic!(
            "debug dump file not written within timeout: {}",
            path.display()
        );
    }

    #[tokio::test]
    async fn json_dump_request_includes_request_metadata() {
        let dir = tempdir().unwrap();
        let dumper = DebugDumper::new(dir.path(), DumpFormat::Json).unwrap();
        let messages = sample_messages();
        let tools = sample_tools();

        let _ = dumper.dump_request(&RequestDebugDump {
            model_name: "claude-sonnet-test",
            messages: &messages,
            tools: &tools,
            provider_request: serde_json::json!({
                "model": "claude-sonnet-test",
                "max_tokens": 4096,
                "tools": [{ "name": "read_file" }],
                "temperature": 0.7,
                "cache_control": { "type": "ephemeral" }
            }),
            memcot_state: None,
        });

        let payload = read_request_dump(dir.path()).await;
        assert_eq!(payload["model"], "claude-sonnet-test");
        assert_eq!(payload["max_tokens"], 4096);
        assert_eq!(payload["tools"][0]["name"], "read_file");
        assert_eq!(payload["temperature"], 0.7);
        assert_eq!(payload["cache_control"]["type"], "ephemeral");
        assert_eq!(payload["messages"][1]["content"], "hello");
    }

    #[tokio::test]
    async fn raw_dump_request_includes_request_metadata() {
        let dir = tempdir().unwrap();
        let dumper = DebugDumper::new(dir.path(), DumpFormat::Raw).unwrap();
        let messages = sample_messages();
        let tools = sample_tools();

        let _ = dumper.dump_request(&RequestDebugDump {
            model_name: "gpt-5-mini",
            messages: &messages,
            tools: &tools,
            provider_request: serde_json::json!({
                "model": "gpt-5-mini",
                "max_completion_tokens": 2048,
                "messages": [{ "role": "user", "content": "hello" }],
                "tools": [{ "type": "function", "function": { "name": "read_file" } }],
                "temperature": 0.3,
                "cache_control": null
            }),
            memcot_state: None,
        });

        let payload = read_request_dump(dir.path()).await;
        assert_eq!(payload["model"], "gpt-5-mini");
        assert_eq!(payload["max_tokens"], 2048);
        assert_eq!(payload["tools"][0]["function"]["name"], "read_file");
        assert_eq!(payload["temperature"], 0.3);
        assert_eq!(payload["messages"][0]["content"], "hello");
    }

    #[tokio::test]
    async fn memcot_state_written_to_dump_when_present() {
        for fmt in [DumpFormat::Json, DumpFormat::Raw] {
            let dir = tempdir().unwrap();
            let dumper = DebugDumper::new(dir.path(), fmt).unwrap();
            let messages = sample_messages();
            let tools = sample_tools();

            let _ = dumper.dump_request(&RequestDebugDump {
                model_name: "test-model",
                messages: &messages,
                tools: &tools,
                provider_request: serde_json::json!({ "model": "test-model", "max_tokens": 1024 }),
                memcot_state: Some("Rust uses LLVM; user is refactoring the parser"),
            });

            let payload = read_request_dump(dir.path()).await;
            assert_eq!(
                payload["memcot_state"], "Rust uses LLVM; user is refactoring the parser",
                "memcot_state must appear in {fmt:?} dump"
            );
        }
    }

    #[tokio::test]
    async fn memcot_state_null_when_absent() {
        let dir = tempdir().unwrap();
        let dumper = DebugDumper::new(dir.path(), DumpFormat::Json).unwrap();
        let messages = sample_messages();
        let tools = sample_tools();

        let _ = dumper.dump_request(&RequestDebugDump {
            model_name: "test-model",
            messages: &messages,
            tools: &tools,
            provider_request: serde_json::json!({ "model": "test-model", "max_tokens": 1024 }),
            memcot_state: None,
        });

        let payload = read_request_dump(dir.path()).await;
        assert!(
            payload["memcot_state"].is_null(),
            "memcot_state must be null when None"
        );
    }

    /// Smoke test for #6029: `dump_request` returns promptly and the write still lands
    /// afterward. NOT a strict regression guard — a revert to a synchronous single-file write
    /// would typically also complete in 1-3ms on a tmpfs-backed tempdir and would still pass
    /// the 50ms budget below. The write path isn't injectable/mockable here, so a tighter
    /// assertion (e.g. proving the write is dispatched to a *different* thread than the
    /// caller) isn't practical without adding test-only instrumentation to `DebugDumper`. Keep
    /// this as a coarse sanity check plus the file-existence check below, not proof of
    /// non-blocking behavior.
    #[tokio::test]
    async fn dump_request_smoke_returns_promptly_and_write_still_lands() {
        let dir = tempdir().unwrap();
        let dumper = DebugDumper::new(dir.path(), DumpFormat::Json).unwrap();
        let messages = sample_messages();
        let tools = sample_tools();

        let start = std::time::Instant::now();
        let _ = dumper.dump_request(&RequestDebugDump {
            model_name: "test-model",
            messages: &messages,
            tools: &tools,
            provider_request: serde_json::json!({ "model": "test-model", "max_tokens": 1024 }),
            memcot_state: None,
        });
        // Coarse budget only (see doc comment above) — not a strict non-blocking proof.
        assert!(
            start.elapsed() < std::time::Duration::from_millis(50),
            "dump_request must return without waiting on the blocking write"
        );

        // The write still completes shortly afterward (fire-and-forget, not dropped).
        let _ = read_request_dump(dir.path()).await;
    }

    // --- Image redaction (#6306) ---

    #[tokio::test]
    async fn json_dump_redacts_image_bytes_by_default() {
        let dir = tempdir().unwrap();
        let dumper = DebugDumper::new(dir.path(), DumpFormat::Json).unwrap();
        let messages = sample_image_messages();
        let tools = sample_tools();

        let _ = dumper.dump_request(&RequestDebugDump {
            model_name: "test-model",
            messages: &messages,
            tools: &tools,
            provider_request: serde_json::json!({ "model": "test-model", "max_tokens": 1024 }),
            memcot_state: None,
        });

        let payload = read_request_dump(dir.path()).await;
        let dumped_json = payload.to_string();
        assert!(
            !dumped_json.contains(&sample_image_base64()),
            "json dump must not contain raw base64 image bytes by default"
        );
        let image_part = &payload["messages"][1]["parts"][1];
        assert_eq!(image_part["mime_type"], SAMPLE_IMAGE_MIME);
        let marker = image_part["data"].as_str().unwrap();
        assert!(
            marker.starts_with("<redacted image: image/png,"),
            "unexpected marker: {marker}"
        );
        // Sibling text part must be untouched.
        assert_eq!(payload["messages"][1]["parts"][0]["text"], "describe this");
    }

    #[tokio::test]
    async fn raw_dump_redacts_image_bytes_via_fallback_content_blocks() {
        let dir = tempdir().unwrap();
        let dumper = DebugDumper::new(dir.path(), DumpFormat::Raw).unwrap();
        let messages = sample_image_messages();
        let tools = sample_tools();

        // No "messages"/"system" key present, so raw_dump falls back to
        // messages_to_api_value/part_to_block, which mirrors Claude's content-block shape.
        let _ = dumper.dump_request(&RequestDebugDump {
            model_name: "claude-sonnet-test",
            messages: &messages,
            tools: &tools,
            provider_request: serde_json::json!({ "model": "claude-sonnet-test" }),
            memcot_state: None,
        });

        let payload = read_request_dump(dir.path()).await;
        let dumped_json = payload.to_string();
        assert!(
            !dumped_json.contains(&sample_image_base64()),
            "raw dump fallback content blocks must not contain raw base64 image bytes by default"
        );
        let image_block = &payload["messages"][0]["content"][1];
        assert_eq!(image_block["type"], "image");
        assert_eq!(image_block["source"]["media_type"], SAMPLE_IMAGE_MIME);
        let marker = image_block["source"]["data"].as_str().unwrap();
        assert!(
            marker.starts_with("<redacted image: image/png,"),
            "unexpected marker: {marker}"
        );
    }

    #[tokio::test]
    async fn raw_dump_redacts_openai_style_image_url_in_provider_request() {
        let dir = tempdir().unwrap();
        let dumper = DebugDumper::new(dir.path(), DumpFormat::Raw).unwrap();
        let messages = sample_messages();
        let tools = sample_tools();
        let data_url = format!("data:{SAMPLE_IMAGE_MIME};base64,{}", sample_image_base64());

        // provider_request already carries a "messages" key (real OpenAI vision wire format),
        // so raw_dump clones it verbatim without hitting the part_to_block fallback.
        let _ = dumper.dump_request(&RequestDebugDump {
            model_name: "gpt-5-mini",
            messages: &messages,
            tools: &tools,
            provider_request: serde_json::json!({
                "model": "gpt-5-mini",
                "messages": [{
                    "role": "user",
                    "content": [
                        { "type": "text", "text": "describe this" },
                        { "type": "image_url", "image_url": { "url": data_url } },
                    ],
                }],
            }),
            memcot_state: None,
        });

        let payload = read_request_dump(dir.path()).await;
        let dumped_json = payload.to_string();
        assert!(
            !dumped_json.contains(&sample_image_base64()),
            "raw dump of a real provider_request must not contain raw base64 image bytes by default"
        );
        let url = payload["messages"][0]["content"][1]["image_url"]["url"]
            .as_str()
            .unwrap();
        assert!(
            url.starts_with("<redacted image: image/png,"),
            "unexpected marker: {url}"
        );
    }

    #[tokio::test]
    async fn raw_dump_redacts_gemini_and_ollama_style_provider_request_shapes() {
        let dir = tempdir().unwrap();
        let dumper = DebugDumper::new(dir.path(), DumpFormat::Raw).unwrap();
        let messages = sample_messages();
        let tools = sample_tools();
        let b64 = sample_image_base64();

        let _ = dumper.dump_request(&RequestDebugDump {
            model_name: "test-model",
            messages: &messages,
            tools: &tools,
            provider_request: serde_json::json!({
                "model": "test-model",
                // Gemini shape.
                "contents": [{
                    "role": "user",
                    "parts": [{ "inlineData": { "mimeType": SAMPLE_IMAGE_MIME, "data": b64 } }],
                }],
                // Ollama shape (also satisfies the "messages" key so the fallback isn't hit).
                "messages": [{ "role": "user", "content": "hi", "images": [b64] }],
            }),
            memcot_state: None,
        });

        let payload = read_request_dump(dir.path()).await;
        let dumped_json = payload.to_string();
        assert!(
            !dumped_json.contains(&sample_image_base64()),
            "raw dump must redact both Gemini inlineData and Ollama images-array shapes"
        );
        let gemini_data = payload["contents"][0]["parts"][0]["inlineData"]["data"]
            .as_str()
            .unwrap();
        assert!(gemini_data.starts_with("<redacted image:"));
        let ollama_image = payload["messages"][0]["images"][0].as_str().unwrap();
        assert!(ollama_image.starts_with("<redacted image:"));
    }

    #[tokio::test]
    async fn include_raw_images_opt_in_preserves_full_bytes() {
        for fmt in [DumpFormat::Json, DumpFormat::Raw] {
            let dir = tempdir().unwrap();
            let dumper = DebugDumper::new(dir.path(), fmt)
                .unwrap()
                .with_include_raw_images(true);
            let messages = sample_image_messages();
            let tools = sample_tools();

            let _ = dumper.dump_request(&RequestDebugDump {
                model_name: "test-model",
                messages: &messages,
                tools: &tools,
                provider_request: serde_json::json!({ "model": "test-model" }),
                memcot_state: None,
            });

            let payload = read_request_dump(dir.path()).await;
            assert!(
                payload.to_string().contains(&sample_image_base64()),
                "with include_raw_images=true, {fmt:?} dump must contain the full base64 image bytes"
            );
        }
    }

    /// Cross-crate regression guard (#6306 critic finding S1): the 6 tests above all hand-author
    /// JSON matching `redact_image_payloads`'s own assumed shapes, which only proves the
    /// redactor redacts shapes it already knows about — a real provider renaming/restructuring
    /// its image field (or a new provider) could silently reopen the leak with every other test
    /// still green. This drives the *actual* `LlmProvider::debug_request_json` implementation
    /// of each real provider — the exact code path `raw_dump`'s primary branch consumes in
    /// production via `RequestDebugDump::provider_request` — through the real `dump_request`
    /// write path, and asserts no raw base64 image data survives.
    #[tokio::test]
    async fn raw_dump_redacts_real_provider_debug_request_json_for_every_provider() {
        use zeph_llm::provider::LlmProvider;

        let messages = sample_image_messages();
        let b64 = sample_image_base64();

        let claude = zeph_llm::claude::ClaudeProvider::new(
            "test-key".to_owned(),
            "claude-sonnet-test".to_owned(),
            4096,
        );
        let openai = zeph_llm::openai::OpenAiProvider::new(zeph_llm::openai::OpenAiConfig {
            api_key: "test-key".to_owned(),
            base_url: "https://api.openai.com/v1".to_owned(),
            model: "gpt-5-mini".to_owned(),
            max_tokens: 4096,
            embedding_model: None,
            reasoning_effort: None,
            context_window: None,
            completion_tokens_param: None,
        });
        let gemini = zeph_llm::gemini::GeminiProvider::new(
            "test-key".to_owned(),
            "gemini-2.5-flash".to_owned(),
            4096,
        );
        let ollama = zeph_llm::ollama::OllamaProvider::new(
            "http://localhost:11434",
            "llava".to_owned(),
            "nomic-embed-text".to_owned(),
        );

        let providers: Vec<(&str, serde_json::Value)> = vec![
            ("claude", claude.debug_request_json(&messages, &[], false)),
            ("openai", openai.debug_request_json(&messages, &[], false)),
            ("gemini", gemini.debug_request_json(&messages, &[], false)),
            ("ollama", ollama.debug_request_json(&messages, &[], false)),
        ];

        for (name, provider_request) in providers {
            assert!(
                provider_request.to_string().contains(&b64),
                "test setup sanity check: {name}'s debug_request_json must actually contain \
                 the raw base64 image bytes before redaction (otherwise this test proves \
                 nothing)"
            );

            let dir = tempdir().unwrap();
            let dumper = DebugDumper::new(dir.path(), DumpFormat::Raw).unwrap();
            let _ = dumper.dump_request(&RequestDebugDump {
                model_name: name,
                messages: &messages,
                tools: &[],
                provider_request,
                memcot_state: None,
            });

            let payload = read_request_dump(dir.path()).await;
            assert!(
                !payload.to_string().contains(&b64),
                "{name}'s real debug_request_json output must not contain raw base64 image \
                 bytes after passing through the real dump_request/raw_dump redaction path"
            );
        }
    }

    #[test]
    fn redact_image_payloads_leaves_non_image_values_untouched() {
        let mut value = serde_json::json!({
            "model": "test-model",
            "messages": [{
                "role": "user",
                "content": [
                    { "type": "text", "text": "hello world" },
                    { "type": "tool_use", "id": "t1", "name": "read_file", "input": { "path": "a.rs" } },
                    { "type": "tool_result", "tool_use_id": "t1", "content": "file contents", "is_error": false },
                ],
            }],
            "temperature": 0.5,
        });
        let before = value.clone();
        redact_image_payloads(&mut value);
        assert_eq!(
            value, before,
            "non-image JSON must be unchanged by redaction"
        );
    }
}
