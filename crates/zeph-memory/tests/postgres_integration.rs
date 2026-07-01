// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `PostgreSQL` integration tests for `zeph-memory`.
//!
//! These tests require Docker to be running. They run in CI as part of the
//! `build-tests`/`integration` jobs in `.github/workflows/ci.yml`. Run locally with:
//! ```bash
//! cargo nextest run -p zeph-memory --features test-utils --test postgres_integration --run-ignored ignored-only
//! ```
//! Scoping to `--test postgres_integration` matters: `test-utils` enables the `postgres`
//! feature alongside the crate's default `sqlite` feature, and `DbConfig::connect()` gives
//! `postgres` cfg-priority whenever both are enabled (see `zeph-db/src/pool.rs`). Running the
//! crate-wide `--ignored` command instead would route unrelated `SqliteStore::new(":memory:")`
//! calls in other ignored tests (e.g. `hela_spreading_activation.rs`) through `connect_postgres`
//! and fail them with a `RelativeUrlWithoutBase` error.
//!
//! Regression coverage for issue #5364: several dynamic-SQL call sites in
//! `zeph-memory` built `IN (...)` lists and `INSERT ... ON CONFLICT` statements with
//! hand-rolled `?` placeholders that were never converted to `PostgreSQL`'s `$N`
//! syntax via `zeph_db::placeholder_list`/`rewrite_placeholders`. Each test below
//! exercises one of the fixed call sites against a real Postgres instance and
//! asserts actual row-level results (not just absence of error), since a bind-count
//! mismatch can in some cases silently return zero rows rather than erroring.

#[cfg(feature = "test-utils")]
mod pg {
    use testcontainers::runners::AsyncRunner as _;
    use testcontainers_modules::postgres::Postgres;
    use zeph_common::SessionId;
    use zeph_db::DbConfig;
    use zeph_memory::db_vector_store::DbVectorStore;
    use zeph_memory::graph::activation::ActivatedFact;
    use zeph_memory::graph::implicit_conflict;
    use zeph_memory::graph::store::GraphStore;
    use zeph_memory::graph::types::EntityType;
    use zeph_memory::store::SqliteStore;
    use zeph_memory::types::MessageId;
    use zeph_memory::{VectorStore, episodic_graph, snapshot};

    async fn start_pg() -> (zeph_db::DbPool, impl Drop) {
        let image = Postgres::default();
        let container = image.start().await.expect("docker must be available");
        let host = container.get_host().await.unwrap();
        let port = container.get_host_port_ipv4(5432).await.unwrap();
        let url = format!("postgres://postgres:postgres@{host}:{port}/postgres");
        let config = DbConfig {
            url,
            max_connections: 5,
            pool_size: 5,
        };
        let pool = config.connect().await.expect("failed to connect to PG");
        (pool, container)
    }

    // ── graph/store: add_alias + find_entity_by_alias + bfs + decay ────────────

    #[tokio::test]
    #[ignore = "requires Docker"]
    async fn graph_store_add_alias_and_lookup() {
        let (pool, _container) = start_pg().await;
        let graph = GraphStore::new(pool);

        let entity_id = graph
            .upsert_entity("Vim", "vim", EntityType::Tool, None, None)
            .await
            .unwrap();

        graph.add_alias(entity_id.0, "vi").await.unwrap();
        // Calling add_alias twice must stay idempotent (UNIQUE + INSERT_IGNORE).
        graph.add_alias(entity_id.0, "vi").await.unwrap();

        let found = graph
            .find_entity_by_alias("vi", EntityType::Tool)
            .await
            .unwrap()
            .expect("alias lookup must find the entity");
        assert_eq!(
            found.id, entity_id,
            "alias lookup must resolve to the same entity id"
        );

        let aliases = graph.aliases_for_entity(entity_id.0).await.unwrap();
        assert_eq!(
            aliases.len(),
            1,
            "duplicate alias must not be inserted twice"
        );
    }

