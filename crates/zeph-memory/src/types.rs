// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Core identifier and tier types used throughout `zeph-memory`.

/// Memory tier classification for the AOI four-layer architecture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
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
        f.write_str(self.as_str())
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

impl std::fmt::Display for ConversationId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::fmt::Display for MessageId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
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
}
