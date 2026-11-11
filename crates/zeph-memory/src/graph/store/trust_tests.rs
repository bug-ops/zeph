// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `trust_level` persistence and NULL-absorbing merge tests (#6953, spec-067).

use super::*;
use crate::graph::types::{EdgeType, GraphOrigin, GraphProvenance};
use crate::store::SqliteStore;
use zeph_llm::provider::TurnContextTrust as T;

async fn setup() -> GraphStore {
    let store = SqliteStore::new(":memory:").await.unwrap();
    GraphStore::new(store.pool().clone())
}

fn prov(trust: T) -> GraphProvenance {
    GraphProvenance {
        origin: GraphOrigin::Ingest,
        import_batch_id: "batch-trust".to_owned(),
        source_uri: None,
        trust,
    }
}

async fn entity_trust(gs: &GraphStore, name: &str) -> Option<T> {
    gs.find_entity(name, EntityType::Concept)
        .await
        .unwrap()
        .unwrap()
        .trust_level
}

async fn edge_trust(gs: &GraphStore, id: i64) -> Result<Option<T>, MemoryError> {
    let raw = format!(
        "SELECT {} FROM graph_edges WHERE id = ?1",
        edge_select_cols("")
    );
    let row: EdgeRow = sqlx::query_as(sqlx::AssertSqlSafe(raw))
        .bind(id)
        .fetch_one(&gs.pool)
        .await
        .unwrap();
    edge_from_row(row).map(|e| e.trust_level)
}

async fn concept(gs: &GraphStore, name: &str, provenance: Option<&GraphProvenance>) -> i64 {
    gs.upsert_entity(name, name, EntityType::Concept, None, provenance)
        .await
        .unwrap()
        .0
}

async fn edge(gs: &GraphStore, a: i64, b: i64, provenance: Option<&GraphProvenance>) -> i64 {
    gs.insert_edge(a, b, "rel", "fact", 0.5, None, provenance)
        .await
        .unwrap()
}

#[tokio::test]
async fn trust_level_persisted_for_every_tier_and_null_for_conversation() {
    let gs = setup().await;
    for (name, trust) in [
        ("t0", T::Trusted),
        ("t1", T::LocalUntrusted),
        ("t2", T::ExternalUntrusted),
    ] {
        concept(&gs, name, Some(&prov(trust))).await;
        assert_eq!(entity_trust(&gs, name).await, Some(trust));
    }
    let a = concept(&gs, "conv", None).await;
    assert_eq!(entity_trust(&gs, "conv").await, None);

    let b = concept(&gs, "other", None).await;
    let ingest = edge(&gs, a, b, Some(&prov(T::LocalUntrusted))).await;
    let conv = gs
        .insert_edge(a, b, "r2", "f2", 0.9, None, None)
        .await
        .unwrap();
    assert_eq!(
        edge_trust(&gs, ingest).await.unwrap(),
        Some(T::LocalUntrusted)
    );
    assert_eq!(edge_trust(&gs, conv).await.unwrap(), None);
}

#[tokio::test]
async fn entity_trust_merge_takes_least_trusted_tier() {
    let gs = setup().await;
    for trust in [
        T::LocalUntrusted,
        T::Trusted,
        T::ExternalUntrusted,
        T::Trusted,
    ] {
        concept(&gs, "m", Some(&prov(trust))).await;
    }
    assert_eq!(entity_trust(&gs, "m").await, Some(T::ExternalUntrusted));
}

#[tokio::test]
async fn entity_trust_null_absorbs_in_both_orders() {
    let gs = setup().await;
    concept(&gs, "o1", Some(&prov(T::Trusted))).await;
    concept(&gs, "o1", None).await;
    assert_eq!(
        entity_trust(&gs, "o1").await,
        None,
        "ingest 0 then conversation"
    );
    concept(&gs, "o1", Some(&prov(T::Trusted))).await;
    assert_eq!(entity_trust(&gs, "o1").await, None, "NULL stays sticky");

    concept(&gs, "o2", None).await;
    concept(&gs, "o2", Some(&prov(T::Trusted))).await;
    assert_eq!(
        entity_trust(&gs, "o2").await,
        None,
        "conversation then ingest 0"
    );
}

#[tokio::test]
async fn edge_trust_merge_and_null_absorb_on_reencounter() {
    let gs = setup().await;
    let a = concept(&gs, "ta", None).await;
    let b = concept(&gs, "tb", None).await;

    let id = edge(&gs, a, b, Some(&prov(T::LocalUntrusted))).await;
    assert_eq!(edge(&gs, a, b, Some(&prov(T::Trusted))).await, id);
    assert_eq!(edge_trust(&gs, id).await.unwrap(), Some(T::LocalUntrusted));
    edge(&gs, a, b, Some(&prov(T::ExternalUntrusted))).await;
    assert_eq!(
        edge_trust(&gs, id).await.unwrap(),
        Some(T::ExternalUntrusted)
    );
    edge(&gs, a, b, None).await;
    assert_eq!(
        edge_trust(&gs, id).await.unwrap(),
        None,
        "ingest then conversation"
    );
    edge(&gs, a, b, Some(&prov(T::Trusted))).await;
    assert_eq!(
        edge_trust(&gs, id).await.unwrap(),
        None,
        "NULL stays sticky"
    );

    let c = concept(&gs, "tc", None).await;
    let d = concept(&gs, "td", None).await;
    let conv = edge(&gs, c, d, None).await;
    edge(&gs, c, d, Some(&prov(T::Trusted))).await;
    assert_eq!(
        edge_trust(&gs, conv).await.unwrap(),
        None,
        "conversation then ingest"
    );
}

