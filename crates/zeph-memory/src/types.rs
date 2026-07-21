// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Core identifier and tier types used throughout `zeph-memory`.

/// Memory tier classification for the AOI four-layer architecture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum MemoryTier {
    /// Current conversation window. Virtual tier — not stored in the DB.
    Working,
    /// Session-bound messages. Default tier for all persisted messages.
    Episodic,
    /// Cross-session distilled facts. Promoted from Episodic when a fact
    /// appears in `promotion_min_sessions`+ distinct sessions.
    Semantic,
    /// Long-lived user attributes (preferences, domain knowledge, working style).
    /// Extracted from conversation history and injected into context (#2461).
    Persona,
}

impl MemoryTier {
    /// Return the canonical lowercase string representation.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_memory::MemoryTier;
    ///
    /// assert_eq!(MemoryTier::Episodic.as_str(), "episodic");
    /// assert_eq!(MemoryTier::Semantic.as_str(), "semantic");
    /// ```
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Working => "working",
            Self::Episodic => "episodic",
            Self::Semantic => "semantic",
            Self::Persona => "persona",
        }
    }
}

impl std::fmt::Display for MemoryTier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.pad(self.as_str())
    }
}

impl std::str::FromStr for MemoryTier {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "working" => Ok(Self::Working),
            "episodic" => Ok(Self::Episodic),
            "semantic" => Ok(Self::Semantic),
            "persona" => Ok(Self::Persona),
            other => Err(format!("unknown memory tier: {other}")),
        }
    }
}

/// Strongly typed wrapper for conversation row IDs.
///
/// Wraps the `SQLite` `conversations.id` integer primary key to prevent accidental
/// confusion with [`MessageId`] or [`MemSceneId`] values.
///
/// # Examples
///
/// ```
/// use zeph_memory::ConversationId;
///
/// let id = ConversationId(42);
/// assert_eq!(id.to_string(), "42");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, sqlx::Type)]
#[sqlx(transparent)]
pub struct ConversationId(pub i64);

impl std::fmt::Display for ConversationId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Strongly typed wrapper for message row IDs.
///
/// Wraps the `SQLite` `messages.id` integer primary key to prevent confusion
/// with [`ConversationId`] or [`MemSceneId`] values.
///
/// # Examples
///
/// ```
/// use zeph_memory::MessageId;
///
/// let id = MessageId(7);
/// assert_eq!(id.to_string(), "7");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, sqlx::Type)]
#[sqlx(transparent)]
pub struct MessageId(pub i64);

/// Strongly typed wrapper for `mem_scene` row IDs.
///
/// Wraps the `SQLite` `mem_scenes.id` integer primary key. Used by the scene
/// consolidation subsystem to identify distinct conversational scenes.
///
/// # Examples
///
/// ```
/// use zeph_memory::MemSceneId;
///
/// let id = MemSceneId(3);
/// assert_eq!(id.to_string(), "3");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, sqlx::Type)]
#[sqlx(transparent)]
pub struct MemSceneId(pub i64);

impl std::fmt::Display for MemSceneId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::fmt::Display for MessageId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Strongly typed wrapper for `experience_nodes.id` row IDs.
///
/// Prevents accidental confusion with [`EntityId`], [`ConversationId`], or [`MessageId`]
/// at experience-memory API boundaries.
///
/// # Examples
///
/// ```
/// use zeph_memory::ExperienceId;
///
/// let id = ExperienceId(10);
/// assert_eq!(id.to_string(), "10");
/// ```
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    sqlx::Type,
    serde::Serialize,
    serde::Deserialize,
)]
#[sqlx(transparent)]
pub struct ExperienceId(pub i64);

impl std::fmt::Display for ExperienceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Strongly typed wrapper for `graph_entities.id` row IDs.
///
/// Prevents confusion with [`ExperienceId`] or other integer IDs at graph-store
/// API boundaries.
///
/// # Examples
///
/// ```
/// use zeph_memory::EntityId;
///
/// let id = EntityId(5);
/// assert_eq!(id.to_string(), "5");
/// ```
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    sqlx::Type,
    serde::Serialize,
    serde::Deserialize,
)]
#[sqlx(transparent)]
pub struct EntityId(pub i64);

impl std::fmt::Display for EntityId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Discriminates which subsystem produced a [`UsageRecord`] row (issue #6549).
///
/// `Conversation` rows link to a persisted `messages.id` via [`UsageRecord::message_id`];
/// the other three variants are background/orchestration calls that never produce a
/// conversational `Message`, so `message_id` stays `None` on those rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UsageSource {
    /// A turn-loop LLM call tied to a persisted assistant message.
    Conversation,
    /// A scheduler/orchestration planner LLM call (`plan.rs`).
    Planner,
    /// A scheduler/orchestration aggregator LLM call (`plan.rs`).
    Aggregator,
    /// A verifier-ensemble member LLM call (`scheduler_loop.rs`).
    EnsembleMember,
}

impl UsageSource {
    /// Return the canonical string stored in the `usage_records.source` column.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_memory::UsageSource;
    ///
    /// assert_eq!(UsageSource::Conversation.as_str(), "conversation");
    /// ```
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Conversation => "conversation",
            Self::Planner => "planner",
            Self::Aggregator => "aggregator",
            Self::EnsembleMember => "ensemble_member",
        }
    }
}

impl std::fmt::Display for UsageSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.pad(self.as_str())
    }
}

