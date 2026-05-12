// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! EM-Graph: episodic event extraction and causal linking (issue #3713).
//!
//! The Episodic Memory Graph stores conversation events (actions, decisions, discoveries,
//! errors) as nodes connected by causal relationships. This is distinct from the
//! entity-centric MAGMA graph stored in `graph_edges` — EM-Graph captures *what happened*
//! and *why*, while MAGMA captures *what is related to what*.
//!
//! # Storage
//!
//! Events are stored in `episodic_events` and causal links in `causal_links`
//! (both created by migration 086). Messages are never deleted (spec 001-6), so
//! FK references from events to messages are always valid even after optical forgetting
//! compresses the message content.
//!
//! # Extraction
//!
//! [`extract_events`] calls an LLM to identify events in a conversation turn.
//! [`link_events`] detects causal relationships between the new events and
//! recent events in the same session.
//!
//! # Retrieval
//!
//! [`recall_episodic_causal`] finds events relevant to a query and walks the
//! causal graph to build a chain of causally-related events.

use std::sync::Arc;
use std::time::Duration;

use zeph_llm::any::AnyProvider;
use zeph_llm::provider::{LlmProvider as _, Message, MessageMetadata, Role};

pub use zeph_config::memory::EmGraphConfig;

use crate::error::MemoryError;
use crate::store::SqliteStore;
use crate::types::MessageId;

// SQLite default SQLITE_MAX_VARIABLE_NUMBER = 999. Each causal hop uses |frontier| + |visited|
// bind parameters. Cap visited to stay within the limit with any config value of max_chain_depth.
const MAX_CAUSAL_VISITED: usize = 400;

// ── Domain types ──────────────────────────────────────────────────────────────

/// An episodic event extracted from a conversation turn.
#[derive(Debug, Clone)]
pub struct EpisodicEvent {
    /// `SQLite` row ID (`0` when not yet persisted).
    pub id: i64,
    /// Session identifier this event belongs to.
    pub session_id: String,
    /// The message that triggered this event.
    pub message_id: MessageId,
    /// Short event category (e.g. `"decision"`, `"discovery"`, `"error"`, `"tool_use"`).
    pub event_type: String,
    /// One-sentence description of the event.
    pub summary: String,
    /// Optional embedding blob (populated when vector search is enabled).
    pub embedding: Option<Vec<u8>>,
    /// Unix timestamp of creation.
    pub created_at: i64,
}

/// A directed causal link between two episodic events.
#[derive(Debug, Clone)]
pub struct CausalLink {
    /// `SQLite` row ID (`0` when not yet persisted).
    pub id: i64,
    /// Source (cause) event ID.
    pub cause_event_id: i64,
    /// Target (effect) event ID.
    pub effect_event_id: i64,
    /// Causal strength in [0.0, 1.0].
    pub strength: f32,
    /// Unix timestamp of creation.
    pub created_at: i64,
}

// ── Event extraction ──────────────────────────────────────────────────────────

/// Extract episodic events from a conversation turn via LLM.
///
/// Returns a list of events identified in `content`. The caller is responsible for
/// persisting the events via [`store_events`].
///
/// Falls back to an empty list on LLM failure (fail-open: missing events are a
/// quality degradation, not a correctness error).
///
/// # Errors
///
/// Returns an error only if the LLM call produces an error that cannot be recovered.
/// Network timeouts and parse failures return an empty list instead.
pub async fn extract_events(
    provider: &Arc<AnyProvider>,
    content: &str,
    session_id: &str,
    message_id: MessageId,
    config: &EmGraphConfig,
) -> Vec<EpisodicEvent> {
    let _span =
        tracing::debug_span!("memory.em_graph.extract_events", message_id = message_id.0).entered();

    if !config.enabled {
        return vec![];
    }

    let snippet = content.chars().take(2000).collect::<String>();

    let prompt = format!(
        "Identify episodic events in the following conversation turn. \
        An event is a concrete action, decision, discovery, or error. \
        Return a JSON array of objects with fields: \
        {{\"event_type\": \"<type>\", \"summary\": \"<one sentence>\"}}. \
        Types: decision, discovery, error, tool_use, question, answer, other. \
        Return [] if no notable events. Output JSON only.\n\nTurn:\n{snippet}"
    );

    let messages = vec![
        Message {
            role: Role::System,
            content: "You are an episodic memory extractor. Extract concrete events from \
                      conversation turns as structured JSON. Output only valid JSON, no preamble."
                .to_owned(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        },
        Message {
            role: Role::User,
            content: prompt,
            parts: vec![],
            metadata: MessageMetadata::default(),
        },
    ];

    let raw = match tokio::time::timeout(Duration::from_secs(10), provider.chat(&messages)).await {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "em_graph: event extraction LLM call failed");
            return vec![];
        }
        Err(_) => {
            tracing::warn!("em_graph: event extraction timed out");
            return vec![];
        }
    };

    parse_events_response(&raw, session_id, message_id)
}