    #[tokio::test]
    #[ignore = "requires Docker"]
    async fn graph_store_bfs_traversal() {
        let (pool, _container) = start_pg().await;
        let graph = GraphStore::new(pool);

        let alice = graph
            .upsert_entity("Alice", "alice", EntityType::Person, None, None)
            .await
            .unwrap();
        let vim = graph
            .upsert_entity("Vim", "vim", EntityType::Tool, None, None)
            .await
            .unwrap();

        graph
            .insert_edge(alice.0, vim.0, "uses", "Alice uses Vim", 0.9, None, None)
            .await
            .unwrap();

        let (entities, edges) = graph.bfs(alice.0, 2).await.unwrap();
        assert_eq!(edges.len(), 1, "BFS must traverse the single edge");
        assert!(entities.iter().any(|e| e.id == vim));
    }

    #[tokio::test]
    #[ignore = "requires Docker"]
    async fn graph_store_decay_edge_retrieval_counts() {
        let (pool, _container) = start_pg().await;
        let graph = GraphStore::new(pool.clone());

        let a = graph
            .upsert_entity("A", "a", EntityType::Concept, None, None)
            .await
            .unwrap();
        let b = graph
            .upsert_entity("B", "b", EntityType::Concept, None, None)
            .await
            .unwrap();
        let edge_id = graph
            .insert_edge(a.0, b.0, "relates_to", "A relates to B", 0.8, None, None)
            .await
            .unwrap();

        // Force the edge into a decay-eligible state: retrieved before, long ago.
        sqlx::query(zeph_db::sql!(
            "UPDATE graph_edges SET retrieval_count = 10, last_retrieved_at = 0 WHERE id = ?"
        ))
        .bind(edge_id)
        .execute(&pool)
        .await
        .unwrap();

        let updated = graph.decay_edge_retrieval_counts(0.5, 60).await.unwrap();
        assert_eq!(updated, 1, "exactly one stale edge must be decayed");

        let retrieval_count: i32 = sqlx::query_scalar(zeph_db::sql!(
            "SELECT retrieval_count FROM graph_edges WHERE id = ?"
        ))
        .bind(edge_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            retrieval_count, 5,
            "retrieval_count must be halved by decay_lambda=0.5"
        );
    }

    /// Regression test for a reviewer-found bug: `insert_edge`'s existing-row dedup branch
    /// (`insert_edge_typed`, hit whenever the same `(source, target, relation, edge_type)` is
    /// re-asserted — the normal way this ~65-call-site API is used during graph ingestion)
    /// decoded `confidence_fast`/`confidence_slow` (`REAL` on Postgres) directly as `f64`,
    /// the same defect class already fixed at 14 other `EdgeRow`-projecting queries in this
    /// PR but missed here because this query returns a raw tuple, not an `EdgeRow`. The first
    /// `insert_edge` call never hits this branch (no existing row), which is why no other test
    /// in this file caught it.
    #[tokio::test]
    #[ignore = "requires Docker"]
    async fn graph_store_insert_edge_reasserts_existing_edge() {
        let (pool, _container) = start_pg().await;
        let graph = GraphStore::new(pool.clone());

        let a = graph
            .upsert_entity("Alice", "alice", EntityType::Person, None, None)
            .await
            .unwrap();
        let b = graph
            .upsert_entity("Bob", "bob", EntityType::Person, None, None)
            .await
            .unwrap();

        let first_id = graph
            .insert_edge(a.0, b.0, "knows", "Alice knows Bob", 0.5, None, None)
            .await
            .unwrap();

        // Re-assert the same fact with higher confidence — must hit the UPDATE/dedup branch,
        // not crash, and not insert a second row.
        let second_id = graph
            .insert_edge(
                a.0,
                b.0,
                "knows",
                "Alice knows Bob (again)",
                0.9,
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            second_id, first_id,
            "re-asserting the same edge must update the existing row, not insert a new one"
        );

        let row_count: i64 = sqlx::query_scalar(zeph_db::sql!(
            "SELECT COUNT(*) FROM graph_edges WHERE source_entity_id = ? AND target_entity_id = ? \
             AND relation = ?"
        ))
        .bind(a.0)
        .bind(b.0)
        .bind("knows")
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            row_count, 1,
            "exactly one row must exist after re-assertion"
        );