impl std::str::FromStr for UsageSource {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "conversation" => Ok(Self::Conversation),
            "planner" => Ok(Self::Planner),
            "aggregator" => Ok(Self::Aggregator),
            "ensemble_member" => Ok(Self::EnsembleMember),
            other => Err(format!("unknown usage source: {other}")),
        }
    }
}

/// A durable per-LLM-call usage/cost/latency record (issue #6549, per-message usage tracking).
///
/// Written alongside every production call site that feeds `CostTracker::record_usage`
/// (the turn loop, planner, aggregator, and ensemble-member paths) so the sum of a UTC
/// day's rows reconciles with `CostTracker::current_spend()`. `message_id`/`conversation_id`
/// are `None` for background/orchestration rows ([`UsageSource::Planner`],
/// [`UsageSource::Aggregator`], [`UsageSource::EnsembleMember`]) that have no persisted
/// conversational `Message`.
///
/// # Examples
///
/// ```
/// use zeph_memory::{UsageRecord, UsageSource};
///
/// let record = UsageRecord {
///     message_id: None,
///     conversation_id: None,
///     source: UsageSource::Planner,
///     provider_name: "quality".to_string(),
///     model_name: "claude-sonnet-5".to_string(),
///     input_tokens: 100,
///     output_tokens: 50,
///     cache_read_tokens: 0,
///     cache_write_tokens: 0,
///     reasoning_tokens: None,
///     cost_cents: 0.05,
///     latency_ms: 800,
///     ttft_ms: None,
///     tokens_per_sec: None,
/// };
/// assert_eq!(record.source.as_str(), "planner");
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct UsageRecord {
    /// `Some` for conversational turn rows; `None` for background/orchestration rows.
    pub message_id: Option<MessageId>,
    /// `Some` whenever the conversation is known at write time; `None` for rows written
    /// outside any conversation context (e.g. a scheduled task with no active turn).
    pub conversation_id: Option<ConversationId>,
    /// Which subsystem produced this row.
    pub source: UsageSource,
    /// The `[[llm.providers]]` entry name that served the call.
    pub provider_name: String,
    /// The model identifier used for the call.
    pub model_name: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    /// Subset of `output_tokens` (`OpenAI` o-series only). `None` when the provider does
    /// not report reasoning tokens separately.
    pub reasoning_tokens: Option<u64>,
    /// Cost in cents, computed by `CostTracker::price_of` — the same pricing source of
    /// truth used by the live daily-budget aggregate.
    pub cost_cents: f64,
    /// Full call latency (request send to response fully received); always populated.
    pub latency_ms: u64,
    /// True time-to-first-token when the call streamed (currently: the speculative-decoding
    /// path only), or a time-to-first-byte proxy otherwise. `None` only for the in-process
    /// Candle backend.
    pub ttft_ms: Option<u64>,
    /// Derived throughput: `output_tokens / ((latency_ms - ttft_ms) / 1000)`. `None`
    /// unless both `ttft_ms` and a positive generation window are available.
    pub tokens_per_sec: Option<f64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_tier_round_trip() {
        for tier in [
            MemoryTier::Working,
            MemoryTier::Episodic,
            MemoryTier::Semantic,
            MemoryTier::Persona,
        ] {
            let s = tier.as_str();
            let parsed: MemoryTier = s.parse().expect("should parse");
            assert_eq!(parsed, tier);
            assert_eq!(format!("{tier}"), s);
        }
    }

    #[test]
    fn memory_tier_unknown_string_errors() {
        assert!("unknown".parse::<MemoryTier>().is_err());
    }

    /// Locks in the `f.pad` fix (#6066): `f.write_str` ignores width/fill/align flags.
    /// `f.pad` must reproduce the same padding a plain `&str` would get under an
    /// identical width specifier.
    #[test]
    fn memory_tier_display_respects_width() {
        assert_eq!(
            format!("{:<10}", MemoryTier::Working),
            format!("{:<10}", "working")
        );
        assert_eq!(
            format!("{:>10}", MemoryTier::Semantic),
            format!("{:>10}", "semantic")
        );
    }

    #[test]
    fn memory_tier_serde_round_trip() {
        let json = serde_json::to_string(&MemoryTier::Semantic).unwrap();
        assert_eq!(json, "\"semantic\"");
        let parsed: MemoryTier = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, MemoryTier::Semantic);
    }

    #[test]
    fn conversation_id_display() {
        let id = ConversationId(42);
        assert_eq!(format!("{id}"), "42");
    }

    #[test]
    fn message_id_display() {
        let id = MessageId(7);
        assert_eq!(format!("{id}"), "7");
    }

    #[test]
    fn conversation_id_eq() {
        assert_eq!(ConversationId(1), ConversationId(1));
        assert_ne!(ConversationId(1), ConversationId(2));
    }

    #[test]
    fn message_id_copy() {
        let id = MessageId(5);
        let copied = id;
        assert_eq!(id, copied);
    }

    #[test]
    fn experience_id_display() {
        let id = ExperienceId(10);
        assert_eq!(format!("{id}"), "10");
    }

    #[test]
    fn entity_id_display() {
        let id = EntityId(5);
        assert_eq!(format!("{id}"), "5");
    }

    #[test]
    fn experience_id_ord() {
        assert!(ExperienceId(1) < ExperienceId(2));
        assert_eq!(ExperienceId(3), ExperienceId(3));
    }

    #[test]
    fn entity_id_hash() {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        set.insert(EntityId(1));
        set.insert(EntityId(2));
        set.insert(EntityId(1));
        assert_eq!(set.len(), 2);
    }
}
