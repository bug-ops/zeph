// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use super::store::GraphStore;
use super::types::EntityType;
use crate::error::MemoryError;
use crate::types::MessageId;

/// Maximum byte length for entity names stored in the graph.
const MAX_ENTITY_NAME_BYTES: usize = 512;
/// Maximum byte length for relation strings.
const MAX_RELATION_BYTES: usize = 256;
/// Maximum byte length for fact strings.
const MAX_FACT_BYTES: usize = 2048;

/// Strip ASCII control characters and Unicode `BiDi` override codepoints.
fn strip_control_chars(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_control() && !matches!(*c as u32, 0x202A..=0x202E | 0x2066..=0x2069))
        .collect()
}

/// Truncate a string to at most `max_bytes` bytes at a valid UTF-8 char boundary.
fn truncate_to_bytes(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut boundary = max_bytes;
    while !s.is_char_boundary(boundary) {
        boundary -= 1;
    }
    &s[..boundary]
}

pub struct EntityResolver<'a> {
    store: &'a GraphStore,
}

impl<'a> EntityResolver<'a> {
    #[must_use]
    pub fn new(store: &'a GraphStore) -> Self {
        Self { store }
    }

    /// Resolve an extracted entity using the alias-first canonicalization pipeline.
    ///
    /// Pipeline:
    /// 1. Normalize: trim, lowercase, strip control chars, truncate to 512 bytes.
    /// 2. Parse entity type (fallback to Concept on unknown).
    /// 3. Alias lookup: search `graph_entity_aliases` by normalized name + `entity_type`.
    ///    If found, touch `last_seen_at` and return the existing entity id.
    /// 4. Canonical name lookup: search `graph_entities` by `canonical_name` + `entity_type`.
    ///    If found, touch `last_seen_at` and return the existing entity id.
    /// 5. Create: upsert new entity with `canonical_name` = normalized name.
    /// 6. Register the normalized form (and original trimmed form if different) as aliases.
    ///
    /// NOTE: Steps 3-6 are not wrapped in an explicit transaction. `SQLite` serializes all writes
    /// (single-writer WAL), so concurrent data corruption is impossible. Two concurrent calls with
    /// the same name would both reach step 5, where `ON CONFLICT(canonical_name, entity_type)`
    /// makes the second call a no-op update, and step 6 is idempotent via `INSERT OR IGNORE`.
    ///
    /// # Errors
    ///
    /// Returns an error if the entity name is empty after normalization, or if the DB operation fails.
    pub async fn resolve(
        &self,
        name: &str,
        entity_type: &str,
        summary: Option<&str>,
    ) -> Result<i64, MemoryError> {
        let normalized = Self::normalize_name(name);

        if normalized.is_empty() {
            return Err(MemoryError::GraphStore("empty entity name".into()));
        }

        let et = Self::parse_entity_type(entity_type);

        // The surface form preserves the original casing for user-facing display.
        let surface_name = name.trim().to_owned();

        // Step 3: alias-first lookup (filters by entity_type to prevent cross-type collisions).
        if let Some(entity) = self.store.find_entity_by_alias(&normalized, et).await? {
            self.store
                .upsert_entity(&surface_name, &entity.canonical_name, et, summary)
                .await?;
            return Ok(entity.id);
        }

        // Step 4: canonical name lookup.
        if let Some(entity) = self.store.find_entity(&normalized, et).await? {
            self.store
                .upsert_entity(&surface_name, &entity.canonical_name, et, summary)
                .await?;
            return Ok(entity.id);
        }

        // Step 5: no match — create new entity with canonical_name = normalized.
        let id = self
            .store
            .upsert_entity(&surface_name, &normalized, et, summary)
            .await?;

        // Step 6: register the normalized form as alias.
        self.store.add_alias(id, &normalized).await?;

        // Also register the original trimmed lowercased form if it differs from normalized
        // (e.g. when control chars were stripped, leaving a shorter string).
        // Apply same truncation as normalize_name for a consistent security boundary (Fix 3).
        let original_trimmed = name.trim().to_lowercase();
        let original_clean_str = strip_control_chars(&original_trimmed);
        let original_clean = truncate_to_bytes(&original_clean_str, MAX_ENTITY_NAME_BYTES);
        if original_clean != normalized {
            self.store.add_alias(id, original_clean).await?;
        }

        Ok(id)
    }