fn parse_events_response(raw: &str, session_id: &str, message_id: MessageId) -> Vec<EpisodicEvent> {
    let json_str = raw
        .find('[')
        .and_then(|s| raw[s..].rfind(']').map(|e| &raw[s..=s + e]))
        .unwrap_or("[]");

    let values: Vec<serde_json::Value> = serde_json::from_str(json_str).unwrap_or_default();

    values
        .into_iter()
        .filter_map(|v| {
            let event_type = v.get("event_type")?.as_str()?.to_owned();
            let summary = v.get("summary")?.as_str()?.to_owned();
            if summary.is_empty() {
                return None;
            }
            Some(EpisodicEvent {
                id: 0,
                session_id: session_id.to_owned(),
                message_id,
                event_type,
                summary,
                embedding: None,
                created_at: 0,
            })
        })
        .collect()
}

// ── Causal link detection ──────────────────────────────────────────────────────

/// Detect causal links between `new_events` and `recent_events` via LLM.
///
/// Returns a list of causal links. The caller is responsible for persisting them
/// via [`store_links`] after persisting the events via [`store_events`].
///
/// **Ordering requirement**: `store_events` MUST be called before `link_events` so
/// that `new_events[i].id` reflects the real database row ID. The LLM prompt embeds
/// event IDs; IDs of 0 (pre-persistence default) will produce links that cannot be
/// matched after `store_events` assigns the real IDs.
///
/// Returns an empty list on LLM failure (fail-open).
pub async fn link_events(
    provider: &Arc<AnyProvider>,
    new_events: &[EpisodicEvent],
    recent_events: &[EpisodicEvent],
    config: &EmGraphConfig,
) -> Vec<CausalLink> {
    let _span = tracing::debug_span!(
        "memory.em_graph.link_events",
        new_count = new_events.len(),
        recent_count = recent_events.len()
    )
    .entered();

    if !config.enabled || new_events.is_empty() || recent_events.is_empty() {
        return vec![];
    }

    // Summaries are stored LLM output; cap length to limit prompt injection surface.
    let new_desc: Vec<String> = new_events
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let s: String = e.summary.chars().take(200).collect();
            format!("NEW[{i}] (id={}): {s}", e.id)
        })
        .collect();

    let recent_desc: Vec<String> = recent_events
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let s: String = e.summary.chars().take(200).collect();
            format!("RECENT[{i}] (id={}): {s}", e.id)
        })
        .collect();

    let prompt = format!(
        "Given these recent events and new events, identify causal relationships \
        (cause → effect). Return a JSON array of objects: \
        {{\"cause_id\": <event_id>, \"effect_id\": <event_id>, \"strength\": 0.0-1.0}}. \
        Only include strong causal links (strength >= 0.5). Output [] if none.\n\n\
        Recent events:\n{}\n\nNew events:\n{}",
        recent_desc.join("\n"),
        new_desc.join("\n"),
    );

    let messages = vec![
        Message {
            role: Role::System,
            content: "You are a causal reasoning engine. Identify cause-and-effect \
                      relationships between events. Output only valid JSON."
                .to_owned(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        },
        Message {
            role: Role::User,
            content: prompt,
            parts: vec![],
            metadata: MessageMetadata::default(),
        },
    ];

    let raw = match tokio::time::timeout(Duration::from_secs(10), provider.chat(&messages)).await {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "em_graph: causal link LLM call failed");
            return vec![];
        }
        Err(_) => {
            tracing::warn!("em_graph: causal link detection timed out");
            return vec![];
        }
    };

    parse_links_response(&raw)
}

fn parse_links_response(raw: &str) -> Vec<CausalLink> {
    let json_str = raw
        .find('[')
        .and_then(|s| raw[s..].rfind(']').map(|e| &raw[s..=s + e]))
        .unwrap_or("[]");

    let values: Vec<serde_json::Value> = serde_json::from_str(json_str).unwrap_or_default();

    values
        .into_iter()
        .filter_map(|v| {
            let cause_id = v.get("cause_id")?.as_i64()?;
            let effect_id = v.get("effect_id")?.as_i64()?;
            #[allow(clippy::cast_possible_truncation)]
            let strength = v
                .get("strength")
                .and_then(serde_json::Value::as_f64)
                .map_or(0.5, |s| s.clamp(0.0, 1.0) as f32);
            if strength < 0.5 {
                return None;
            }
            Some(CausalLink {
                id: 0,
                cause_event_id: cause_id,
                effect_event_id: effect_id,
                strength,
                created_at: 0,
            })
        })
        .collect()
}

