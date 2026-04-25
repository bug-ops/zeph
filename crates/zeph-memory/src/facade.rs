// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Thin trait abstraction over [`crate::semantic::SemanticMemory`].
//!
//! `MemoryFacade` is a narrow interface covering the four operations the agent loop
//! depends on: `remember`, `recall`, `summarize`, and `compact`. It exists for two
//! reasons:
//!
//! 1. **Unit testing** — `InMemoryFacade` implements the trait using in-process
//!    storage so tests that exercise memory-dependent agent logic do not need `SQLite`
//!    or `Qdrant`.
//!
//! 2. **Future migration path** — the agent can eventually hold
//!    `Arc<dyn MemoryFacade>` instead of `Arc<SemanticMemory>`. Phase 2 will
//!    require making the trait object-safe (e.g. via `#[trait_variant::make]`
//!    or boxed-future signatures). This PR implements Phase 1 only.
//!
//! # Design constraints
//!
//! `MemoryEntry.parts` uses `Vec<serde_json::Value>` (opaque JSON) rather than
//! `Vec<zeph_llm::MessagePart>` to keep `zeph-memory` free of `zeph-llm` dependencies.

use std::collections::BTreeMap;
use std::sync::Mutex;

use crate::error::MemoryError;
use crate::types::{ConversationId, MessageId};

// ── Supporting types ─────────────────────────────────────────────────────────

/// An entry to persist in memory.
///
/// The `parts` field is opaque `serde_json::Value` — callers are responsible for
/// serializing their content model. This avoids a `zeph-memory` → `zeph-llm`
/// dependency at the trait boundary.
///
/// # Examples
///
/// ```
/// use zeph_memory::facade::MemoryEntry;
/// use zeph_memory::ConversationId;
///
/// let entry = MemoryEntry {
///     conversation_id: ConversationId(1),
///     role: "user".into(),
///     content: "Hello".into(),
///     parts: vec![],
///     metadata: None,
/// };
/// assert_eq!(entry.role, "user");
/// ```
#[derive(Debug, Clone)]
pub struct MemoryEntry {
    /// Conversation this entry belongs to.
    pub conversation_id: ConversationId,
    /// Role string (`"user"`, `"assistant"`, `"system"`).
    pub role: String,
    /// Flat text content of the message.
    pub content: String,
    /// Structured content parts as opaque JSON values.
    pub parts: Vec<serde_json::Value>,
    /// Optional per-entry metadata as opaque JSON.
    pub metadata: Option<serde_json::Value>,
}

/// A matching entry returned by a recall query.
///
/// # Examples
///
/// ```
/// use zeph_memory::facade::{MemoryMatch, MemorySource};
///
/// let m = MemoryMatch {
///     content: "Rust is a systems language.".into(),
///     score: 0.92,
///     source: MemorySource::Semantic,
/// };
/// assert!(m.score > 0.0);
/// ```
#[derive(Debug, Clone)]
pub struct MemoryMatch {
    /// Matching message content.
    pub content: String,
    /// Relevance score in `[0.0, 1.0]`.
    pub score: f32,
    /// Backend that produced this match.
    pub source: MemorySource,
}

/// Which memory backend produced a [`MemoryMatch`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemorySource {
    /// Qdrant vector search.
    Semantic,
    /// Timestamp-filtered `SQLite` FTS5.
    Episodic,
    /// Graph traversal from extracted entities.
    Graph,
    /// `SQLite` FTS5 keyword search.
    Keyword,
}

/// Input context for a compaction run.
///
/// # Examples
///
/// ```
/// use zeph_memory::facade::CompactionContext;
/// use zeph_memory::ConversationId;
///
/// let ctx = CompactionContext { conversation_id: ConversationId(1), token_budget: 4096 };
/// assert_eq!(ctx.token_budget, 4096);
/// ```
#[derive(Debug, Clone)]
pub struct CompactionContext {
    /// Conversation to compact.
    pub conversation_id: ConversationId,
    /// Target token budget; the compactor aims to bring context below this limit.
    pub token_budget: usize,
}

