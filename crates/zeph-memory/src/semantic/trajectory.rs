// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Trajectory-informed memory extraction (#2498).
//!
//! After each agent turn containing tool calls, a fast LLM provider analyzes the turn
//! and produces procedural (reusable how-to patterns) and episodic (one-off event) entries.
//! Entries are stored per-conversation so concurrent sessions do not interfere (critic S1).
//!
//! Extraction is always fire-and-forget (caller uses `tokio::spawn`) — no latency added to
//! the response path (critic M3).

use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::time::timeout;
use zeph_llm::any::AnyProvider;
use zeph_llm::provider::{Message, Role};

use crate::error::MemoryError;

const EXTRACTION_SYSTEM_PROMPT: &str = "\
You are a trajectory memory extractor. Given messages from an agent turn that included tool \
calls, extract reusable patterns and notable events.

Rules:
1. Classify each entry as 'procedural' (a reusable how-to pattern — general technique) or \
   'episodic' (a one-off event — specific occurrence).
2. Focus on the intent (goal), outcome (result), and tools used.
3. Confidence: 0.8-1.0 for clear outcomes, 0.4-0.7 for ambiguous ones.
4. Keep intent and outcome concise (one sentence each).
5. Return empty array if no meaningful entries can be extracted.

Output JSON array:
[
  {
    \"kind\": \"procedural|episodic\",
    \"intent\": \"what the agent was trying to accomplish\",
    \"outcome\": \"what actually happened\",
    \"tools_used\": [\"tool_name\", ...],
    \"confidence\": 0.0-1.0
  }
]";

/// A single extracted trajectory entry (in-memory, before storage).
#[derive(Debug, Clone)]
pub struct TrajectoryEntry {
    pub kind: String,
    pub intent: String,
    pub outcome: String,
    pub tools_used: Vec<String>,
    pub confidence: f64,
}

/// Configuration for trajectory extraction.
pub struct TrajectoryExtractionConfig {
    pub enabled: bool,
    /// Maximum messages fed to the LLM per extraction pass.
    pub max_messages: usize,
    /// LLM timeout in seconds.
    pub extraction_timeout_secs: u64,
}

#[derive(Debug, Deserialize, Serialize)]
struct RawEntry {
    kind: String,
    intent: String,
    outcome: String,
    #[serde(default)]
    tools_used: Vec<String>,
    confidence: f64,
}

/// Extract trajectory entries from a turn's messages.
///
/// Returns the extracted entries. Parse failures are logged and treated as zero entries.
///
/// # Errors
///
/// Returns an error only for transport-level LLM failures.
pub async fn extract_trajectory_entries(
    provider: &AnyProvider,
    messages: &[Message],
    config: &TrajectoryExtractionConfig,
) -> Result<Vec<TrajectoryEntry>, MemoryError> {
    if !config.enabled || messages.is_empty() {
        return Ok(Vec::new());
    }

    let messages_to_send: Vec<&Message> = messages
        .iter()
        .rev()
        .take(config.max_messages)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();

    let user_prompt = build_extraction_prompt(&messages_to_send);

    let llm_messages = [
        Message::from_legacy(Role::System, EXTRACTION_SYSTEM_PROMPT),
        Message::from_legacy(Role::User, user_prompt),
    ];

    let extraction_timeout = Duration::from_secs(config.extraction_timeout_secs);
    let response = match timeout(
        extraction_timeout,
        provider.chat_with_named_provider("trajectory", &llm_messages),
    )
    .await
    {
        Ok(Ok(text)) => text,
        Ok(Err(e)) => return Err(MemoryError::Llm(e)),
        Err(_) => {
            tracing::warn!(
                "trajectory extraction timed out after {}s",
                config.extraction_timeout_secs
            );
            return Ok(Vec::new());
        }
    };

    let entries = parse_extraction_response(&response);
    Ok(entries)
}

fn build_extraction_prompt(messages: &[&Message]) -> String {
    let mut prompt = String::from("Agent turn messages:\n");
    for (i, msg) in messages.iter().enumerate() {
        use std::fmt::Write as _;
        let role = format!("{:?}", msg.role);
        let _ = writeln!(prompt, "[{}] {}: {}", i + 1, role, msg.content);
    }
    prompt.push_str("\nExtract trajectory entries as JSON array.");
    prompt
}

fn parse_extraction_response(response: &str) -> Vec<TrajectoryEntry> {
    let raw: Vec<RawEntry> = if let Ok(v) = serde_json::from_str(response) {
        v
    } else if let (Some(start), Some(end)) = (response.find('['), response.rfind(']'))
        && end > start
    {
        serde_json::from_str(&response[start..=end]).unwrap_or_default()
    } else {
        tracing::warn!(
            "trajectory extraction: failed to parse response (len={}): {:.200}",
            response.len(),
            response
        );
        return Vec::new();
    };

    raw.into_iter()
        .filter(|e| !e.intent.is_empty() && !e.outcome.is_empty())
        .filter(|e| matches!(e.kind.as_str(), "procedural" | "episodic"))
        .map(|e| TrajectoryEntry {
            kind: e.kind,
            intent: e.intent,
            outcome: e.outcome,
            tools_used: e.tools_used,
            confidence: e.confidence.clamp(0.0, 1.0),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_direct_json_array() {
        let json = r#"[{"kind":"procedural","intent":"read a file","outcome":"file read ok","tools_used":["read_file"],"confidence":0.9}]"#;
        let entries = parse_extraction_response(json);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].kind, "procedural");
        assert_eq!(entries[0].intent, "read a file");
        assert_eq!(entries[0].tools_used, vec!["read_file"]);
        assert!((entries[0].confidence - 0.9).abs() < 1e-9);
    }

    #[test]
    fn parse_json_embedded_in_prose() {
        let response = "Here you go:\n[{\"kind\":\"episodic\",\"intent\":\"fixed a bug\",\"outcome\":\"patch applied\",\"tools_used\":[],\"confidence\":0.8}]\nDone.";
        let entries = parse_extraction_response(response);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].kind, "episodic");
    }

    #[test]
    fn parse_empty_array() {
        let entries = parse_extraction_response("[]");
        assert!(entries.is_empty());
    }

    #[test]
    fn parse_invalid_json_returns_empty() {
        let entries = parse_extraction_response("not json");
        assert!(entries.is_empty());
    }

    #[test]
    fn parse_filters_unknown_kind() {
        let json =
            r#"[{"kind":"unknown","intent":"x","outcome":"y","tools_used":[],"confidence":0.5}]"#;
        let entries = parse_extraction_response(json);
        assert!(entries.is_empty(), "unknown kind must be filtered out");
    }

    #[test]
    fn parse_clamps_confidence() {
        let json = r#"[{"kind":"procedural","intent":"x","outcome":"y","tools_used":[],"confidence":1.5}]"#;
        let entries = parse_extraction_response(json);
        assert_eq!(entries.len(), 1);
        assert!(
            (entries[0].confidence - 1.0).abs() < 1e-9,
            "confidence must be clamped to 1.0"
        );
    }

    #[test]
    fn parse_filters_empty_intent_or_outcome() {
        let json =
            r#"[{"kind":"procedural","intent":"","outcome":"y","tools_used":[],"confidence":0.8}]"#;
        let entries = parse_extraction_response(json);
        assert!(entries.is_empty(), "empty intent must be filtered");
    }
}
