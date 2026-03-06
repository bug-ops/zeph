// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Debug dump writer for a single agent session.
//!
//! When active, every LLM request/response pair and raw tool output is written to
//! numbered files in a timestamped subdirectory of the configured output directory.
//! Intended for context debugging only — do not use in production.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use serde::{Deserialize, Serialize};
use zeph_llm::provider::{Message, MessagePart, Role};

/// Output format for debug dump files.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DumpFormat {
    /// Write LLM requests as pretty-printed JSON (`{id}-request.json`).
    #[default]
    Json,
    /// Write LLM requests as human-readable Markdown (`{id}-request.md`).
    Md,
}

pub struct DebugDumper {
    dir: PathBuf,
    counter: AtomicU32,
    format: DumpFormat,
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
    pub fn dump_request(&self, messages: &[Message]) -> u32 {
        let id = self.next_id();
        match self.format {
            DumpFormat::Json => {
                let json = serde_json::to_string_pretty(messages)
                    .unwrap_or_else(|e| format!("serialization error: {e}"));
                self.write(&format!("{id:04}-request.json"), json.as_bytes());
            }
            DumpFormat::Md => {
                let md = messages_to_markdown(messages);
                self.write(&format!("{id:04}-request.md"), md.as_bytes());
            }
        }
        id
    }

    /// Dump the LLM response corresponding to a prior [`dump_request`] call.
    pub fn dump_response(&self, id: u32, response: &str) {
        self.write(&format!("{id:04}-response.txt"), response.as_bytes());
    }

    /// Dump raw tool output before any truncation or summarization.
    pub fn dump_tool_output(&self, tool_name: &str, output: &str) {
        let id = self.next_id();
        let safe_name: String = tool_name
            .chars()
            .map(|c| {
                if c.is_alphanumeric() || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        self.write(&format!("{id:04}-tool-{safe_name}.txt"), output.as_bytes());
    }
}

fn role_label(role: Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
    }
}

fn messages_to_markdown(messages: &[Message]) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    for (i, msg) in messages.iter().enumerate() {
        let _ = write!(out, "## {} · {}\n\n", i + 1, role_label(msg.role));
        if msg.parts.is_empty() {
            // Legacy flat-content message
            out.push_str(&msg.content);
            out.push_str("\n\n");
        } else {
            for part in &msg.parts {
                render_part(&mut out, part);
            }
        }
        out.push_str("---\n\n");
    }
    out
}

fn render_part(out: &mut String, part: &MessagePart) {
    use std::fmt::Write as _;
    match part {
        MessagePart::Text { text } => {
            out.push_str(text);
            out.push_str("\n\n");
        }
        MessagePart::Recall { text } => {
            out.push_str("### recall\n\n");
            out.push_str(text);
            out.push_str("\n\n");
        }
        MessagePart::CodeContext { text } => {
            out.push_str("### code-context\n\n");
            out.push_str("```\n");
            out.push_str(text);
            out.push_str("\n```\n\n");
        }
        MessagePart::Summary { text } => {
            out.push_str("### summary\n\n");
            out.push_str(text);
            out.push_str("\n\n");
        }
        MessagePart::CrossSession { text } => {
            out.push_str("### cross-session\n\n");
            out.push_str(text);
            out.push_str("\n\n");
        }
        MessagePart::ToolOutput { tool_name, body, .. } => {
            let _ = write!(out, "### tool-output: {tool_name}\n\n```\n");
            out.push_str(body);
            out.push_str("\n```\n\n");
        }
        MessagePart::ToolUse { id, name, input } => {
            let input_str = serde_json::to_string_pretty(input)
                .unwrap_or_else(|_| input.to_string());
            let _ = write!(out, "### tool-use: {name} (id: {id})\n\n```json\n");
            out.push_str(&input_str);
            out.push_str("\n```\n\n");
        }
        MessagePart::ToolResult { tool_use_id, content, is_error } => {
            let tag = if *is_error { "tool-result [error]" } else { "tool-result" };
            let _ = write!(out, "### {tag} (id: {tool_use_id})\n\n```\n");
            out.push_str(content);
            out.push_str("\n```\n\n");
        }
        MessagePart::ThinkingBlock { thinking, .. } => {
            out.push_str("### thinking\n\n");
            out.push_str(thinking);
            out.push_str("\n\n");
        }
        MessagePart::RedactedThinkingBlock { .. } => {
            out.push_str("### thinking (redacted)\n\n");
        }
        MessagePart::Image(img) => {
            let _ = write!(out, "### image ({})\n\n", img.mime_type);
        }
    }
}
