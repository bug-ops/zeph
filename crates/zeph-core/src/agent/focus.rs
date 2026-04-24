// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Focus Agent: active context compression via explicit bracketed exploration phases (#1850).
//!
//! Two native tools — `start_focus` and `complete_focus` — let the LLM explicitly bracket
//! exploration phases. On `complete_focus`, messages since the checkpoint are summarised,
//! the summary is appended to the pinned Knowledge block, and the bracketed messages are
//! removed from the conversation history.
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use uuid::Uuid;
use zeph_llm::provider::{Message, MessageMetadata, Role, ToolDefinition};

use crate::config::FocusConfig;
use zeph_common::text::estimate_tokens;

/// Build tool definitions for `start_focus` and `complete_focus` (#1850).
///
/// These are injected into the tool list when `focus.enabled = true`.
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
            output_schema: None,
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
            output_schema: None,
        },
    ]
}

/// Build the tool definition for `compress_context` (#2218).
///
/// Always available when `context-compression` feature is enabled, regardless of compression
/// strategy. The strategy controls automatic compression; this tool provides explicit control.
pub(crate) fn compress_context_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: "compress_context".into(),
        description: "Compress the current conversation context. Summarizes conversation history \
                      (excluding pinned Knowledge) into a compact summary, appends it to the \
                      Knowledge block, and removes the original messages from context.\n\n\
                      Use when the conversation is getting long and you want to free context space. \
                      Cannot be called while a focus session is active or while another compression \
                      is in progress.\n\nParameters: none.\nReturns: confirmation with token count."
            .into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {},
            "required": []
        }),
        output_schema: None,
    }
}

// Used by build_knowledge_message (context-compression feature).
pub(crate) const KNOWLEDGE_BLOCK_PREFIX: &str = "[knowledge]\n";

/// Indicates the origin of a knowledge block entry.
///
/// Used to differentiate LLM-authored summaries from auto-consolidated context segments,
/// allowing eviction policy to prefer evicting `AutoConsolidated` before `LlmCurated`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum KnowledgeBlockSource {
    /// Created by the LLM via `complete_focus` or `compress_context`.
    LlmCurated,
    /// Created by automatic context consolidation (Focus compression strategy).
    AutoConsolidated,
}

#[derive(Debug, Clone)]
pub(crate) struct KnowledgeBlock {
    pub(crate) content: String,
    pub(crate) source: KnowledgeBlockSource,
}

