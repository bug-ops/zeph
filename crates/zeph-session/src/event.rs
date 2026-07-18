// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! The `SessionEvent` schema and its on-disk envelope.
//!
//! Every line appended to a session's `events.jsonl` is one JSON-encoded
//! [`SessionEventEnvelope`]. `seq` is the source of truth for ordering (see INV-SP-1/INV-SP-2 in
//! `specs/068-session-persistence/spec.md` §13); `ts_ms` is informational only.

use serde::{Deserialize, Serialize};
use zeph_common::memory::AnchoredSummary;
use zeph_llm::provider::MessagePart;

/// One line of a session's `events.jsonl` append-only log.
///
/// # Examples
///
/// ```
/// use zeph_session::event::{SessionEvent, SessionEventEnvelope};
///
/// let envelope = SessionEventEnvelope::new(
///     0,
///     None,
///     None,
///     SessionEvent::UserMessage { text: "hello".to_owned(), image_refs: vec![] },
/// );
/// let line = serde_json::to_string(&envelope).expect("serializable");
/// let round_tripped: SessionEventEnvelope =
///     serde_json::from_str(&line).expect("deserializable");
/// assert_eq!(round_tripped.seq, 0);
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionEventEnvelope {
    /// Monotonic, gap-free, per-session sequence number starting at 0.
    pub seq: u64,
    /// Wall-clock milliseconds (UTC) at append time. Informational only — `seq` orders events.
    pub ts_ms: i64,
    /// Groups events emitted within one agent turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<u64>,
    /// Fork provenance: set only on the first event of a forked child log, referencing the
    /// parent's `seq` at the fork point.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_seq: Option<u64>,
    /// The tagged event payload, nested under the `kind` key (spec §4.2).
    pub kind: SessionEvent,
    /// Keyed-BLAKE3 hash chain link (hex-encoded), binding this event's content and the
    /// previous event's hash (issue #6360). `None` on every event means this log predates the
    /// feature or history-chain verification is disabled for this process (legacy,
    /// auto-trusted-once per spec-069 FR-006). Additive field: `#[serde(default)]` means an
    /// older reader/writer that doesn't know this field ignores it, and legacy logs without it
    /// parse unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chain: Option<String>,
}

impl SessionEventEnvelope {
    /// Construct an envelope with `ts_ms` set to the current wall-clock time.
    #[must_use]
    pub fn new(
        seq: u64,
        turn_id: Option<u64>,
        parent_seq: Option<u64>,
        kind: SessionEvent,
    ) -> Self {
        Self {
            seq,
            ts_ms: now_ms(),
            turn_id,
            parent_seq,
            kind,
            chain: None,
        }
    }
}

/// Current wall-clock time in milliseconds since the Unix epoch, saturating on overflow.
#[must_use]
pub fn now_ms() -> i64 {
    let dur = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    i64::try_from(dur.as_millis()).unwrap_or(i64::MAX)
}

/// The kind of a persisted conversation-session event.
///
/// See `specs/068-session-persistence/spec.md` §4.3 for the full contract. `MessagePart` is
/// reused from [`zeph_llm::provider`] and `AnchoredSummary` from [`zeph_common::memory`] — this
/// enum MUST NOT redefine either.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionEvent {
    /// First event of a session's log; also written as the header line of a forked child log.
    SessionStarted {
        session_id: String,
        cwd: String,
        provider_name: String,
        model: String,
        /// `(parent_session_id, parent_seq_at_fork)`, set only for forked sessions.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        forked_from: Option<(String, u64)>,
    },
    /// A user turn.
    UserMessage {
        text: String,
        /// Content-hash refs into the session's `blobs/` directory.
        #[serde(default)]
        image_refs: Vec<String>,
    },
    /// An assistant turn.
    AssistantMessage { parts: Vec<MessagePart> },
    /// A model-initiated tool invocation.
    ToolCall {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    /// The result of a [`SessionEvent::ToolCall`]. Replay never re-executes tools; it folds this
    /// recorded output.
    ToolResult {
        id: String,
        name: String,
        output: String,
        is_error: bool,
        duration_ms: u64,
    },
    /// Durable, replayable condensation of a `seq` range (distinct from live in-memory
    /// compaction; see spec §8.1).
    Condensation {
        /// `[inclusive, inclusive]` seq range replaced by `summary`.
        replaced_seq_range: (u64, u64),
        summary: AnchoredSummary,
        tokens_before: u32,
        tokens_after: u32,
    },
    /// Recorded when live hard-compaction fires during a turn, so replay can fold the same
    /// prune/summary deterministically.
    Compaction {
        tier: CompactionTier,
        cleared_count: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summary: Option<AnchoredSummary>,
    },
    /// Non-destructive provenance record appended to the **parent** log when a child session is
    /// forked from it.
    ForkPoint { new_session_id: String },
    /// The active provider/model changed mid-session.
    ModelChanged {
        provider_name: String,
        model: String,
    },
    /// The session ended; `reason` is one of `user_quit` | `idle_ttl` | `shutdown` | `error`.
    SessionEnded { reason: String },
}

/// Which compaction threshold fired for a [`SessionEvent::Compaction`] event.
///
/// Mirrors `zeph_context::manager::CompactionTier` (soft 70% / hard 90% budget thresholds) but is
/// redefined here rather than imported: `zeph-context` is a context-assembly crate the session
/// event schema should not need to pull in just for this one enum, and the two enums are kept in
/// sync manually since compaction tiers change rarely.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionTier {
    /// Soft threshold (~70% of budget): a light prune.
    Soft,
    /// Hard threshold (~90% of budget): an aggressive prune.
    Hard,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_round_trips_through_json() {
        let envelope = SessionEventEnvelope::new(
            5,
            Some(2),
            None,
            SessionEvent::ToolResult {
                id: "t1".to_owned(),
                name: "shell".to_owned(),
                output: "ok".to_owned(),
                is_error: false,
                duration_ms: 12,
            },
        );
        let json = serde_json::to_string(&envelope).unwrap();
        let back: SessionEventEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(back.seq, 5);
        assert_eq!(back.turn_id, Some(2));
        assert!(back.parent_seq.is_none());
        assert!(matches!(back.kind, SessionEvent::ToolResult { .. }));
    }

    #[test]
    fn session_started_forked_from_round_trips() {
        let envelope = SessionEventEnvelope::new(
            0,
            None,
            Some(41),
            SessionEvent::SessionStarted {
                session_id: "child".to_owned(),
                cwd: "/tmp".to_owned(),
                provider_name: "claude".to_owned(),
                model: "opus".to_owned(),
                forked_from: Some(("parent".to_owned(), 41)),
            },
        );
        let json = serde_json::to_string(&envelope).unwrap();
        let back: SessionEventEnvelope = serde_json::from_str(&json).unwrap();
        let SessionEvent::SessionStarted { forked_from, .. } = back.kind else {
            panic!("expected SessionStarted");
        };
        assert_eq!(forked_from, Some(("parent".to_owned(), 41)));
    }
}
