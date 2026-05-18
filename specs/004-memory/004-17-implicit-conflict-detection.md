---
aliases:
  - CUPMem
  - Implicit Conflict Detection
  - STALE Conflict Resolution
tags:
  - sdd
  - spec
  - memory
  - graph
  - experimental
created: 2026-05-18
status: draft
related:
  - "[[MOC-specs]]"
  - "[[constitution]]"
  - "[[004-memory/spec]]"
  - "[[004-7-memory-apex-magma]]"
  - "[[004-6-graph-memory]]"
  - "[[001-system-invariants/spec]]"
---

# Spec: Implicit Conflict Detection in SYNAPSE Recall (STALE / CUPMem)

> [!info]
> Extends APEX-MEM [[004-7-memory-apex-magma]] with write-time implicit conflict
> detection via fuzzy predicate matching and propagation-aware SYNAPSE recall that
> resolves conflicting beliefs before returning fact sets to the agent.
> Addresses the gap identified in STALE benchmark (arXiv:2605.06527) and tracked
> in GitHub issue [#3702](https://github.com/rabax/zeph/issues/3702).

## Sources

### External
- **STALE: Benchmarking Implicit Memory Conflicts in Long-Context Agents**
  (arXiv:2605.06527, 2026) — evaluates three failure dimensions: State Resolution,
  Premise Resistance, Implicit Policy Adaptation; frontier models achieve only 55.2%
  accuracy on implicit conflict detection
- **CUPMem: Conflict-Unaware Propagation Memory** (arXiv:2605.06527 companion) —
  write-time structured state consolidation with propagation-aware search

### Internal

| File | Contents |
|------|----------|
| `crates/zeph-memory/src/graph/store.rs` | Edge CRUD, `insert_or_supersede` (APEX-MEM) |
| `crates/zeph-memory/src/graph/extractor.rs` | LLM extraction → predicate strings |
| `crates/zeph-memory/src/semantic/graph.rs` | SYNAPSE spreading activation recall |
| `crates/zeph-memory/src/graph/types.rs` | `Edge`, `EdgeType`, temporal fields |
| `crates/zeph-memory/src/graph/ontology.rs` | Ontology normalization (APEX-MEM) |

---

## 1. Overview

### Problem Statement

APEX-MEM [[004-7-memory-apex-magma]] resolves conflicts between edges that share an
**identical `canonical_relation`** — this covers explicit supersession (e.g.,
`works_at` written twice with different targets). However, a large class of real
conflicts is **implicit**: later observations invalidate earlier beliefs without the
predicate strings being equal or near-equal.

Three concrete failure dimensions from the STALE benchmark:

1. **State Resolution**: the agent asserts a later observation (`Alice switched to
   Provider B`) but recall returns both the old belief (`uses Provider A`) and the new
   fact without resolving the conflict, because `uses` ≠ `switched_to` at the string
   level.
2. **Premise Resistance**: the agent is queried with a stale presupposition
   (`"Given that Alice uses Provider A, ..."`) and fails to reject the premise because
   the contradicting newer fact is not surfaced during recall.
3. **Implicit Policy Adaptation**: a policy fact changes (e.g., `max_retries = 3` →
   `max_retries = 5`) but the predicates differ semantically and the policy update is
   not propagated to downstream reasoning.

Existing MAGMA issue #2441 documents the symptom: SYNAPSE returns both stale and
current facts for semantically related predicates without resolution.

### Goal

Two complementary mechanisms:

1. **`ImplicitConflictDetector`** at write time: when a new edge is extracted, compare
   its predicate against existing active edges on the same source entity using both
   string-distance (Levenshtein) and semantic similarity (embedding cosine). When a
   likely conflict is detected, mark the old edge for resolution and apply the
   configured strategy.
2. **Propagation-aware SYNAPSE recall**: after assembling the candidate fact set, follow
   causal-link neighbors transitively to surface facts that may supersede the retrieved
   candidates, then apply conflict resolution before returning results to the agent.

### Out of Scope

- Replacing the APEX-MEM explicit-supersession mechanism (this spec adds implicit
  detection on top; explicit detection remains unchanged)
- Changing the `EdgeType` taxonomy or the MAGMA four-subgraph model
- Cross-entity coreference resolution beyond the existing `EntityResolver`
- New database migrations beyond adding an `implicit_conflict_candidates` staging table

---

## 2. User Stories

### US-001: Implicit fact supersession at write time
AS AN agent that processes information spanning days or weeks
I WANT the memory system to recognize when a new extracted fact semantically
supersedes an older one — even when predicates differ in wording
SO THAT the primary recall path returns only current beliefs

**Acceptance criteria:**
```
GIVEN an active edge (Agent-X, uses, Provider-A) in the graph
  AND a new edge (Agent-X, switched_to, Provider-B) is extracted
  AND "uses" and "switched_to" have embedding cosine similarity ≥ 0.82
WHEN insert_or_supersede runs for the new edge
THEN the ImplicitConflictDetector identifies (Agent-X, uses, Provider-A) as a candidate
AND the configured resolution strategy is applied (default: flag for LLM mediation)
AND SYNAPSE no longer returns Provider-A as the active provider for Agent-X
```

### US-002: Stale premise rejection in recall
AS AN agent processing a user query with a stale presupposition
I WANT recall to surface the superseding fact alongside the stale one
SO THAT the agent can reject the stale premise in its response

**Acceptance criteria:**
```
GIVEN the graph contains (Policy, max_retries, 3) [older]
  AND (Policy, retry_limit, 5) [newer, different predicate]
  AND embedding similarity between "max_retries" and "retry_limit" ≥ 0.80
WHEN SYNAPSE recalls facts about Policy
THEN both edges are returned with conflict metadata
AND the conflict metadata indicates which edge is the authoritative head
AND the result is annotated with is_implicit_conflict = true
```

### US-003: Async write-time consolidation
AS AN operator running long-lived agents
I WANT an optional background pass to review the episodic edge log for implicit conflicts
  and promote resolved facts to MAGMA
SO THAT conflicts accumulated over long windows are periodically resolved without
blocking the agent turn loop

**Acceptance criteria:**
```
GIVEN implicit_consolidation_daemon.enabled = true
  AND the daemon runs on its configured schedule (default: every 2 hours)
WHEN the daemon reviews the edge log
THEN for each implicit conflict candidate pair, it applies the configured strategy
AND resolved facts are committed to the committed edge store via insert_or_supersede
AND a log entry records: pair_ids, resolution_strategy, resolved_at, outcome
```

---

## 3. Functional Requirements

| ID | Requirement | Priority |
|----|------------|----------|
| FR-001 | WHEN a new edge is extracted AND `implicit_conflict_detection.enabled = true` THE SYSTEM SHALL compare the new edge's `canonical_relation` against all active edge predicates on the same source entity using the configured similarity method | must |
| FR-002 | Similarity methods SHALL include: `levenshtein` (normalized edit distance), `embedding` (cosine on predicate embedding), `both` (either match triggers detection) | must |
| FR-003 | WHEN similarity ≥ `conflict_similarity_threshold` (default 0.80) AND the existing edge has a different `canonical_relation` than the new edge THE SYSTEM SHALL mark the pair as an implicit conflict candidate in `implicit_conflict_candidates` | must |
| FR-004 | The resolution strategy for detected implicit conflicts SHALL be one of: `recency` (mark older edge superseded), `confidence` (pick higher confidence), `llm` (call `implicit_conflict_provider`), `flag_only` (mark without resolving; default) | must |
| FR-005 | WHEN strategy is `flag_only` THE SYSTEM SHALL insert the candidate pair into `implicit_conflict_candidates` but NOT supersede either edge; SYNAPSE recall annotates flagged conflicts in the result | must |
| FR-006 | WHEN strategy is `recency` or `confidence` THE SYSTEM SHALL call `insert_or_supersede` on the older or lower-confidence edge within the same write transaction | must |
| FR-007 | WHEN strategy is `llm` THE SYSTEM SHALL call `implicit_conflict_provider` asynchronously with both edge facts as context; on timeout (default 800 ms) fall back to `flag_only` | must |
| FR-008 | SYNAPSE recall SHALL, after assembling the candidate set, optionally follow causal-link edges up to `propagation_depth` hops (default 2) to surface potential superseding facts | should |
| FR-009 | WHEN SYNAPSE returns an edge that has a pending entry in `implicit_conflict_candidates` THE SYSTEM SHALL annotate the result with `is_implicit_conflict = true` and include the conflicting candidate in a `conflict_metadata` field | must |
| FR-010 | `implicit_conflict_candidates` entries SHALL be cleaned up when: (a) a resolution is applied, (b) either edge is explicitly superseded via APEX-MEM, or (c) `candidate_ttl_days` (default 30) expires | should |
| FR-011 | THE SYSTEM SHALL NOT run implicit conflict detection for edges with `cardinality = n` (multi-valued predicates) — only cardinality-1 predicates are candidates for implicit supersession | must |
| FR-012 | WHEN `implicit_consolidation_daemon.enabled = true` THE SYSTEM SHALL schedule an async task (via `zeph-scheduler`) that periodically reviews `implicit_conflict_candidates` and applies the configured strategy to unresolved pairs | should |
| FR-013 | Config flag `[memory.graph.implicit_conflict] enabled` SHALL gate all new code paths; when `false`, write-time detection and SYNAPSE annotation are skipped entirely | must |
| FR-014 | Every new code path introduced by this spec SHALL be instrumented with `tracing::info_span!` per the naming convention `memory.graph.implicit_conflict.<operation>` | must |
| FR-015 | THE SYSTEM SHALL export Prometheus counters: `implicit_conflict_candidates_total`, `implicit_conflict_resolved_total{strategy}`, `implicit_conflict_llm_timeouts_total` | should |

---

## 4. Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Performance | Write-time implicit conflict detection (Levenshtein method) SHALL complete in < 5 ms at p95 for source entities with ≤ 100 active edges |
| NFR-002 | Performance | Write-time detection using the embedding method requires a pre-computed predicate embedding; embedding computation is NEVER on the synchronous write path — embeddings are computed in the background queue and the similarity check is deferred until the embedding is available |
| NFR-003 | Performance | SYNAPSE propagation-aware recall overhead (2-hop causal traversal) SHALL add < 10 ms at p95 on graphs with ≤ 10k edges |
| NFR-004 | Performance | When `enabled = false`, write-time detection contributes zero overhead |
| NFR-005 | Reliability | LLM-mediated resolution has an 800 ms timeout; on timeout the strategy falls back to `flag_only` — no write operation blocks on an LLM call |
| NFR-006 | Reliability | Implicit conflict detection is additive on top of APEX-MEM; disabling it (`enabled = false`) must reproduce exactly pre-spec APEX-MEM behavior |
| NFR-007 | Accuracy | On the STALE benchmark's State Resolution sub-task, the `embedding` similarity method SHALL achieve ≥ 70% detection rate at ≤ 15% false-positive rate with the default threshold of 0.80 (validated against synthetic benchmark fixtures) |
| NFR-008 | Maintainability | `conflict_similarity_threshold` and the similarity method are runtime-configurable; no rebuild required to tune detection sensitivity |

---

## 5. Data Model Changes

### New Table: `implicit_conflict_candidates`

```sql
CREATE TABLE IF NOT EXISTS implicit_conflict_candidates (
    id              INTEGER PRIMARY KEY,
    edge_a_id       INTEGER NOT NULL REFERENCES edges(id),
    edge_b_id       INTEGER NOT NULL REFERENCES edges(id),
    similarity      REAL    NOT NULL,
    method          TEXT    NOT NULL,  -- "levenshtein" | "embedding" | "both"
    status          TEXT    NOT NULL DEFAULT 'pending',
                                       -- "pending" | "resolved" | "expired"
    resolution      TEXT,              -- NULL | "recency" | "confidence" | "llm" | "flag_only"
    created_at      INTEGER NOT NULL,
    resolved_at     INTEGER,
    expires_at      INTEGER NOT NULL
);

CREATE INDEX idx_icc_edge_a ON implicit_conflict_candidates(edge_a_id);
CREATE INDEX idx_icc_edge_b ON implicit_conflict_candidates(edge_b_id);
CREATE INDEX idx_icc_status  ON implicit_conflict_candidates(status, expires_at);
```

### `Edge` struct additions (extends APEX-MEM `Edge`)

No schema changes to the `edges` table are required. Implicit conflict metadata is
stored in `implicit_conflict_candidates` with foreign keys to edge ids.

### Database migration

Migration `047_implicit_conflict_candidates.sql` — creates the staging table above.
Wrapped in `BEGIN IMMEDIATE; ... COMMIT;` per constitution.

---

## 6. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|-------------------|
| Source entity has 0 active edges (first write) | Skip detection; no candidates possible |
| Two edges match above threshold but both are cardinality-n | Skip detection; no implicit conflict for multi-valued predicates (FR-011) |
| Embedding for a predicate is not yet computed | Defer embedding-based detection until embedding is available; if deferred and LLM fallback is `flag_only`, no candidate is created for that pair until embedding arrives |
| LLM resolution provider times out | Fall back to `flag_only`; increment `implicit_conflict_llm_timeouts_total` |
| Both edges in a pair are superseded before resolution | Mark candidate as `expired`; no resolution needed |
| Consolidation daemon runs while write-time detection is in progress | Per-entity lock in APEX-MEM serializes; daemon's `insert_or_supersede` call will observe the latest head |
| Detection identifies a false positive (predicates similar in form but semantically unrelated) | With `flag_only` strategy, the flag is present in results but no edge is superseded; LLM resolution will reject the conflict on review; false positives are a calibration concern, not a correctness issue |
| `candidate_ttl_days` expires for an unresolved pair | Set `status = expired`; pair is excluded from future SYNAPSE annotations; original edges unchanged |

---

## 7. Config

```toml
[memory.graph.implicit_conflict]
enabled = false                        # opt-in; default off

# Similarity method: "levenshtein" | "embedding" | "both"
similarity_method = "levenshtein"

# Similarity threshold above which a pair is a conflict candidate [0.0, 1.0]
conflict_similarity_threshold = 0.80

# Resolution strategy: "flag_only" | "recency" | "confidence" | "llm"
resolution_strategy = "flag_only"

# Provider name from [[llm.providers]] for LLM-mediated resolution (required for strategy = "llm")
implicit_conflict_provider = ""

# Timeout for LLM resolution call
conflict_llm_timeout_ms = 800

# Days before an unresolved candidate entry expires
candidate_ttl_days = 30

# SYNAPSE propagation depth for surfacing potential superseding facts
propagation_depth = 2

[memory.graph.implicit_conflict.consolidation_daemon]
enabled = false
# Schedule: cron expression or interval (seconds)
interval_seconds = 7200               # 2 hours
# Max candidates processed per daemon run
batch_size = 100
```

---

## 8. Key Invariants

### Always (without asking)
- Implicit conflict detection is additive over APEX-MEM; no existing write or recall paths are modified when `enabled = false`
- Cardinality-n predicates are never flagged as implicit conflicts
- The write-time detection path never blocks on an LLM call; LLM resolution is async-only
- `implicit_conflict_candidates` entries are cleaned up when either edge is superseded, resolved, or expired
- `propagation_depth = 0` disables SYNAPSE propagation-aware recall; default is 2 hops

### Ask First
- Raising `conflict_similarity_threshold` below 0.70 (significantly increases false positives)
- Switching `resolution_strategy` from `flag_only` to `recency` or `confidence` in production without running calibration tests
- Enabling `similarity_method = "embedding"` before predicate embedding backfill is complete

### Never
- Apply implicit conflict resolution to cardinality-n predicates
- Block the synchronous write path on embedding computation or LLM calls
- Supersede an edge via implicit detection without following the APEX-MEM `insert_or_supersede` contract (FR-006)
- Return `implicit_conflict_candidates` entries directly to the LLM as part of the main context

---

## 9. Success Criteria

| ID | Metric | Target |
|----|--------|--------|
| SC-001 | STALE State Resolution sub-task detection rate (embedding method, default threshold) | ≥ 70% |
| SC-002 | False-positive rate on a benign 100-edge graph with no implicit conflicts | ≤ 15% |
| SC-003 | Write-time detection latency (Levenshtein method, 100 active edges) | < 5 ms at p95 |
| SC-004 | SYNAPSE propagation-aware recall overhead (2-hop causal, 10k edges) | < 10 ms at p95 |
| SC-005 | Implicit conflict detection disabled (`enabled = false`) reproduces pre-spec APEX-MEM behavior | 100% |

---

## 10. Acceptance Criteria

```
GIVEN implicit_conflict.enabled = true
  AND similarity_method = "levenshtein"
  AND an active edge (X, uses, Provider-A)
  AND a new edge (X, switched_to, Provider-B) with Levenshtein similarity < 0.80
WHEN insert_or_supersede runs for the new edge
THEN no conflict candidate is created (similarity below threshold)
AND both edges remain active

GIVEN an active edge (X, uses, Provider-A)
  AND a new edge (X, employs, Provider-B) with Levenshtein similarity ≥ 0.80
  AND resolution_strategy = "flag_only"
WHEN insert_or_supersede runs for the new edge
THEN one row is inserted into implicit_conflict_candidates with status = "pending"
AND both edges remain in the committed edge store (neither superseded)
AND SYNAPSE recall annotates both edges with is_implicit_conflict = true

GIVEN a candidate pair in implicit_conflict_candidates with status = "pending"
  AND resolution_strategy = "recency"
  AND the consolidation daemon runs
WHEN the daemon processes the pair
THEN the older edge is superseded via insert_or_supersede
AND the candidate row is updated to status = "resolved", resolution = "recency"
AND implicit_conflict_resolved_total{strategy="recency"} increments

GIVEN implicit_conflict.enabled = false
WHEN any number of edges are written or recalled
THEN implicit_conflict_candidates remains empty
AND SYNAPSE results contain no is_implicit_conflict annotations
```

---

## 11. Implementation Notes

- New module: `crates/zeph-memory/src/graph/implicit_conflict.rs` — owns
  `ImplicitConflictDetector`, candidate staging, and the Levenshtein check.
- Embedding-based similarity reuses the existing embedding infrastructure from
  `zeph-memory/src/embeddings/` — predicate strings are embedded the same way as
  message content; the embedding queue is shared.
- SYNAPSE propagation-aware recall is an opt-in post-processing pass added to
  `graph_recall_activated` in `crates/zeph-memory/src/semantic/graph.rs`; it queries
  the `edges` table for causal-edge neighbors of the retrieved node set and re-runs
  head-of-chain filtering on the expanded set.
- The consolidation daemon is a `zeph-scheduler` task registered at startup when both
  `graph.implicit_conflict.enabled = true` and `consolidation_daemon.enabled = true`.
  It shares the `ConsolidationTask` infrastructure introduced by HeLa-Mem
  [[004-11-memory-hela-mem]] to avoid duplicating scheduler registration boilerplate.
- Migration 047 is separate from APEX-MEM migration 042; both can be applied independently.

---

## 12. Open Questions

> [!question]
> - **Embedding-based predicate similarity at write time**: computing predicate embeddings
>   on every write introduces latency unless pre-computed. The proposed approach (defer
>   detection until embedding is available) means a conflict candidate may not be created
>   until the next batch embedding cycle. This lag needs to be bounded and communicated
>   to operators. Exact deferral semantics must be defined before FR-002 (embedding method)
>   is implemented.
> - **STALE benchmark integration**: validation of the 70% detection rate target (SC-001)
>   requires adapting the STALE evaluation suite to Zeph's edge schema. A benchmark
>   adapter must be written in `zeph-bench` before the success criterion can be measured.

---

## 13. See Also

- [[constitution]] — project principles
- [[004-memory/spec]] — memory system parent index
- [[004-7-memory-apex-magma]] — APEX-MEM (explicit supersession; this spec adds implicit detection on top)
- [[004-6-graph-memory]] — MAGMA typed edges, SYNAPSE recall
- [[004-11-memory-hela-mem]] — HeLa-Mem consolidation daemon (shared scheduler infrastructure)
- [[001-system-invariants/spec]] — system-wide invariants
- [[MOC-specs]] — all specifications