/// Tracks the state of the active focus session.
// Fields and methods below are consumed by context-compression feature paths.
pub(crate) struct FocusState {
    pub(crate) config: FocusConfig,
    /// Accumulated knowledge entries from all completed focus sessions.
    pub(crate) knowledge_blocks: Vec<KnowledgeBlock>,
    /// Marker UUID written into the checkpoint message's `focus_marker_id` field.
    /// `None` = no active focus session.
    pub(crate) active_marker: Option<Uuid>,
    /// Human-readable scope label provided by the LLM via `start_focus`.
    pub(crate) active_scope: Option<String>,
    /// Turns elapsed since the last `complete_focus` call (or session start).
    pub(crate) turns_since_focus: usize,
    /// Turns elapsed since the last reminder was injected.
    pub(crate) turns_since_reminder: usize,
    /// Concurrency guard: `true` while `compress_context` is executing.
    /// Prevents double compression and races with reactive compaction.
    pub(crate) compressing: Arc<AtomicBool>,
}
impl FocusState {
    pub(crate) fn new(config: FocusConfig) -> Self {
        Self {
            config,
            knowledge_blocks: Vec::<KnowledgeBlock>::new(),
            active_marker: None,
            active_scope: None,
            turns_since_focus: 0,
            turns_since_reminder: 0,
            compressing: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Try to acquire the compression lock.
    ///
    /// Returns `true` if acquired (caller must call `release_compression()` when done).
    /// Returns `false` if another compression is already in progress.
    pub(crate) fn try_acquire_compression(&self) -> bool {
        self.compressing
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
    }

    /// Release the compression lock.
    pub(crate) fn release_compression(&self) {
        self.compressing.store(false, Ordering::Release);
    }

    /// Returns `true` if a focus session is currently active.
    pub(crate) fn is_active(&self) -> bool {
        self.active_marker.is_some()
    }

    /// Reset focus state for a new conversation.
    ///
    /// Clears the active session and accumulated knowledge so the new conversation
    /// starts without stale focus context.
    pub(crate) fn reset(&mut self) {
        self.knowledge_blocks.clear();
        self.active_marker = None;
        self.active_scope = None;
        self.turns_since_focus = 0;
        self.turns_since_reminder = 0;
        self.compressing
            .store(false, std::sync::atomic::Ordering::Release);
    }

    /// Increment turn counters. Called at the start of each user-message turn.
    pub(crate) fn tick(&mut self) {
        self.turns_since_focus = self.turns_since_focus.saturating_add(1);
        self.turns_since_reminder = self.turns_since_reminder.saturating_add(1);
    }

    /// Append a completed focus summary to the accumulated knowledge.
    ///
    /// Enforces `max_knowledge_tokens` by evicting entries when the serialized block would
    /// exceed the cap. Eviction prefers `AutoConsolidated` blocks over `LlmCurated` ones;
    /// within the same source tier, oldest entries are evicted first (SEC-CC-01 fix).
    ///
    /// Returns `true` if the Knowledge block was actually updated.
    pub(crate) fn append_knowledge(
        &mut self,
        summary: String,
        source: KnowledgeBlockSource,
    ) -> bool {
        if summary.is_empty() {
            return false;
        }
        self.knowledge_blocks.push(KnowledgeBlock {
            content: summary,
            source,
        });
        // Enforce the token cap. Eviction order: AutoConsolidated first (oldest), then LlmCurated.
        // Each block is approximated at 4 chars/token.
        loop {
            if self.knowledge_blocks.len() <= 1 {
                break;
            }
            let approx_tokens: usize = self
                .knowledge_blocks
                .iter()
                .map(|b| estimate_tokens(&b.content))
                .sum();
            if approx_tokens <= self.config.max_knowledge_tokens {
                break;
            }
            // Prefer evicting the oldest AutoConsolidated block.
            if let Some(pos) = self
                .knowledge_blocks
                .iter()
                .position(|b| b.source == KnowledgeBlockSource::AutoConsolidated)
            {
                self.knowledge_blocks.remove(pos);
            } else {
                // All remaining are LlmCurated — evict oldest.
                self.knowledge_blocks.remove(0);
            }
        }
        true
    }

    /// Append an LLM-curated summary (from `complete_focus` or `compress_context`).
    pub(crate) fn append_llm_knowledge(&mut self, summary: String) -> bool {
        self.append_knowledge(summary, KnowledgeBlockSource::LlmCurated)
    }

    /// Append an auto-consolidated summary (from the Focus strategy auto-consolidation path).
    pub(crate) fn append_auto_knowledge(&mut self, summary: String) -> bool {
        self.append_knowledge(summary, KnowledgeBlockSource::AutoConsolidated)
    }

    /// Build the pinned Knowledge block message, or `None` if there is no knowledge yet.
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
            body.push_str(&block.content);
        }

        Some(Message {
            role: Role::System,
            content: body,
            parts: vec![],
            metadata: MessageMetadata::focus_pinned(),
        })
    }

    /// Start a new focus session. Returns the marker UUID embedded in the checkpoint message.
    pub(crate) fn start(&mut self, scope: String) -> Uuid {
        let marker = Uuid::new_v4();
        self.active_marker = Some(marker);
        self.active_scope = Some(scope);
        marker
    }