// ── Persistence ───────────────────────────────────────────────────────────────

/// Persist extracted events to the `episodic_events` table.
///
/// All inserts are batched inside a single transaction. On success, `events[i].id`
/// is updated to the assigned row ID.
///
/// # Errors
///
/// Returns an error if any insert or the transaction commit fails.
pub async fn store_events(
    store: &SqliteStore,
    events: &mut [EpisodicEvent],
) -> Result<(), MemoryError> {
    if events.is_empty() {
        return Ok(());
    }
    let mut tx = store.pool().begin().await?;
    for event in events.iter_mut() {
        let id = sqlx::query_scalar::<_, i64>(
            "INSERT INTO episodic_events (session_id, message_id, event_type, summary, created_at)
             VALUES (?, ?, ?, ?, unixepoch())
             RETURNING id",
        )
        .bind(&event.session_id)
        .bind(event.message_id.0)
        .bind(&event.event_type)
        .bind(&event.summary)
        .fetch_one(&mut *tx)
        .await?;
        event.id = id;
    }
    tx.commit().await?;
    Ok(())
}

/// Persist causal links to the `causal_links` table.
///
/// All inserts are batched inside a single transaction. Duplicate
/// `(cause_event_id, effect_event_id)` pairs are silently ignored via `INSERT OR IGNORE`
/// (requires the `UNIQUE` constraint added in migration 086).
///
/// # Errors
///
/// Returns an error if any insert or the transaction commit fails.
pub async fn store_links(store: &SqliteStore, links: &[CausalLink]) -> Result<(), MemoryError> {
    if links.is_empty() {
        return Ok(());
    }
    let mut tx = store.pool().begin().await?;
    for link in links {
        sqlx::query(
            "INSERT OR IGNORE INTO causal_links
             (cause_event_id, effect_event_id, strength, created_at)
             VALUES (?, ?, ?, unixepoch())",
        )
        .bind(link.cause_event_id)
        .bind(link.effect_event_id)
        .bind(link.strength)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

// ── Recall ────────────────────────────────────────────────────────────────────

/// Retrieve recent events for a session (context for causal linking).
///
/// Returns the most recent `limit` events ordered by creation time descending.
///
/// # Errors
///
/// Returns an error if the database query fails.
pub async fn fetch_recent_events(
    store: &SqliteStore,
    session_id: &str,
    limit: usize,
) -> Result<Vec<EpisodicEvent>, MemoryError> {
    let rows = sqlx::query_as::<_, (i64, String, i64, String, String, i64)>(
        "SELECT id, session_id, message_id, event_type, summary, created_at
         FROM episodic_events
         WHERE session_id = ?
         ORDER BY created_at DESC
         LIMIT ?",
    )
    .bind(session_id)
    .bind(i64::try_from(limit).unwrap_or(i64::MAX))
    .fetch_all(store.pool())
    .await?;

    Ok(rows
        .into_iter()
        .map(
            |(id, session_id, message_id, event_type, summary, created_at)| EpisodicEvent {
                id,
                session_id,
                message_id: MessageId(message_id),
                event_type,
                summary,
                embedding: None,
                created_at,
            },
        )
        .collect())
}

/// Retrieve a causal chain of events starting from a seed event.
///
/// Walks `causal_links` forward (cause → effect) up to `max_depth` hops,
/// collecting the causally-connected event chain ordered by event ID.
///
/// # Errors
///
/// Returns an error if any database query fails.
pub async fn recall_episodic_causal(
    store: &SqliteStore,
    seed_event_id: i64,
    session_id: &str,
    max_depth: u32,
    config: &EmGraphConfig,
) -> Result<Vec<EpisodicEvent>, MemoryError> {
    let _span =
        tracing::debug_span!("memory.em_graph.causal_recall", seed_event_id, max_depth).entered();

    if !config.enabled {
        return Ok(vec![]);
    }

    let mut visited: Vec<i64> = vec![seed_event_id];
    let mut frontier: Vec<i64> = vec![seed_event_id];

    for depth in 0..max_depth {
        if frontier.is_empty() || visited.len() >= MAX_CAUSAL_VISITED {
            break;
        }

        let frontier_ph = frontier.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let visited_ph = visited.iter().map(|_| "?").collect::<Vec<_>>().join(",");

        let query = format!(
            "SELECT DISTINCT effect_event_id FROM causal_links
             WHERE cause_event_id IN ({frontier_ph})
               AND effect_event_id NOT IN ({visited_ph})"
        );

        let mut q = sqlx::query_scalar::<_, i64>(&query);
        for &id in &frontier {
            q = q.bind(id);
        }
        for &id in &visited {
            q = q.bind(id);
        }

        let next: Vec<i64> = q.fetch_all(store.pool()).await?;

        tracing::debug!(depth, next_count = next.len(), "em_graph: causal hop");
        visited.extend_from_slice(&next);
        frontier = next;
    }

    if visited.is_empty() {
        return Ok(vec![]);
    }

    // Fetch all collected events ordered by creation time.
    let placeholders = visited.iter().map(|_| "?").collect::<Vec<_>>().join(",");

    let query = format!(
        "SELECT id, session_id, message_id, event_type, summary, created_at
         FROM episodic_events
         WHERE id IN ({placeholders}) AND session_id = ?
         ORDER BY created_at ASC"
    );

    let mut q = sqlx::query_as::<_, (i64, String, i64, String, String, i64)>(&query);
    for &id in &visited {
        q = q.bind(id);
    }
    q = q.bind(session_id);

    let rows = q.fetch_all(store.pool()).await?;

    Ok(rows
        .into_iter()
        .map(
            |(id, session_id, message_id, event_type, summary, created_at)| EpisodicEvent {
                id,
                session_id,
                message_id: MessageId(message_id),
                event_type,
                summary,
                embedding: None,
                created_at,
            },
        )
        .collect())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use zeph_config::providers::ProviderName;

    #[test]
    fn parse_events_response_valid_json() {
        let raw = r#"[{"event_type":"decision","summary":"User chose approach A"},{"event_type":"discovery","summary":"Found a bug in module X"}]"#;
        let events = parse_events_response(raw, "sess-1", MessageId(42));
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event_type, "decision");
        assert_eq!(events[1].summary, "Found a bug in module X");
        assert_eq!(events[0].message_id, MessageId(42));
        assert_eq!(events[0].session_id, "sess-1");
    }

    #[test]
    fn parse_events_response_empty_array() {
        let events = parse_events_response("[]", "sess-1", MessageId(1));
        assert!(events.is_empty());
    }

    #[test]
    fn parse_events_response_malformed_json() {
        let events = parse_events_response("not json", "sess-1", MessageId(1));
        assert!(events.is_empty());
    }

    #[test]
    fn parse_events_response_skips_empty_summary() {
        let raw = r#"[{"event_type":"decision","summary":""}]"#;
        let events = parse_events_response(raw, "sess-1", MessageId(1));
        assert!(events.is_empty(), "empty summary must be skipped");
    }

    #[test]
    fn parse_links_response_valid_json() {
        let raw = r#"[{"cause_id":1,"effect_id":2,"strength":0.8}]"#;
        let links = parse_links_response(raw);
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].cause_event_id, 1);
        assert_eq!(links[0].effect_event_id, 2);
        assert!((links[0].strength - 0.8).abs() < 0.01);
    }

    #[test]
    fn parse_links_response_filters_weak_links() {
        let raw = r#"[{"cause_id":1,"effect_id":2,"strength":0.3}]"#;
        let links = parse_links_response(raw);
        assert!(
            links.is_empty(),
            "weak links (strength < 0.5) must be filtered"
        );
    }

    #[test]
    fn parse_links_response_empty() {
        let links = parse_links_response("[]");
        assert!(links.is_empty());
    }

    #[test]
    fn em_graph_config_defaults() {
        let cfg = EmGraphConfig::default();
        assert!(!cfg.enabled);
        assert_eq!(cfg.max_chain_depth, 3);
    }

    #[tokio::test]
    async fn store_and_fetch_events_in_memory_db() {
        use crate::store::SqliteStore;

        let store = SqliteStore::new(":memory:")
            .await
            .expect("SqliteStore::new");
        let cid = store.create_conversation().await.expect("conversation");
        let mid = store
            .save_message(cid, "user", "hello world")
            .await
            .expect("save_message");

        let mut events = vec![EpisodicEvent {
            id: 0,
            session_id: "test-session".to_owned(),
            message_id: mid,
            event_type: "decision".to_owned(),
            summary: "User decided to use approach A".to_owned(),
            embedding: None,
            created_at: 0,
        }];

        store_events(&store, &mut events)
            .await
            .expect("store_events");
        assert!(events[0].id > 0, "id must be assigned after insert");

        let fetched = fetch_recent_events(&store, "test-session", 10)
            .await
            .expect("fetch_recent_events");
        assert_eq!(fetched.len(), 1);
        assert_eq!(fetched[0].summary, "User decided to use approach A");
    }

    #[tokio::test]
    async fn store_and_recall_causal_chain() {
        use crate::store::SqliteStore;

        let store = SqliteStore::new(":memory:")
            .await
            .expect("SqliteStore::new");
        let cid = store.create_conversation().await.expect("conversation");
        let mid = store
            .save_message(cid, "user", "test")
            .await
            .expect("save_message");

        let mut events = vec![
            EpisodicEvent {
                id: 0,
                session_id: "sess".to_owned(),
                message_id: mid,
                event_type: "discovery".to_owned(),
                summary: "Found a bug".to_owned(),
                embedding: None,
                created_at: 0,
            },
            EpisodicEvent {
                id: 0,
                session_id: "sess".to_owned(),
                message_id: mid,
                event_type: "decision".to_owned(),
                summary: "Decided to fix it".to_owned(),
                embedding: None,
                created_at: 0,
            },
        ];
        store_events(&store, &mut events)
            .await
            .expect("store_events");

        let link = CausalLink {
            id: 0,
            cause_event_id: events[0].id,
            effect_event_id: events[1].id,
            strength: 0.9,
            created_at: 0,
        };
        store_links(&store, &[link]).await.expect("store_links");

        let config = EmGraphConfig {
            enabled: true,
            extract_provider: ProviderName::default(),
            max_chain_depth: 3,
        };
        let chain = recall_episodic_causal(&store, events[0].id, "sess", 3, &config)
            .await
            .expect("recall_episodic_causal");

        assert_eq!(
            chain.len(),
            2,
            "chain must include seed and causally-linked event"
        );
    }

    #[test]
    fn parse_links_response_strength_at_boundary_included() {
        // strength == 0.5 is exactly at the threshold — must be included (filter is `< 0.5`)
        let raw = r#"[{"cause_id":1,"effect_id":2,"strength":0.5}]"#;
        let links = parse_links_response(raw);
        assert_eq!(
            links.len(),
            1,
            "strength=0.5 must be included (threshold is strict < 0.5)"
        );
        assert!((links[0].strength - 0.5).abs() < 0.001);
    }

    #[tokio::test]
    async fn recall_episodic_causal_disabled_returns_empty() {
        use crate::store::SqliteStore;

        let store = SqliteStore::new(":memory:")
            .await
            .expect("SqliteStore::new");
        let config = EmGraphConfig {
            enabled: false,
            extract_provider: ProviderName::default(),
            max_chain_depth: 3,
        };
        let result = recall_episodic_causal(&store, 1, "sess", 3, &config).await;
        assert!(result.is_ok());
        assert!(
            result.unwrap().is_empty(),
            "disabled config must return empty"
        );
    }

    #[tokio::test]
    async fn store_links_is_idempotent_with_unique_constraint() {
        use crate::store::SqliteStore;

        let store = SqliteStore::new(":memory:")
            .await
            .expect("SqliteStore::new");
        let cid = store.create_conversation().await.expect("conversation");
        let mid = store
            .save_message(cid, "user", "test")
            .await
            .expect("save_message");

        let mut events = vec![
            EpisodicEvent {
                id: 0,
                session_id: "sess".to_owned(),
                message_id: mid,
                event_type: "decision".to_owned(),
                summary: "A".to_owned(),
                embedding: None,
                created_at: 0,
            },
            EpisodicEvent {
                id: 0,
                session_id: "sess".to_owned(),
                message_id: mid,
                event_type: "discovery".to_owned(),
                summary: "B".to_owned(),
                embedding: None,
                created_at: 0,
            },
        ];
        store_events(&store, &mut events)
            .await
            .expect("store_events");

        let link = CausalLink {
            id: 0,
            cause_event_id: events[0].id,
            effect_event_id: events[1].id,
            strength: 0.8,
            created_at: 0,
        };
        // Insert twice — second must be ignored, not duplicated.
        store_links(&store, std::slice::from_ref(&link))
            .await
            .expect("first store_links");
        store_links(&store, &[link])
            .await
            .expect("second store_links (idempotent)");

        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM causal_links WHERE cause_event_id = ? AND effect_event_id = ?",
        )
        .bind(events[0].id)
        .bind(events[1].id)
        .fetch_one(store.pool())
        .await
        .expect("count query");

        assert_eq!(
            count, 1,
            "duplicate causal links must be deduplicated by UNIQUE constraint"
        );
    }
}