/// Result of a compaction run.
#[derive(Debug, Clone)]
pub struct CompactionResult {
    /// Summary text replacing the compacted messages.
    pub summary: String,
    /// Number of messages that were replaced by the summary.
    pub messages_compacted: usize,
}

// ── MemoryFacade trait ────────────────────────────────────────────────────────

/// Narrow read/write interface over a memory backend.
///
/// Implement this trait to provide an alternative backend for unit testing
/// (see [`InMemoryFacade`]) or future agent refactoring.
///
/// # Contract
///
/// - `remember` stores a message and returns its stable ID.
/// - `recall` performs a best-effort similarity search; empty results are valid.
/// - `summarize` returns a textual summary of the conversation so far.
/// - `compact` reduces context size to within `ctx.token_budget`.
///
/// Implementations must be `Send + Sync` to support `Arc<dyn MemoryFacade>` usage.
// The `Send` bound on returned futures is guaranteed by the `Send + Sync` supertrait
// requirement on all implementors. `async_fn_in_trait` fires because auto-trait bounds
// on the implicit future type cannot be declared without #[trait_variant::make], which
// is deferred to Phase 2 when the trait becomes object-safe.
#[allow(async_fn_in_trait)]
pub trait MemoryFacade: Send + Sync {
    /// Store a memory entry and return its ID.
    ///
    /// # Errors
    ///
    /// Returns [`MemoryError`] if the backend fails to persist the entry.
    async fn remember(&self, entry: MemoryEntry) -> Result<MessageId, MemoryError>;

    /// Retrieve the most relevant entries for `query`, up to `limit` results.
    ///
    /// # Errors
    ///
    /// Returns [`MemoryError`] if the recall query fails.
    async fn recall(&self, query: &str, limit: usize) -> Result<Vec<MemoryMatch>, MemoryError>;

    /// Produce a textual summary of `conv_id`.
    ///
    /// # Errors
    ///
    /// Returns [`MemoryError`] if summarization fails.
    async fn summarize(&self, conv_id: ConversationId) -> Result<String, MemoryError>;

    /// Compact a conversation to fit within the token budget.
    ///
    /// # Errors
    ///
    /// Returns [`MemoryError`] if compaction fails.
    async fn compact(&self, ctx: &CompactionContext) -> Result<CompactionResult, MemoryError>;
}

// ── InMemoryFacade ────────────────────────────────────────────────────────────

/// In-process test double for [`MemoryFacade`].
///
/// Stores entries in a `Vec` and uses substring matching for recall.
/// No `SQLite` or `Qdrant` required — suitable for unit tests that exercise
/// memory-dependent agent logic without external infrastructure.
///
/// # Examples
///
/// ```no_run
/// # use zeph_memory::facade::{InMemoryFacade, MemoryEntry, MemoryFacade};
/// # use zeph_memory::ConversationId;
/// # #[tokio::main] async fn main() {
/// let facade = InMemoryFacade::new();
/// let entry = MemoryEntry {
///     conversation_id: ConversationId(1),
///     role: "user".into(),
///     content: "Rust borrow checker".into(),
///     parts: vec![],
///     metadata: None,
/// };
/// let id = facade.remember(entry).await.unwrap();
/// let matches = facade.recall("borrow", 10).await.unwrap();
/// assert!(!matches.is_empty());
/// # }
/// ```
#[derive(Debug, Default)]
pub struct InMemoryFacade {
    entries: Mutex<BTreeMap<i64, MemoryEntry>>,
    next_id: Mutex<i64>,
}

