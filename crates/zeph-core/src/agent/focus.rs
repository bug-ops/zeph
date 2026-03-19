// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Focus Agent: active context compression via explicit bracketed exploration phases (#1850).
//!
//! Two native tools — `start_focus` and `complete_focus` — let the LLM explicitly bracket
//! exploration phases. On `complete_focus`, messages since the checkpoint are summarised,
//! the summary is appended to the pinned Knowledge block, and the bracketed messages are
//! removed from the conversation history.

#[cfg(feature = "context-compression")]
use uuid::Uuid;
#[cfg(feature = "context-compression")]
use zeph_llm::provider::{Message, MessageMetadata, Role, ToolDefinition};

use crate::config::FocusConfig;

/// Build tool definitions for `start_focus` and `complete_focus` (#1850).
///
/// These are injected into the tool list when `focus.enabled = true`.
#[cfg(feature = "context-compression")]
pub(crate) fn focus_tool_definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            name: "start_focus".into(),
            description: "Start a focused exploration phase. Use before diving into a subtask \
                          (e.g., reading many files, running experiments). The conversation since \
                          this checkpoint will be summarized and compressed when you call \
                          complete_focus.\n\nParameters: scope (string, required) — a concise \
                          description of what you are about to explore.\nReturns: confirmation \
                          with a checkpoint marker ID.\nExample: {\"scope\": \"reading auth \
                          middleware files\"}"
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "scope": {
                        "type": "string",
                        "description": "Concise label for what you are about to explore."
                    }
                },
                "required": ["scope"]
            }),
        },
        ToolDefinition {
            name: "complete_focus".into(),
            description: "Complete the active focus phase. Summarizes the conversation since the \
                          last start_focus checkpoint, appends the summary to the pinned Knowledge \
                          block, and removes the bracketed messages from context.\n\nParameters: \
                          summary (string, required) — what you learned or accomplished.\nReturns: \
                          confirmation or error if no focus session is active.\nExample: \
                          {\"summary\": \"Found that auth.rs uses JWT with RS256. Key file: \
                          src/auth.rs:42.\"}"
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "summary": {
                        "type": "string",
                        "description": "What you learned or accomplished during this focus phase."
                    }
                },
                "required": ["summary"]
            }),
        },
    ]
}

// Used by build_knowledge_message (context-compression feature).
#[cfg_attr(not(feature = "context-compression"), allow(dead_code))]
pub(crate) const KNOWLEDGE_BLOCK_PREFIX: &str = "[knowledge]\n";

/// Tracks the state of the active focus session.
// Fields and methods below are consumed by context-compression feature paths.
#[cfg_attr(not(feature = "context-compression"), allow(dead_code))]
pub(crate) struct FocusState {
    pub(crate) config: FocusConfig,
    /// Accumulated knowledge entries from all completed focus sessions.
    pub(crate) knowledge_blocks: Vec<String>,
    /// Marker UUID written into the checkpoint message's `focus_marker_id` field.
    /// `None` = no active focus session.
    #[cfg(feature = "context-compression")]
    pub(crate) active_marker: Option<Uuid>,
    /// Human-readable scope label provided by the LLM via `start_focus`.
    pub(crate) active_scope: Option<String>,
    /// Turns elapsed since the last `complete_focus` call (or session start).
    pub(crate) turns_since_focus: usize,
    /// Turns elapsed since the last reminder was injected.
    pub(crate) turns_since_reminder: usize,
}

#[cfg_attr(not(feature = "context-compression"), allow(dead_code))]
impl FocusState {
    pub(crate) fn new(config: FocusConfig) -> Self {
        Self {
            config,
            knowledge_blocks: Vec::new(),
            #[cfg(feature = "context-compression")]
            active_marker: None,
            active_scope: None,
            turns_since_focus: 0,
            turns_since_reminder: 0,
        }
    }