    fn normalize_name(name: &str) -> String {
        let lowered = name.trim().to_lowercase();
        let cleaned = strip_control_chars(&lowered);
        let normalized = truncate_to_bytes(&cleaned, MAX_ENTITY_NAME_BYTES).to_owned();
        if normalized.len() < cleaned.len() {
            tracing::debug!(
                "graph resolver: entity name truncated to {} bytes",
                MAX_ENTITY_NAME_BYTES
            );
        }
        normalized
    }

    fn parse_entity_type(entity_type: &str) -> EntityType {
        entity_type
            .trim()
            .to_lowercase()
            .parse::<EntityType>()
            .unwrap_or_else(|_| {
                tracing::debug!(
                    "graph resolver: unknown entity type {:?}, falling back to Concept",
                    entity_type
                );
                EntityType::Concept
            })
    }

    /// Resolve an extracted edge: deduplicate or supersede existing edges.
    ///
    /// - If an active edge with the same direction and relation exists with an identical fact,
    ///   returns `None` (deduplicated).
    /// - If an active edge with the same direction and relation exists with a different fact,
    ///   invalidates the old edge and inserts the new one, returning `Some(new_id)`.
    /// - If no matching edge exists, inserts a new edge and returns `Some(new_id)`.
    ///
    /// Relation and fact strings are sanitized (control chars stripped, length-capped).
    ///
    /// # Errors
    ///
    /// Returns an error if any database operation fails.
    pub async fn resolve_edge(
        &self,
        source_id: i64,
        target_id: i64,
        relation: &str,
        fact: &str,
        confidence: f32,
        episode_id: Option<MessageId>,
    ) -> Result<Option<i64>, MemoryError> {
        let relation_clean = strip_control_chars(&relation.trim().to_lowercase());
        let normalized_relation = truncate_to_bytes(&relation_clean, MAX_RELATION_BYTES).to_owned();

        let fact_clean = strip_control_chars(fact.trim());
        let normalized_fact = truncate_to_bytes(&fact_clean, MAX_FACT_BYTES).to_owned();

        // Fetch only exact-direction edges — no reverse edges to filter out
        let existing_edges = self.store.edges_exact(source_id, target_id).await?;

        let matching = existing_edges
            .iter()
            .find(|e| e.relation == normalized_relation);

        if let Some(old) = matching {
            if old.fact == normalized_fact {
                // Exact duplicate — skip
                return Ok(None);
            }
            // Same relation, different fact — supersede
            self.store.invalidate_edge(old.id).await?;
        }

        let new_id = self
            .store
            .insert_edge(
                source_id,
                target_id,
                &normalized_relation,
                &normalized_fact,
                confidence,
                episode_id,
            )
            .await?;
        Ok(Some(new_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sqlite::SqliteStore;

    async fn setup() -> GraphStore {
        let store = SqliteStore::new(":memory:").await.unwrap();
        GraphStore::new(store.pool().clone())
    }

    #[tokio::test]
    async fn resolve_creates_new_entity() {
        let gs = setup().await;
        let resolver = EntityResolver::new(&gs);
        let id = resolver
            .resolve("alice", "person", Some("a person"))
            .await
            .unwrap();
        assert!(id > 0);
    }

    #[tokio::test]
    async fn resolve_updates_existing_entity() {
        let gs = setup().await;
        let resolver = EntityResolver::new(&gs);
        let id1 = resolver.resolve("alice", "person", None).await.unwrap();
        let id2 = resolver
            .resolve("alice", "person", Some("updated summary"))
            .await
            .unwrap();
        assert_eq!(id1, id2);

        let entity = gs
            .find_entity("alice", EntityType::Person)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(entity.summary.as_deref(), Some("updated summary"));
    }

    #[tokio::test]
    async fn resolve_unknown_type_falls_back_to_concept() {
        let gs = setup().await;
        let resolver = EntityResolver::new(&gs);
        let id = resolver
            .resolve("my_thing", "unknown_type", None)
            .await
            .unwrap();
        assert!(id > 0);

        // Verify it was stored as Concept
        let entity = gs
            .find_entity("my_thing", EntityType::Concept)
            .await
            .unwrap();
        assert!(entity.is_some());
    }

    #[tokio::test]
    async fn resolve_empty_name_returns_error() {
        let gs = setup().await;
        let resolver = EntityResolver::new(&gs);

        let result_empty = resolver.resolve("", "concept", None).await;
        assert!(result_empty.is_err());
        assert!(matches!(
            result_empty.unwrap_err(),
            MemoryError::GraphStore(_)
        ));

        let result_whitespace = resolver.resolve("   ", "concept", None).await;
        assert!(result_whitespace.is_err());
    }

    #[tokio::test]
    async fn resolve_case_insensitive() {
        let gs = setup().await;
        let resolver = EntityResolver::new(&gs);

        let id1 = resolver.resolve("Rust", "language", None).await.unwrap();
        let id2 = resolver.resolve("rust", "language", None).await.unwrap();
        assert_eq!(
            id1, id2,
            "'Rust' and 'rust' should resolve to the same entity"
        );
    }

    #[tokio::test]
    async fn resolve_edge_inserts_new() {
        let gs = setup().await;
        let resolver = EntityResolver::new(&gs);

        let src = gs
            .upsert_entity("src", "src", EntityType::Concept, None)
            .await
            .unwrap();
        let tgt = gs
            .upsert_entity("tgt", "tgt", EntityType::Concept, None)
            .await
            .unwrap();

        let result = resolver
            .resolve_edge(src, tgt, "uses", "src uses tgt", 0.9, None)
            .await
            .unwrap();
        assert!(result.is_some());
        assert!(result.unwrap() > 0);
    }

    #[tokio::test]
    async fn resolve_edge_deduplicates_identical() {
        let gs = setup().await;
        let resolver = EntityResolver::new(&gs);

        let src = gs
            .upsert_entity("a", "a", EntityType::Concept, None)
            .await
            .unwrap();
        let tgt = gs
            .upsert_entity("b", "b", EntityType::Concept, None)
            .await
            .unwrap();

        let first = resolver
            .resolve_edge(src, tgt, "uses", "a uses b", 0.9, None)
            .await
            .unwrap();
        assert!(first.is_some());

        let second = resolver
            .resolve_edge(src, tgt, "uses", "a uses b", 0.9, None)
            .await
            .unwrap();
        assert!(second.is_none(), "identical edge should be deduplicated");
    }

    #[tokio::test]
    async fn resolve_edge_supersedes_contradictory() {
        let gs = setup().await;
        let resolver = EntityResolver::new(&gs);

        let src = gs
            .upsert_entity("x", "x", EntityType::Concept, None)
            .await
            .unwrap();
        let tgt = gs
            .upsert_entity("y", "y", EntityType::Concept, None)
            .await
            .unwrap();

        let first_id = resolver
            .resolve_edge(src, tgt, "prefers", "x prefers y (old)", 0.8, None)
            .await
            .unwrap()
            .unwrap();

        let second_id = resolver
            .resolve_edge(src, tgt, "prefers", "x prefers y (new)", 0.9, None)
            .await
            .unwrap()
            .unwrap();

        assert_ne!(first_id, second_id, "superseded edge should have a new ID");

        // Old edge should be invalidated
        let active_count = gs.active_edge_count().await.unwrap();
        assert_eq!(active_count, 1, "only new edge should be active");
    }

    #[tokio::test]
    async fn resolve_edge_direction_sensitive() {
        // A->B "uses" should not interfere with B->A "uses" dedup
        let gs = setup().await;
        let resolver = EntityResolver::new(&gs);

        let a = gs
            .upsert_entity("node_a", "node_a", EntityType::Concept, None)
            .await
            .unwrap();
        let b = gs
            .upsert_entity("node_b", "node_b", EntityType::Concept, None)
            .await
            .unwrap();

        // Insert A->B
        let id1 = resolver
            .resolve_edge(a, b, "uses", "A uses B", 0.9, None)
            .await
            .unwrap();
        assert!(id1.is_some());

        // Insert B->A with different fact — should NOT invalidate A->B (different direction)
        let id2 = resolver
            .resolve_edge(b, a, "uses", "B uses A (different direction)", 0.9, None)
            .await
            .unwrap();
        assert!(id2.is_some());

        // Both edges should still be active
        let active_count = gs.active_edge_count().await.unwrap();
        assert_eq!(active_count, 2, "both directional edges should be active");
    }

    #[tokio::test]
    async fn resolve_edge_normalizes_relation_case() {
        let gs = setup().await;
        let resolver = EntityResolver::new(&gs);

        let src = gs
            .upsert_entity("p", "p", EntityType::Concept, None)
            .await
            .unwrap();
        let tgt = gs
            .upsert_entity("q", "q", EntityType::Concept, None)
            .await
            .unwrap();

        // Insert with uppercase relation
        let id1 = resolver
            .resolve_edge(src, tgt, "Uses", "p uses q", 0.9, None)
            .await
            .unwrap();
        assert!(id1.is_some());

        // Insert with lowercase relation — same normalized relation, same fact → deduplicate
        let id2 = resolver
            .resolve_edge(src, tgt, "uses", "p uses q", 0.9, None)
            .await
            .unwrap();
        assert!(id2.is_none(), "normalized relations should deduplicate");
    }

    // ── IC-01: entity_type lowercased before parse ────────────────────────────

    #[tokio::test]
    async fn resolve_entity_type_uppercase_parsed_correctly() {
        let gs = setup().await;
        let resolver = EntityResolver::new(&gs);

        // "Person" (title case from LLM) should parse as EntityType::Person, not fall back to Concept
        let id = resolver
            .resolve("test_entity", "Person", None)
            .await
            .unwrap();
        assert!(id > 0);

        let entity = gs
            .find_entity("test_entity", EntityType::Person)
            .await
            .unwrap();
        assert!(entity.is_some(), "entity should be stored as Person type");
    }

    #[tokio::test]
    async fn resolve_entity_type_all_caps_parsed_correctly() {
        let gs = setup().await;
        let resolver = EntityResolver::new(&gs);

        let id = resolver.resolve("my_lang", "LANGUAGE", None).await.unwrap();
        assert!(id > 0);

        let entity = gs
            .find_entity("my_lang", EntityType::Language)
            .await
            .unwrap();
        assert!(entity.is_some(), "entity should be stored as Language type");
    }

    // ── SEC-GRAPH-01: entity name length cap ──────────────────────────────────

    #[tokio::test]
    async fn resolve_truncates_long_entity_name() {
        let gs = setup().await;
        let resolver = EntityResolver::new(&gs);

        let long_name = "a".repeat(1024);
        let id = resolver.resolve(&long_name, "concept", None).await.unwrap();
        assert!(id > 0);

        // Entity should exist with a truncated name (512 bytes)
        let entity = gs
            .find_entity(&"a".repeat(512), EntityType::Concept)
            .await
            .unwrap();
        assert!(entity.is_some(), "truncated name should be stored");
    }

    // ── SEC-GRAPH-02: control character stripping ─────────────────────────────

    #[tokio::test]
    async fn resolve_strips_control_chars_from_name() {
        let gs = setup().await;
        let resolver = EntityResolver::new(&gs);

        // Name with null byte and a BiDi override
        let name_with_ctrl = "rust\x00lang";
        let id = resolver
            .resolve(name_with_ctrl, "language", None)
            .await
            .unwrap();
        assert!(id > 0);

        // Stored name should have control chars removed
        let entity = gs
            .find_entity("rustlang", EntityType::Language)
            .await
            .unwrap();
        assert!(
            entity.is_some(),
            "control chars should be stripped from stored name"
        );
    }

    #[tokio::test]
    async fn resolve_strips_bidi_overrides_from_name() {
        let gs = setup().await;
        let resolver = EntityResolver::new(&gs);

        // U+202E is RIGHT-TO-LEFT OVERRIDE — a BiDi spoof character
        let name_with_bidi = "rust\u{202E}lang";
        let id = resolver
            .resolve(name_with_bidi, "language", None)
            .await
            .unwrap();
        assert!(id > 0);

        let entity = gs
            .find_entity("rustlang", EntityType::Language)
            .await
            .unwrap();
        assert!(entity.is_some(), "BiDi override chars should be stripped");
    }

    // ── Helper unit tests for sanitization functions ──────────────────────────

    #[test]
    fn strip_control_chars_removes_ascii_controls() {
        assert_eq!(strip_control_chars("hello\x00world"), "helloworld");
        assert_eq!(strip_control_chars("tab\there"), "tabhere");
        assert_eq!(strip_control_chars("new\nline"), "newline");
    }

    #[test]
    fn strip_control_chars_removes_bidi() {
        let bidi = "\u{202E}spoof";
        assert_eq!(strip_control_chars(bidi), "spoof");
    }

    #[test]
    fn strip_control_chars_preserves_normal_unicode() {
        assert_eq!(strip_control_chars("привет мир"), "привет мир");
        assert_eq!(strip_control_chars("日本語"), "日本語");
    }

    #[test]
    fn truncate_to_bytes_exact_boundary() {
        let s = "hello";
        assert_eq!(truncate_to_bytes(s, 5), "hello");
        assert_eq!(truncate_to_bytes(s, 3), "hel");
    }

    #[test]
    fn truncate_to_bytes_respects_utf8_boundary() {
        // "é" is 2 bytes in UTF-8 — truncating at 1 byte should give ""
        let s = "élan";
        let truncated = truncate_to_bytes(s, 1);
        assert!(s.is_char_boundary(truncated.len()));
    }

    // ── Canonicalization / alias tests ────────────────────────────────────────

    #[tokio::test]
    async fn resolve_creates_entity_with_canonical_name() {
        let gs = setup().await;
        let resolver = EntityResolver::new(&gs);
        let id = resolver.resolve("Rust", "language", None).await.unwrap();
        assert!(id > 0);
        let entity = gs
            .find_entity("rust", EntityType::Language)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(entity.canonical_name, "rust");
    }

    #[tokio::test]
    async fn resolve_adds_alias_on_create() {
        let gs = setup().await;
        let resolver = EntityResolver::new(&gs);
        let id = resolver.resolve("Rust", "language", None).await.unwrap();
        let aliases = gs.aliases_for_entity(id).await.unwrap();
        assert!(
            !aliases.is_empty(),
            "new entity should have at least one alias"
        );
        assert!(aliases.iter().any(|a| a.alias_name == "rust"));
    }

    #[tokio::test]
    async fn resolve_reuses_entity_by_alias() {
        let gs = setup().await;
        let resolver = EntityResolver::new(&gs);

        // Create entity and register an alias
        let id1 = resolver.resolve("rust", "language", None).await.unwrap();
        gs.add_alias(id1, "rust-lang").await.unwrap();

        // Resolve using the alias — should return the same entity
        let id2 = resolver
            .resolve("rust-lang", "language", None)
            .await
            .unwrap();
        assert_eq!(
            id1, id2,
            "'rust-lang' alias should resolve to same entity as 'rust'"
        );
    }

    #[tokio::test]
    async fn resolve_alias_match_respects_entity_type() {
        let gs = setup().await;
        let resolver = EntityResolver::new(&gs);

        // "python" as a Language
        let lang_id = resolver.resolve("python", "language", None).await.unwrap();

        // "python" as a Tool should create a separate entity (different type)
        let tool_id = resolver.resolve("python", "tool", None).await.unwrap();
        assert_ne!(
            lang_id, tool_id,
            "same name with different type should be separate entities"
        );
    }

    #[tokio::test]
    async fn resolve_preserves_existing_aliases() {
        let gs = setup().await;
        let resolver = EntityResolver::new(&gs);

        let id = resolver.resolve("rust", "language", None).await.unwrap();
        gs.add_alias(id, "rust-lang").await.unwrap();

        // Upserting same entity should not remove prior aliases
        resolver
            .resolve("rust", "language", Some("updated"))
            .await
            .unwrap();
        let aliases = gs.aliases_for_entity(id).await.unwrap();
        assert!(
            aliases.iter().any(|a| a.alias_name == "rust-lang"),
            "prior alias must be preserved"
        );
    }

    #[tokio::test]
    async fn resolve_original_form_registered_as_alias() {
        let gs = setup().await;
        let resolver = EntityResolver::new(&gs);

        // "  Rust  " — original trimmed lowercased form is "rust", same as normalized
        // So only one alias should be registered (no duplicate)
        let id = resolver
            .resolve("  Rust  ", "language", None)
            .await
            .unwrap();
        let aliases = gs.aliases_for_entity(id).await.unwrap();
        assert!(aliases.iter().any(|a| a.alias_name == "rust"));
    }

    #[tokio::test]
    async fn resolve_entity_with_many_aliases() {
        let gs = setup().await;
        let id = gs
            .upsert_entity("bigentity", "bigentity", EntityType::Concept, None)
            .await
            .unwrap();
        for i in 0..100 {
            gs.add_alias(id, &format!("alias-{i}")).await.unwrap();
        }
        let aliases = gs.aliases_for_entity(id).await.unwrap();
        assert_eq!(aliases.len(), 100);

        // Fuzzy search should still work via alias
        let results = gs.find_entities_fuzzy("alias-50", 10).await.unwrap();
        assert!(results.iter().any(|e| e.id == id));
    }
}