        let (confidence, confidence_fast, confidence_slow): (f64, f64, f64) =
            sqlx::query_as(zeph_db::sql!(
                "SELECT confidence, CAST(confidence_fast AS DOUBLE PRECISION), \
                        CAST(confidence_slow AS DOUBLE PRECISION) FROM graph_edges WHERE id = ?"
            ))
            .bind(first_id)
            .fetch_one(&pool)
            .await
            .unwrap();

        // Benna-Fusi update with default rates (fast=0.5, slow=0.05):
        // confidence = max(0.9, 0.5) = 0.9
        // new_fast = 0.5 + 0.5 * (0.9 - 0.5) = 0.7
        // new_slow = 0.5 + 0.05 * (0.7 - 0.5) = 0.51
        assert!(
            (confidence - 0.9).abs() < 1e-6,
            "confidence must be the max of stored and asserted: got {confidence}"
        );
        assert!(
            (confidence_fast - 0.7).abs() < 1e-6,
            "confidence_fast must move toward the new assertion: got {confidence_fast}"
        );
        assert!(
            (confidence_slow - 0.51).abs() < 1e-6,
            "confidence_slow must integrate the new fast value: got {confidence_slow}"
        );
    }

    /// Same defect class and reviewer finding as
    /// [`graph_store_insert_edge_reasserts_existing_edge`], but for the `record_reassertion`
    /// path reached via `insert_or_supersede` (re-asserting an edge with an identical
    /// `(source, target, canonical_relation, edge_type, fact)` tuple), not `insert_edge`'s
    /// own dedup branch. Confirms `record_reassertion`'s `confidence_fast`/`confidence_slow`
    /// SELECT (also `REAL` on Postgres) is reachable and now decodes correctly.
    #[tokio::test]
    #[ignore = "requires Docker"]
    async fn graph_store_insert_or_supersede_reasserts_existing_edge() {
        let (pool, _container) = start_pg().await;
        let graph = GraphStore::new(pool.clone());

        let a = graph
            .upsert_entity("Carol", "carol", EntityType::Person, None, None)
            .await
            .unwrap();
        let b = graph
            .upsert_entity("Dave", "dave", EntityType::Person, None, None)
            .await
            .unwrap();

        let first_id = graph
            .insert_or_supersede(
                a.0,
                b.0,
                "manages",
                "manages",
                "Carol manages Dave",
                0.5,
                None,
                zeph_memory::graph::types::EdgeType::Semantic,
                false,
            )
            .await
            .unwrap();

        // Identical (source, target, canonical_relation, edge_type, fact) re-assertion must
        // hit `record_reassertion`, not crash, and not create a second row.
        let second_id = graph
            .insert_or_supersede(
                a.0,
                b.0,
                "manages",
                "manages",
                "Carol manages Dave",
                0.9,
                None,
                zeph_memory::graph::types::EdgeType::Semantic,
                false,
            )
            .await
            .unwrap();
        assert_eq!(
            second_id, first_id,
            "identical re-assertion must update the existing row via record_reassertion"
        );

        let row_count: i64 = sqlx::query_scalar(zeph_db::sql!(
            "SELECT COUNT(*) FROM graph_edges WHERE source_entity_id = ? AND target_entity_id = ? \
             AND canonical_relation = ?"
        ))
        .bind(a.0)
        .bind(b.0)
        .bind("manages")
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            row_count, 1,
            "exactly one row must exist after re-assertion"
        );
    }

    // ── messages: batch ID lookups + fidelity CASE update ──────────────────────

    #[tokio::test]
    #[ignore = "requires Docker"]
    async fn messages_batch_id_lookups() {
        let (pool, _container) = start_pg().await;
        let store = SqliteStore::from_pool(pool);

        let cid = store.create_conversation().await.unwrap();
        let m1 = store.save_message(cid, "user", "hello").await.unwrap();
        let m2 = store.save_message(cid, "assistant", "hi").await.unwrap();

        let by_ids = store.messages_by_ids(&[m1, m2]).await.unwrap();
        assert_eq!(by_ids.len(), 2, "IN-list lookup must return both messages");

        let scores = store.fetch_importance_scores(&[m1, m2]).await.unwrap();
        assert_eq!(scores.len(), 2);

        store.increment_access_counts(&[m1]).await.unwrap();
        let counts = store.message_access_counts(&[m1, m2]).await.unwrap();
        assert_eq!(counts.get(&m1).copied(), Some(1));
        assert_eq!(counts.get(&m2).copied(), Some(0));

        let tiers = store.fetch_tiers(&[m1, m2]).await.unwrap();
        assert_eq!(tiers.len(), 2);
    }

    #[tokio::test]
    #[ignore = "requires Docker"]
    async fn messages_update_fidelity_tags_case_batch() {
        let (pool, _container) = start_pg().await;
        let store = SqliteStore::from_pool(pool);

        let cid = store.create_conversation().await.unwrap();
        let m1 = store.save_message(cid, "user", "one").await.unwrap();
        let m2 = store.save_message(cid, "user", "two").await.unwrap();

        store
            .update_fidelity_tags(&[(m1, 1), (m2, 2)])
            .await
            .unwrap();

        let tags: Vec<(MessageId, i16)> = sqlx::query_as(zeph_db::sql!(
            "SELECT id, fidelity_tag FROM messages WHERE id IN (?, ?) ORDER BY id ASC"
        ))
        .bind(m1)
        .bind(m2)
        .fetch_all(store.pool())
        .await
        .unwrap();

        assert_eq!(tags.len(), 2);
        assert_eq!(tags[0].1, 1);
        assert_eq!(tags[1].1, 2);
    }

    // ── messages: forgetting sweep (downscale / replay / prune) ────────────────

    #[tokio::test]
    #[ignore = "requires Docker"]
    async fn messages_forgetting_sweep_downscale_and_prune() {
        let (pool, _container) = start_pg().await;
        let store = SqliteStore::from_pool(pool.clone());

        let cid = store.create_conversation().await.unwrap();
        let low = store
            .save_message(cid, "user", "low importance")
            .await
            .unwrap();

        // Force a low importance_score so the prune phase picks it up, and mark it as
        // never accessed so replay protection does not apply.
        sqlx::query(zeph_db::sql!(
            "UPDATE messages SET importance_score = 0.01, access_count = 0, last_accessed = NULL \
             WHERE id = ?"
        ))
        .bind(low)
        .execute(&pool)
        .await
        .unwrap();

        let config = zeph_common::config::memory::ForgettingConfig {
            enabled: true,
            decay_rate: 0.1,
            forgetting_floor: 0.5,
            sweep_batch_size: 100,
            replay_window_hours: 0,
            replay_min_access_count: 1_000_000,
            protect_recent_hours: 0,
            protect_min_access_count: 1_000_000,
            ..Default::default()
        };

        let result = store.run_forgetting_sweep_tx(&config).await.unwrap();
        assert_eq!(
            result.downscaled, 1,
            "the only active message must be downscaled"
        );
        assert_eq!(
            result.pruned, 1,
            "importance below floor and unprotected must be pruned"
        );

        let is_deleted: bool = sqlx::query_scalar(zeph_db::sql!(
            "SELECT deleted_at IS NOT NULL FROM messages WHERE id = ?"
        ))
        .bind(low)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(is_deleted, "pruned message must be soft-deleted");
    }

    // ── messages: consolidation-source insert (Pattern B) ───────────────────────

    #[tokio::test]
    #[ignore = "requires Docker"]
    async fn messages_consolidation_merge_links_sources() {
        let (pool, _container) = start_pg().await;
        let store = SqliteStore::from_pool(pool.clone());

        let cid = store.create_conversation().await.unwrap();
        let s1 = store.save_message(cid, "user", "part one").await.unwrap();
        let s2 = store.save_message(cid, "user", "part two").await.unwrap();

        let ok = store
            .apply_consolidation_merge(cid, "user", "merged content", &[s1, s2], 0.9, 0.5)
            .await
            .unwrap();
        assert!(ok, "merge above confidence threshold must succeed");

        let source_count: i64 = sqlx::query_scalar(zeph_db::sql!(
            "SELECT COUNT(*) FROM memory_consolidation_sources WHERE source_id IN (?, ?)"
        ))
        .bind(s1)
        .bind(s2)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            source_count, 2,
            "both sources must be linked to the consolidated message"
        );

        let consolidated: Vec<(bool,)> = sqlx::query_as(zeph_db::sql!(
            "SELECT consolidated FROM messages WHERE id IN (?, ?)"
        ))
        .bind(s1)
        .bind(s2)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert!(
            consolidated.iter().all(|(c,)| *c),
            "sources must be marked consolidated"
        );
    }

    #[tokio::test]
    #[ignore = "requires Docker"]
    async fn messages_consolidation_update_links_additional_sources() {
        let (pool, _container) = start_pg().await;
        let store = SqliteStore::from_pool(pool.clone());

        let cid = store.create_conversation().await.unwrap();
        let target = store.save_message(cid, "user", "original").await.unwrap();
        let extra = store.save_message(cid, "user", "extra").await.unwrap();

        let ok = store
            .apply_consolidation_update(target, "updated content", &[extra], 0.9, 0.5)
            .await
            .unwrap();
        assert!(ok, "update above confidence threshold must succeed");

        let source_count: i64 = sqlx::query_scalar(zeph_db::sql!(
            "SELECT COUNT(*) FROM memory_consolidation_sources WHERE consolidated_id = ? AND source_id = ?"
        ))
        .bind(target)
        .bind(extra)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            source_count, 1,
            "additional source must be linked to target"
        );

        let row: (String, bool) = sqlx::query_as(zeph_db::sql!(
            "SELECT content, consolidated FROM messages WHERE id = ?"
        ))
        .bind(target)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row.0, "updated content");
        assert!(row.1, "target must be marked consolidated");

        let extra_consolidated: bool = sqlx::query_scalar(zeph_db::sql!(
            "SELECT consolidated FROM messages WHERE id = ?"
        ))
        .bind(extra)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(
            extra_consolidated,
            "additional source must be marked consolidated"
        );
    }

    // ── episodic_graph: causal hop recall ───────────────────────────────────────

    #[tokio::test]
    #[ignore = "requires Docker"]
    async fn episodic_graph_causal_recall_walks_hops() {
        let (pool, _container) = start_pg().await;
        let store = SqliteStore::from_pool(pool);
        let session_id = SessionId::new("sess-causal-1");

        // episodic_events.message_id has a FK into messages(id); real rows are required
        // (unlike SQLite, Postgres enforces this constraint).
        let cid = store.create_conversation().await.unwrap();
        let seed_message = store.save_message(cid, "user", "seed").await.unwrap();
        let effect_message = store.save_message(cid, "user", "effect").await.unwrap();

        let mut events = vec![
            episodic_graph::EpisodicEvent {
                id: 0,
                session_id: session_id.clone(),
                message_id: seed_message,
                event_type: "decision".into(),
                summary: "seed event".into(),
                embedding: None,
                created_at: 0,
            },
            episodic_graph::EpisodicEvent {
                id: 0,
                session_id: session_id.clone(),
                message_id: effect_message,
                event_type: "discovery".into(),
                summary: "effect event".into(),
                embedding: None,
                created_at: 0,
            },
        ];
        episodic_graph::store_events(&store, &mut events)
            .await
            .unwrap();
        let seed_id = events[0].id;
        let effect_id = events[1].id;

        let link = episodic_graph::CausalLink {
            id: 0,
            cause_event_id: seed_id,
            effect_event_id: effect_id,
            strength: 0.7,
            created_at: 0,
        };
        episodic_graph::store_links(&store, std::slice::from_ref(&link))
            .await
            .unwrap();

        let config = zeph_config::memory::EmGraphConfig {
            enabled: true,
            max_chain_depth: 3,
            ..Default::default()
        };

        let recalled = episodic_graph::recall_episodic_causal(
            &store,
            seed_id,
            session_id.as_str(),
            config.max_chain_depth,
            &config,
        )
        .await
        .unwrap();

        let ids: Vec<i64> = recalled.iter().map(|e| e.id).collect();
        assert!(
            ids.contains(&seed_id),
            "seed event must be in the recall set"
        );
        assert!(
            ids.contains(&effect_id),
            "causally-linked effect event must be recalled"
        );

        let recent = episodic_graph::fetch_recent_events(&store, session_id.as_str(), 10)
            .await
            .unwrap();
        assert_eq!(
            recent.len(),
            2,
            "fetch_recent_events must return both events"
        );
    }

    // ── implicit_conflict: pending-candidate lookup across two IN clauses ──────

    #[tokio::test]
    #[ignore = "requires Docker"]
    async fn implicit_conflict_annotate_finds_pending_candidates() {
        let (pool, _container) = start_pg().await;
        let graph = GraphStore::new(pool.clone());

        let a = graph
            .upsert_entity("X", "x", EntityType::Concept, None, None)
            .await
            .unwrap();
        let b = graph
            .upsert_entity("Y", "y", EntityType::Concept, None, None)
            .await
            .unwrap();
        let edge_a = graph
            .insert_edge(a.0, b.0, "relates_to", "X relates to Y", 0.8, None, None)
            .await
            .unwrap();
        let edge_b = graph
            .insert_edge(b.0, a.0, "relates_to", "Y relates to X", 0.8, None, None)
            .await
            .unwrap();

        let candidate_id: i64 = sqlx::query_scalar(zeph_db::sql!(
            "INSERT INTO implicit_conflict_candidates \
             (edge_a_id, edge_b_id, similarity, method, status, created_at, expires_at) \
             VALUES (?, ?, 0.95, 'embedding', 'pending', 1000000, 9999999) RETURNING id"
        ))
        .bind(edge_a)
        .bind(edge_b)
        .fetch_one(&pool)
        .await
        .unwrap();

        let mut facts = vec![
            ActivatedFact {
                edge: dummy_edge(edge_a),
                activation_score: 1.0,
                is_implicit_conflict: false,
                conflict_candidate_id: None,
            },
            ActivatedFact {
                edge: dummy_edge(edge_b),
                activation_score: 1.0,
                is_implicit_conflict: false,
                conflict_candidate_id: None,
            },
        ];

        let mut tx = pool.begin().await.unwrap();
        implicit_conflict::annotate_conflicts(&mut facts, &mut tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();

        assert!(facts[0].conflict_candidate_id.is_some());
        assert_eq!(
            facts[0].conflict_candidate_id,
            facts[1].conflict_candidate_id
        );
        assert_eq!(facts[0].conflict_candidate_id, Some(candidate_id));
    }

    /// Minimal placeholder `Edge` for tests that only need `edge.id` populated
    /// (the only field `annotate_conflicts` reads from `ActivatedFact`).
    fn dummy_edge(id: i64) -> zeph_memory::graph::types::Edge {
        zeph_memory::graph::types::Edge {
            id,
            ..zeph_memory::graph::types::Edge::synthetic_anchor(0)
        }
    }

    // ── compression_guidelines: mark_used + count_unused ────────────────────────

    #[tokio::test]
    #[ignore = "requires Docker"]
    async fn compression_guidelines_mark_failure_pairs_used() {
        let (pool, _container) = start_pg().await;
        let store = SqliteStore::from_pool(pool);

        let cid = store.create_conversation().await.unwrap();
        let id1 = store
            .log_compression_failure(cid, "ctx-1", "reason-1", "general")
            .await
            .unwrap();
        let id2 = store
            .log_compression_failure(cid, "ctx-2", "reason-2", "general")
            .await
            .unwrap();

        let before = store.count_unused_failure_pairs().await.unwrap();
        assert_eq!(before, 2);

        store.mark_failure_pairs_used(&[id1, id2]).await.unwrap();

        let after = store.count_unused_failure_pairs().await.unwrap();
        assert_eq!(after, 0, "both failure pairs must be marked used");
    }

    // ── mem_scenes: member insert loop (Pattern B) ──────────────────────────────

    #[tokio::test]
    #[ignore = "requires Docker"]
    async fn mem_scenes_insert_links_all_members() {
        let (pool, _container) = start_pg().await;
        let store = SqliteStore::from_pool(pool.clone());

        let cid = store.create_conversation().await.unwrap();
        let m1 = store.save_message(cid, "user", "a").await.unwrap();
        let m2 = store.save_message(cid, "user", "b").await.unwrap();

        let scene_id = store
            .insert_mem_scene("label", "profile", &[m1, m2])
            .await
            .unwrap();

        let member_count: i64 = sqlx::query_scalar(zeph_db::sql!(
            "SELECT COUNT(*) FROM mem_scene_members WHERE scene_id = ?"
        ))
        .bind(scene_id.0)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(member_count, 2, "both members must be linked to the scene");
    }

    // ── acp_sessions: create (Pattern B) ─────────────────────────────────────────

    #[tokio::test]
    #[ignore = "requires Docker"]
    async fn acp_sessions_create_is_idempotent() {
        let (pool, _container) = start_pg().await;
        let store = SqliteStore::from_pool(pool.clone());

        store.create_acp_session("sess-1").await.unwrap();
        // Calling twice must not error (INSERT_IGNORE / ON CONFLICT DO NOTHING).
        store.create_acp_session("sess-1").await.unwrap();

        let count: i64 = sqlx::query_scalar(zeph_db::sql!(
            "SELECT COUNT(*) FROM acp_sessions WHERE id = ?"
        ))
        .bind("sess-1")
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(count, 1, "duplicate create must not insert a second row");

        let cid = store.create_conversation().await.unwrap();
        store
            .create_acp_session_with_conversation("sess-2", cid)
            .await
            .unwrap();
        let linked_cid: (Option<i64>,) = sqlx::query_as(zeph_db::sql!(
            "SELECT conversation_id FROM acp_sessions WHERE id = ?"
        ))
        .bind("sess-2")
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(linked_cid.0, Some(cid.0));
    }

    // ── snapshot: import (Pattern B insert paths) ───────────────────────────────

    #[tokio::test]
    #[ignore = "requires Docker"]
    async fn snapshot_export_then_import_round_trip() {
        let (pool, _container) = start_pg().await;
        let store = SqliteStore::from_pool(pool.clone());

        let cid = store.create_conversation().await.unwrap();
        store.save_message(cid, "user", "hello").await.unwrap();
        store.save_message(cid, "assistant", "world").await.unwrap();

        let exported = snapshot::export_snapshot(&store).await.unwrap();
        assert_eq!(exported.conversations.len(), 1);
        assert_eq!(exported.conversations[0].messages.len(), 2);

        // Import into a second, empty database to exercise the INSERT_IGNORE path
        // (re-importing into the same DB would just hit the "already exists" branch).
        let (pool2, _container2) = start_pg().await;
        let store2 = SqliteStore::from_pool(pool2.clone());

        let stats = snapshot::import_snapshot(&store2, exported).await.unwrap();
        assert_eq!(stats.conversations_imported, 1);
        assert_eq!(stats.messages_imported, 2);

        let msg_count: i64 = sqlx::query_scalar(zeph_db::sql!("SELECT COUNT(*) FROM messages"))
            .fetch_one(&pool2)
            .await
            .unwrap();
        assert_eq!(msg_count, 2, "both imported messages must be persisted");
    }

    // ── db_vector_store: ensure_collection (Pattern B) ──────────────────────────

    #[tokio::test]
    #[ignore = "requires Docker"]
    async fn db_vector_store_ensure_collection_is_idempotent() {
        let (pool, _container) = start_pg().await;
        let store = DbVectorStore::new(pool.clone());

        store.ensure_collection("test_collection", 4).await.unwrap();
        // Calling twice must not error (INSERT_IGNORE / ON CONFLICT DO NOTHING).
        store.ensure_collection("test_collection", 4).await.unwrap();

        let count: i64 = sqlx::query_scalar(zeph_db::sql!(
            "SELECT COUNT(*) FROM vector_collections WHERE name = ?"
        ))
        .bind("test_collection")
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            count, 1,
            "duplicate ensure_collection must not insert a second row"
        );
    }

    // ── five_signal: access frequency batch load (extra fix beyond the debugger's
    //    original list, found during implementation — hardcoded sqlite-only `?N`) ─

    #[tokio::test]
    #[ignore = "requires Docker"]
    async fn access_frequency_load_for_candidates_batch() {
        use zeph_memory::five_signal::access_frequency::AccessFrequencyCache;

        let (pool, _container) = start_pg().await;
        let store = SqliteStore::from_pool(pool.clone());
        let cache = AccessFrequencyCache::new(pool.clone());

        let cid = store.create_conversation().await.unwrap();
        let hot = store.save_message(cid, "user", "hot fact").await.unwrap();
        let cold = store.save_message(cid, "user", "cold fact").await.unwrap();

        // 5 access log entries for `hot`, none for `cold`.
        for _ in 0..5 {
            sqlx::query(zeph_db::sql!(
                "INSERT INTO fact_access_log (fact_id, fact_type, session_id, accessed_at) \
                 VALUES (?, 'message', 'sess-freq', 0)"
            ))
            .bind(hot.0)
            .execute(&pool)
            .await
            .unwrap();
        }

        let scores = cache
            .load_for_candidates("sess-freq", &[hot, cold])
            .await
            .unwrap();

        assert_eq!(
            scores.len(),
            2,
            "every requested candidate gets a score, even at zero"
        );
        assert!(
            scores.get(&hot).copied().unwrap_or(0.0) > 0.0,
            "hot fact must have a positive normalized access frequency"
        );
        assert_eq!(
            scores.get(&cold).copied(),
            Some(0.0),
            "cold fact with zero accesses normalizes to exactly 0.0"
        );
    }

    // ── admission_training: mark_recalled batch update (extra fix beyond the
    //    debugger's original list, found during implementation) ───────────────

    #[tokio::test]
    #[ignore = "requires Docker"]
    async fn admission_training_mark_recalled_batch() {
        let (pool, _container) = start_pg().await;
        let store = SqliteStore::from_pool(pool.clone());

        let cid = store.create_conversation().await.unwrap();
        let m1 = store.save_message(cid, "user", "x").await.unwrap();

        let id1 = store
            .record_admission_training(
                zeph_memory::store::admission_training::AdmissionTrainingInput {
                    message_id: Some(m1),
                    conversation_id: cid,
                    content: "x",
                    role: "user",
                    composite_score: 0.5,
                    was_admitted: true,
                    features_json: "{}",
                },
            )
            .await
            .unwrap();

        store.mark_training_recalled(&[m1]).await.unwrap();

        let was_recalled: i32 = sqlx::query_scalar(zeph_db::sql!(
            "SELECT was_recalled FROM admission_training_data WHERE id = ?"
        ))
        .bind(id1)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(was_recalled, 1);
    }
}