impl InMemoryFacade {
    /// Create a new empty facade.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Return the number of stored entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.lock().map_or(0, |g| g.len())
    }

    /// Return `true` if no entries have been stored.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl MemoryFacade for InMemoryFacade {
    async fn remember(&self, entry: MemoryEntry) -> Result<MessageId, MemoryError> {
        let mut id_guard = self
            .next_id
            .lock()
            .map_err(|e| MemoryError::LockPoisoned(format!("InMemoryFacade lock poisoned: {e}")))?;
        *id_guard += 1;
        let id = *id_guard;
        let mut entries = self
            .entries
            .lock()
            .map_err(|e| MemoryError::LockPoisoned(format!("InMemoryFacade lock poisoned: {e}")))?;
        entries.insert(id, entry);
        Ok(MessageId(id))
    }

    async fn recall(&self, query: &str, limit: usize) -> Result<Vec<MemoryMatch>, MemoryError> {
        let entries = self
            .entries
            .lock()
            .map_err(|e| MemoryError::LockPoisoned(format!("InMemoryFacade lock poisoned: {e}")))?;
        let query_lower = query.to_lowercase();
        let mut matches: Vec<MemoryMatch> = entries
            .values()
            .filter(|e| e.content.to_lowercase().contains(&query_lower))
            .map(|e| MemoryMatch {
                content: e.content.clone(),
                score: 1.0,
                source: MemorySource::Keyword,
            })
            .take(limit)
            .collect();
        // Stable order for deterministic tests
        matches.sort_by(|a, b| a.content.cmp(&b.content));
        Ok(matches)
    }

    async fn summarize(&self, conv_id: ConversationId) -> Result<String, MemoryError> {
        let entries = self
            .entries
            .lock()
            .map_err(|e| MemoryError::LockPoisoned(format!("InMemoryFacade lock poisoned: {e}")))?;
        let texts: Vec<&str> = entries
            .values()
            .filter(|e| e.conversation_id == conv_id)
            .map(|e| e.content.as_str())
            .collect();
        Ok(texts.join("\n"))
    }

    async fn compact(&self, ctx: &CompactionContext) -> Result<CompactionResult, MemoryError> {
        let mut entries = self
            .entries
            .lock()
            .map_err(|e| MemoryError::LockPoisoned(format!("InMemoryFacade lock poisoned: {e}")))?;
        let ids_to_remove: Vec<i64> = entries
            .iter()
            .filter(|(_, e)| e.conversation_id == ctx.conversation_id)
            .map(|(&id, _)| id)
            .collect();
        let count = ids_to_remove.len();
        let summary: Vec<String> = ids_to_remove
            .iter()
            .filter_map(|id| entries.get(id).map(|e| e.content.clone()))
            .collect();
        for id in &ids_to_remove {
            entries.remove(id);
        }
        Ok(CompactionResult {
            summary: summary.join("\n"),
            messages_compacted: count,
        })
    }
}

// ── SemanticMemory impl ───────────────────────────────────────────────────────

impl MemoryFacade for crate::semantic::SemanticMemory {
    async fn remember(&self, entry: MemoryEntry) -> Result<MessageId, MemoryError> {
        let parts_json = serde_json::to_string(&entry.parts).map_err(MemoryError::Json)?;
        let (id_opt, _embedded) = self
            .remember_with_parts(
                entry.conversation_id,
                &entry.role,
                &entry.content,
                &parts_json,
                None,
            )
            .await?;
        id_opt.ok_or_else(|| {
            MemoryError::InvalidInput("message rejected by admission control".into())
        })
    }

    async fn recall(&self, query: &str, limit: usize) -> Result<Vec<MemoryMatch>, MemoryError> {
        let recalled = self.recall(query, limit, None).await?;
        Ok(recalled
            .into_iter()
            .map(|r| MemoryMatch {
                content: r.message.content,
                score: r.score,
                source: MemorySource::Semantic,
            })
            .collect())
    }

    async fn summarize(&self, conv_id: ConversationId) -> Result<String, MemoryError> {
        let summaries = self.load_summaries(conv_id).await?;
        Ok(summaries
            .into_iter()
            .map(|s| s.content)
            .collect::<Vec<_>>()
            .join("\n"))
    }

