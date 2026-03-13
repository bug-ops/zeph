// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Debug dump writer for a single agent session.
//!
//! When active, every LLM request/response pair and raw tool output is written to
//! numbered files in a timestamped subdirectory of the configured output directory.
//! Intended for context debugging only — do not use in production.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use zeph_llm::provider::{Message, MessagePart, Role, ToolDefinition};

/// Output format for debug dump files.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DumpFormat {
    /// Write LLM requests as pretty-printed internal zeph-llm JSON (`{id}-request.json`).
    #[default]
    Json,
    /// Write LLM requests as the actual API payload sent to the provider (`{id}-request.json`):
    /// system extracted, `agent_invisible` messages filtered, parts rendered as content blocks.
    Raw,
}

pub struct DebugDumper {
    dir: PathBuf,
    counter: AtomicU32,
    format: DumpFormat,
}

pub struct RequestDebugDump<'a> {
    pub model_name: &'a str,
    pub messages: &'a [Message],
    pub tools: &'a [ToolDefinition],
    pub provider_request: serde_json::Value,
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
            counter: AtomicU32::new(0),
            format,
        })
    }

    /// Return the session dump directory.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn next_id(&self) -> u32 {
        self.counter.fetch_add(1, Ordering::Relaxed)
    }

    fn write(&self, filename: &str, content: &[u8]) {
        let path = self.dir.join(filename);
        if let Err(e) = std::fs::write(&path, content) {
            tracing::warn!(path = %path.display(), error = %e, "debug dump write failed");
        }
    }

    /// Dump the messages about to be sent to the LLM.
    ///
    /// Returns an ID that must be passed to [`dump_response`] to correlate request and response.
    pub fn dump_request(&self, request: &RequestDebugDump<'_>) -> u32 {
        let id = self.next_id();
        let json = match self.format {
            DumpFormat::Json => json_dump(request),
            DumpFormat::Raw => raw_dump(request),
        };
        self.write(&format!("{id:04}-request.json"), json.as_bytes());
        id
    }

    /// Dump the LLM response corresponding to a prior [`dump_request`] call.
    pub fn dump_response(&self, id: u32, response: &str) {
        self.write(&format!("{id:04}-response.txt"), response.as_bytes());
    }

    /// Dump raw tool output before any truncation or summarization.
    pub fn dump_tool_output(&self, tool_name: &str, output: &str) {
        let id = self.next_id();
        let safe_name = sanitize_dump_name(tool_name);
        self.write(&format!("{id:04}-tool-{safe_name}.txt"), output.as_bytes());
    }

    /// Dump a tool error with error classification for debugging transient/permanent failures.
    pub fn dump_tool_error(&self, tool_name: &str, error: &zeph_tools::ToolError) {
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

fn json_dump(request: &RequestDebugDump<'_>) -> String {
    let payload = serde_json::json!({
        "model": extract_model(&request.provider_request, request.model_name),
        "max_tokens": extract_max_tokens(&request.provider_request),
        "messages": serde_json::to_value(request.messages)
            .unwrap_or(serde_json::Value::Array(vec![])),
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
    });
    serde_json::to_string_pretty(&payload).unwrap_or_else(|e| format!("serialization error: {e}"))
}

fn raw_dump(request: &RequestDebugDump<'_>) -> String {
    let mut payload = if request.provider_request.is_object() {
        request.provider_request.clone()
    } else {
        serde_json::json!({})
    };
    let generic = messages_to_api_value(request.messages);
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
        if !obj.contains_key("messages")
            && !obj.contains_key("system")
            && let Some(generic_obj) = generic.as_object()
        {
            for (key, value) in generic_obj {
                obj.insert(key.clone(), value.clone());
            }
        }
    }
    serde_json::to_string_pretty(&payload).unwrap_or_else(|e| format!("serialization error: {e}"))
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
        .filter(|m| m.metadata.agent_visible && m.role == Role::System)
        .map(zeph_llm::provider::Message::to_llm_content)
        .collect::<Vec<_>>()
        .join("\n\n");

    let chat: Vec<serde_json::Value> = messages
        .iter()
        .filter(|m| m.metadata.agent_visible && m.role != Role::System)
        .filter_map(|m| {
            let role = match m.role {
                Role::User => "user",
                Role::Assistant => "assistant",
                Role::System => return None,
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
                format!("[tool output: {tool_name}] (pruned)")
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
        MessagePart::ThinkingBlock { .. } | MessagePart::RedactedThinkingBlock { .. } => None,
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

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
        }]
    }

    fn read_request_dump(dir: &Path) -> serde_json::Value {
        let session = std::fs::read_dir(dir)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        serde_json::from_str(&std::fs::read_to_string(session.join("0000-request.json")).unwrap())
            .unwrap()
    }

    #[test]
    fn json_dump_request_includes_request_metadata() {
        let dir = tempdir().unwrap();
        let dumper = DebugDumper::new(dir.path(), DumpFormat::Json).unwrap();
        let messages = sample_messages();
        let tools = sample_tools();

        dumper.dump_request(&RequestDebugDump {
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
        });

        let payload = read_request_dump(dir.path());
        assert_eq!(payload["model"], "claude-sonnet-test");
        assert_eq!(payload["max_tokens"], 4096);
        assert_eq!(payload["tools"][0]["name"], "read_file");
        assert_eq!(payload["temperature"], 0.7);
        assert_eq!(payload["cache_control"]["type"], "ephemeral");
        assert_eq!(payload["messages"][1]["content"], "hello");
    }

    #[test]
    fn raw_dump_request_includes_request_metadata() {
        let dir = tempdir().unwrap();
        let dumper = DebugDumper::new(dir.path(), DumpFormat::Raw).unwrap();
        let messages = sample_messages();
        let tools = sample_tools();

        dumper.dump_request(&RequestDebugDump {
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
        });

        let payload = read_request_dump(dir.path());
        assert_eq!(payload["model"], "gpt-5-mini");
        assert_eq!(payload["max_tokens"], 2048);
        assert_eq!(payload["tools"][0]["function"]["name"], "read_file");
        assert_eq!(payload["temperature"], 0.3);
        assert_eq!(payload["messages"][0]["content"], "hello");
    }
}
