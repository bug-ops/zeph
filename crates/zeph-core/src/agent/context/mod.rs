// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

mod assembly;
mod summarization;

use zeph_memory::TokenCounter;

use super::{Agent, Channel, Message};

pub(super) fn chunk_messages(
    messages: &[Message],
    budget: usize,
    oversized: usize,
    tc: &TokenCounter,
) -> Vec<Vec<Message>> {
    let mut chunks: Vec<Vec<Message>> = Vec::new();
    let mut current: Vec<Message> = Vec::new();
    let mut current_tokens = 0usize;

    for msg in messages {
        let msg_tokens = tc.count_message_tokens(msg);

        if msg_tokens >= oversized {
            // Oversized message gets its own chunk
            if !current.is_empty() {
                chunks.push(std::mem::take(&mut current));
                current_tokens = 0;
            }
            chunks.push(vec![msg.clone()]);
        } else if current_tokens + msg_tokens > budget && !current.is_empty() {
            chunks.push(std::mem::take(&mut current));
            current_tokens = 0;
            current.push(msg.clone());
            current_tokens += msg_tokens;
        } else {
            current.push(msg.clone());
            current_tokens += msg_tokens;
        }
    }

    if !current.is_empty() {
        chunks.push(current);
    }

    if chunks.is_empty() {
        chunks.push(Vec::new());
    }

    chunks
}

pub(super) use crate::text::truncate_to_chars as truncate_chars;

/// Cap an LLM summary to `max_chars` characters (SEC-02).
///
/// Prevents a misbehaving LLM backend from returning an arbitrarily large summary that
/// would expand rather than shrink the context window after compaction.
pub(super) fn cap_summary(s: String, max_chars: usize) -> String {
    match s.char_indices().nth(max_chars) {
        Some((byte_idx, _)) => {
            tracing::warn!(
                original_chars = s.chars().count(),
                cap = max_chars,
                "LLM summary exceeded cap, truncating"
            );
            format!("{}…", &s[..byte_idx])
        }
        None => s,
    }
}

/// Return type from `compact_context()` that distinguishes between successful compaction,
/// probe rejection, and no-op.
///
/// Gives `maybe_compact()` enough information to handle probe rejection without triggering
/// the `Exhausted` state — which would only be correct if summarization itself is stuck.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CompactionOutcome {
    /// Messages were drained and replaced with a summary.
    Compacted,
    /// Probe rejected the summary — original messages are preserved.
    /// Caller must NOT check `freed_tokens` or transition to `Exhausted`.
    ProbeRejected,
    /// No compaction was performed (too few messages, empty `to_compact`, etc.).
    NoChange,
}

/// Tagged output of each concurrent context-fetch future.
///
/// Using an enum instead of a tuple allows individual sources to be added or
/// removed (including cfg-gated ones) without rewriting the join combinator.
pub(super) enum ContextSlot {
    Summaries(Option<Message>),
    CrossSession(Option<Message>),
    /// Semantic recall result. Carries the formatted message and the top-1 similarity score.
    SemanticRecall(Option<Message>, Option<f32>),
    DocumentRag(Option<Message>),
    Corrections(Option<Message>),
    CodeContext(Option<String>),
    GraphFacts(Option<Message>),
}
impl<C: Channel> Agent<C> {
    pub(super) fn compaction_tier(&self) -> super::context_manager::CompactionTier {
        self.context_manager
            .compaction_tier(self.providers.cached_prompt_tokens)
    }
}

#[cfg(test)]
mod tests;
