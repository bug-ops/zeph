---
aliases:
  - MemTier Five-Signal Retrieval
  - Five-Signal SYNAPSE
  - Async Consolidation Daemon
tags:
  - sdd
  - spec
  - memory
  - retrieval
  - experimental
created: 2026-05-18
status: draft
related:
  - "[[MOC-specs]]"
  - "[[constitution]]"
  - "[[004-memory/spec]]"
  - "[[004-6-graph-memory]]"
  - "[[004-5-temporal-decay]]"
  - "[[004-11-memory-hela-mem]]"
  - "[[001-system-invariants/spec]]"
---

# Spec: Five-Signal Retrieval and Async Consolidation Daemon (MemTier)

> [!info]
> Extends SYNAPSE recall with three additional retrieval signals (access frequency,
> causal distance, novelty) beyond the current two-signal baseline (recency +
> semantic relevance), and introduces an async consolidation daemon that promotes
> high-utility episodic facts to the semantic layer.
> Based on MemTier analysis (arXiv:2605.03675). Tracked in GitHub issue
> [#3703](https://github.com/rabax/zeph/issues/3703).

## Sources

### External
- **MemTier: Tiered Memory Architecture for Long-Horizon Agent Tasks**
  (arXiv:2605.03675, 2026) — identifies four failure modes in flat-file memory over
  72-hour windows; five-signal weighted retrieval achieves +33pp on LongMemEval-S;
  async consolidation daemon reduces tool-execution success degradation from 14pp → 0pp

### Internal

| File | Contents |
|------|----------|
| `crates/zeph-memory/src/semantic/mod.rs` | SYNAPSE recall, `SemanticMemory` |
| `crates/zeph-memory/src/semantic/graph.rs` | `graph_recall_activated`, spreading activation |
| `crates/zeph-memory/src/graph/store.rs` | MAGMA graph traversal (causal edges) |
| `crates/zeph-scheduler/src/lib.rs` | Background task scheduler |
| `crates/zeph-experiments/src/lib.rs` | Self-learning / hyperparameter tuning framework |
| `crates/zeph-memory/src/sleepgate/mod.rs` | SleepGate importance-score forgetting |

---

## 1. Overview

### Problem Statement

Zeph's current SYNAPSE recall weights two signals: **temporal recency** and **semantic
relevance** (Qdrant vector similarity). Over 72-hour agent windows, three failure modes
emerge that these two signals cannot address:

1. **Retrieval interference**: frequently-queried but semantically distant facts are
   ranked below rarely-accessed but semantically close facts. An access frequency signal
   corrects this.
2. **Goal-disconnected recall**: facts far removed from the current agent goal are
   ranked equally with directly causally connected facts. A causal distance signal
   (graph hops from the current goal node) corrects this.
3. **Stale-episodic interference**: facts old relative to agent initialization but still
   semantically similar to current queries pollute results. A novelty signal (decay from
   agent initialization) corrects this.

Additionally, SleepGate performs synchronous single-pass forgetting. For long-running
agents, a continuously running async consolidation daemon that promotes hot episodic
facts to the semantic (Qdrant) layer and deprioritizes cold facts prevents the
14-percentage-point tool-execution success degradation observed by MemTier over
72-hour windows.

### Goal

Two complementary changes:

1. **Five-signal SYNAPSE retrieval**: add `access_frequency`, `causal_distance`, and
   `novelty` signals to the SYNAPSE recall scoring function, with configurable per-signal
   weights.
2. **Async consolidation daemon**: background scheduler task (via `zeph-scheduler`) that
   periodically reviews the episodic log, computes five-signal scores, promotes top-K
   facts to Qdrant, and demotes cold facts below a retention threshold.

### Out of Scope

- Replacing the existing two-signal SYNAPSE baseline (new signals are additive with
  weight 0.0 by default, preserving backward compatibility)
- Modifying SleepGate's synchronous forgetting pass (the daemon is a companion, not a
  replacement)
