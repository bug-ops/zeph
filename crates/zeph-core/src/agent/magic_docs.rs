// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `MagicDocs` auto-maintained markdown (#2702).
//!
//! Detects files containing `# MAGIC DOC:` when read via file tools and registers
//! them in a per-session registry. After every Nth tool-call turn, a background
//! task updates each registered doc via a constrained LLM subagent.

use std::collections::HashMap;
use std::path::PathBuf;

use zeph_llm::provider::{LlmProvider as _, Message, MessagePart, Role};

use crate::channel::Channel;

/// Marker header that identifies a file as auto-maintained.
const MAGIC_DOC_HEADER: &str = "# MAGIC DOC:";

/// Tool names that perform file reads (case-insensitive).
const FILE_READ_TOOLS: &[&str] = &["read", "file_read", "cat", "view", "open"];

/// Per-session `MagicDocs` state.
pub(crate) struct MagicDocsState {
    /// Registered magic doc paths → turn number of last update.
    pub(crate) registered: HashMap<PathBuf, u32>,
    /// Pending background update handle.
    pub(crate) pending: Option<tokio::task::JoinHandle<()>>,
}

impl MagicDocsState {
    pub(super) fn new() -> Self {
        Self {
            registered: HashMap::new(),
            pending: None,
        }
    }
}

impl<C: Channel> super::Agent<C> {
    /// Detect `# MAGIC DOC:` headers in `ToolOutput` parts and register their paths.
    ///
    /// Call this after pushing an assistant message that may contain `ToolOutput` parts.
    /// No-op when `MagicDocs` is disabled.
    pub(super) fn detect_magic_docs_in_messages(&mut self) {
        if !self.memory_state.magic_docs_config.enabled {
            return;
        }

        // Scan the last assistant message for ToolOutput parts from file-read tools.
        let Some(last_msg) = self.msg.messages.last() else {
            return;
        };
        if last_msg.role != Role::Assistant {
            return;
        }

        // Walk all messages looking for ToolUse → ToolOutput pairs where ToolOutput has magic header.
        self.scan_messages_for_magic_docs();
    }

    fn scan_messages_for_magic_docs(&mut self) {
        // Walk assistant messages: find ToolUse parts whose paired ToolOutput contains MAGIC_DOC_HEADER.
        // Extract the file path from the ToolUse input.
        let turn = u32::try_from(self.sidequest.turn_counter).unwrap_or(u32::MAX);

        for msg in &self.msg.messages {
            if msg.role != Role::Assistant {
                continue;
            }
            // Pair ToolUse and ToolOutput parts by position.
            let mut last_tool_use_name: Option<&str> = None;
            let mut last_tool_use_path: Option<String> = None;

            for part in &msg.parts {
                match part {
                    MessagePart::ToolUse { name, input, .. } => {
                        last_tool_use_name = Some(name.as_str());
                        last_tool_use_path = extract_file_path_from_input(input);
                    }
                    MessagePart::ToolOutput {
                        tool_name, body, ..
                    } => {
                        let tname = last_tool_use_name.unwrap_or(tool_name.as_str());
                        if is_file_read_tool(tname)
                            && body.contains(MAGIC_DOC_HEADER)
                            && let Some(ref path_str) = last_tool_use_path
                        {
                            let path = PathBuf::from(path_str);
                            self.magic_docs_state
                                .registered
                                .entry(path.clone())
                                .or_insert(turn);
                            tracing::debug!(
                                path = %path.display(),
                                "magic_docs: registered doc"
                            );
                        }
                        last_tool_use_name = None;
                        last_tool_use_path = None;
                    }
                    _ => {}
                }
            }
        }
    }