    /// Complete the active session. Clears active marker and scope; resets reminder counters.
    /// The caller is responsible for appending the summary to `knowledge_blocks`.
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

/// RAII guard that releases the compression lock on drop.
///
/// Holds a clone of the `Arc<AtomicBool>` flag so it can release the lock without
/// holding a borrow on the parent `FocusState`. This avoids borrow-checker conflicts
/// when the rest of the function requires `&mut FocusState` (e.g. `append_auto_knowledge`).
///
/// Prevents the `compressing` flag from being permanently set to `true` when the
/// future holding the lock is cancelled at an `.await` point or if a panic propagates
/// before `release_compression()` is explicitly called.
pub(crate) struct CompressionGuard(pub(crate) Arc<AtomicBool>);

impl Drop for CompressionGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
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
        assert!(state.append_llm_knowledge("test summary".to_string()));
        assert_eq!(state.knowledge_blocks.len(), 1);
    }

    #[test]
    fn append_knowledge_ignores_empty() {
        let mut state = FocusState::new(FocusConfig::default());
        assert!(!state.append_llm_knowledge(String::new()));
        assert!(state.knowledge_blocks.is_empty());
    }
    #[test]
    fn start_sets_marker_and_scope() {
        let mut state = FocusState::new(FocusConfig::default());
        let marker = state.start("test scope".to_string());
        assert!(state.is_active());
        assert_eq!(state.active_marker, Some(marker));
        assert_eq!(state.active_scope.as_deref(), Some("test scope"));
    }
    #[test]
    fn complete_clears_state() {
        let mut state = FocusState::new(FocusConfig::default());
        state.start("test".to_string());
        state.complete();
        assert!(!state.is_active());
        assert_eq!(state.turns_since_focus, 0);
    }
    #[test]
    fn build_knowledge_message_none_when_empty() {
        let state = FocusState::new(FocusConfig::default());
        assert!(state.build_knowledge_message().is_none());
    }
    #[test]
    fn build_knowledge_message_contains_prefix() {
        let mut state = FocusState::new(FocusConfig::default());
        state.append_llm_knowledge("my summary".to_string());
        let msg = state.build_knowledge_message().unwrap();
        assert!(msg.content.starts_with(KNOWLEDGE_BLOCK_PREFIX));
        assert!(msg.content.contains("my summary"));
        assert!(msg.metadata.focus_pinned);
    }

    // SEC-CC-01: append_knowledge must enforce max_knowledge_tokens cap (FIFO eviction).
    #[test]
    fn append_knowledge_evicts_oldest_when_over_token_cap() {
        let config = FocusConfig {
            max_knowledge_tokens: 10,
            ..FocusConfig::default()
        }; // 10 tokens ≈ 40 chars
        let mut state = FocusState::new(config);

        // Add entries that will exceed the cap
        state.append_llm_knowledge("a".repeat(100)); // ~25 tokens (400 chars / 4)
        state.append_llm_knowledge("b".repeat(100));
        state.append_llm_knowledge("c".repeat(100));

        // Should have evicted oldest entries to stay within cap
        // At least one entry must remain (we never evict the last one)
        assert!(
            !state.knowledge_blocks.is_empty(),
            "at least one block must remain"
        );
        // The oldest ('a' block) should have been evicted if we have multiple blocks
        if state.knowledge_blocks.len() > 1 {
            assert!(
                !state.knowledge_blocks[0].content.starts_with('a'),
                "oldest block must be evicted first"
            );
        }
    }

    #[test]
    fn append_knowledge_preserves_single_entry_regardless_of_size() {
        let config = FocusConfig {
            max_knowledge_tokens: 1,
            ..FocusConfig::default()
        }; // impossibly small cap
        let mut state = FocusState::new(config);
        state.append_llm_knowledge("very long summary that exceeds any token cap".to_string());
        // Must keep at least 1 block — never evict all
        assert_eq!(
            state.knowledge_blocks.len(),
            1,
            "must never evict the last entry"
        );
    }

    // T-HIGH-01: focus is_active correctly reflects session state.
    #[test]
    fn is_active_returns_false_before_start() {
        let state = FocusState::new(FocusConfig::default());
        assert!(!state.is_active());
    }
    #[test]
    fn is_active_returns_true_during_session() {
        let mut state = FocusState::new(FocusConfig::default());
        state.start("scope".to_string());
        assert!(state.is_active());
    }
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

    // Test: concurrency guard prevents double-compression (#2218).
    #[test]
    fn try_acquire_compression_prevents_double_call() {
        let state = FocusState::new(FocusConfig::default());

        // First acquire must succeed.
        assert!(
            state.try_acquire_compression(),
            "first acquire must succeed"
        );

        // Second acquire must fail while first is held.
        assert!(
            !state.try_acquire_compression(),
            "second acquire must fail while lock is held"
        );

        // After release, acquire must succeed again.
        state.release_compression();
        assert!(
            state.try_acquire_compression(),
            "acquire must succeed after release"
        );
        state.release_compression();
    }

    // Test: Knowledge block persists across compaction cycles (#2218).
    // Verifies that knowledge accumulated before a compress_context call
    // is preserved after another append_knowledge call.
    #[test]
    fn knowledge_block_persists_across_multiple_appends() {
        let mut state = FocusState::new(FocusConfig::default());

        state.append_llm_knowledge("First compression summary.".to_string());
        state.append_llm_knowledge("Second compression summary.".to_string());

        assert_eq!(
            state.knowledge_blocks.len(),
            2,
            "both summaries must persist"
        );
        assert!(state.knowledge_blocks[0].content.contains("First"));
        assert!(state.knowledge_blocks[1].content.contains("Second"));
    }

    // Test: Knowledge block message contains all summaries after multiple compressions.
    #[test]
    fn knowledge_message_contains_all_summaries() {
        let mut state = FocusState::new(FocusConfig::default());
        state.append_llm_knowledge("Summary A: learned about auth.".to_string());
        state.append_llm_knowledge("Summary B: learned about storage.".to_string());

        let msg = state.build_knowledge_message().unwrap();
        assert!(msg.content.contains("Summary A"));
        assert!(msg.content.contains("Summary B"));
        assert!(msg.metadata.focus_pinned, "knowledge block must be pinned");
    }

    #[test]
    fn reset_clears_all_session_state() {
        let mut state = FocusState::new(FocusConfig::default());
        state.knowledge_blocks.push(KnowledgeBlock {
            content: "some knowledge".to_string(),
            source: KnowledgeBlockSource::LlmCurated,
        });
        state.active_scope = Some("active scope".to_string());
        state.turns_since_focus = 7;
        state.turns_since_reminder = 3;
        state.reset();
        assert!(state.knowledge_blocks.is_empty());
        assert!(state.active_scope.is_none());
        assert_eq!(state.turns_since_focus, 0);
        assert_eq!(state.turns_since_reminder, 0);
    }

    #[test]
    fn append_auto_knowledge_uses_auto_consolidated_source() {
        let mut state = FocusState::new(FocusConfig::default());
        assert!(state.append_auto_knowledge("auto summary".to_string()));
        assert_eq!(state.knowledge_blocks.len(), 1);
        assert_eq!(
            state.knowledge_blocks[0].source,
            KnowledgeBlockSource::AutoConsolidated
        );
    }

    // SEC-CC-02: AutoConsolidated evicted before LlmCurated under token cap.
    #[test]
    fn eviction_prefers_auto_consolidated_over_llm_curated() {
        let config = FocusConfig {
            max_knowledge_tokens: 10,
            ..FocusConfig::default()
        };
        let mut state = FocusState::new(config);
        state.append_llm_knowledge("a".repeat(20)); // LlmCurated
        state.append_auto_knowledge("b".repeat(20)); // AutoConsolidated
        state.append_llm_knowledge("c".repeat(20)); // LlmCurated — triggers eviction

        // AutoConsolidated block must be evicted first.
        assert!(
            state
                .knowledge_blocks
                .iter()
                .all(|b| b.source != KnowledgeBlockSource::AutoConsolidated),
            "AutoConsolidated block must be evicted before LlmCurated"
        );
    }
}
