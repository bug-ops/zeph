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
//!
//! Regression coverage for issues #5488/#5491: several call sites used raw
//! `sqlx::query`/`query_as`/`query_scalar` with literal `?`/`?N` placeholders instead of
//! going through the `sql!()` macro, and a handful of call sites already wrapped in
//! `sql!()` still used `SQLite`'s `?1`/`?2` numbered-placeholder syntax — which
//! `sql!()`'s `rewrite_placeholders` does not parse, producing malformed `$11` instead
//! of `$1` under Postgres. The tests below close the Postgres coverage gap for those
//! fixes; see the `NOTE` comment near the end of this module for one call-site family
//! that could not get coverage here due to an unrelated bug.
//!
//! Regression coverage for issue #5524: `agent_sessions.rs`'s `upsert_agent_session`
//! (INSERT ... ON CONFLICT) and both branches of `list_agent_sessions`'s SELECT used raw
//! `?`-placeholder string literals passed directly to `zeph_db::query`/`query_as` instead
//! of through `sql!()`, so the `?` tokens were never rewritten to Postgres's `$1, $2, ...`.
//! A live Postgres run against the `sql!()`-only fix then surfaced a second defect blocking
//! closure: `created_at`/`last_active_at` are `TIMESTAMPTZ` columns but `AgentSessionRow`
//! binds plain `String`s, which Postgres rejects (no implicit text->timestamptz cast).
//! `upsert_agent_session` now appends the new `Dialect::TIMESTAMPTZ_CAST` constant
//! (`::timestamptz` on Postgres, empty on `SQLite`) after those two placeholders.
//!
//! Regression coverage for issue #5508: `unixepoch()` (SQLite-only) was embedded as a
//! literal in SQL text at four call sites (`episodic_consolidation.rs::fetch_candidates`/
//! `mark_consolidated`, `graph/belief.rs::mark_promoted`/`apply_evidence_update`), which
//! has no Postgres equivalent and fails at runtime. Fixed by interpolating
//! `<ActiveDialect as Dialect>::EPOCH_NOW` via `format!()` before `rewrite_placeholders`.
//!
//! Regression coverage for issue #5525: `episodic_consolidation.rs::compute_cognitive_weight`'s
//! `SELECT COALESCE(SUM(CASE ... THEN 1.0 ... END), 0.0)` decoded directly into `f64`, but
//! Postgres types a `SUM()` over decimal literals as `NUMERIC`, which sqlx cannot decode into
//! `f64` without an explicit cast. Fixed with `CAST(... AS DOUBLE PRECISION)`, the same idiom
//! already used at the 14 `EdgeRow`-projecting call sites referenced above (#5364). This was
//! the last blocker on the full `run_episodic_consolidation_sweep` pipeline (`fetch_candidates`
//! → `compute_cognitive_weight` → ... → `mark_consolidated`) getting real Postgres round-trip
//! coverage — see `episodic_consolidation_sweep_promotes_fact_postgres` below.
//!
//! Regression coverage for issue #5539: `graph/entity_lock.rs::extend_lock` built its
//! `expires_at` offset expression with `SQLite`-only `datetime(expires_at, ? || ' seconds')`
//! syntax, which is invalid against Postgres's `TIMESTAMPTZ` column (no `datetime()`
//! function). The existing unit tests in `entity_lock.rs` only run against `SQLite` and
//! assert the boolean return value, so they could not catch the syntax mismatch. The tests
//! below exercise `extend_lock` (and the sibling `try_acquire_once`, fixed earlier in
//! PR #5537 with the same dialect-split pattern but never covered here either) against a
//! live Postgres instance and assert the actual `expires_at` column value moved, not just
//! that the call succeeded.

#[cfg(feature = "test-utils")]
mod pg {
    use std::time::Duration;

    use testcontainers::ImageExt as _;
    use testcontainers::runners::AsyncRunner as _;
    use testcontainers_modules::postgres::Postgres;
    use zeph_common::SessionId;
    use zeph_common::types::ProviderName;
    use zeph_db::DbConfig;
    use zeph_llm::any::AnyProvider;
    use zeph_llm::mock::MockProvider;
    use zeph_memory::db_vector_store::DbVectorStore;
    use zeph_memory::embedding_store::EmbeddingStore;
    use zeph_memory::graph::EntityLockManager;
    use zeph_memory::graph::activation::ActivatedFact;
    use zeph_memory::graph::belief::{BeliefMemConfig, BeliefStore};
    use zeph_memory::graph::implicit_conflict;
    use zeph_memory::graph::store::GraphStore;
    use zeph_memory::graph::types::{EdgeType, EntityType};
    use zeph_memory::in_memory_store::InMemoryVectorStore;
    use zeph_memory::store::admission_training::AdmissionTrainingInput;
    use zeph_memory::store::{
        AcpSessionConfigSnapshot, AgentSessionRow, SessionChannel, SessionKind, SessionStatus,
        SqliteStore,
    };
    use zeph_memory::types::MessageId;
    use zeph_memory::{
        NewExperimentResult, NoteLinkingConfig, RetrievalFailureRecord, RetrievalFailureType,
        VectorStore, episodic_consolidation, episodic_graph, link_memory_notes, snapshot,
    };

    // Generous startup timeout: under concurrent CI load (see #5546/#5547), the
    // default 60s can elapse before Postgres is ready, and testcontainers-rs 0.27.3
    // leaks the container on a startup-timeout cancel (no Drop guard is ever created).
    async fn start_pg() -> (zeph_db::DbPool, impl Drop) {
        let image = Postgres::default().with_startup_timeout(Duration::from_mins(2));
        let container = image.start().await.expect("docker must be available");
        let host = container.get_host().await.unwrap();
        let port = container.get_host_port_ipv4(5432).await.unwrap();
        let url = format!("postgres://postgres:postgres@{host}:{port}/postgres");
        let config = DbConfig { url, pool_size: 5 };
        let pool = config.connect().await.expect("failed to connect to PG");
        (pool, container)
    }