    /// If conditions are met, spawn a background task to update registered magic docs.
    ///
    /// Spawns a `tokio::task` that runs concurrently with the next user turn.
    /// No-op when `MagicDocs` is disabled, no docs are registered, or update is not due.
    pub(super) fn maybe_update_magic_docs(&mut self) {
        let cfg = self.memory_state.magic_docs_config.clone();
        if !cfg.enabled || self.magic_docs_state.registered.is_empty() {
            return;
        }

        // Await any previous pending update before spawning another.
        if let Some(handle) = self.magic_docs_state.pending.take()
            && !handle.is_finished()
        {
            tracing::debug!("magic_docs: previous update still running, skipping this turn");
            return;
        }

        let current_turn = u32::try_from(self.sidequest.turn_counter).unwrap_or(u32::MAX);
        let due_paths: Vec<PathBuf> = self
            .magic_docs_state
            .registered
            .iter()
            .filter(|(_, last_turn)| {
                current_turn.saturating_sub(**last_turn) >= cfg.min_turns_between_updates
            })
            .map(|(p, _)| p.clone())
            .collect();

        if due_paths.is_empty() {
            return;
        }

        // Resolve the update provider.
        let provider = if cfg.update_provider.is_empty() {
            self.provider.clone()
        } else if let (Some(entry), Some(snapshot)) = (
            self.providers
                .provider_pool
                .iter()
                .find(|e| e.name.as_deref() == Some(cfg.update_provider.as_str())),
            self.providers.provider_config_snapshot.as_ref(),
        ) {
            crate::bootstrap::provider::build_provider_for_switch(entry, snapshot).unwrap_or_else(
                |e| {
                    tracing::warn!(
                        provider = cfg.update_provider.as_str(),
                        error = %e,
                        "magic_docs: failed to build update_provider, falling back"
                    );
                    self.provider.clone()
                },
            )
        } else {
            self.provider.clone()
        };

        let max_iterations = cfg.max_iterations;
        tracing::info!(
            docs = due_paths.len(),
            "magic_docs: spawning background update"
        );
        let _ = self
            .session
            .status_tx
            .as_ref()
            .map(|tx| tx.send(format!("Updating {} magic doc(s)…", due_paths.len())));

        let handle = tokio::spawn(async move {
            for path in &due_paths {
                if let Err(e) = update_magic_doc(path, &provider, usize::from(max_iterations)).await
                {
                    tracing::warn!(
                        path = %path.display(),
                        error = %e,
                        "magic_docs: update failed"
                    );
                }
            }
        });

        // Mark all due paths as updated (due_paths moved into spawn — use registered keys).
        for path in self.magic_docs_state.registered.values_mut() {
            if current_turn.saturating_sub(*path) >= cfg.min_turns_between_updates {
                *path = current_turn;
            }
        }

        self.magic_docs_state.pending = Some(handle);
    }
}

/// Build a short LLM prompt asking the agent to refresh the magic doc.
fn build_update_prompt(path: &std::path::Path, content: &str) -> String {
    format!(
        "You are maintaining an auto-updated documentation file at `{}`.\n\n\
         The file currently contains:\n\n```\n{}\n```\n\n\
         Based on the content above and any knowledge you have, update the file \
         to keep it accurate and current. Preserve the `# MAGIC DOC:` header line. \
         Write the updated content to the file using the appropriate edit tool.",
        path.display(),
        content
    )
}

/// Run a single magic doc update using a minimal LLM call.
///
/// For MVP: reads the file content, calls the LLM to produce an updated version,
/// and writes it back. Does not spawn a full sub-agent — uses a single LLM call.
async fn update_magic_doc(
    path: &std::path::Path,
    provider: &zeph_llm::any::AnyProvider,
    _max_iterations: usize,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let content = tokio::fs::read_to_string(path).await?;

    if !content.contains(MAGIC_DOC_HEADER) {
        return Ok(());
    }

    let prompt = build_update_prompt(path, &content);
    let messages = vec![Message {
        role: Role::User,
        content: prompt.clone(),
        parts: vec![MessagePart::Text { text: prompt }],
        metadata: zeph_llm::provider::MessageMetadata::default(),
    }];

    let updated = provider.chat(&messages).await?;

    if !updated.is_empty() && updated.contains(MAGIC_DOC_HEADER) {
        tokio::fs::write(path, &updated).await?;
        tracing::info!(path = %path.display(), "magic_docs: doc updated");
    }

    Ok(())
}

fn is_file_read_tool(name: &str) -> bool {
    let lower = name.to_lowercase();
    FILE_READ_TOOLS.contains(&lower.as_str())
}

fn extract_file_path_from_input(input: &serde_json::Value) -> Option<String> {
    // Common field names used by file-read tools.
    for key in &["file_path", "path", "filename", "file"] {
        if let Some(v) = input.get(key).and_then(|v| v.as_str()) {
            return Some(v.to_owned());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_file_read_tool_case_insensitive() {
        assert!(is_file_read_tool("Read"));
        assert!(is_file_read_tool("FILE_READ"));
        assert!(!is_file_read_tool("bash"));
    }

    #[test]
    fn extract_file_path_from_common_inputs() {
        let input = serde_json::json!({"file_path": "/tmp/test.md"});
        assert_eq!(
            extract_file_path_from_input(&input),
            Some("/tmp/test.md".into())
        );
        let input2 = serde_json::json!({"path": "/foo/bar.md"});
        assert_eq!(
            extract_file_path_from_input(&input2),
            Some("/foo/bar.md".into())
        );
        let input3 = serde_json::json!({"cmd": "ls"});
        assert!(extract_file_path_from_input(&input3).is_none());
    }

    #[test]
    fn build_update_prompt_contains_magic_doc_header() {
        let path = std::path::Path::new("/tmp/test.md");
        let content = format!("{MAGIC_DOC_HEADER} My Doc\nContent here.");
        let prompt = build_update_prompt(path, &content);
        assert!(prompt.contains(MAGIC_DOC_HEADER));
        assert!(prompt.contains("/tmp/test.md"));
    }
}
