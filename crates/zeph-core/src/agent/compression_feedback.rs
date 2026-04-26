// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Failure detection for ACON compression guidelines (#1647).
//!
//! Pure detection helpers live in [`zeph_context::compression_feedback`].
//! This module contains only the `Agent`-level integration: logging to `SQLite`
//! and extracting the compaction summary from message history.

use crate::agent::Agent;
use crate::channel::Channel;

pub use zeph_context::compression_feedback::{
    classify_failure_category, detect_compression_failure,
};

impl<C: Channel> Agent<C> {
    /// Check the LLM response for signs of context loss after compaction.
    ///
    /// Fires only when:
    /// 1. The feature is enabled in config
    /// 2. A hard compaction has occurred in this session
    /// 3. The number of turns since last compaction is within the detection window
    /// 4. Both uncertainty and prior-context signals are present in the response
    ///
    /// If all conditions are met, logs a failure pair to `SQLite` (non-fatal on error).
    pub(crate) async fn maybe_log_compression_failure(&self, response: &str) {
        let config = &self
            .services
            .memory
            .compaction
            .compression_guidelines_config;

        if !config.enabled {
            return;
        }

        let Some(turns) = self.context_manager.turns_since_last_hard_compaction else {
            return;
        };
        if turns > config.detection_window_turns {
            return;
        }

        let Some(detection_meta) = detect_compression_failure(response, true) else {
            return;
        };

        tracing::debug!(meta = %detection_meta, "compression failure detected");

        let compressed_context = self.extract_last_compaction_summary();

        let Some(memory) = &self.services.memory.persistence.memory else {
            return;
        };
        let Some(cid) = self.services.memory.persistence.conversation_id else {
            return;
        };

        let category = classify_failure_category(&compressed_context);

        let sqlite = memory.sqlite();
        if let Err(e) = sqlite
            .log_compression_failure(cid, &compressed_context, response, category)
            .await
        {
            tracing::warn!("failed to log compression failure pair: {e:#}");
        } else {
            tracing::info!(
                turns_since_compaction = turns,
                category,
                "compression failure detected and logged"
            );
        }
    }

    /// Extract the most recent compaction summary text from the message history.
    ///
    /// After `compact_context()`, a `[conversation summary — N messages compacted]`
    /// system message is inserted at index 1. This method scans positions 1..4
    /// to find and return that summary text.
    fn extract_last_compaction_summary(&self) -> String {
        const SUMMARY_MARKER: &str = "[conversation summary";
        for msg in self.msg.messages.iter().skip(1).take(3) {
            if msg.content.starts_with(SUMMARY_MARKER) {
                return msg.content.clone();
            }
        }
        String::new()
    }
}