#[tokio::test]
async fn edge_reassertion_path_absorbs_null_trust() {
    let gs = setup().await;
    let a = concept(&gs, "ra", None).await;
    let b = concept(&gs, "rb", None).await;
    let write = |p: Option<GraphProvenance>| {
        let gs = &gs;
        async move {
            gs.insert_or_supersede_with_turn_index_and_metrics(
                a,
                b,
                "rel",
                "rel",
                "same fact",
                0.5,
                None,
                EdgeType::Semantic,
                true,
                None,
                None,
                p.as_ref(),
            )
            .await
            .unwrap()
        }
    };
    let id = write(Some(prov(T::Trusted))).await;
    assert_eq!(write(None).await, id, "identical fact is a reassertion");
    assert_eq!(edge_trust(&gs, id).await.unwrap(), None);
}

#[tokio::test]
async fn out_of_range_trust_level_is_a_decode_error() {
    let gs = setup().await;
    let a = concept(&gs, "ta", None).await;
    let b = concept(&gs, "tb", None).await;
    let id = edge(&gs, a, b, None).await;
    sqlx::query("UPDATE graph_edges SET trust_level = 9 WHERE id = ?1")
        .bind(id)
        .execute(&gs.pool)
        .await
        .unwrap();
    assert!(matches!(
        edge_trust(&gs, id).await,
        Err(MemoryError::GraphStore(_))
    ));
    sqlx::query("UPDATE graph_entities SET trust_level = -1 WHERE canonical_name = 'ta'")
        .execute(&gs.pool)
        .await
        .unwrap();
    assert!(gs.find_entity("ta", EntityType::Concept).await.is_err());
}

#[tokio::test]
async fn migration_117_backfills_trust_without_laundering_rewritten_rows() {
    let gs = setup().await;
    // (name, origin, last_seen_at): `origin` keeps the first writer, so a later write only moves
    // `last_seen_at`.
    for (name, origin, last_seen) in [
        ("b-ingest", "ingest", "2026-01-01 00:00:00"),
        ("b-ingest-rewritten", "ingest", "2026-02-01 00:00:00"),
        ("b-sub", "subagent", "2026-01-01 00:00:00"),
        ("b-ext", "external-agent", "2026-01-01 00:00:00"),
        ("b-conv", "conversation", "2026-01-01 00:00:00"),
    ] {
        sqlx::query(
            "INSERT INTO graph_entities \
             (name, canonical_name, entity_type, origin, first_seen_at, last_seen_at) \
             VALUES (?1, ?1, 'concept', ?2, '2026-01-01 00:00:00', ?3)",
        )
        .bind(name)
        .bind(origin)
        .bind(last_seen)
        .execute(&gs.pool)
        .await
        .unwrap();
    }
    let a = concept(&gs, "edge-a", None).await;
    let b = concept(&gs, "edge-b", None).await;
    let mut edge_ids = Vec::new();
    for (relation, origin) in [
        ("e-ingest", GraphOrigin::Ingest),
        ("e-sub", GraphOrigin::Subagent),
        ("e-ext", GraphOrigin::ExternalAgent),
    ] {
        let p = GraphProvenance {
            origin,
            ..prov(T::Trusted)
        };
        let id = gs
            .insert_edge(a, b, relation, "fact", 0.5, None, Some(&p))
            .await
            .unwrap();
        edge_ids.push(id);
    }
    sqlx::query("UPDATE graph_entities SET trust_level = NULL")
        .execute(&gs.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE graph_edges SET trust_level = NULL")
        .execute(&gs.pool)
        .await
        .unwrap();

    let migration = include_str!("../../../../zeph-db/migrations/sqlite/117_graph_trust_level.sql");
    for stmt in migration.split(';') {
        let sql = stmt
            .lines()
            .filter(|l| !l.trim_start().starts_with("--"))
            .collect::<Vec<_>>()
            .join("\n");
        if sql.trim_start().starts_with("UPDATE") {
            sqlx::query(sqlx::AssertSqlSafe(sql))
                .execute(&gs.pool)
                .await
                .unwrap();
        }
    }

    assert_eq!(entity_trust(&gs, "b-ingest").await, Some(T::Trusted));
    assert_eq!(
        entity_trust(&gs, "b-ingest-rewritten").await,
        None,
        "an ingest row re-written later must not be backfilled Trusted"
    );
    assert_eq!(entity_trust(&gs, "b-sub").await, Some(T::LocalUntrusted));
    assert_eq!(entity_trust(&gs, "b-ext").await, Some(T::ExternalUntrusted));
    assert_eq!(entity_trust(&gs, "b-conv").await, None);
    assert_eq!(edge_trust(&gs, edge_ids[0]).await.unwrap(), None);
    assert_eq!(
        edge_trust(&gs, edge_ids[1]).await.unwrap(),
        Some(T::LocalUntrusted)
    );
    assert_eq!(
        edge_trust(&gs, edge_ids[2]).await.unwrap(),
        Some(T::ExternalUntrusted)
    );
}