    /// Returns `true` if a focus session is currently active.
    #[cfg_attr(not(feature = "context-compression"), allow(clippy::unused_self))]
    pub(crate) fn is_active(&self) -> bool {
        #[cfg(feature = "context-compression")]
        {
            self.active_marker.is_some()
        }
        #[cfg(not(feature = "context-compression"))]
        {
            false
        }
    }

    /// Increment turn counters. Called at the start of each user-message turn.
    pub(crate) fn tick(&mut self) {
        self.turns_since_focus = self.turns_since_focus.saturating_add(1);
        self.turns_since_reminder = self.turns_since_reminder.saturating_add(1);
    }

    /// Append a completed focus summary to the accumulated knowledge.
    ///
    /// Enforces `max_knowledge_tokens` by evicting the oldest entry (FIFO) when the
    /// serialized block would exceed the cap (SEC-CC-01 fix).
    ///
    /// Returns `true` if the Knowledge block was actually updated.
    pub(crate) fn append_knowledge(&mut self, summary: String) -> bool {
        if summary.is_empty() {
            return false;
        }
        self.knowledge_blocks.push(summary);
        // Enforce the token cap by evicting from the front (oldest-first) until within budget.
        // Each block is approximated at 4 chars/token (fast, no external dep needed here).
        while self.knowledge_blocks.len() > 1 {
            let total_chars: usize = self.knowledge_blocks.iter().map(String::len).sum();
            #[allow(clippy::integer_division)]
            let approx_tokens = total_chars / 4;
            if approx_tokens <= self.config.max_knowledge_tokens {
                break;
            }
            self.knowledge_blocks.remove(0);
        }
        true
    }

    /// Build the pinned Knowledge block message, or `None` if there is no knowledge yet.
    #[cfg(feature = "context-compression")]
    pub(crate) fn build_knowledge_message(&self) -> Option<Message> {
        if self.knowledge_blocks.is_empty() {
            return None;
        }

        let mut body = String::from(KNOWLEDGE_BLOCK_PREFIX);
        for (i, block) in self.knowledge_blocks.iter().enumerate() {
            if i > 0 {
                body.push('\n');
            }
            body.push_str("## Focus summary ");
            body.push_str(&(i + 1).to_string());
            body.push('\n');
            body.push_str(block);
        }

        Some(Message {
            role: Role::System,
            content: body,
            parts: vec![],
            metadata: MessageMetadata::focus_pinned(),
        })
    }

    /// Start a new focus session. Returns the marker UUID embedded in the checkpoint message.
    #[cfg(feature = "context-compression")]
    pub(crate) fn start(&mut self, scope: String) -> Uuid {
        let marker = Uuid::new_v4();
        self.active_marker = Some(marker);
        self.active_scope = Some(scope);
        marker
    }

    /// Complete the active session. Clears active marker and scope; resets reminder counters.
    /// The caller is responsible for appending the summary to `knowledge_blocks`.
    #[cfg(feature = "context-compression")]
    pub(crate) fn complete(&mut self) {
        self.active_marker = None;
        self.active_scope = None;
        self.turns_since_focus = 0;
        self.turns_since_reminder = 0;
    }
}