    async fn compact(&self, ctx: &CompactionContext) -> Result<CompactionResult, MemoryError> {
        let before = self.message_count(ctx.conversation_id).await?;
        let messages_compacted = usize::try_from(before).unwrap_or(0);
        // Trigger a summarization pass to reduce context below the token budget.
        // The message_count parameter drives how many messages to summarize at once.
        // Approximate: 4 chars per token; produce a target message count that
        // keeps the resulting context under the token budget.
        let target_msgs = ctx.token_budget.checked_div(4).unwrap_or(512);
        let _ = self.summarize(ctx.conversation_id, target_msgs).await?;
        let summary = self
            .load_summaries(ctx.conversation_id)
            .await?
            .into_iter()
            .map(|s| s.content)
            .collect::<Vec<_>>()
            .join("\n");
        Ok(CompactionResult {
            summary,
            messages_compacted,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn remember_and_recall() {
        let facade = InMemoryFacade::new();
        let entry = MemoryEntry {
            conversation_id: ConversationId(1),
            role: "user".into(),
            content: "Rust ownership model".into(),
            parts: vec![],
            metadata: None,
        };
        let id = facade.remember(entry).await.unwrap();
        assert_eq!(id, MessageId(1));

        let matches = facade.recall("ownership", 10).await.unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].content, "Rust ownership model");
        assert_eq!(matches[0].source, MemorySource::Keyword);
    }

    #[tokio::test]
    async fn recall_no_match() {
        let facade = InMemoryFacade::new();
        let entry = MemoryEntry {
            conversation_id: ConversationId(1),
            role: "user".into(),
            content: "Rust ownership model".into(),
            parts: vec![],
            metadata: None,
        };
        facade.remember(entry).await.unwrap();
        let matches = facade.recall("Python", 10).await.unwrap();
        assert!(matches.is_empty());
    }

    #[tokio::test]
    async fn summarize_joins_content() {
        let facade = InMemoryFacade::new();
        for content in ["Hello", "World"] {
            facade
                .remember(MemoryEntry {
                    conversation_id: ConversationId(1),
                    role: "user".into(),
                    content: content.into(),
                    parts: vec![],
                    metadata: None,
                })
                .await
                .unwrap();
        }
        let summary = facade.summarize(ConversationId(1)).await.unwrap();
        assert!(summary.contains("Hello") && summary.contains("World"));
    }

    #[tokio::test]
    async fn compact_removes_conversation_entries() {
        let facade = InMemoryFacade::new();
        facade
            .remember(MemoryEntry {
                conversation_id: ConversationId(1),
                role: "user".into(),
                content: "entry 1".into(),
                parts: vec![],
                metadata: None,
            })
            .await
            .unwrap();
        facade
            .remember(MemoryEntry {
                conversation_id: ConversationId(2),
                role: "user".into(),
                content: "other conv".into(),
                parts: vec![],
                metadata: None,
            })
            .await
            .unwrap();

        let result = facade
            .compact(&CompactionContext {
                conversation_id: ConversationId(1),
                token_budget: 100,
            })
            .await
            .unwrap();

        assert_eq!(result.messages_compacted, 1);
        assert!(result.summary.contains("entry 1"));
        // Other conversation untouched
        assert_eq!(facade.len(), 1);
    }

    #[tokio::test]
    async fn recall_respects_limit() {
        let facade = InMemoryFacade::new();
        for i in 0..5 {
            facade
                .remember(MemoryEntry {
                    conversation_id: ConversationId(1),
                    role: "user".into(),
                    content: format!("memory item {i}"),
                    parts: vec![],
                    metadata: None,
                })
                .await
                .unwrap();
        }
        let matches = facade.recall("memory", 3).await.unwrap();
        assert_eq!(matches.len(), 3);
    }
}