    // ── #5816 (gap 4): #5801 stale cross-DB entity id regression, on Postgres ──
    //
    // `resolve_local_target_id`/`EntityResolver::merge_entity` re-resolve a vector-store
    // candidate's `entity_id` against the *local* database rather than trusting the
    // payload verbatim (#5801). That resolution logic never touches backend-specific SQL
    // directly — it goes through `upsert_entity`'s `RETURNING id`, a single shared
    // implementation already exercised by the ~20 other tests in this file — but this
    // test proves the full note-linking path (`link_memory_notes` ->
    // `resolve_local_target_id` -> `upsert_entity`) round-trips correctly against a real
    // Postgres instance, mirroring `link_memory_notes_corrects_stale_cross_db_target_id`
    // in `semantic/tests/graph.rs` (SQLite-only).
    #[tokio::test]
    #[ignore = "requires Docker"]
    async fn note_linking_corrects_stale_cross_db_target_id_postgres() {
        const STALE_CROSS_DB_ID: i64 = 777_777_777;

        let (pool, _container) = start_pg().await;
        let graph = GraphStore::new(pool.clone());

        let mem_store = Box::new(InMemoryVectorStore::new());
        let embedding_store =
            std::sync::Arc::new(EmbeddingStore::with_store(mem_store, pool.clone()));
        embedding_store
            .ensure_named_collection("zeph_graph_entities", 384)
            .await
            .unwrap();

        let source_id = graph
            .upsert_entity(
                "pg_phantom_source",
                "pg_phantom_source",
                EntityType::Concept,
                None,
                None,
            )
            .await
            .unwrap()
            .0;
        let source_point_id = uuid::Uuid::new_v4().to_string();
        let source_payload = serde_json::json!({
            "entity_id": source_id,
            "canonical_name": "pg_phantom_source",
            "entity_type": "concept",
            "name": "pg_phantom_source",
            "summary": "",
        });
        embedding_store
            .upsert_to_collection(
                "zeph_graph_entities",
                &source_point_id,
                source_payload,
                vec![0.0_f32; 384],
            )
            .await
            .unwrap();
        graph
            .set_entity_qdrant_point_id(source_id, &source_point_id)
            .await
            .unwrap();

        // Phantom candidate: exists only in the vector store, with an `entity_id` that has
        // no local row — simulates a point left over from a different, now-gone Postgres
        // database sharing the same vector collection.
        let phantom_payload = serde_json::json!({
            "entity_id": STALE_CROSS_DB_ID,
            "canonical_name": "pg_phantom_target",
            "name": "pg_phantom_target",
            "entity_type": "concept",
            "summary": "",
        });
        embedding_store
            .upsert_to_collection(
                "zeph_graph_entities",
                &uuid::Uuid::new_v4().to_string(),
                phantom_payload,
                vec![0.0_f32; 384],
            )
            .await
            .unwrap();

        let mut mock = MockProvider::default();
        mock.supports_embeddings = true;
        let provider = AnyProvider::Mock(mock);

        let cfg = NoteLinkingConfig {
            enabled: true,
            similarity_threshold: 0.0,
            top_k: 5,
            timeout_secs: 10,
        };

        let stats =
            link_memory_notes(&[source_id], pool.clone(), embedding_store, provider, &cfg).await;

        assert_eq!(
            stats.edges_created, 1,
            "the link edge must still be created via the corrected local id, not silently dropped"
        );

        let stale_id_referenced: i64 = sqlx::query_scalar(zeph_db::sql!(
            "SELECT COUNT(*) FROM graph_edges WHERE source_entity_id = ?1 OR target_entity_id = ?1"
        ))
        .bind(STALE_CROSS_DB_ID)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            stale_id_referenced, 0,
            "the created edge must never reference the stale cross-DB id (would violate the FK)"
        );

        let phantom = graph
            .find_entity("pg_phantom_target", EntityType::Concept)
            .await
            .unwrap();
        assert!(
            phantom.is_some(),
            "a local entity must be created for the stale candidate so the edge has a valid FK target"
        );
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
        let store = SqliteStore::from_pool(pool).await.unwrap();

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
        let store = SqliteStore::from_pool(pool).await.unwrap();

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

    // ── messages: compacted_at write path (#5561) ───────────────────────────────
    //
    // `replace_conversation`/`apply_tool_pair_summaries` bind a Rust-formatted
    // epoch-seconds `String` into `compacted_at` via `Dialect::timestamptz_from_epoch`
    // (`to_timestamp(?::double precision)` on Postgres). A string-equality unit test on
    // the emitted SQL fragment cannot catch a Postgres function-resolution error (e.g.
    // `to_timestamp(text)` does not exist — only `to_timestamp(double precision)`); only
    // executing the UPDATE against a real Postgres backend can. These two tests do that,
    // then decode `compacted_at` back via `EXTRACT(EPOCH FROM ...)` to confirm the value
    // round-trips to a sane epoch, not just that the statement didn't error.

    #[tokio::test]
    #[ignore = "requires Docker"]
    async fn replace_conversation_writes_compacted_at_on_postgres() {
        let (pool, _container) = start_pg().await;
        let store = SqliteStore::from_pool(pool).await.unwrap();

        let cid = store.create_conversation().await.unwrap();
        let m1 = store.save_message(cid, "user", "one").await.unwrap();
        let m2 = store.save_message(cid, "user", "two").await.unwrap();

        let before = i64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        )
        .unwrap();

        store
            .replace_conversation(cid, m1..=m2, "assistant", "compacted summary")
            .await
            .unwrap();

        let (visibility, compacted_epoch): (String, i64) = sqlx::query_as(zeph_db::sql!(
            "SELECT visibility, EXTRACT(EPOCH FROM compacted_at)::BIGINT FROM messages WHERE id = ?"
        ))
        .bind(m1)
        .fetch_one(store.pool())
        .await
        .unwrap();

