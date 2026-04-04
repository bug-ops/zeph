// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Session-scoped write buffer for batching memory writes (#2478).
//!
//! Accumulates writes during a turn and flushes them in a single `BEGIN IMMEDIATE`
//! transaction at turn end, reducing `SQLite` write-lock contention from N writes/turn
//! to 1 transaction/turn.

use std::collections::VecDeque;

use crate::types::{ConversationId, MemoryTier};

/// A single buffered write operation waiting to be flushed to the store.
pub enum BufferedWrite {
    /// Save a message to the messages table.
    SaveMessage {
        conversation_id: ConversationId,
        role: String,
        content: String,
        tier: MemoryTier,
    },
    /// Upsert a persona fact.
    UpsertPersonaFact {
        category: String,
        content: String,
        confidence: f64,
        source_conversation_id: Option<i64>,
        supersedes_id: Option<i64>,
    },
    /// Store an embedding in the vector backend (dispatched after `SQLite` commit).
    StoreEmbedding {
        collection: String,
        point_id: String,
        vector: Vec<f32>,
        payload: serde_json::Value,
    },
}

/// Session-scoped write buffer.
///
/// Writes are queued via `push()` and flushed to the store in one transaction
/// via `drain()`. The buffer is NOT thread-safe — it is owned by a single agent loop.
pub struct WriteBuffer {
    pending: VecDeque<BufferedWrite>,
    /// Maximum pending writes before auto-flush is signalled.
    capacity: usize,
}

impl WriteBuffer {
    /// Create a new `WriteBuffer` with the given capacity.
    ///
    /// When `capacity` is reached, `push()` signals the caller to flush.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            pending: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    /// Queue a write operation.
    ///
    /// Returns `true` if the buffer has reached capacity and should be flushed
    /// before the next `push()`.
    pub fn push(&mut self, write: BufferedWrite) -> bool {
        self.pending.push_back(write);
        self.pending.len() >= self.capacity
    }

    /// Drain all pending writes, returning them in insertion order.
    ///
    /// After this call, the buffer is empty and ready for the next turn.
    pub fn drain(&mut self) -> Vec<BufferedWrite> {
        self.pending.drain(..).collect()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.pending.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ConversationId, MemoryTier};

    fn make_save_message() -> BufferedWrite {
        BufferedWrite::SaveMessage {
            conversation_id: ConversationId(1),
            role: "user".into(),
            content: "hello".into(),
            tier: MemoryTier::Episodic,
        }
    }

    #[test]
    fn push_increases_len() {
        let mut buf = WriteBuffer::new(5);
        assert!(buf.is_empty());
        buf.push(make_save_message());
        assert_eq!(buf.len(), 1);
    }

    #[test]
    fn push_returns_false_below_capacity() {
        let mut buf = WriteBuffer::new(3);
        let at_capacity = buf.push(make_save_message());
        assert!(!at_capacity);
        let at_capacity = buf.push(make_save_message());
        assert!(!at_capacity);
    }

    #[test]
    fn push_returns_true_at_capacity() {
        let mut buf = WriteBuffer::new(2);
        buf.push(make_save_message());
        let at_capacity = buf.push(make_save_message());
        assert!(at_capacity);
    }

    #[test]
    fn drain_returns_all_items_and_clears_buffer() {
        let mut buf = WriteBuffer::new(10);
        buf.push(make_save_message());
        buf.push(make_save_message());
        buf.push(make_save_message());

        let drained = buf.drain();
        assert_eq!(drained.len(), 3);
        assert!(buf.is_empty());
        assert_eq!(buf.len(), 0);
    }

    #[test]
    fn drain_on_empty_buffer_returns_empty_vec() {
        let mut buf = WriteBuffer::new(5);
        let drained = buf.drain();
        assert!(drained.is_empty());
    }

    #[test]
    fn capacity_one_signals_on_first_push() {
        let mut buf = WriteBuffer::new(1);
        let at_capacity = buf.push(make_save_message());
        assert!(at_capacity);
    }
}