impl Default for FocusState {
    fn default() -> Self {
        Self::new(FocusConfig::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_state_is_inactive() {
        let state = FocusState::new(FocusConfig::default());
        assert!(!state.is_active());
        assert!(state.knowledge_blocks.is_empty());
    }

    #[test]
    fn append_knowledge_adds_entry() {
        let mut state = FocusState::new(FocusConfig::default());
        assert!(state.append_knowledge("test summary".to_string()));
        assert_eq!(state.knowledge_blocks.len(), 1);
    }

    #[test]
    fn append_knowledge_ignores_empty() {
        let mut state = FocusState::new(FocusConfig::default());
        assert!(!state.append_knowledge(String::new()));
        assert!(state.knowledge_blocks.is_empty());
    }

    #[cfg(feature = "context-compression")]
    #[test]
    fn start_sets_marker_and_scope() {
        let mut state = FocusState::new(FocusConfig::default());
        let marker = state.start("test scope".to_string());
        assert!(state.is_active());
        assert_eq!(state.active_marker, Some(marker));
        assert_eq!(state.active_scope.as_deref(), Some("test scope"));
    }

    #[cfg(feature = "context-compression")]
    #[test]
    fn complete_clears_state() {
        let mut state = FocusState::new(FocusConfig::default());
        state.start("test".to_string());
        state.complete();
        assert!(!state.is_active());
        assert_eq!(state.turns_since_focus, 0);
    }

    #[cfg(feature = "context-compression")]
    #[test]
    fn build_knowledge_message_none_when_empty() {
        let state = FocusState::new(FocusConfig::default());
        assert!(state.build_knowledge_message().is_none());
    }

    #[cfg(feature = "context-compression")]
    #[test]
    fn build_knowledge_message_contains_prefix() {
        let mut state = FocusState::new(FocusConfig::default());
        state.append_knowledge("my summary".to_string());
        let msg = state.build_knowledge_message().unwrap();
        assert!(msg.content.starts_with(KNOWLEDGE_BLOCK_PREFIX));
        assert!(msg.content.contains("my summary"));
        assert!(msg.metadata.focus_pinned);
    }

    // SEC-CC-01: append_knowledge must enforce max_knowledge_tokens cap (FIFO eviction).
    #[test]
    fn append_knowledge_evicts_oldest_when_over_token_cap() {
        let mut config = FocusConfig::default();
        config.max_knowledge_tokens = 10; // 10 tokens ≈ 40 chars
        let mut state = FocusState::new(config);

        // Add entries that will exceed the cap
        state.append_knowledge("a".repeat(100)); // ~25 tokens (400 chars / 4)
        state.append_knowledge("b".repeat(100));
        state.append_knowledge("c".repeat(100));

        // Should have evicted oldest entries to stay within cap
        // At least one entry must remain (we never evict the last one)
        assert!(
            !state.knowledge_blocks.is_empty(),
            "at least one block must remain"
        );
        // The oldest ('a' block) should have been evicted if we have multiple blocks
        if state.knowledge_blocks.len() > 1 {
            assert!(
                !state.knowledge_blocks[0].starts_with('a'),
                "oldest block must be evicted first"
            );
        }
    }

    #[test]
    fn append_knowledge_preserves_single_entry_regardless_of_size() {
        let mut config = FocusConfig::default();
        config.max_knowledge_tokens = 1; // impossibly small cap
        let mut state = FocusState::new(config);
        state.append_knowledge("very long summary that exceeds any token cap".to_string());
        // Must keep at least 1 block — never evict all
        assert_eq!(
            state.knowledge_blocks.len(),
            1,
            "must never evict the last entry"
        );
    }

    // T-HIGH-01: focus is_active correctly reflects session state.
    #[cfg(feature = "context-compression")]
    #[test]
    fn is_active_returns_false_before_start() {
        let state = FocusState::new(FocusConfig::default());
        assert!(!state.is_active());
    }

    #[cfg(feature = "context-compression")]
    #[test]
    fn is_active_returns_true_during_session() {
        let mut state = FocusState::new(FocusConfig::default());
        state.start("scope".to_string());
        assert!(state.is_active());
    }

    #[cfg(feature = "context-compression")]
    #[test]
    fn is_active_returns_false_after_complete() {
        let mut state = FocusState::new(FocusConfig::default());
        state.start("scope".to_string());
        state.complete();
        assert!(!state.is_active());
    }

    #[test]
    fn tick_increments_counters() {
        let mut state = FocusState::new(FocusConfig::default());
        state.tick();
        state.tick();
        assert_eq!(state.turns_since_focus, 2);
        assert_eq!(state.turns_since_reminder, 2);
    }
}