        assert_eq!(visibility, "user_only");
        assert!(
            (before - 60..=before + 60).contains(&compacted_epoch),
            "compacted_at must round-trip to a value near 'now' ({before}), got {compacted_epoch}"
        );
    }

    #[tokio::test]
    #[ignore = "requires Docker"]
    async fn apply_tool_pair_summaries_writes_compacted_at_on_postgres() {
        let (pool, _container) = start_pg().await;
        let store = SqliteStore::from_pool(pool).await.unwrap();

        let cid = store.create_conversation().await.unwrap();
        let tool_use = store
            .save_message(cid, "assistant", "tool_use")
            .await
            .unwrap();
        let tool_result = store
            .save_message(cid, "user", "tool_result")
            .await
            .unwrap();

        let before = i64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        )
        .unwrap();

        store
            .apply_tool_pair_summaries(
                cid,
                &[tool_use.0, tool_result.0],
                &["[tool summary] did the thing".to_string()],
            )
            .await
            .unwrap();

        let rows: Vec<(String, i64)> = sqlx::query_as(zeph_db::sql!(
            "SELECT visibility, EXTRACT(EPOCH FROM compacted_at)::BIGINT FROM messages \
             WHERE id IN (?, ?) ORDER BY id ASC"
        ))
        .bind(tool_use)
        .bind(tool_result)
        .fetch_all(store.pool())
        .await
        .unwrap();

        assert_eq!(rows.len(), 2);
        for (visibility, compacted_epoch) in rows {
            assert_eq!(visibility, "user_only");
            assert!(
                (before - 60..=before + 60).contains(&compacted_epoch),
                "compacted_at must round-trip to a value near 'now' ({before}), got {compacted_epoch}"
            );
        }
    }

    // ── messages: forgetting sweep (downscale / replay / prune) ────────────────

    #[tokio::test]
    #[ignore = "requires Docker"]
    async fn messages_forgetting_sweep_downscale_and_prune() {
        let (pool, _container) = start_pg().await;
        let store = SqliteStore::from_pool(pool.clone()).await.unwrap();

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

    // ── messages: get_eviction_candidates INT4/TIMESTAMPTZ decode ──────────────
    //
    // Bonus regression coverage for a fix already merged in PR #5560: `get_eviction_candidates`
    // used to decode `created_at` (`TIMESTAMPTZ` on Postgres, migrations/postgres/001_init.sql)
    // and `last_accessed` (`TIMESTAMPTZ`, migrations/postgres/020_eviction_columns.sql) directly
    // into `String`/`Option<String>`, which `sqlx-postgres` rejects outright (`ColumnDecode`) —
    // the function could not run against Postgres at all. That defect (and the co-located
    // `access_count` INT4-as-i64 mismatch) was already fixed by #5560, but shipped without
    // dedicated Postgres coverage for this function; this test closes that gap. It accesses a
    // message (populating `last_accessed`, which is `NULL` until first access) and asserts the
    // full row decodes with non-empty `created_at`/`last_accessed` and the expected
    // `access_count`.
    #[tokio::test]
    #[ignore = "requires Docker"]
    async fn messages_get_eviction_candidates_decodes_timestamps_and_access_count_postgres() {
        let (pool, _container) = start_pg().await;
        let store = SqliteStore::from_pool(pool).await.unwrap();

        let cid = store.create_conversation().await.unwrap();
        let msg = store.save_message(cid, "user", "hello").await.unwrap();
        store.increment_access_counts(&[msg]).await.unwrap();
        store.increment_access_counts(&[msg]).await.unwrap();

        let candidates = store.get_eviction_candidates().await.unwrap();
        let entry = candidates
            .iter()
            .find(|e| e.id == msg)
            .expect("saved message must appear among eviction candidates");

        assert!(
            !entry.created_at.is_empty(),
            "created_at must decode as non-empty text, not error with ColumnDecode"
        );
        assert!(
            entry.last_accessed.as_ref().is_some_and(|s| !s.is_empty()),
            "last_accessed must decode as non-empty text after access, not error with ColumnDecode"
        );
        assert_eq!(
            entry.access_count, 2,
            "access_count must decode as the full INTEGER value, not truncate/zero"
        );
    }

    // ── messages: consolidation-source insert (Pattern B) ───────────────────────

    #[tokio::test]
    #[ignore = "requires Docker"]
    async fn messages_consolidation_merge_links_sources() {
        let (pool, _container) = start_pg().await;
        let store = SqliteStore::from_pool(pool.clone()).await.unwrap();

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
        let store = SqliteStore::from_pool(pool.clone()).await.unwrap();

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
        let store = SqliteStore::from_pool(pool).await.unwrap();
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
        let store = SqliteStore::from_pool(pool).await.unwrap();

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

    // ── compression_guidelines: load_compression_guidelines_meta INT4/TIMESTAMPTZ
    //    decode ─────────────────────────────────────────────────────────────────
    //
    // Bonus regression coverage for a fix already merged in PR #5560:
    // `load_compression_guidelines_meta` used to decode `version` (`INTEGER`/`INT4` on Postgres)
    // directly into `i64` and `created_at` (`TIMESTAMPTZ` on Postgres) directly into `String`,
    // both of which `sqlx-postgres` rejects with a `ColumnDecode` mismatch (`String`/`i64` are
    // only compatible with `TEXT`/`INT8`-family OIDs). That defect was already fixed by #5560,
    // but shipped without dedicated Postgres
    // coverage for this function; this test closes that gap. A `SQLite`-only unit test cannot
    // catch either mismatch: `SQLite` is dynamically typed, so the same decode succeeds there
    // regardless of the fix. This test saves two versions and asserts the second (non-zero, so a
    // truncated/zeroed `i32`-as-`i64` misread would be caught) round-trips correctly against a
    // real Postgres instance, with a non-empty `created_at`.
    #[tokio::test]
    #[ignore = "requires Docker"]
    async fn compression_guidelines_meta_decodes_version_and_created_at_postgres() {
        let (pool, _container) = start_pg().await;
        let store = SqliteStore::from_pool(pool).await.unwrap();

        store
            .save_compression_guidelines("first guideline", 4, None)
            .await
            .unwrap();
        let v2 = store
            .save_compression_guidelines("second guideline", 8, None)
            .await
            .unwrap();
        assert_eq!(v2, 2, "second save must produce version 2");

        let (version, created_at) = store.load_compression_guidelines_meta(None).await.unwrap();
        assert_eq!(
            version, 2,
            "version must decode as the latest INTEGER value, not truncate/zero"
        );
        assert!(
            !created_at.is_empty(),
            "created_at must decode as non-empty text, not error with ColumnDecode"
        );
    }

    // ── mem_scenes: member insert loop (Pattern B) ──────────────────────────────

    #[tokio::test]
    #[ignore = "requires Docker"]
    async fn mem_scenes_insert_links_all_members() {
        let (pool, _container) = start_pg().await;
        let store = SqliteStore::from_pool(pool.clone()).await.unwrap();

        let cid = store.create_conversation().await.unwrap();
        let m1 = store.save_message(cid, "user", "a").await.unwrap();
        let m2 = store.save_message(cid, "user", "b").await.unwrap();
        let m3 = store.save_message(cid, "user", "c").await.unwrap();

        // Promote all three to the semantic tier so `find_unscened_semantic_messages`
        // (#5544's fix target) can see them.
        for id in [m1, m2, m3] {
            sqlx::query(zeph_db::sql!(
                "UPDATE messages SET tier = 'semantic' WHERE id = ?"
            ))
            .bind(id.0)
            .execute(&pool)
            .await
            .unwrap();
        }

        // Bound `LIMIT ?` exercises the exact placeholder-rewrite path #5544 fixes: the
        // function previously passed a raw string straight to `query_as`, bypassing
        // `sql!()`'s rewrite of `?` to Postgres's `$1`.
        let unscened = store.find_unscened_semantic_messages(2).await.unwrap();
        assert_eq!(
            unscened.len(),
            2,
            "LIMIT bind must be honored under Postgres"
        );

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

        // After assigning m1/m2 to a scene, only m3 remains unscened.
        let remaining = store.find_unscened_semantic_messages(100).await.unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].0, m3);
    }

    // ── acp_sessions: create (Pattern B) ─────────────────────────────────────────

    #[tokio::test]
    #[ignore = "requires Docker"]
    async fn acp_sessions_create_is_idempotent() {
        let (pool, _container) = start_pg().await;
        let store = SqliteStore::from_pool(pool.clone()).await.unwrap();

        store.create_acp_session("sess-1", None).await.unwrap();
        // Calling twice must not error (INSERT_IGNORE / ON CONFLICT DO NOTHING).
        store.create_acp_session("sess-1", None).await.unwrap();

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
            .create_acp_session_with_conversation("sess-2", cid, None)
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

    // ── acp_sessions: list_acp_sessions / get_acp_session_info (#5527) ──────────
    //
    // `created_at`/`updated_at` are `TIMESTAMPTZ` on Postgres (`TEXT` on SQLite);
    // `AcpSessionInfo` binds plain `String`s. Regression coverage for issue #5527:
    // both queries now project through `Dialect::select_as_text`, mirroring the
    // `agent_sessions.rs::list_agent_sessions` fix for #5524.

    #[tokio::test]
    #[ignore = "requires Docker"]
    async fn acp_sessions_list_and_get_include_timestamps_postgres() {
        let (pool, _container) = start_pg().await;
        let store = SqliteStore::from_pool(pool).await.unwrap();

        store.create_acp_session("sess-ts-1", None).await.unwrap();

        let list = store.list_acp_sessions(10).await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, "sess-ts-1");
        assert!(
            !list[0].created_at.is_empty(),
            "created_at must decode as non-empty text"
        );
        assert!(
            !list[0].updated_at.is_empty(),
            "updated_at must decode as non-empty text"
        );

        let info = store
            .get_acp_session_info("sess-ts-1")
            .await
            .unwrap()
            .expect("session must exist");
        assert_eq!(info.id, "sess-ts-1");
        assert!(!info.created_at.is_empty());
        assert!(!info.updated_at.is_empty());
    }

    // Regression coverage for issue #5980: `list_acp_sessions`/`list_acp_sessions_for_owner`
    // bound `LIMIT ?` with `-1` as a `limit == 0` ("unlimited") sentinel. `LIMIT -1` is a
    // `SQLite`-only convenience; `PostgreSQL` rejects a negative `LIMIT` at execution time
    // (`ERROR: LIMIT must not be negative`), so `limit = 0` against Postgres previously errored
    // instead of returning every row.
    #[tokio::test]
    #[ignore = "requires Docker"]
    async fn acp_sessions_list_unlimited_when_zero_postgres() {
        let (pool, _container) = start_pg().await;
        let store = SqliteStore::from_pool(pool).await.unwrap();

        for i in 0..5u8 {
            store
                .create_acp_session(&format!("sess-{i}"), Some("owner-a"))
                .await
                .unwrap();
        }

        let all = store.list_acp_sessions(0).await.unwrap();
        assert_eq!(all.len(), 5);

        let for_owner = store
            .list_acp_sessions_for_owner(0, "owner-a")
            .await
            .unwrap();
        assert_eq!(for_owner.len(), 5);
    }

    // ── acp_sessions: session_config snapshot (#5373/#5384) ─────────────────────
    //
    // `thinking_enabled` is `INTEGER` on SQLite vs `BOOLEAN` on Postgres (migration
    // 105_acp_session_config); these tests exercise the dialect-generic
    // save_session_config/get_session_config round trip against real Postgres to catch a
    // silent boolean-as-integer coercion bug (precedent: #5364/#5377).

    #[tokio::test]
    #[ignore = "requires Docker"]
    async fn session_config_round_trips_thinking_enabled_true() {
        let (pool, _container) = start_pg().await;
        let store = SqliteStore::from_pool(pool).await.unwrap();

        store.create_acp_session("sess-1", None).await.unwrap();
        let snapshot = AcpSessionConfigSnapshot {
            current_model: "claude:opus".to_owned(),
            temperature_preset: "creative".to_owned(),
            thinking_enabled: true,
            auto_approve_level: "auto-edit".to_owned(),
        };
        store
            .save_session_config("sess-1", &snapshot)
            .await
            .unwrap();

        let loaded = store
            .get_session_config("sess-1")
            .await
            .unwrap()
            .expect("snapshot must be present after save");
        assert_eq!(loaded.current_model, "claude:opus");
        assert_eq!(loaded.temperature_preset, "creative");
        assert!(
            loaded.thinking_enabled,
            "thinking_enabled=true must round-trip as a proper bool on Postgres"
        );
        assert_eq!(loaded.auto_approve_level, "auto-edit");
    }

    #[tokio::test]
    #[ignore = "requires Docker"]
    async fn session_config_round_trips_thinking_enabled_false() {
        let (pool, _container) = start_pg().await;
        let store = SqliteStore::from_pool(pool).await.unwrap();

        store.create_acp_session("sess-1", None).await.unwrap();
        let snapshot = AcpSessionConfigSnapshot {
            current_model: "claude:sonnet".to_owned(),
            temperature_preset: "precise".to_owned(),
            thinking_enabled: false,
            auto_approve_level: "manual".to_owned(),
        };
        store
            .save_session_config("sess-1", &snapshot)
            .await
            .unwrap();

        let loaded = store
            .get_session_config("sess-1")
            .await
            .unwrap()
            .expect("snapshot must be present after save");
        assert_eq!(loaded.current_model, "claude:sonnet");
        assert_eq!(loaded.temperature_preset, "precise");
        assert!(
            !loaded.thinking_enabled,
            "thinking_enabled=false must round-trip as a proper bool, not coerce to true"
        );
        assert_eq!(loaded.auto_approve_level, "manual");
    }

    #[tokio::test]
    #[ignore = "requires Docker"]
    async fn session_config_missing_snapshot_returns_none() {
        let (pool, _container) = start_pg().await;
        let store = SqliteStore::from_pool(pool).await.unwrap();

        store.create_acp_session("sess-1", None).await.unwrap();
        assert!(
            store.get_session_config("sess-1").await.unwrap().is_none(),
            "session with no saved config must return None, not a zeroed snapshot"
        );
    }

    #[tokio::test]
    #[ignore = "requires Docker"]
    async fn session_config_unknown_session_returns_none() {
        let (pool, _container) = start_pg().await;
        let store = SqliteStore::from_pool(pool).await.unwrap();

        assert!(
            store.get_session_config("no-such").await.unwrap().is_none(),
            "nonexistent session must return None"
        );
    }

    // ── snapshot: import (Pattern B insert paths) ───────────────────────────────

    #[tokio::test]
    #[ignore = "requires Docker"]
    async fn snapshot_export_then_import_round_trip() {
        let (pool, _container) = start_pg().await;
        let store = SqliteStore::from_pool(pool.clone()).await.unwrap();

        let cid = store.create_conversation().await.unwrap();
        store.save_message(cid, "user", "hello").await.unwrap();
        store.save_message(cid, "assistant", "world").await.unwrap();

        let exported = snapshot::export_snapshot(&store).await.unwrap();
        assert_eq!(exported.conversations.len(), 1);
        assert_eq!(exported.conversations[0].messages.len(), 2);

        // Import into a second, empty database to exercise the INSERT_IGNORE path
        // (re-importing into the same DB would just hit the "already exists" branch).
        let (pool2, _container2) = start_pg().await;
        let store2 = SqliteStore::from_pool(pool2.clone()).await.unwrap();

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
        let store = SqliteStore::from_pool(pool.clone()).await.unwrap();
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
        let store = SqliteStore::from_pool(pool.clone()).await.unwrap();

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

    // ── Regression coverage for #5488/#5491: sql!() bypasses and the ?N-inside-sql!()
    // Postgres rewrite defect (raw `?` characters silently become `$11` instead of `$1`
    // when the literal SQL used SQLite's numbered-placeholder syntax). Each test below
    // exercises a call site that was fixed as part of that pass and had zero Postgres
    // coverage before. ─────────────────────────────────────────────────────────────

    #[tokio::test]
    #[ignore = "requires Docker"]
    async fn access_frequency_log_access_writes_entry() {
        use zeph_memory::five_signal::access_frequency::AccessFrequencyCache;

        let (pool, _container) = start_pg().await;
        let store = SqliteStore::from_pool(pool.clone()).await.unwrap();
        let cache = AccessFrequencyCache::new(pool.clone());

        let cid = store.create_conversation().await.unwrap();
        let msg = store
            .save_message(cid, "user", "logged fact")
            .await
            .unwrap();

        cache.log_access(msg, "message", "sess-log").await;

        let (fact_id, fact_type, session_id): (i64, String, String) =
            sqlx::query_as(zeph_db::sql!(
                "SELECT fact_id, fact_type, session_id FROM fact_access_log WHERE fact_id = ?"
            ))
            .bind(msg.0)
            .fetch_one(&pool)
            .await
            .unwrap();

        assert_eq!(fact_id, msg.0);
        assert_eq!(fact_type, "message");
        assert_eq!(session_id, "sess-log");
    }

    #[tokio::test]
    #[ignore = "requires Docker"]
    async fn graph_store_source_entity_id_for_edge_postgres() {
        let (pool, _container) = start_pg().await;
        let graph = GraphStore::new(pool.clone());

        let a = graph
            .upsert_entity("Ivy", "ivy", EntityType::Person, None, None)
            .await
            .unwrap();
        let b = graph
            .upsert_entity("Jack", "jack", EntityType::Person, None, None)
            .await
            .unwrap();
        let edge_id = graph
            .insert_edge(a.0, b.0, "knows", "Ivy knows Jack", 0.9, None, None)
            .await
            .unwrap();

        let found = graph.source_entity_id_for_edge(edge_id).await.unwrap();
        assert_eq!(found, Some(a.0));

        let missing = graph.source_entity_id_for_edge(999_999_999).await.unwrap();
        assert_eq!(missing, None);
    }

    #[tokio::test]
    #[ignore = "requires Docker"]
    async fn graph_store_insert_or_supersede_with_conflict_detection_and_depth_check() {
        let (pool, _container) = start_pg().await;
        let graph = GraphStore::new(pool.clone());

        let a = graph
            .upsert_entity("Eve", "eve", EntityType::Person, None, None)
            .await
            .unwrap();
        let b = graph
            .upsert_entity("Frank", "frank", EntityType::Person, None, None)
            .await
            .unwrap();

        let detector_config = zeph_config::memory::ImplicitConflictConfig {
            enabled: true,
            ..Default::default()
        };
        let detector = implicit_conflict::ImplicitConflictDetector::new(detector_config);
        let ontology = zeph_memory::graph::ontology::OntologyTable::from_default(64);

        let first_id = graph
            .insert_or_supersede_with_conflict_detection(
                a.0,
                b.0,
                "manages",
                "manages",
                "Eve manages Frank",
                0.6,
                None,
                zeph_memory::graph::types::EdgeType::Semantic,
                true,
                None,
                Some(&detector),
                Some(&ontology),
            )
            .await
            .unwrap();

        // Different fact text for the same (source, target, canonical_relation, edge_type) head
        // must go through the supersede chain — exercising check_supersede_depth_in_tx — and,
        // since the detector is enabled, the conflict-candidate raw query.
        let second_id = graph
            .insert_or_supersede_with_conflict_detection(
                a.0,
                b.0,
                "manages",
                "manages",
                "Eve no longer manages Frank",
                0.9,
                None,
                zeph_memory::graph::types::EdgeType::Semantic,
                true,
                None,
                Some(&detector),
                Some(&ontology),
            )
            .await
            .unwrap();

        assert_ne!(
            first_id, second_id,
            "differing fact text must create a new superseding row, not a reassertion"
        );

        let supersedes: Option<i64> = sqlx::query_scalar(zeph_db::sql!(
            "SELECT supersedes FROM graph_edges WHERE id = ?"
        ))
        .bind(second_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            supersedes,
            Some(first_id),
            "second edge must record the first as superseded"
        );
    }

    #[tokio::test]
    #[ignore = "requires Docker"]
    async fn implicit_conflict_stage_candidates_inserts_pending_row() {
        let (pool, _container) = start_pg().await;
        let graph = GraphStore::new(pool.clone());

        let a = graph
            .upsert_entity("G", "g", EntityType::Concept, None, None)
            .await
            .unwrap();
        let b = graph
            .upsert_entity("H", "h", EntityType::Concept, None, None)
            .await
            .unwrap();
        let edge_a = graph
            .insert_edge(a.0, b.0, "relates_to", "G relates to H", 0.8, None, None)
            .await
            .unwrap();
        let edge_b = graph
            .insert_edge(b.0, a.0, "relates_to", "H relates to G", 0.8, None, None)
            .await
            .unwrap();

        let detector = implicit_conflict::ImplicitConflictDetector::new(
            zeph_config::memory::ImplicitConflictConfig {
                enabled: true,
                ..Default::default()
            },
        );

        let candidates = vec![implicit_conflict::ConflictCandidate {
            edge_a_id: edge_a,
            edge_b_id: edge_b,
            similarity: 0.9,
            method: "levenshtein".to_owned(),
        }];

        let mut tx = pool.begin().await.unwrap();
        detector
            .stage_candidates(&candidates, &mut tx, 30)
            .await
            .unwrap();
        tx.commit().await.unwrap();

        let row: (i64, i64, String, String) = sqlx::query_as(zeph_db::sql!(
            "SELECT edge_a_id, edge_b_id, method, status FROM implicit_conflict_candidates \
             WHERE edge_a_id = ? AND edge_b_id = ?"
        ))
        .bind(edge_a)
        .bind(edge_b)
        .fetch_one(&pool)
        .await
        .unwrap();

        assert_eq!(row.0, edge_a);
        assert_eq!(row.1, edge_b);
        assert_eq!(row.2, "levenshtein");
        assert_eq!(row.3, "pending");
    }

    // ── five_signal::consolidation: run_once created_at TIMESTAMPTZ decode (#5507) ─────
    //
    // Regression coverage for #5507: `messages.created_at` is `TIMESTAMPTZ` in the Postgres
    // schema but `run_once` used to decode it via `row.get::<i64, _>("created_at")`, which
    // panicked with `ColumnDecode { ... mismatched types ... INT8 ... TIMESTAMPTZ }` for any
    // row once `scheduler` + `postgres` were both enabled. The fix casts the column through
    // `Dialect::epoch_from_col` (same pattern as `semantic::recall`'s `created_at_map` fetch)
    // before decoding. `run_once`/`apply_promotions`/`apply_demotions` are private, so this
    // drives them through the only public entry point, `ConsolidationHandler`'s `TaskHandler`
    // impl (`execute`).

    #[tokio::test]
    #[ignore = "requires Docker"]
    #[cfg(feature = "scheduler")]
    async fn five_signal_consolidation_promotes_fresh_fact() {
        use zeph_config::memory::{FiveSignalConfig, FiveSignalConsolidationConfig};
        use zeph_memory::five_signal::FiveSignalRuntime;
        use zeph_memory::five_signal::consolidation::ConsolidationHandler;
        use zeph_scheduler::TaskHandler as _;

        let (pool, _container) = start_pg().await;
        let store = SqliteStore::from_pool(pool.clone()).await.unwrap();
        let graph_store = std::sync::Arc::new(GraphStore::new(pool.clone()));

        let cid = store.create_conversation().await.unwrap();
        let msg_id = store.save_message(cid, "user", "fresh fact").await.unwrap();

        // session_start ~= now, so the freshly-inserted message's TIMESTAMPTZ `created_at`
        // (defaulted to `NOW()` by the schema) yields novelty ~= 1.0 once correctly decoded.
        let session_start = chrono::Utc::now().timestamp() - 5;
        let signal_config = FiveSignalConfig {
            w_recency: 0.0,
            w_relevance: 0.0,
            w_frequency: 0.0,
            w_causal: 0.0,
            w_novelty: 1.0,
            consolidation_daemon: FiveSignalConsolidationConfig {
                enabled: true,
                promotion_score_threshold: 0.5,
                ..FiveSignalConsolidationConfig::default()
            },
            ..FiveSignalConfig::default()
        };
        let daemon_config = signal_config.consolidation_daemon.clone();

        let runtime = std::sync::Arc::new(FiveSignalRuntime::new(
            signal_config,
            pool.clone(),
            graph_store,
            None,
            session_start,
            "sess-consolidation-pg",
        ));

        let handler = ConsolidationHandler::new(runtime, daemon_config);
        handler
            .execute(&serde_json::json!({}))
            .await
            .expect("consolidation run must not fail decoding created_at as TIMESTAMPTZ");

        let (qdrant_promoted, memory_tier): (i64, Option<String>) = sqlx::query_as(zeph_db::sql!(
            "SELECT qdrant_promoted, memory_tier FROM messages WHERE id = ?"
        ))
        .bind(msg_id.0)
        .fetch_one(&pool)
        .await
        .unwrap();

        assert_eq!(
            qdrant_promoted, 1,
            "fresh, high-novelty fact must be promoted"
        );
        assert_eq!(memory_tier.as_deref(), Some("semantic"));
    }

    // ── agent_sessions: upsert_agent_session + list_agent_sessions (Postgres) ──
    // Regression coverage for issue #5524, completing the fix after a live Postgres run
    // surfaced a second, previously-theorized-only defect: `agent_sessions.created_at`/
    // `last_active_at` are `TIMESTAMPTZ` in the Postgres schema
    // (`zeph-db/migrations/postgres/090_agent_sessions.sql`) but `AgentSessionRow.created_at`/
    // `last_active_at` are plain `String`, bound as text — Postgres has no implicit
    // text->timestamptz cast, so the INSERT failed with `PgDatabaseError` 42804 even after the
    // `sql!()` placeholder rewrite was fixed. `upsert_agent_session` now appends
    // `<ActiveDialect as Dialect>::TIMESTAMPTZ_CAST` (`::timestamptz` on Postgres, empty on
    // SQLite) directly after the `created_at`/`last_active_at` placeholders, mirroring the
    // existing `JSON_CAST` idiom used for `messages.parts`.

    #[tokio::test]
    #[ignore = "requires Docker"]
    async fn agent_sessions_upsert_and_list_postgres() {
        let (pool, _container) = start_pg().await;
        let store = SqliteStore::from_pool(pool).await.unwrap();

        let row = AgentSessionRow {
            id: "sess-pg-1".to_owned(),
            kind: SessionKind::Interactive,
            status: SessionStatus::Active,
            channel: SessionChannel::Cli,
            model: "claude-sonnet-5".to_owned(),
            created_at: "2026-07-03T00:00:00".to_owned(),
            last_active_at: "2026-07-03T00:00:00".to_owned(),
            turns: 3,
            prompt_tokens: 100,
            completion_tokens: 50,
            reasoning_tokens: 0,
            cost_cents: 1.5,
            goal_text: None,
        };
        store.upsert_agent_session(&row).await.unwrap();

        // Second upsert exercises the ON CONFLICT DO UPDATE branch.
        let mut updated = row.clone();
        updated.turns = 7;
        updated.status = SessionStatus::Completed;
        updated.last_active_at = "2026-07-03T00:05:00".to_owned();
        store.upsert_agent_session(&updated).await.unwrap();

        let all = store.list_agent_sessions(10, None).await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].id, "sess-pg-1");
        assert_eq!(all[0].turns, 7);
        assert_eq!(all[0].status, SessionStatus::Completed);

        let filtered = store
            .list_agent_sessions(10, Some(SessionStatus::Completed))
            .await
            .unwrap();
        assert_eq!(filtered.len(), 1);
        let none_active = store
            .list_agent_sessions(10, Some(SessionStatus::Active))
            .await
            .unwrap();
        assert!(none_active.is_empty());

        // Regression coverage for issue #5980: `limit = 0` ("unlimited") previously bound
        // `LIMIT -1`, which Postgres rejects (`ERROR: LIMIT must not be negative`), in both
        // the filtered and unfiltered branches of this query.
        let unlimited = store.list_agent_sessions(0, None).await.unwrap();
        assert_eq!(unlimited.len(), 1);
        let unlimited_filtered = store
            .list_agent_sessions(0, Some(SessionStatus::Completed))
            .await
            .unwrap();
        assert_eq!(unlimited_filtered.len(), 1);
    }

    // ── episodic_consolidation: full sweep (fetch_candidates → compute_cognitive_weight →
    // extract_facts_via_llm → mark_consolidated), Postgres ─────────────────────────────────
    //
    // Regression coverage for issue #5525 (see module docstring). Exercises the full
    // `run_episodic_consolidation_sweep` pipeline end-to-end against real Postgres, closing the
    // coverage gap left by #5508's `fetch_candidates`/`mark_consolidated` fix.

    #[tokio::test]
    #[ignore = "requires Docker"]
    async fn episodic_consolidation_sweep_promotes_fact_postgres() {
        let (pool, _container) = start_pg().await;
        let store = SqliteStore::from_pool(pool.clone()).await.unwrap();
        let session_id = SessionId::new("sess-consolidation-1");

        let cid = store.create_conversation().await.unwrap();
        let message_id = store
            .save_message(cid, "user", "Alice uses Rust for systems programming")
            .await
            .unwrap();

        // Inserted directly (rather than via `episodic_graph::store_events`, which always
        // stamps `created_at` at insert time) so `created_at` is safely in the past —
        // `fetch_candidates` requires `created_at < NOW() - min_age_secs`, and a
        // server-timestamped row risks landing in the same second as the sweep's query.
        let created_at = chrono::Utc::now().timestamp() - 600;
        let event_id: i64 = sqlx::query_scalar(zeph_db::sql!(
            "INSERT INTO episodic_events (session_id, message_id, event_type, summary, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             RETURNING id"
        ))
        .bind(session_id.as_str())
        .bind(message_id)
        .bind("tool_call")
        .bind("Alice prefers Rust")
        .bind(created_at)
        .fetch_one(&pool)
        .await
        .unwrap();

        let llm_response = format!(
            r#"[{{"fact":"Alice uses Rust for systems programming","source_event_ids":[{event_id}]}}]"#
        );
        let mut mock = MockProvider::default();
        mock.default_response = llm_response;
        let provider = AnyProvider::Mock(mock);

        let config = episodic_consolidation::EpisodicConsolidationConfig {
            enabled: true,
            consolidation_provider: ProviderName::default(),
            interval_secs: 1800,
            batch_size: 30,
            min_age_secs: 0,
            dedup_jaccard_threshold: 0.6,
        };

        let result = episodic_consolidation::run_episodic_consolidation_sweep(
            pool.clone(),
            &provider,
            &config,
            None,
        )
        .await
        .unwrap();

        assert_eq!(result.events_processed, 1);
        assert_eq!(
            result.facts_promoted, 1,
            "compute_cognitive_weight's NUMERIC->f64 decode must succeed for the sweep to reach \
             fact promotion (#5525)"
        );

        let count: i64 =
            sqlx::query_scalar(zeph_db::sql!("SELECT COUNT(*) FROM consolidated_facts"))
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(count, 1, "one fact must be persisted to consolidated_facts");

        let consolidated_at: Option<i64> = sqlx::query_scalar(zeph_db::sql!(
            "SELECT consolidated_at FROM episodic_events WHERE id = ?1"
        ))
        .bind(event_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(
            consolidated_at.is_some(),
            "event must be marked consolidated after sweep"
        );
    }

    // ── belief: record_evidence (apply_evidence_update) + mark_promoted (Postgres) ──
    // Regression coverage for issue #5508. `mark_promoted` and `apply_evidence_update` had
    // zero test coverage under either backend before this PR.

    #[tokio::test]
    #[ignore = "requires Docker"]
    async fn belief_store_record_evidence_and_promote_postgres() {
        let (pool, _container) = start_pg().await;
        let graph = GraphStore::new(pool.clone());

        let source = graph
            .upsert_entity("Rust", "rust", EntityType::Tool, None, None)
            .await
            .unwrap();
        let target = graph
            .upsert_entity(
                "Memory Safety",
                "memory-safety",
                EntityType::Concept,
                None,
                None,
            )
            .await
            .unwrap();

        let belief_store = BeliefStore::new(
            pool.clone(),
            BeliefMemConfig {
                enabled: true,
                min_entry_prob: 0.3,
                promote_threshold: 0.85,
                max_candidates_per_group: 10,
                retrieval_top_k: 3,
                belief_decay_rate: 0.0,
            },
        );

        // First call hits insert_new_belief; the remaining five hit apply_evidence_update —
        // the exact call site that embedded the SQLite-only unixepoch() before this fix.
        let mut promoted = None;
        for _ in 0..6 {
            promoted = belief_store
                .record_evidence(
                    source.0,
                    target.0,
                    "provides",
                    "provides",
                    "Rust provides memory safety",
                    EdgeType::Semantic,
                    0.3,
                    None,
                )
                .await
                .unwrap();
        }
        let belief = promoted.expect("six evidence updates at 0.3 must cross the 0.85 threshold");
        assert!(belief.prob >= 0.85);

        // Commit a real edge and mark the belief promoted — the other call site that
        // embedded the SQLite-only unixepoch() before this fix.
        let committed_edge_id = graph
            .insert_edge(
                source.0,
                target.0,
                "provides",
                "Rust provides memory safety",
                belief.prob,
                None,
                None,
            )
            .await
            .unwrap();

        belief_store
            .mark_promoted(belief.id, committed_edge_id)
            .await
            .unwrap();

        let (promoted_at, promoted_edge_id): (Option<i64>, Option<i64>) = sqlx::query_as(
            zeph_db::sql!("SELECT promoted_at, promoted_edge_id FROM pending_beliefs WHERE id = ?"),
        )
        .bind(belief.id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(
            promoted_at.is_some(),
            "mark_promoted must set promoted_at via the EPOCH_NOW-based UPDATE"
        );
        assert_eq!(promoted_edge_id, Some(committed_edge_id));
    }

    #[tokio::test]
    #[ignore = "requires Docker"]
    async fn optical_forgetting_summarizes_compressed_message() {
        use std::sync::Arc;

        use zeph_llm::any::AnyProvider;
        use zeph_llm::mock::MockProvider;
        use zeph_memory::optical_forgetting::run_optical_forgetting_sweep;

        let (pool, _container) = start_pg().await;
        let store = SqliteStore::from_pool(pool.clone()).await.unwrap();

        let cid = store.create_conversation().await.unwrap();
        let msg_id = store
            .save_message(cid, "user", "original content")
            .await
            .unwrap();

        // Force the message into `Compressed` fidelity with prior compressed_content so
        // Phase 2 (fetch_compressed_candidates + store_summary_only) picks it up.
        sqlx::query(zeph_db::sql!(
            "UPDATE messages SET content_fidelity = 'Compressed', \
             compressed_content = 'prior summary', importance_score = 1.0 WHERE id = ?"
        ))
        .bind(msg_id.0)
        .execute(&pool)
        .await
        .unwrap();

        let provider = Arc::new(AnyProvider::Mock(MockProvider::with_responses(vec![
            "one-line summary".to_owned(),
        ])));

        let config = zeph_config::memory::OpticalForgettingConfig {
            enabled: true,
            summarize_after_turns: 0,
            sweep_batch_size: 50,
            ..Default::default()
        };

        let result = run_optical_forgetting_sweep(&store, &provider, &config, 0.0)
            .await
            .unwrap();

        assert_eq!(
            result.summarized, 1,
            "the Compressed message must transition to SummaryOnly"
        );

        let (fidelity, content): (String, String) = sqlx::query_as(zeph_db::sql!(
            "SELECT content_fidelity, content FROM messages WHERE id = ?"
        ))
        .bind(msg_id.0)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(fidelity, "SummaryOnly");
        assert_eq!(content, "one-line summary");
    }

    // ── entity_lock: extend_lock + try_acquire dialect-split SQL (#5539) ───────

    /// Fetch `entity_advisory_locks.expires_at` as a Unix epoch (seconds), via the same
    /// `Dialect::epoch_from_col` cast used in production code (`semantic::recall`'s
    /// `created_at_map` fetch, `five_signal_consolidation_promotes_fresh_fact` above) to
    /// decode a `TIMESTAMPTZ` column without requiring sqlx's `chrono` feature.
    async fn fetch_lock_expires_at_epoch(pool: &zeph_db::DbPool, entity_name: &str) -> i64 {
        let epoch_expr =
            <zeph_db::ActiveDialect as zeph_db::dialect::Dialect>::epoch_from_col("expires_at");
        let raw = format!("SELECT {epoch_expr} FROM entity_advisory_locks WHERE entity_name = ?");
        sqlx::query_scalar(sqlx::AssertSqlSafe(zeph_db::rewrite_placeholders(&raw)))
            .bind(entity_name)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    #[ignore = "requires Docker"]
    async fn entity_lock_extend_lock_advances_expiry_postgres() {
        let (pool, _container) = start_pg().await;
        let mgr = EntityLockManager::new(pool.clone(), "session-pg-ext");

        assert!(
            mgr.try_acquire("entity::PgExt").await.unwrap(),
            "initial acquire must succeed"
        );
        let before = fetch_lock_expires_at_epoch(&pool, "entity::PgExt").await;

        let extended = mgr.extend_lock("entity::PgExt", 3600).await.unwrap();
        assert!(
            extended,
            "extend_lock must succeed against a live Postgres instance \
             (regression check for #5539: SQLite-only datetime() syntax used to fail here)"
        );

        let after = fetch_lock_expires_at_epoch(&pool, "entity::PgExt").await;
        assert_eq!(
            after - before,
            3600,
            "expires_at must advance by exactly extra_secs via \
             `expires_at + INTERVAL '1 second' * ?`"
        );
    }

    #[tokio::test]
    #[ignore = "requires Docker"]
    async fn entity_lock_try_acquire_postgres() {
        let (pool, _container) = start_pg().await;
        let mgr = EntityLockManager::new(pool.clone(), "session-pg-acq");

        // Exercises `try_acquire_once`'s `NOW() + INTERVAL 'N seconds'` Postgres branch
        // (the sibling fix from PR #5537, previously uncovered by any Postgres test).
        assert!(
            mgr.try_acquire("entity::PgAcq").await.unwrap(),
            "try_acquire must succeed against a live Postgres instance"
        );

        let now_epoch = chrono::Utc::now().timestamp();
        let expires_epoch = fetch_lock_expires_at_epoch(&pool, "entity::PgAcq").await;
        assert!(
            expires_epoch > now_epoch,
            "newly acquired lock must expire in the future (TTL applied via NOW() + INTERVAL)"
        );
    }

    // ── Regression coverage for #5538: TIMESTAMPTZ->String decode defect + the paired
    // TIMESTAMPTZ_CAST-on-bind defect, found during a full-crate dialect sweep. Each test
    // below exercises a call site that decodes a `TIMESTAMPTZ` column into a `String` field
    // and/or binds a Rust-formatted timestamp string against one, neither of which had
    // Postgres coverage before. ──────────────────────────────────────────────────────

    #[tokio::test]
    #[ignore = "requires Docker"]
    async fn graph_store_edges_at_timestamp_filters_and_decodes() {
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
        graph
            .insert_edge(a.0, b.0, "knows", "Alice knows Bob", 0.5, None, None)
            .await
            .unwrap();

        // A future timestamp must still find the still-active edge (valid_to IS NULL),
        // exercising both the `TIMESTAMPTZ_CAST` bind and the `EdgeRow` decode together.
        let edges = graph
            .edges_at_timestamp(a.0, "2099-01-01 00:00:00")
            .await
            .unwrap();
        assert_eq!(
            edges.len(),
            1,
            "active edge must be found at a future timestamp"
        );
        assert_eq!(edges[0].fact, "Alice knows Bob");
        assert!(
            !edges[0].valid_from.is_empty(),
            "valid_from must decode to a non-empty string"
        );

        // A timestamp before the edge existed must find nothing.
        let edges_before = graph
            .edges_at_timestamp(a.0, "2000-01-01 00:00:00")
            .await
            .unwrap();
        assert!(
            edges_before.is_empty(),
            "edge must not be valid before its valid_from"
        );
    }

    #[tokio::test]
    #[ignore = "requires Docker"]
    async fn graph_store_communities_decode_timestamps() {
        let (pool, _container) = start_pg().await;
        let graph = GraphStore::new(pool.clone());

        let a = graph
            .upsert_entity("Alice", "alice", EntityType::Person, None, None)
            .await
            .unwrap();
        let community_id = graph
            .upsert_community("team-a", "Alice's team", &[a.0], None)
            .await
            .unwrap();

        let all = graph.all_communities().await.unwrap();
        assert_eq!(all.len(), 1);
        assert!(
            !all[0].created_at.is_empty() && !all[0].updated_at.is_empty(),
            "created_at/updated_at must decode to non-empty strings"
        );

        let found = graph
            .find_community_by_id(community_id)
            .await
            .unwrap()
            .expect("community must be found by id");
        assert_eq!(found.name, "team-a");
        assert!(!found.created_at.is_empty() && !found.updated_at.is_empty());
    }

    // `experiments::experiment_results_since`/`list_experiment_results`/`best_experiment_result`
    // and `admission_training::get_training_batch` were also fixed for the `TIMESTAMPTZ`->
    // `String` decode defect, but a co-located, pre-existing column type mismatch
    // (`experiment_results.latency_ms`/`tokens_used` are `INTEGER`/`INT4` vs. the tuple's `i64`;
    // `admission_training_data.composite_score` is `REAL`/`FLOAT4` vs. the tuple's `f64`) made
    // that fix unreachable on Postgres until the co-located `CAST(... AS BIGINT)`/
    // `CAST(... AS DOUBLE PRECISION)` casts below were folded in. These tests exercise both the
    // `TIMESTAMPTZ` decode/bind-cast and the widening casts together.
    #[tokio::test]
    #[ignore = "requires Docker"]
    async fn experiments_list_best_and_since_decode_on_postgres() {
        let (pool, _container) = start_pg().await;
        let store = SqliteStore::from_pool(pool).await.unwrap();

        store
            .insert_experiment_result(&NewExperimentResult {
                session_id: "sess-1",
                parameter: "temperature",
                value_json: r#"{"type":"Float","value":0.7}"#,
                baseline_score: 7.0,
                candidate_score: 8.0,
                delta: 1.0,
                latency_ms: 500,
                tokens_used: 100,
                accepted: true,
                source: "manual",
            })
            .await
            .unwrap();

        let listed = store
            .list_experiment_results(Some("sess-1"), 10)
            .await
            .unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].latency_ms, 500);
        assert_eq!(listed[0].tokens_used, 100);
        assert!(!listed[0].created_at.is_empty());

        let best = store.best_experiment_result(None).await.unwrap().unwrap();
        assert_eq!(best.latency_ms, 500);
        assert_eq!(best.tokens_used, 100);

        let since = store
            .experiment_results_since("2000-01-01 00:00:00")
            .await
            .unwrap();
        assert_eq!(since.len(), 1);
        assert_eq!(since[0].tokens_used, 100);
    }

    #[tokio::test]
    #[ignore = "requires Docker"]
    async fn admission_training_get_training_batch_decodes_on_postgres() {
        let (pool, _container) = start_pg().await;
        let store = SqliteStore::from_pool(pool).await.unwrap();
        let cid = store.create_conversation().await.unwrap();

        store
            .record_admission_training(AdmissionTrainingInput {
                message_id: None,
                conversation_id: cid,
                content: "content",
                role: "user",
                composite_score: 0.5,
                was_admitted: false,
                features_json: "[]",
            })
            .await
            .unwrap();

        let batch = store.get_training_batch(10).await.unwrap();
        assert_eq!(batch.len(), 1);
        assert!((batch[0].composite_score - 0.5).abs() < 1e-6);
        assert!(!batch[0].created_at.is_empty());
    }

    // ── retrieval_failures: bind-cast DELETE (#5538) ────────────────────────────

    #[tokio::test]
    #[ignore = "requires Docker"]
    async fn retrieval_failures_purge_old_deletes_by_timestamptz_cutoff() {
        let (pool, _container) = start_pg().await;
        let store = SqliteStore::from_pool(pool.clone()).await.unwrap();

        store
            .record_retrieval_failure(&RetrievalFailureRecord {
                conversation_id: None,
                turn_index: 0,
                failure_type: RetrievalFailureType::NoHit,
                retrieval_strategy: "semantic".to_owned(),
                query_text: "query".to_owned(),
                query_len: 5,
                top_score: None,
                confidence_threshold: None,
                result_count: 0,
                latency_ms: 10,
                edge_types: None,
                error_context: None,
            })
            .await
            .unwrap();

        // Backdate `created_at` well past any retention window so the bind-cast DELETE
        // (`WHERE created_at < ?::timestamptz`) actually matches the row.
        sqlx::query(zeph_db::sql!(
            "UPDATE memory_retrieval_failures SET created_at = NOW() - INTERVAL '30 days'"
        ))
        .execute(&pool)
        .await
        .unwrap();

        let deleted = store.purge_old_retrieval_failures(7).await.unwrap();
        assert_eq!(
            deleted, 1,
            "row older than the retention cutoff must be purged"
        );
    }

    // ── persona: `AS`-alias `#[derive(FromRow)]` projection (#5538) ─────────────

    #[tokio::test]
    #[ignore = "requires Docker"]
    async fn persona_load_facts_decodes_timestamptz_columns() {
        let (pool, _container) = start_pg().await;
        let store = SqliteStore::from_pool(pool).await.unwrap();

        store
            .upsert_persona_fact("preference", "prefers dark mode", 0.9, None, None)
            .await
            .unwrap();

        let facts = store.load_persona_facts(0.0).await.unwrap();
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].content, "prefers dark mode");
        assert!(!facts[0].created_at.is_empty());
        assert!(!facts[0].updated_at.is_empty());
    }

    // ── skills: tuple-decode cluster (#5538) ─────────────────────────────────────

    #[tokio::test]
    #[ignore = "requires Docker"]
    async fn skills_load_versions_decodes_timestamptz_column() {
        let (pool, _container) = start_pg().await;
        let store = SqliteStore::from_pool(pool).await.unwrap();

        let v1 = store
            .save_skill_version("git", 1, "body v1", "Git helper", "manual", None, None)
            .await
            .unwrap();
        store.activate_skill_version("git", v1).await.unwrap();

        let versions = store.load_skill_versions("git").await.unwrap();
        assert_eq!(versions.len(), 1);
        assert_eq!(versions[0].version, 1);
        assert_eq!(versions[0].success_count, 0);
        assert_eq!(versions[0].failure_count, 0);
        assert!(versions[0].is_active);
        assert!(!versions[0].created_at.is_empty());

        let active = store.active_skill_version("git").await.unwrap().unwrap();
        assert_eq!(active.id, v1);
        assert_eq!(active.version, 1);
        assert!(!active.created_at.is_empty());
    }

    // Exercises `predecessor_version`, the third call site sharing `SkillVersionTuple` (#5573) —
    // `skills_load_versions_decodes_timestamptz_column` above already covers the other two.
    #[tokio::test]
    #[ignore = "requires Docker"]
    async fn skills_predecessor_version_decodes_integer_columns() {
        let (pool, _container) = start_pg().await;
        let store = SqliteStore::from_pool(pool).await.unwrap();

        let v1 = store
            .save_skill_version("git", 1, "body v1", "Git helper", "manual", None, None)
            .await
            .unwrap();
        let v2 = store
            .save_skill_version("git", 2, "body v2", "Git helper v2", "auto", None, Some(v1))
            .await
            .unwrap();
        store.activate_skill_version("git", v2).await.unwrap();

        let predecessor = store.predecessor_version(v2).await.unwrap().unwrap();
        assert_eq!(predecessor.id, v1);
        assert_eq!(predecessor.version, 1);
        assert_eq!(predecessor.success_count, 0);
        assert_eq!(predecessor.failure_count, 0);
    }

    // ── skill_usage: invocation_count INT4 decode (#5591) ───────────────────────
    //
    // `load_skill_usage` already decodes `invocation_count` (`INTEGER`/`INT4` on Postgres) as
    // `i32` and widens it to `i64` (see `store/skills.rs`), but had zero Postgres coverage
    // pinning that decode. This test records a distinct, non-default invocation count and
    // asserts it survives the round trip unchanged.

    #[tokio::test]
    #[ignore = "requires Docker"]
    async fn skills_load_skill_usage_decodes_invocation_count_postgres() {
        let (pool, _container) = start_pg().await;
        let store = SqliteStore::from_pool(pool).await.unwrap();

        for _ in 0..7 {
            store.record_skill_usage(&["git"]).await.unwrap();
        }

        let usage = store.load_skill_usage().await.unwrap();
        assert_eq!(usage.len(), 1);
        assert_eq!(usage[0].skill_name, "git");
        assert_eq!(
            usage[0].invocation_count, 7,
            "invocation_count must decode as the full INTEGER value, not truncate/zero"
        );
    }
}
