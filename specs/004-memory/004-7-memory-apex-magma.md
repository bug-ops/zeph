---
aliases:
  - APEX-MEM
  - Append-Only MAGMA
  - Temporal Graph Memory
tags:
  - sdd
  - spec
  - memory
  - graph
  - experimental
created: 2026-04-19
status: draft
related:
  - "[[MOC-specs]]"
  - "[[constitution]]"
  - "[[004-memory/spec]]"
  - "[[012-graph-memory/spec]]"
  - "[[004-6-graph-memory]]"
  - "[[024-multi-model-design/spec]]"
---

# Spec: APEX-MEM — Append-Only MAGMA with Ontology Normalization

> [!info]
> Converts the MAGMA edge store from destructive updates to an append-only log with
> explicit supersession pointers; adds an ontology-normalization layer for predicate
> canonicalization; adds a SYNAPSE-side conflict resolution pass that deterministically
> (or, optionally, via LLM) selects the authoritative value when multiple edges assert
> conflicting values for the same `(subject, predicate)`. Resolves GitHub issue
> [#3223](https://github.com/rabax/zeph/issues/3223).

## Sources

### External
- **APEX-MEM: Append-Only Provenance for Agent Memory** (research memo, 2026)
- **Zep / Graphiti temporal KG** — `valid_from`, `valid_until` edge invalidation (arXiv:2501.13956)
- **MAGMA** — multi-graph agent memory with typed edges (arXiv:2601.03236)
- **Temporal Versioning on KG Edges** (arXiv:2504.19413)

### Internal
| File | Contents |
|---|---|
| `crates/zeph-memory/src/graph/store.rs` | Edge CRUD, current destructive update path |
| `crates/zeph-memory/src/graph/types.rs` | `Edge`, `EdgeType`, temporal fields |
| `crates/zeph-memory/src/graph/extractor.rs` | LLM extraction → predicate strings |
| `crates/zeph-memory/src/graph/retrieval.rs` | BFS + typed retrieval |
| `crates/zeph-memory/src/semantic/graph.rs` | SYNAPSE spreading activation integration |

---

## 1. Overview

### Problem Statement

Current MAGMA edges support temporal versioning via `valid_from` / `valid_to`, but
updates are applied **in-place** when the LLM extracts a new fact for the same
`(source, target, relation, edge_type)` tuple. Three concrete issues:

1. **Lost provenance.** When the same predicate is reasserted with a different value,
   the previous value is overwritten. History is only preserved via `episode_id`,
   which does not encode the semantic supersession relationship.
2. **Predicate drift.** The LLM emits synonyms — `works_at`, `employed_by`, `job_at` —
   for the same relation. BFS and SYNAPSE treat these as separate edges. Query
   `"where does X work"` matches only the exact lexical form.
3. **Silent conflicts.** When two edges assert different values for
   `(subject, predicate)` with overlapping validity windows, recall pipelines return
   both facts without resolution, confusing the LLM downstream.

### Goal

Three coupled changes:

1. **Append-only edge log**: inserts never update; "updating" an edge inserts a new
   edge with `supersedes` pointing to the prior edge's id. Retrieval walks the chain
   to the head by default; history is queryable explicitly.
2. **Ontology normalization layer**: canonical predicate mapping from a configurable
   table plus an LLM-assisted fallback. Extraction stores both the raw predicate and
   the canonical form.
3. **Conflict resolution pass** in SYNAPSE recall: when multiple edges share
   `(subject, canonical_predicate)`, pick one authoritative value per the configured
   strategy (`recency` | `confidence` | `llm`).

### Out of Scope

- Migrating other stores (messages, summaries) to append-only — graph only
- Changing `EdgeType` semantics or the four-subgraph taxonomy
- Cross-entity coreference resolution beyond the existing `EntityResolver`
- Replacing BFS (append-only and normalization apply to both BFS and SYNAPSE paths)

---

## 2. User Stories

### US-001: Current fact surfacing
AS A user who updates facts over time (e.g., job change)
I WANT the agent to surface the most current information
SO THAT outdated facts do not interfere with current context

**Acceptance criteria:**
```
GIVEN an edge (Alice, works_at, Acme) was written three sessions ago
  AND a new edge (Alice, employed_by, Globex) is written in the current session
WHEN a recall query asks where Alice works
THEN only the Globex edge is returned in the main result
AND the Acme edge is accessible only via edge_history()
```

### US-002: Predicate canonicalization
AS A developer
I WANT memory graph integrity to not depend on fuzzy string matching for conflict detection
SO THAT "works_at" and "employed_by" resolve to the same canonical fact

**Acceptance criteria:**
```
GIVEN the ontology maps "employed_by" → canonical "works_at"
  AND "job_at" → canonical "works_at"
WHEN extraction produces edges with predicates "works_at", "employed_by", and "job_at"
THEN all three share canonical_relation = "works_at"
AND BFS and SYNAPSE treat them as the same predicate for head-of-chain and conflict resolution
```

### US-003: Temporal history inspection
AS AN operator
I WANT to inspect the full temporal history of a graph edge
SO THAT I can audit how the agent's understanding of a fact evolved

**Acceptance criteria:**
```
GIVEN a predicate (Alice, works_at) has been superseded twice over the session
WHEN edge_history(current_head_id) is called
THEN all three historical edges are returned in reverse chronological order
AND each entry includes valid_from, confidence, and the episode_id that produced it
AND the result is accessible via the /graph TUI command without enabling any debug flag
```

---

## 3. Functional Requirements

| ID | Requirement | Priority |
|----|------------|----------|
| FR-001 | WHEN a write arrives AND a cardinality-1 head edge exists for `(source, target, canonical_relation, edge_type)` (see FR-014) THE SYSTEM SHALL execute a single atomic transaction that (a) inserts the new row with `supersedes = prior_head.id` AND (b) sets `prior_head.valid_to = now` — both succeed or both roll back | must |
| FR-002 | Edge writes SHALL never `UPDATE` `relation`, `fact`, `confidence`, `valid_from`, `canonical_relation`, `supersedes`, or `edge_type` — these columns are immutable after insert | must |
| FR-003 | `valid_to` and `expired_at` MAY each be set exactly once by the append-only write path (FR-001) or an explicit close call; subsequent writes targeting the same column are idempotent no-ops | must |
| FR-004 | WHEN extraction produces a predicate THE SYSTEM SHALL resolve it through the ontology layer and store both `relation` (raw) and `canonical_relation` | must |
| FR-005 | Ontology resolution SHALL first consult the configured canonical table (static TOML); if unresolved, call `ontology_provider` with a constrained prompt; cache the mapping in a bounded LRU (see §8) | should |
| FR-006 | Head-of-chain SHALL be defined as: **the row with the greatest `created_at` within the equivalence class `(source_entity_id, target_entity_id, canonical_relation, edge_type)` among rows where `valid_to IS NULL AND expired_at IS NULL`**. Default BFS and SYNAPSE traversal SHALL filter edges through this definition | must |
| FR-007 | A helper `edge_history(head_id)` SHALL walk the `supersedes` chain and return the full timeline ordered newest→oldest | should |
| FR-008 | WHEN SYNAPSE assembles the result set AND multiple head edges share `(subject, canonical_relation)` AND that `(canonical_relation, edge_type)` has `cardinality = 1` per the ontology THE SYSTEM SHALL invoke the configured conflict resolver to pick one authoritative edge. Predicates with `cardinality = n` (multi-valued, default for unknown predicates) SHALL pass through all head edges unchanged | must |
| FR-009 | The conflict resolver strategy SHALL be one of: `recency` (pick newest `valid_from`), `confidence` (pick highest `confidence`), `llm` (call `conflict_resolution_provider`) | must |
| FR-010 | `llm` strategy SHALL respect a 500 ms timeout AND a per-turn budget `conflict_llm_budget_per_turn` (default 3 resolutions); on timeout or budget exhaustion fall back to `recency` | must |
| FR-011 | WHEN conflict resolution drops an edge from the result set THE SYSTEM SHALL retain the dropped edge in a diagnostic "alternatives" field accessible via `/graph` tooling, not passed to the main LLM. Default `retain_alternatives_for_diagnostics = false` | should |
| FR-012 | DB migration 042 SHALL be atomic (`BEGIN IMMEDIATE; … COMMIT;`); on failure the DB remains in the pre-042 state. Operations: (a) `ADD COLUMN supersedes`, (b) `ADD COLUMN canonical_relation`, (c) `ADD COLUMN cardinality` to ontology table if persisted, (d) backfill `canonical_relation = relation`, `supersedes = NULL`, (e) create partial index `idx_edges_head_active`, (f) **replace** (not drop) the pre-APEX uniqueness index with a new partial unique index restricted to the active head (see §3). `DROP INDEX` MUST NOT occur without a replacement constraint inside the same transaction | must |
| FR-013 | WHEN `[memory.graph.apex_mem]` is disabled THE SYSTEM SHALL behave as pre-APEX MAGMA: legacy `store.rs` write path honours the partial unique index on active heads (rollback-safe, see §3) | must |
| FR-014 | Ontology entries SHALL carry a `cardinality` field in `{1, n}` defaulting to `n` (multi-valued) when unspecified. The `default.toml` SHALL explicitly mark cardinality-1 predicates (`works_at`, `lives_in`, `born_in`, `manages`) | must |
| FR-015 | WHEN a write asserts a value byte-identical to the current head (same `target`, `relation`, `fact`, `edge_type`) THE SYSTEM SHALL insert a **reassertion event row** in the `edge_reassertions` table `(head_edge_id, asserted_at, episode_id, confidence)` instead of inserting a new edge; this preserves provenance without violating the immutability invariant (FR-002) | must |
| FR-016 | Every subsystem path introduced by this spec (`ontology.resolve`, `store.insert_or_supersede`, `semantic.conflict.resolve`, `semantic.conflict.llm`) SHALL be wrapped in a `tracing::info_span!` with the documented name | must |

---

## 4. Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Performance | `insert_or_supersede` (single edge write including supersession pointer update) SHALL complete in < 10 ms at p95 on the primary write path (SQLite with WAL mode) |
| NFR-002 | Performance | Ontology resolution SHALL complete in < 1 ms for table hits; LLM fallback is bounded to 500 ms via timeout with a `recency` fallback ensuring no call blocks indefinitely |
| NFR-003 | Performance | Conflict resolution via `recency` or `confidence` strategy SHALL complete in < 5 ms at p99 (SQL sort on indexed columns); `llm` strategy respects the 500 ms timeout with `recency` fallback |
| NFR-004 | Performance | Head-of-chain query (`idx_edges_head_active` partial index) SHALL not degrade BFS or SYNAPSE latency by more than 5% vs. pre-APEX on benchmark fixtures with 100k edges |
| NFR-005 | Reliability | DB migration 042 is atomic (`BEGIN IMMEDIATE; … COMMIT;`); a failed migration leaves the DB in pre-042 state with no half-applied columns or indexes |
| NFR-006 | Reliability | Feature flag `enabled = false` is a safe runtime rollback — legacy write path satisfies both the old and new partial indexes without requiring a reverse migration |
| NFR-007 | Reliability | `llm` conflict resolver and ontology fallback both fail to `recency` on timeout or error — no write or recall operation blocks on an LLM call |
| NFR-008 | Maintainability | Ontology table is a plain TOML file with a documented schema; operators can add canonical mappings and reload without a process restart (`/graph ontology reload`) |
| NFR-009 | Maintainability | Conflict resolver strategy is a config enum; adding a new strategy requires only a new enum variant and a resolver function — no changes to the store or retrieval layers |
| NFR-010 | Observability | Prometheus counters SHALL be exported: `apex_mem_supersedes_total`, `apex_mem_conflicts_total{strategy}`, `apex_mem_llm_timeouts_total`, `apex_mem_unmapped_predicates_total` |
| NFR-011 | Observability | All new subsystem paths SHALL be instrumented with `tracing::info_span!` per FR-016 naming convention (`ontology.resolve`, `store.insert_or_supersede`, `semantic.conflict.resolve`, `semantic.conflict.llm`) |
| NFR-012 | Security | `edge_history()` results are not exposed in default recall paths; history access is explicit and operator-initiated only — prevents inadvertent PII surfacing from old superseded facts |
| NFR-013 | Security | `canonical_relation` is always lowercase-trimmed before storage; no injection via predicate strings (no SQL string interpolation — parameterized queries only) |

---

## 5. Data Model Changes

### Schema Migration (`042_apex_mem.sql`)

Wrapped in a single transaction (`BEGIN IMMEDIATE; … COMMIT;`). Either the DB lands
fully on 042 or fully on 041; no half-migrated state.

```sql
BEGIN IMMEDIATE;

ALTER TABLE edges ADD COLUMN supersedes INTEGER REFERENCES edges(id);
ALTER TABLE edges ADD COLUMN canonical_relation TEXT;

-- backfill phase 1: copy raw relation as canonical (idempotent)
UPDATE edges SET canonical_relation = relation WHERE canonical_relation IS NULL;

-- replace the legacy active-head unique index WITHOUT an intermediate drop-only state.
-- The old index 'uq_graph_edges_active' remains usable by rollback (enabled=false).
-- The new partial index tightens uniqueness to the append-only head of chain.
CREATE UNIQUE INDEX IF NOT EXISTS uq_graph_edges_active_head
  ON edges(source_entity_id, target_entity_id, canonical_relation, edge_type)
  WHERE valid_to IS NULL AND expired_at IS NULL;

CREATE INDEX IF NOT EXISTS idx_edges_supersedes ON edges(supersedes);
CREATE INDEX IF NOT EXISTS idx_edges_head_active
  ON edges(source_entity_id, canonical_relation, edge_type, created_at DESC)
  WHERE valid_to IS NULL AND expired_at IS NULL;

-- reassertion events (FR-015)
CREATE TABLE IF NOT EXISTS edge_reassertions (
    id             INTEGER PRIMARY KEY,
    head_edge_id   INTEGER NOT NULL REFERENCES edges(id),
    asserted_at    INTEGER NOT NULL,
    episode_id     TEXT,
    confidence     REAL NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_reassertions_head
  ON edge_reassertions(head_edge_id, asserted_at DESC);

COMMIT;
```

**Rollback discipline.** The pre-APEX uniqueness index `uq_graph_edges_active` is
**retained** — not dropped. The legacy (flag-disabled) write path continues to satisfy
it because destructive updates leave exactly one active row per
`(source, target, relation, edge_type)`. The new partial index
`uq_graph_edges_active_head` is compatible: it keys on `canonical_relation` which is
backfilled to `relation`, so legacy writes still satisfy both constraints. This
preserves `enabled = false` as a safe runtime rollback without requiring a reverse
migration.

**Ontology cardinality storage.** Cardinality is sourced from the TOML ontology table
(§3.4) at process start; it is not persisted per-edge. This keeps edge rows immutable
and lets ontology tuning take effect on the next `/graph ontology reload` without
schema churn.

A secondary opt-in pass `migrate_canonical_relations(ontology)` is provided as a
separate CLI subcommand (not part of 042) — it re-canonicalises legacy rows by
inserting new supersedes rows. Default is not to run it; users opt in after reviewing
the ontology table. Documented as a known limitation: without this pass, pre-existing
synonym edges remain under their raw predicates.

### `Edge` struct additions

```rust
pub type EdgeId = i64;  // matches SQLite INTEGER PRIMARY KEY

pub struct Edge {
    // ... existing fields ...
    pub canonical_relation: String,
    pub supersedes: Option<EdgeId>,
}
```

### Ontology Table

TOML file `ontology/default.toml` shipped in the repo, overridable by config. Each
canonical predicate declares a `cardinality` in `{1, n}` (default `n` when absent).

```toml
# ontology/default.toml
[[predicate]]
canonical   = "works_at"
aliases     = ["employed_by", "job_at", "works_for"]
cardinality = 1

[[predicate]]
canonical   = "lives_in"
aliases     = ["resides_in", "based_in"]
cardinality = 1

[[predicate]]
canonical   = "owns"
aliases     = ["has", "possesses"]
cardinality = "n"

[[predicate]]
canonical   = "depends_on"
aliases     = ["requires", "needs"]
cardinality = "n"

[[predicate]]
canonical   = "knows"
aliases     = []
cardinality = "n"
```

Loaded into two structures:

- `alias_to_canonical: HashMap<String, String>` — case-insensitive, trimmed
- `cardinality: HashMap<(String /* canonical */, EdgeType), Cardinality>` — keyed on (canonical, edge_type); missing entries default to `Cardinality::Many`

**Cache bound & reload.** The in-memory ontology cache (including LLM-fallback
entries) is an LRU bounded by `ontology_cache_max_entries` (default 4096). The
`/graph ontology reload` TUI command reloads the TOML table and clears the LRU.
LLM-fallback entries that fail validation against the fresh table are evicted.

---

## 6. Key Invariants

### Always (without asking)
- Edge inserts are append-only; `UPDATE` is permitted only on `valid_to` and `expired_at`, and each at most once per row
- The supersede write (insert new row + close prior head's `valid_to`) is a single atomic SQLite transaction — partial state is impossible (FR-001)
- At any point in time, a cardinality-1 `(source, target, canonical_relation, edge_type)` equivalence class has at most one row with `valid_to IS NULL AND expired_at IS NULL` (enforced by `uq_graph_edges_active_head`)
- Repeated byte-identical assertions are recorded in `edge_reassertions`, never as new edges — provenance is preserved without duplicating the chain (FR-015)
- `supersedes` always points to an existing edge id in the same store; chains are acyclic (enforced at insert time)
- Head-of-chain query is the default for all recall paths; history access is explicit
- `canonical_relation` is always lowercase, trimmed, stripped of control characters
- Ontology resolution is deterministic given the same table and cache state
- Conflict resolver runs only for cardinality-1 predicates; cardinality-n predicates pass all head edges through unchanged
- `llm` strategy bounded by 500 ms timeout AND per-turn budget with `recency` fallback
- Disabled feature flag bypasses all new code paths; legacy uniqueness index is retained so rollback is safe
- Migration 042 is wrapped in `BEGIN IMMEDIATE; ... COMMIT;` — half-migrated state is impossible

### Ask First
- Changing the default conflict strategy from `recency`
- Adding new canonical predicates to `ontology/default.toml` (review for semantic drift)
- Raising the LLM timeout beyond 500 ms
- Exposing history walks in user-facing recall paths (privacy consideration)

### Never
- Update `relation`, `fact`, `confidence`, or `valid_from` on an existing edge row
- Create a cycle in the `supersedes` graph
- Return superseded edges from default recall (only from `edge_history()`)
- Persist `canonical_relation` in mixed case
- Block on the LLM conflict resolver — the 500 ms timeout is mandatory
- Remove the head-of-chain filter from BFS or SYNAPSE

---

## 7. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|-------------------|
| Two concurrent writes on the same `(source, target, canonical_relation, edge_type)` | Per-entity lock serializes; second write's `supersedes` points at the first's inserted id |
| Ontology LLM fallback returns a synonym already in the table | Dedup to existing canonical; log at `DEBUG` |
| Ontology fallback returns a brand new canonical not in the table | Persist raw predicate unchanged; set `canonical_relation = relation`; log at `WARN` with `unmapped_predicate` counter |
| Conflict resolver `llm` strategy timeout | Fall back to `recency`; increment `conflict_llm_timeouts` |
| Head-of-chain query returns 0 edges but history has entries | All edges closed (valid_to set); return empty result; history still available |
| Backfill migration interrupted | Idempotent; running again completes missing rows; no duplicate canonical writes |
| `supersedes` would create a cycle (pathological) | Reject insert with `GraphError::SupersedeCycle`; surface through `remember()` error path |
| Edge asserts value byte-identical to current head | Insert one row into `edge_reassertions` referencing the head (FR-015); do NOT insert a new edge row; do NOT modify the head; log at `DEBUG` |
| Two head edges coexist for cardinality-n predicate (e.g., `owns`) | Both returned; no conflict resolver invoked |
| Two head edges coexist for cardinality-1 predicate (e.g., `works_at`) with different targets | Conflict resolver invoked per FR-008; one winner returned, loser(s) go to diagnostic alternatives (opt-in) |
| Unknown predicate with no ontology entry | Defaults to cardinality-n (multi-valued) — safe default that preserves data |
| Ontology TOML reloaded mid-session | `/graph ontology reload` clears the LRU; in-flight writes continue with the pre-reload mapping for their transaction |

---

## 8. Config

```toml
[memory.graph.apex_mem]
enabled = true

[memory.graph.apex_mem.ontology]
# path to a TOML canonical-predicate table; empty = use embedded default
table_path = ""
# references [[llm.providers]] by name; empty = disable LLM ontology fallback
ontology_provider = ""
unmapped_predicate_warn = true
ontology_cache_max_entries = 4096     # LRU bound on alias_to_canonical cache

[memory.graph.apex_mem.conflict_resolution]
# strategy: "recency" | "confidence" | "llm"
strategy = "recency"
conflict_resolution_provider = ""     # required when strategy = "llm"
timeout_ms = 500
conflict_llm_budget_per_turn = 3      # beyond this, fall back to recency
retain_alternatives_for_diagnostics = false   # opt-in; default off to save memory
```

---

## 9. Success Criteria

- [ ] Property test: 10k random edge rewrites never produce a `supersedes` cycle
- [ ] Migration 042 is idempotent (running twice leaves the DB byte-equivalent)
- [ ] Disabling the flag reproduces pre-APEX behavior on an integration fixture
- [ ] Default BFS returns only head edges (unit test with seeded superseded history)
- [ ] `edge_history()` walks the chain in reverse chronological order
- [ ] Ontology table resolves `works_at`/`employed_by`/`job_at` to the same canonical
- [ ] Conflict resolver `recency` strategy picks the newest `valid_from` in property test
- [ ] Conflict resolver `llm` strategy respects 500 ms timeout (integration test)
- [ ] Prometheus metrics export `apex_mem_supersedes_total`, `apex_mem_conflicts_total{strategy}`, `apex_mem_llm_timeouts_total`, `apex_mem_unmapped_predicates_total`
- [ ] LongMemEval-style synthetic benchmark shows ≤ 5% recall regression vs. pre-APEX when flag disabled; ≥ 10% improvement when enabled with ontology + conflict resolver
- [ ] Atomic-write test: induced panic between insert and `valid_to` closure leaves the DB in pre-write state (rollback verified)
- [ ] Cardinality test: `owns` asserted twice with different targets leaves both head edges; `works_at` asserted twice closes the first
- [ ] Reassertion test: byte-identical write appends to `edge_reassertions` with no new edge row
- [ ] `tracing::info_span!` coverage: `memory.graph.apex.ontology_resolve`, `memory.graph.apex.store.insert_or_supersede`, `memory.graph.apex.conflict_resolve`, `memory.graph.apex.conflict_llm` are all emitted during integration tests

---

## 10. Acceptance Criteria

```
GIVEN an edge (Alice, works_at, Acme) exists as head
  AND extraction produces (Alice, employed_by, Globex)
WHEN the extractor writes the new edge
THEN both edges share canonical_relation = "works_at"
AND the new edge's supersedes = prior_edge.id
AND BFS returns the new edge only (head-of-chain)
AND edge_history(new_edge.id) returns [new, prior]

GIVEN two head edges for (Alice, works_at) with different targets
  AND strategy = "recency"
WHEN SYNAPSE assembles the result
THEN exactly one edge is returned in the main result
AND the other is available in the "alternatives" diagnostic field
AND memory.graph.conflicts_resolved_total{strategy="recency"} increments

GIVEN strategy = "llm"
  AND the provider times out after 600 ms
WHEN conflict resolution runs
THEN the recency winner is returned
AND apex_mem_llm_timeouts_total increments
```

---

```
GIVEN a head edge (Alice, works_at, Acme)
  AND a new write (Alice, works_at, Globex)
WHEN insert_or_supersede runs
THEN a single SQLite transaction inserts the new row with supersedes=prior.id
  AND sets prior.valid_to = now in the same transaction
AND after commit, only the new row satisfies the head-of-chain predicate
AND a simulated failure between the two statements rolls the whole transaction back

GIVEN the predicate "owns" with cardinality = n
  AND two head edges (Alice, owns, Book1), (Alice, owns, Book2)
WHEN SYNAPSE assembles results for "what does Alice own"
THEN both edges are returned
AND the conflict resolver is NOT invoked
AND no LLM budget is consumed

GIVEN a write byte-identical to the current head
WHEN insert_or_supersede runs
THEN a row is inserted into edge_reassertions referencing the head
AND no new edge row is created
AND the head remains the head (valid_to untouched)
```

## 11. Implementation Notes

- New module: `crates/zeph-memory/src/graph/ontology.rs` owning the canonical table and cache
- Conflict resolver lives in `crates/zeph-memory/src/semantic/conflict.rs`
- `Edge::supersedes` serialized as integer id; cycle check uses DFS limited to depth 64 (safety cap)
- Ontology cache is an `Arc<RwLock<HashMap<String, String>>>` — read-heavy; LLM fallback writes under write lock; cache persists for session lifetime
- Retain existing `EdgeType`, `bfs_typed`, SYNAPSE algorithms — head-of-chain filter is added as a SQL predicate, not a new code path
- Metrics integrated via existing `MetricsSnapshot` extension pattern
- `retain_alternatives_for_diagnostics` surfaces via `/graph` TUI command for operator inspection
- Cost discipline: ontology fallback and conflict resolver both use fast-tier providers per `[[024-multi-model-design/spec]]`
- Downward migration (for rollback): setting `enabled = false` is safe; columns remain populated, legacy code ignores them

---

## 12. Implementation Notes (Post-Landing)

### insert_or_supersede Unique Index Constraint (#3639)

The write path for `insert_or_supersede` previously could hit a UNIQUE constraint
violation on `uq_graph_edges_active_head` when two concurrent extraction tasks raced
to write the same `(source_entity_id, target_entity_id, canonical_relation, edge_type)`
tuple without the first write having completed its `valid_to` closure.

**Resolution**: The supersede transaction now uses `INSERT OR REPLACE` on the
`edge_reassertions` table for byte-identical writes (FR-015), and the main edge
insert uses an explicit per-entity `SAVEPOINT` guard so a constraint violation from a
concurrent writer triggers a retry-after-reload rather than propagating upward.
The partial unique index `uq_graph_edges_active_head` remains the enforcement
mechanism; the write path is now MVCC-safe under SQLite WAL mode.

**Key invariant added**: `insert_or_supersede` MUST be retried (with exponential
backoff, max 3 attempts) on `SQLITE_CONSTRAINT_UNIQUE` before surfacing as
`GraphError`; the constraint violation indicates a concurrent write that already
advanced the head.

### extract_provider Bypass for QualityGate (#3615)

The `quality_gate_provider` in `[memory.graph]` controls post-write scoring.
LLM-assisted entity extraction (ontology normalization, conflict resolution) uses
a separate `extract_provider` so the quality gate can be bypassed for the extraction
path itself. This prevents the quality gate from gating its own scorer — the gate
only applies to user-generated writes, not to extraction-originated edges.

```toml
[memory.graph]
extract_provider = "fast"         # provider for entity extraction LLM calls
quality_gate_provider = "fast"    # provider for quality gate scoring (empty = disable)
```

When `quality_gate_provider` is empty, the gate is disabled. When `extract_provider`
is empty, it falls back to the primary provider. The two fields are independent.

**Key invariant**: extraction-originated writes MUST bypass the quality gate, not
flow through it. The quality gate applies only to writes that originate from user
memory commands or external memory injection.

---

## 13. Open Questions

> [!question]
> - **Cardinality model for multi-valued predicates**: FR-008 gates conflict resolution on `cardinality = 1` predicates, but the ontology table (§5.3) currently has no explicit `cardinality` column — it is implied for known predicates and defaults to `n` (multi-valued) for unknown ones. FR-014 adds the `cardinality` field to ontology entries, but FR-008 must explicitly document how SYNAPSE distinguishes single-value predicates (e.g., `works_at`) from intrinsically multi-valued ones (e.g., `owns`) before invariant tests for conflict resolution are written. The distinction must be mechanical, not inferred from predicate name.

---

## 14. See Also

- [[constitution]] — project principles
- [[004-memory/spec]] — memory pipeline
- [[012-graph-memory/spec]] — MAGMA + SYNAPSE (extended by this spec)
- [[004-6-graph-memory]] — graph memory sub-spec
- [[memory-write-gate/spec]] — upstream quality gate (contradiction_risk signal composes with APEX-MEM conflicts)
- [[024-multi-model-design/spec]] — provider tier guidance
- [[MOC-specs]] — all specifications

---

## 15. Research Backlog

Research findings pending implementation review. Each entry links to the originating tracking issue and proposes a concrete integration point.

### 14.1 MemCoT — Test-Time Memory Scaling (arXiv:2604.08216)

**Tracking issue**: #3564
**Status**: Implemented (#3592) — see [[004-13-memory-memcot]] for the full sub-spec.

MemCoT introduces a training-free multi-view LTM perception layer (Zoom-In for evidence localization, Zoom-Out for causal context expansion) and a task-conditioned dual short-term memory (`SemanticStateAccumulator` and episodic trajectory). Benchmarked on LoCoMo: GPT-4o-mini F1 = 58.03 vs ~30 baseline.

**Implemented Zeph integration**:
- `SemanticStateAccumulator` attached to `TurnContext` in `zeph-memory`; accumulates per-turn semantic state across the session
- Zoom-In recall view passes a narrowed query over the APEX-MEM resolved edge set to localize evidence
- Zoom-Out recall view expands the query to causal/contextual neighbors
- Config: `[memory.memcot] enabled` (default: false); provider references via `memcot_provider`

**Relevance to APEX-MEM**: APEX-MEM canonicalizes facts at the edge layer. MemCoT's Zoom-In retrieval and semantic-state STM operate one layer above — on top of the resolved edge set returned by SYNAPSE. The two are complementary: APEX-MEM decides which edge wins; MemCoT decides how the winning edges are presented to the reasoning model.

### 14.2 OmniMem — Autoresearch-Guided Memory Discovery (arXiv:2604.01007)

**Tracking issue**: #3566
**Status**: Researched / pending implementation (P3)

OmniMem's autoresearch pipeline runs ~50 self-experiments to discover memory architectural improvements. Top gains: bug fixes (+175%), prompt engineering (+188%), architectural changes (+44%). LoCoMo F1: 0.117 -> 0.598 (+411%).

**Proposed Zeph integration**:
- Log memory retrieval failures in `skill_outcomes` table (new follow-up issue filed)
- Periodic micro-benchmark via `zeph-scheduler`
- SYNAPSE parameter auto-tuning from failure analysis

**Relevance to APEX-MEM**: SYNAPSE has tunable parameters (BFS depth cap, pheromone decay, conflict-resolution thresholds — see §6, §8). OmniMem's closed-loop discovery process suggests these should be optimized against a logged failure dataset rather than hand-set.

### 14.3 OCR-Memory — Visual Trajectory Encoding (arXiv:2604.26622)

**Tracking issue**: #3571
**Status**: Researched / deferred to P4 (requires VLM provider)

OCR-Memory renders agent trajectories as annotated images and uses a locate-and-transcribe retrieval paradigm, avoiding lossy summarization for long sessions. Requires a multimodal LLM for retrieval transcription.

**Proposed Zeph integration** (deferred):
- `zeph-memory` episodic store: render sessions exceeding token threshold to PNG
- Store as Qdrant vector point with image embedding
- Unblocked when `zeph-llm` gains a VLM provider

**Relevance to APEX-MEM**: low. OCR-Memory targets the episodic-trajectory store, not the semantic graph. APEX-MEM's append-only log is orthogonal — it could feed transcript frames into a visual encoder, but this requires a VLM provider that does not exist in `zeph-llm` today.

---

## §16 BeliefMem: Pre-Commitment Probabilistic Edge Layer

**Tracking issue**: #3706
**Status**: Implemented (PR pending)
**Module**: `crates/zeph-memory/src/graph/belief.rs`

### 16.1 Overview

BeliefMem is a staging area for candidate facts that lack sufficient confidence for immediate commitment to the APEX-MEM committed edge store. The extractor assigns a `confidence` score (0.0–1.0) to each extracted edge; edges below the promotion threshold enter `pending_beliefs` for evidence accumulation via the Noisy-OR rule.

**Relationship to APEX-MEM conflict resolution:**
- APEX-MEM conflict resolution operates **post-commitment**: it resolves competing committed heads via `insert_or_supersede`.
- BeliefMem operates **pre-commitment**: it accumulates evidence before the first commit.
- Promotion from BeliefMem → APEX-MEM uses the standard `insert_or_supersede` path.

### 16.2 Schema

Two tables in `zeph-db/migrations/sqlite/084_pending_beliefs.sql`:

- **`pending_beliefs`**: one row per `(source_entity_id, canonical_relation, target_entity_id, edge_type)` candidate. `prob` accumulates via Noisy-OR. `promoted_at` is set when the belief crosses the threshold.
- **`belief_evidence`**: append-only audit log of each Noisy-OR update, recording `prior_prob`, `evidence_prob`, `posterior_prob`.

### 16.3 Noisy-OR Accumulation

```
P_new = 1 - (1 - P_existing_decayed) × (1 - P_evidence)
```

Where `P_existing_decayed = P_existing × exp(-λ × days_since_update)` and λ = `belief_decay_rate` (default 0.01). Setting `belief_decay_rate = 0.0` disables temporal decay.

Both functions are pure and exported from `zeph_memory::graph::belief`:
- `noisy_or(p_existing, p_new) -> f32`
- `time_decayed_prob(prob, days, decay_rate) -> f32`

### 16.4 ExtractedEdge Confidence Field

`ExtractedEdge` in `extractor.rs` now carries an optional `confidence: Option<f32>` field populated by the LLM during extraction. Callers should treat `None` as `1.0` (direct statement, commit immediately via existing path).

### 16.5 Configuration (`[memory.graph.belief_mem]`)

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `enabled` | bool | `false` | Opt-in; disabled by default |
| `min_entry_prob` | f32 | `0.3` | Minimum confidence to enter staging |
| `promote_threshold` | f32 | `0.85` | Threshold for promotion to committed edge |
| `max_candidates_per_group` | usize | `10` | Eviction cap per `(source, canonical_relation)` |
| `retrieval_top_k` | usize | `3` | Candidates returned in fallback recall |
| `belief_decay_rate` | f32 | `0.01` | λ for temporal decay; 0.0 = no decay |

### 16.6 Key Invariants

- `prob` is monotonically non-decreasing for an active (non-promoted) belief (decay happens only when new evidence arrives, not passively).
- Promotion is one-way: once `promoted_at` is set, a belief never re-enters pending.
- Retrieval from `pending_beliefs` is a **fallback only**: used when no committed edge exists for the queried `(source, canonical_relation)` pair; always annotated with `is_uncertain: true`.
- Pending beliefs are **never** returned in default recall paths.
- `noisy_or` guarantees `prob ∈ (0, 1)` given inputs in `(0, 1)`.

### 16.7 NEVER

- NEVER query `pending_beliefs` when a committed edge exists.
- NEVER promote a belief to `graph_edges` directly from `BeliefStore`; always call `GraphStore::insert_or_supersede` first, then `BeliefStore::mark_promoted`.
- NEVER allow `prob` to reach exactly 0.0 or 1.0 (the schema CHECK constraint prevents this).

### 16.8 Acceptance Criteria

- [ ] `pending_beliefs` and `belief_evidence` tables created by migration 084.
- [ ] `BeliefStore::record_evidence` applies temporal decay + Noisy-OR and returns `Some(PendingBelief)` when `prob >= promote_threshold`.
- [ ] `BeliefStore::retrieve_candidates` returns beliefs ordered by `prob DESC` with correct `top_k`.
- [ ] `BeliefStore::mark_promoted` sets `promoted_at` and `promoted_edge_id`.
- [ ] `BeliefStore::evict_stale` deletes rows exceeding `max_candidates_per_group`.
- [ ] `ExtractedEdge::confidence` is populated by the LLM extraction prompt.
- [ ] All pure functions (`noisy_or`, `time_decayed_prob`) have passing unit tests.
- [ ] `cargo build -p zeph-memory` and `cargo clippy -p zeph-memory -- -D warnings` pass.