- PPO-based weight adaptation (tracked as a follow-on task in `zeph-experiments`; not
  part of this spec's must-have scope)
- Changes to the Qdrant index schema beyond adding new payload metadata fields
- New memory tiers beyond the existing SQLite episodic / Qdrant semantic split

---

## 2. User Stories

### US-001: Access-frequency-boosted recall
AS AN agent in a long-running session
I WANT frequently-queried facts to rank higher in retrieval
SO THAT facts I have used repeatedly remain accessible even when semantic similarity
is not the highest

**Acceptance criteria:**
```
GIVEN fact F1 has been queried 20 times (high access_frequency)
  AND fact F2 has never been queried (access_frequency = 0)
  AND F2 has marginally higher semantic similarity to the current query than F1
WHEN SYNAPSE retrieves top-K facts with access_frequency weight > 0
THEN F1 ranks above F2 in the weighted score
```

### US-002: Causal-distance-gated recall
AS AN agent executing a multi-step goal
I WANT facts causally connected to my current goal to rank higher than
semantically similar but causally distant facts
SO THAT recall stays focused on the active task trajectory

**Acceptance criteria:**
```
GIVEN the current goal node G
  AND fact F1 is 1 causal hop from G (causal_distance = 1)
  AND fact F2 is 5 causal hops from G (causal_distance = 5)
  AND F2 has higher semantic similarity than F1
WHEN SYNAPSE retrieves with causal_distance weight > 0
THEN F1 ranks above F2 after signal weighting
```

### US-003: Async promotion of hot episodic facts
AS AN operator running Zeph for multi-day sessions
I WANT the consolidation daemon to automatically promote high-utility episodic facts
to the semantic layer
SO THAT SYNAPSE vector search can surface them without scanning the full SQLite log

**Acceptance criteria:**
```
GIVEN the daemon is enabled with interval_seconds = 7200
  AND fact F has five-signal score above promotion_score_threshold after 2 hours
WHEN the daemon runs
THEN F is upserted into Qdrant with five-signal metadata in its payload
AND the episodic store records F.qdrant_promoted = true
AND the daemon run is logged with: facts_promoted, facts_demoted, run_duration_ms
```

---

## 3. Functional Requirements

| ID | Requirement | Priority |
|----|------------|----------|
| FR-001 | WHEN SYNAPSE computes the retrieval score for a candidate fact THE SYSTEM SHALL compute: `score = w_recency × recency + w_relevance × relevance + w_frequency × access_frequency + w_causal × (1 / causal_distance) + w_novelty × novelty` where each weight is sourced from config | must |
| FR-002 | When `w_frequency = 0`, `w_causal = 0`, `w_novelty = 0` (the defaults), the five-signal formula MUST be algebraically equivalent to the current two-signal baseline | must |
| FR-003 | `access_frequency` for a fact SHALL be tracked in a new `fact_access_log` SQLite table: one row per (fact_id, turn_id) access event; the signal value is `log(1 + access_count)` normalized to `[0.0, 1.0]` across the candidate set | must |
| FR-004 | WHEN a fact is returned to the agent loop (included in the context window) THE SYSTEM SHALL insert one row into `fact_access_log(fact_id, accessed_at, session_id)` | must |
| FR-005 | `causal_distance` for a fact SHALL be computed as the minimum number of causal-type edges in the MAGMA graph between the fact's source entity and the current goal entity; if no goal entity is set, `causal_distance` defaults to a neutral value `neutral_causal_distance` (default 5) | must |
| FR-006 | Goal entity resolution SHALL use the most recently mentioned goal node from the agent's active context (sourced from `TurnContext.current_goal_entity_id`); if absent, the causal distance signal contribution is zero regardless of weight | must |
| FR-007 | `novelty` SHALL be computed as `exp(-λ_novelty × days_since_agent_init)` where `days_since_agent_init` is the fact's `created_at` minus the session start timestamp | must |
| FR-008 | Signal weights SHALL be normalized to sum to 1.0 at startup; if the configured weights do not sum to 1.0, the system SHALL normalize them and log a `WARN` | must |
| FR-009 | Config flag `[memory.five_signal] enabled` SHALL gate all five-signal code paths; when `false`, SYNAPSE uses the existing two-signal formula unchanged | must |
| FR-010 | WHEN `consolidation_daemon.enabled = true` THE SYSTEM SHALL schedule an async task via `zeph-scheduler` that runs at `interval_seconds` intervals and (a) queries the top-K episodic facts by five-signal score, (b) upserts them to Qdrant with signal metadata in payload, (c) marks demoted facts in SQLite | should |
| FR-011 | The consolidation daemon SHALL process at most `batch_size` facts per run to bound runtime; remaining facts are processed in the next run | should |
| FR-012 | WHEN a fact is demoted by the daemon (five-signal score below `demotion_score_threshold`) THE SYSTEM SHALL set `memory_tier = episodic_only` in the episodic store; subsequent Qdrant searches WILL NOT return demoted facts until their score recovers above the promotion threshold | should |
| FR-013 | THE SYSTEM SHALL export Prometheus counters: `five_signal_recall_total`, `consolidation_daemon_runs_total`, `consolidation_promoted_total`, `consolidation_demoted_total` | should |
| FR-014 | Every new code path SHALL be instrumented with `tracing::info_span!` per the naming convention `memory.five_signal.<operation>` | must |

---

## 4. Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Performance | Five-signal SYNAPSE scoring overhead SHALL add < 5 ms at p95 per retrieval call for candidate sets ≤ 200 facts |
| NFR-002 | Performance | `fact_access_log` insert (FR-004) is fire-and-forget (non-blocking); it SHALL complete in < 1 ms at p99 |
| NFR-003 | Performance | Causal distance computation (BFS on MAGMA causal edges) SHALL be bounded by `causal_bfs_max_depth` (default 10 hops); facts beyond this depth receive `neutral_causal_distance` |
| NFR-004 | Performance | Consolidation daemon runs in the background on the scheduler thread; it SHALL not block agent turns. Daemon runtime per batch SHALL be ≤ `daemon_max_runtime_ms` (default 30000 ms); excess facts are deferred to the next run |
| NFR-005 | Reliability | When `enabled = false`, the five-signal code paths contribute zero overhead — all new code branches are behind the feature flag |
| NFR-006 | Reliability | Consolidation daemon failures (Qdrant unavailable, SQLite write error) SHALL be logged and retried on the next scheduled run; agent operation is not interrupted |
| NFR-007 | Reliability | Weight normalization (FR-008) is applied once at startup and cached; it does not occur on the hot retrieval path |
| NFR-008 | Accuracy | On MemTier's LongMemEval-S benchmark, the five-signal retrieval with default weights SHALL improve retrieval accuracy vs. two-signal baseline by ≥ 15pp (validated on synthetic benchmark fixtures in `zeph-bench`) |

---

## 5. Data Model Changes

### New Table: `fact_access_log`

```sql
CREATE TABLE IF NOT EXISTS fact_access_log (
    id          INTEGER PRIMARY KEY,
    fact_id     INTEGER NOT NULL,      -- references messages.id or graph edge id
    fact_type   TEXT    NOT NULL,      -- "message" | "edge"
    session_id  TEXT    NOT NULL,
    accessed_at INTEGER NOT NULL
);

CREATE INDEX idx_fact_access_fact ON fact_access_log(fact_id, accessed_at DESC);
CREATE INDEX idx_fact_access_session ON fact_access_log(session_id, accessed_at DESC);
```

### Qdrant payload additions (no schema migration required)

Promoted facts gain additional metadata fields in the Qdrant point payload:

```json
{
  "access_count": 15,
  "causal_distance_at_promotion": 2,
  "novelty_at_promotion": 0.87,
  "five_signal_score": 0.91,
  "promoted_at": 1747600000,
  "memory_tier": "semantic"
}
```

### SQLite `messages` table additions

```sql
ALTER TABLE messages ADD COLUMN memory_tier TEXT DEFAULT 'episodic';
ALTER TABLE messages ADD COLUMN qdrant_promoted INTEGER DEFAULT 0;  -- boolean
```

Database migration: `048_five_signal_retrieval.sql` — adds `fact_access_log` table,
alters `messages` table. Wrapped in `BEGIN IMMEDIATE; ... COMMIT;`.

---

## 6. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|-------------------|
| `TurnContext.current_goal_entity_id` is not set | Causal distance signal contribution = 0 regardless of `w_causal`; no BFS is run |
| MAGMA graph has no causal-type edges | All facts receive `neutral_causal_distance`; causal signal is uniform and has no discriminating effect |
| `fact_access_log` insert fails (SQLite I/O error) | Log `WARN`; continue agent turn; access count tracking is best-effort |
| Consolidation daemon runs while agent turn is in progress | Qdrant upserts are atomic per-point; concurrent reads from SYNAPSE see either the old or new version — both are valid. No lock is held across the full batch |
| Qdrant is unavailable during daemon promotion | Skip the promotion phase; log `WARN`; try again on the next scheduled run |
| Five-signal weights do not sum to 1.0 in config | Normalize at startup; log `WARN` with original and normalized values |
| `access_count` for a fact overflows the normalized range | `log(1 + access_count)` is bounded by `log(1 + max_access_count)` where `max_access_count` is capped at 10000; normalize against the cap |
| Agent session shorter than `interval_seconds` | Consolidation daemon may not fire; episodic facts remain un-promoted. This is acceptable for short sessions |
| Demoted fact's score recovers above promotion threshold | On the next daemon run, the fact is re-promoted to Qdrant; `qdrant_promoted` flipped back to 1 |

---

## 7. Config

```toml
[memory.five_signal]
enabled = false                        # opt-in; default off

# Signal weights (must sum to 1.0; normalized at startup if they do not)
w_recency    = 0.35
w_relevance  = 0.35
w_frequency  = 0.15
w_causal     = 0.10
w_novelty    = 0.05

# Causal distance BFS bounds
causal_bfs_max_depth   = 10
neutral_causal_distance = 5            # used when no goal entity or goal is beyond max depth

# Novelty decay rate (λ in exp(-λ × days))
novelty_decay_rate = 0.1

[memory.five_signal.consolidation_daemon]
enabled = false
interval_seconds = 7200               # 2 hours
batch_size = 500                      # max facts processed per run
daemon_max_runtime_ms = 30000         # safety cap per run
promotion_score_threshold = 0.70      # five-signal score above which a fact is promoted
demotion_score_threshold = 0.20       # score below which a fact is demoted
top_k_per_run = 500                   # number of top-scoring facts evaluated per run
```

---

## 8. Key Invariants

### Always (without asking)
- When `w_frequency = 0`, `w_causal = 0`, `w_novelty = 0`, the five-signal formula is identical to the current two-signal baseline
- Signal weights are normalized to sum to 1.0 exactly once at startup; the normalized values are used for all retrieval calls in the session
- `fact_access_log` inserts are fire-and-forget; a failure does not interrupt the agent turn
- Causal BFS is bounded by `causal_bfs_max_depth`; unbounded graph traversal is prohibited on the retrieval hot path
- The consolidation daemon runs only on the scheduler thread and never blocks the agent turn thread
- `enabled = false` is a zero-overhead no-op for all five-signal code paths

### Ask First
- Changing default signal weights (affects retrieval behavior for all sessions)
- Enabling the consolidation daemon in production without running benchmark validation (SC-003)
- Setting `demotion_score_threshold` above 0.30 (may demote facts that are still useful)

### Never
- Block the agent turn thread on causal BFS computation (BFS must be bounded and fast)
- Allow the consolidation daemon to acquire a session-level lock that delays agent turns
- Perform Qdrant writes on the synchronous SYNAPSE recall path (promotion is daemon-only)
- Allow `w_recency + w_relevance + w_frequency + w_causal + w_novelty ≠ 1.0` after normalization

---

## 9. Success Criteria

| ID | Metric | Target |
|----|--------|--------|
| SC-001 | Two-signal equivalence: five-signal with `w_frequency = w_causal = w_novelty = 0` produces identical ranking to current baseline | 100% on unit test fixtures |
| SC-002 | LongMemEval-S retrieval accuracy (five-signal vs. two-signal baseline) | ≥ 15pp improvement |
| SC-003 | Tool-execution success degradation over 72-hour synthetic session (with daemon enabled) | ≤ 3pp degradation (vs. 14pp baseline) |
| SC-004 | Five-signal scoring overhead per retrieval call (200-fact candidate set) | < 5 ms at p95 |
| SC-005 | Consolidation daemon batch runtime (500 facts) | < 30 s |

---

## 10. Acceptance Criteria

```
GIVEN five_signal.enabled = true
  AND w_frequency = 0, w_causal = 0, w_novelty = 0
WHEN SYNAPSE retrieves facts
THEN the result ranking is identical to the two-signal baseline
AND no new Prometheus counters are incremented beyond the existing ones

GIVEN five_signal.enabled = true
  AND w_frequency = 0.15
  AND fact F1 has access_count = 50, F2 has access_count = 0
  AND F2 has marginally higher semantic similarity (Δ = 0.02)
WHEN SYNAPSE retrieves top-5 facts
THEN F1 ranks above F2 (access_frequency contribution outweighs Δ similarity)
AND five_signal_recall_total increments

GIVEN consolidation_daemon.enabled = true
  AND fact F has five_signal_score = 0.85 > promotion_score_threshold
WHEN the daemon runs
THEN F is upserted to Qdrant with five_signal_score in payload
AND messages.qdrant_promoted = 1 for F
AND consolidation_promoted_total increments

GIVEN five_signal.enabled = false
WHEN SYNAPSE retrieves any facts
THEN no fact_access_log rows are written
AND no five-signal weights are applied
AND SYNAPSE latency is indistinguishable from the pre-spec baseline
```

---

## 11. Implementation Notes

- Five-signal scoring is a post-processing step applied to the candidate set returned
  by Qdrant and the SQLite episodic search. The existing recall pipeline returns
  `(fact_id, recency_score, relevance_score)` tuples; this spec adds three more score
  fields hydrated from `fact_access_log` (frequency) and the MAGMA graph (causal
  distance). The final weighted sum replaces the current linear combination.
- `fact_access_log` counters are pre-aggregated at query time via a single SQL
  `COUNT(*) GROUP BY fact_id` over the session's access log — no per-fact hot-path
  lookup. The aggregation is cached per turn boundary.
- Causal distance BFS reuses the existing `bfs_typed` infrastructure in
  `graph/store.rs`, filtered to `EdgeType::Causal`. The BFS result is cached per turn
  (keyed on goal entity id and source entity set) to avoid re-traversal within a turn.
- Novelty is a pure arithmetic computation (`exp(-λ × days)`); no I/O required.
- Consolidation daemon registration uses the `ConsolidationTask` pattern established by
  HeLa-Mem [[004-11-memory-hela-mem]]. The five-signal daemon is a distinct task
  registered under the `zeph-scheduler` feature gate.
- PPO-based weight adaptation (MemTier §5) is deferred to `zeph-experiments` as a
  follow-on enhancement. The weight config section is designed to accept automated
  updates from the experiments framework without schema changes.
- Database migration 048 is independent of APEX-MEM migration 042 and CUPMem migration
  047; all three can be applied in any order.

---

## 12. Open Questions

> [!question]
> - **Goal entity sourcing**: FR-006 requires `TurnContext.current_goal_entity_id`.
>   This field does not currently exist in `TurnContext`. It must be defined — either
>   extracted from the current task description via a lightweight NER pass, or set
>   explicitly by the orchestration layer. The mechanism for goal entity resolution
>   must be agreed before FR-005 and FR-006 can be implemented.
> - **LongMemEval-S benchmark adapter**: the ≥ 15pp improvement target (SC-002) requires
>   an adapter in `zeph-bench` that replicates the LongMemEval-S evaluation protocol
>   against Zeph's retrieval stack. This adapter does not yet exist and must be built
>   as part of the implementation work.
> - **PPO weight adaptation scope**: the MemTier paper's PPO agent learns per-agent-profile
>   weights (e.g., code-generation agents weight causal distance higher). Integrating
>   this into `zeph-experiments` is a P4 follow-on. The question of whether weights
>   should be global (per deployment) or per-session should be resolved before
>   implementing the experiments integration.

---

## 13. See Also

- [[constitution]] — project principles
- [[004-memory/spec]] — memory system parent index
- [[004-5-temporal-decay]] — SleepGate forgetting (complementary, not replaced)
- [[004-6-graph-memory]] — MAGMA graph and SYNAPSE recall (extended by this spec)
- [[004-11-memory-hela-mem]] — HeLa-Mem consolidation (shared daemon infrastructure)
- [[001-system-invariants/spec]] — system-wide invariants
- [[MOC-specs]] — all specifications
