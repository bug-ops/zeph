// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Context slot types, compaction outcome, and message-chunking helpers.
//!
//! [`ContextSlot`] tags async fetch results so the assembler's `FuturesUnordered`
//! collector can dispatch results without tuple indexing.
//!
//! [`CompactionOutcome`] communicates the result of one compaction attempt to
//! `maybe_compact` in `zeph-core`.

use zeph_llm::provider::Message;

/// Tagged output of each concurrent context-fetch future.
///
/// Using an enum instead of a tuple allows individual sources to be added or
/// removed (including cfg-gated ones) without rewriting the join combinator.
pub enum ContextSlot {
    /// Past-session summaries (contextual recall).
    Summaries(Option<Message>),
    /// Cross-session memory recall.
    CrossSession(Option<Message>),
    /// Semantic recall result. Carries the formatted message and the top-1 similarity score.
    SemanticRecall(Option<Message>, Option<f32>),
    /// Document RAG result.
    DocumentRag(Option<Message>),
    /// Past user corrections recalled for this turn.
    Corrections(Option<Message>),
    /// Code-index RAG result (repo-map or file context).
    CodeContext(Option<String>),
    /// Knowledge graph fact recall.
    GraphFacts(Option<Message>),
    /// Persona memory facts injected after the system prompt (#2461).
    PersonaFacts(Option<Message>),
    /// Top-k procedural trajectory hints recalled for the current turn (#2498).
    TrajectoryHints(Option<Message>),
    /// `TiMem` tree summary nodes recalled for context (#2262).
    TreeMemory(Option<Message>),
    /// Distilled reasoning strategies recalled for the current turn (#3343).
    ReasoningStrategies(Option<Message>),
}

/// Return type from `compact_context()` that distinguishes between successful compaction,
/// probe rejection, and no-op.
///
/// Gives `maybe_compact()` enough information to handle probe rejection without triggering
/// the `Exhausted` state — which would only be correct if summarization itself is stuck.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionOutcome {
    /// Messages were drained and replaced with a summary.
    Compacted,
    /// Probe rejected the summary — original messages are preserved.
    /// Caller must NOT check `freed_tokens` or transition to `Exhausted`.
    ProbeRejected,
    /// No compaction was performed (too few messages, empty `to_compact`, etc.).
    NoChange,
}

/// Prefix prepended to persona memory injections.
pub const PERSONA_PREFIX: &str = "[Persona context]\n";
/// Prefix prepended to trajectory-hint injections.
pub const TRAJECTORY_PREFIX: &str = "[Past experience]\n";
/// Prefix prepended to reasoning-strategy injections.
pub const REASONING_PREFIX: &str = "[Reasoning Strategy]\n";
/// Prefix prepended to `TiMem` tree memory injections.
pub const TREE_MEMORY_PREFIX: &str = "[Memory summary]\n";

/// Split a message slice into chunks that each fit within `budget` tokens.
///
/// Messages larger than `oversized` tokens each get their own chunk. All other
/// messages are greedily packed. Callers that need at least one chunk will always
/// receive one (empty `Vec<Message>` wrapped in a single chunk).
///
/// `count_message_tokens` is a caller-supplied function that returns the token count
/// for a single message. This avoids a direct dependency on `zeph-memory::TokenCounter`.
#[must_use]
pub fn chunk_messages(
    messages: &[Message],
    budget: usize,
    oversized: usize,
    count_message_tokens: impl Fn(&Message) -> usize,
) -> Vec<Vec<Message>> {
    let mut chunks: Vec<Vec<Message>> = Vec::new();
    let mut current: Vec<Message> = Vec::new();
    let mut current_tokens = 0usize;

    for msg in messages {
        let msg_tokens = count_message_tokens(msg);

        if msg_tokens >= oversized {
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

/// Cap an LLM summary to `max_chars` characters (SEC-02).
///
/// Prevents a misbehaving LLM backend from returning an arbitrarily large summary that
/// would expand rather than shrink the context window after compaction.
#[must_use]
pub fn cap_summary(s: String, max_chars: usize) -> String {
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
