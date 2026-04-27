// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Value types for persistence requests and outcomes.

use zeph_llm::provider::{MessagePart, Role};

/// A request to persist exactly one message to semantic memory and `SQLite`.
///
/// Fully owned — no borrow lifetimes — so it can be returned from the tool dispatcher
/// (which holds many `&mut` borrows of agent state) and drained by a separate
/// [`crate::service::PersistenceService`] call after those borrows are released.
///
/// Clone cost is small: typical message content is less than 10 KB; allocation count
/// is two (one for `content`, one for `parts`) per persist request.
///
/// # TODO(critic): R3 batches persist requests until end of `process_response`.
/// On panic/cancel mid-response, pending requests are LOST, unlike today's inline
/// persist. See critic-3515-3516.md F2. File follow-up issue if observed in live testing.
#[derive(Debug, Clone)]
pub struct PersistMessageRequest {
    /// Message role: `User`, `Assistant`, or `System`.
    pub role: Role,
    /// Full text content of the message (owned copy).
    pub content: String,
    /// Structured message parts (owned copy).
    pub parts: Vec<MessagePart>,
    /// When `true`, Qdrant embedding is skipped if `guard_memory_writes` is enabled.
    /// Set by the exfiltration guard when injection patterns are detected.
    pub has_injection_flags: bool,
}

impl PersistMessageRequest {
    /// Build a persist request by cloning borrowed inputs.
    ///
    /// Used at the dispatcher call site where borrowing the source slices is cheaper
    /// than receiving owned values directly.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_agent_persistence::request::PersistMessageRequest;
    /// use zeph_llm::provider::Role;
    ///
    /// let req = PersistMessageRequest::from_borrowed(
    ///     Role::User,
    ///     "hello",
    ///     &[],
    ///     false,
    /// );
    /// assert_eq!(req.role, Role::User);
    /// assert_eq!(req.content, "hello");
    /// ```
    #[must_use]
    pub fn from_borrowed(
        role: Role,
        content: &str,
        parts: &[MessagePart],
        has_injection_flags: bool,
    ) -> Self {
        Self {
            role,
            content: content.to_owned(),
            parts: parts.to_vec(),
            has_injection_flags,
        }
    }
}

/// Outcome of a single [`crate::service::PersistenceService::persist_message`] call.
#[derive(Debug, Clone)]
pub struct PersistMessageOutcome {
    /// Database ID assigned to the persisted message by `SQLite`, if the write succeeded.
    pub message_id: Option<i64>,
    /// Whether the message was embedded into Qdrant (as opposed to saved to `SQLite` only).
    pub embedded: bool,
    /// Whether a redaction was applied to the content before persistence.
    pub redaction_applied: bool,
    /// Number of bytes written to the persistence layer.
    pub bytes_written: usize,
}

/// Outcome of a [`crate::service::PersistenceService::load_history`] call.
#[derive(Debug, Clone)]
pub struct LoadHistoryOutcome {
    /// Number of messages successfully loaded from `SQLite` and injected into the buffer.
    pub messages_loaded: usize,
    /// Number of orphaned tool-use/tool-result pairs removed during sanitization.
    pub orphan_pairs_removed: usize,
    /// Number of message IDs accumulated in `deferred_hide_ids` for later soft-delete.
    pub deferred_hide_ids_count: usize,
    /// Total message count in `SQLite` for the active conversation.
    pub sqlite_total_messages: u64,
    /// Total semantic fact count in Qdrant for the active conversation.
    pub semantic_total_messages: u64,
}
