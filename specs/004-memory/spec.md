---
aliases:
  - Memory System
  - Memory Pipeline
  - Semantic Memory
tags:
  - sdd
  - spec
  - memory
  - persistence
  - contract
created: 2026-04-08
status: approved
related:
  - "[[MOC-specs]]"
  - "[[001-system-invariants/spec#6. Memory Pipeline Contract]]"
  - "[[002-agent-loop/spec]]"
  - "[[004-6-graph-memory]]"
  - "[[004-16-shadow-memory-safety]]"
  - "[[004-17-implicit-conflict-detection]]"
  - "[[004-18-five-signal-retrieval]]"
  - "[[012-graph-memory/spec]]"
  - "[[031-database-abstraction/spec]]"
---

# Spec: Memory System (Parent Index)

> [!info]
> SQLite + Qdrant dual backend, semantic response cache, anchored summarization,
> compaction probe, importance scoring, admission control, and cost-sensitive routing.

## Overview

This is the **parent specification** for the memory subsystem. For detailed information on
specific areas, refer to the child specs below.

---

## Child Specifications

| Spec | Topic | Purpose |
|------|-------|---------|
| [[004-1-architecture]] | Core Pipeline | Conversation storage, message lifecycle, recall architecture |
| [[004-2-compaction]] | Deferred Summaries | Tool pair summarization, context pressure thresholds, compaction probe |
| [[004-3-admission-control]] | A-MAC & Filtering | Five-factor importance scoring, admission gates, noise filtering |
| [[004-4-embeddings]] | Embedding Generation | Batch strategies, backfill, concurrent workers, TUI integration |
| [[004-5-temporal-decay]] | Retention Scoring | Ebbinghaus forgetting curve, access frequency, decay-based eviction |
| [[004-6-graph-memory]] | Graph Memory | Entity graph, BFS recall, MAGMA typed edges, SYNAPSE spreading activation, A-MEM link weights |
| [[004-7-memory-apex-magma]] | APEX-MEM / MAGMA | Append-only edge log, ontology normalization, SYNAPSE conflict resolution; BeliefMem pre-commitment layer |
| [[004-8-memory-typed-pages]] | ClawVM Typed Pages | `PageType` classification, minimum-fidelity invariants, compaction audit log |
| [[004-9-memory-write-gate]] | MemReader Write Gate | Three-signal write quality scorer composed with A-MAC admission control |
| [[004-13-memory-memcot]] | MemCoT | `SemanticStateAccumulator`, Zoom-In/Zoom-Out evidence localization and causal expansion |
| [[004-16-memory-type-aware-retrieval]] | MemGuard Type-Aware Retrieval | `FunctionalType`-gated retrieval composition, `BehavioralRule` always-composed safety invariant |

See also §"Sub-Specifications" below for [[004-10-memory-memmachine-retrieval]], [[004-11-memory-hela-mem]],
[[004-12-memory-reasoning-bank]], [[004-14-memory-tiering-rfc-decision]],
[[004-15-memory-skill-coevolution-rfc-decision]], [[004-16-shadow-memory-safety]],
[[004-17-implicit-conflict-detection]], and [[004-18-five-signal-retrieval]].

---

## System Architecture

```
SemanticMemory (Arc)
├── SqliteStore         — conversation history, message metadata
├── QdrantStore         — vector embeddings for semantic search
├── GraphStore          — entity/edge graph, see [[004-6-graph-memory]]
└── ResponseCache       — deduplicated LLM response cache
```

---

## Key Contracts

### Message Storage
- Every user + assistant turn persisted to SQLite immediately
- Messages are never deleted — only marked with `compacted_at` or summarized
- `MessageMetadata`: `agent_visible`, `user_visible`, `focus_pinned` — all respected
- Conversation identified by `ConversationId`; one per agent session

### Admission Control
- Not all messages admitted to memory (noise filtering via A-MAC)
- Five-factor scoring (A-MAC paper, arXiv:2603.04549): `future_utility` (0.30, LLM-estimated reuse
  probability), `factual_confidence` (0.15, inverse hedging heuristic), `semantic_novelty` (0.30,
  1 − max similarity to top-3 neighbors), `temporal_recency` (0.10, always 1.0 at write time),
  `content_type_prior` (0.15, message-role prior) — see [[004-3-admission-control]] for the full
  model, including the superseded six-factor design it replaced (issue #4141)
- Threshold-based gate: score < threshold → rejected (returns None)
- Fail-open: admission error → admit message anyway

### Compaction & Eviction
- Soft threshold (~60%) marks tool pairs for summary
- Hard threshold (~90%) applies summaries before LLM call
- Eviction prioritizes low-retention-score messages (Ebbinghaus model)
- Original messages stored in SQLite even after compaction

### Embedding Pipeline
- All admitted messages queued for embedding (async)
- Batched embedding with configurable batch size and timeout
- Backfill at boot recovers unembed messages
- TUI shows queue depth, batch status, backfill progress

### Retention Scoring
- Based on Ebbinghaus forgetting curve: `R(t) = e^(-t / halflife)`
- Boosted by access frequency (messages accessed more often decay slower)
- Scores [0.0, 1.0]: 1.0 fresh+accessed, 0.0 old+never-accessed
- Drives eviction and (optionally) admission decisions

### Tier-Aware Retrieval Gating

Memory is organized in three tiers; retrieval strategy adapts to query complexity:

**Shallow queries** (simple entity lookup, single-turn context, dependency depth < 2 hops)
- Search: SQLite working + episodic stores only
- Outcome: fast, low token cost, reduced latency p95

**Medium queries** (multi-turn reasoning, tool-output dependencies)
- Search: episodic (SQLite) + semantic vectors (Qdrant with MMR reranking)
- Outcome: balanced accuracy and cost

**Deep queries** (complex reasoning, cross-session patterns, causal inference)
- Search: all three tiers (working + episodic + semantic), aggregate by relevance
- Outcome: highest recall, higher token cost

Complexity classification via [[023-complexity-triage-routing/spec]]; same signal applied to memory tier selection.
See [[004-14-memory-tiering-rfc-decision]] for design rationale.

---

## Skill Promotion Pathways

Memory clusters and dense graph regions can be automatically promoted to the skills layer via multiple pathways (see [[004-15-memory-skill-coevolution-rfc-decision]]):

### HeLa-Mem Consolidation Path (Implemented)
Periodic daemon identifies high-weight clusters in the episodic memory graph:
- Query: `SELECT node_id, degree, AVG(weight) FROM ... GROUP BY node_id HAVING degree * AVG(weight) > consolidation_threshold`
- Collect neighboring node summaries; pass to `consolidate_provider` LLM
- LLM output → stored as `PersistentRule` or enqueued as skill draft
- See [[004-11-memory-hela-mem]] §3.4 for implementation details

### Cognitive Folding Path (RFC #4218, P2)
Idle-time memory reorganization via clustering on co-occurrence patterns:
- Triggers when agent idle > `idle_window_ms` (default 5s)
- Extract dense subgraphs via community detection (edge weight > `clustering_threshold`)
- Diversity check: skip homogeneous clusters (entropy threshold)
- Candidates → skill drafts or new episodic entities
- See [[004-11-memory-hela-mem]] §3.6 for details

### Future: MemQ Value Learning Path (RFC #4218, P3)
When [[041-experiments/spec]] framework matures:
- Track `(memory_fact_id, retrieval_context) → outcome` tuples
- Compute Q-values via temporal difference learning
- High-Q facts promoted based on retrieval-context utility
- Tracked in issue #4042

All promotion pathways terminate in the skill registry (see [[005-skills/spec]]) via the `draft_skill()` pathway. No new skill storage infrastructure is required.

---

## Experience Compression Spectrum

`[memory.compression_spectrum]` (disabled by default, #3305, #3350): introduces
`CompressionLevel` (Episodic / Procedural / Declarative) and a `RetrievalPolicy` that
skips episodic recall when the token budget is below configurable thresholds. A background
`PromotionEngine` scans recent episodic memory and promotes repeated patterns to SKILL.md
entries (off hot path, via `JoinSet`).

`ExperienceStore` records tool outcomes fire-and-forget via `TaskClass::Telemetry`;
evolution sweep runs every N user turns; both gate on `memory.graph.experience.enabled`
with zero overhead when disabled (#3318, #3349).

### Key Invariants

- `PromotionEngine` runs off the hot path — NEVER on the agent turn thread
- `ExperienceStore` wiring must be guarded by `memory.graph.experience.enabled`
- `MemoryError::Promotion` is a distinct error variant in `zeph-memory` (thiserror, no anyhow)

## MemFlow Tiered Retrieval (#3791, arXiv:2605.03312)

Intent-driven tiered retrieval with three depth tiers controlled by LLM-based classifier and validator.

| Tier | Intent | Retrieval Scope |
|------|--------|----------------|
| `ProfileLookup` | Simple entity/fact lookup | SQLite working store only |
| `TargetedRetrieval` | Multi-turn reasoning | Episodic + Qdrant semantic |
| `DeepReasoning` | Complex cross-session inference | All tiers + graph traversal |

- Classifier LLM call determines the tier before retrieval; validator LLM call verifies the result post-retrieval
- Both calls route via configurable `*_provider` fields (multi-model pattern)
- Fail-open heuristic: on classifier error or timeout → default to `TargetedRetrieval`
- Disabled by default: `[memory.memflow] enabled = false`

### Config

```toml
[memory.memflow]
enabled = false
classifier_provider = ""   # [[llm.providers]] name; empty = primary
validator_provider  = ""
```

---

## ScrapMem Optical Forgetting (#3791, arXiv:2605.03804)

Progressive `ContentFidelity` decay for messages that have not been accessed recently,
combined with an Episodic Memory Graph (EM-Graph) for causal-temporal event linking.

| Fidelity Level | Storage | Description |
|----------------|---------|-------------|
| `Full` | Complete content | No decay applied |
| `Compressed` | Summarized form | Low-access messages; summary generated at decay point |
| `SummaryOnly` | Brief summary | Very low-access; original tokens freed |

- EM-Graph edges link events by causal and temporal proximity; used for context-aware decay decisions
- Decay is driven by a background loop (`optical_forgetting_loop`) that runs off the hot path
- Disabled by default: `[memory.scrap_mem] enabled = false`

### Key Invariants

- `optical_forgetting_loop` MUST NOT run on the agent turn thread
- Decay is irreversible within a session; original content is not restored on access
- EM-Graph edges persist in SQLite (episodic graph table) — decay state is recoverable across restarts

---

## Tiered Recall (`recall_tiered`) Wired to Agent Loop (#3968)

`recall_tiered` and `optical_forgetting_loop` are now wired into the production agent loop
(previously implemented but not called from `zeph-core`). `recall_tiered` is called from
`ContextAssembler::gather()` as the default semantic recall path when MemFlow is enabled.
When disabled, the prior `recall_semantic` path is used unchanged.

---

## Sub-Specifications

| Sub-spec | Feature |
|---|---|
| [[004-10-memory-memmachine-retrieval]] | MemMachine retrieval depth, query bias correction, episode preservation |
| [[004-11-memory-hela-mem]] | HeLa-Mem Hebbian edge weights, consolidation, spreading activation |
| [[004-12-memory-reasoning-bank]] | ReasoningBank distilled strategy memory, self-judge pipeline |
| [[004-14-memory-tiering-rfc-decision]] | RFC #4217 decision: memory tiering architecture analysis |
| [[004-15-memory-skill-coevolution-rfc-decision]] | RFC #4218 decision: memory–skill coevolution analysis |
| [[004-16-shadow-memory-safety]] | Shadow Memory Safety — trajectory-level attack defense (MAGE, issue #3695) |
| [[004-17-implicit-conflict-detection]] | Implicit Conflict Detection — STALE/CUPMem fuzzy predicate matching and propagation-aware SYNAPSE recall (issue #3702) |
| [[004-18-five-signal-retrieval]] | Five-Signal Retrieval — access frequency, causal distance, novelty signals + async consolidation daemon (MemTier, issue #3703) |

## Configurable Embed Timeout (`[memory.semantic]`)

`embed_timeout_secs` is a configurable field in the semantic memory config (added in commit #4613).
All `embed()` call sites in `zeph-memory`, `zeph-plugins`, and `zeph-index` that were previously
unguarded now carry a `tokio::time::timeout` wrapper (commits #4592, #4597).

```toml
[memory.semantic]
embed_timeout_secs = 5   # per-embed timeout; 0 = disabled
```

This is separate from `context.fidelity.max_embed_input_tokens` (which limits input size) —
`embed_timeout_secs` limits wall-clock duration of the embed call itself.

## Vector Search Limit Clamp (`MAX_SEARCH_LIMIT`, #6553/#6616/#6623)

`zeph_memory::MAX_SEARCH_LIMIT = 100` bounds every caller-supplied `limit`/`top_k` search
parameter reaching Qdrant, closing an oversized-result-set DoS: an unbounded `limit` (e.g. a
misconfigured `memory.retrieval.depth` or a caller passing `usize::MAX`) would otherwise make
`zeph-memory` allocate/deserialize an arbitrarily large Qdrant result set in one call. There is
deliberately no config knob to raise this ceiling.

The enforcement point evolved across three PRs on the same invariant — only the final
(current) state below reflects live behavior:

1. **#6553/PR #6615** (`e0af1f70c`, breaking commit shared with A2A — see
   [[014-a2a/spec]]) added the clamp only at the wrapper layer: `EmbeddingStore::search`/
   `search_collection`, `EmbeddingRegistry::search_raw`, `ReasoningMemory::retrieve_by_embedding`.
   A caller reaching a `VectorStore` implementor directly (e.g. `zeph-index`'s `CodeStore::search`
   or a generic `RetrievalStep<P, V: VectorStore>` pipeline step) bypassed these wrappers entirely.
2. **#6622** (closes #6616) added a `clamp_search_limit` call at the top of `search()` in each of
   the three production implementors (`QdrantOps`, `DbVectorStore`, `InMemoryVectorStore`) —
   but this relied on every implementor remembering to call the helper by convention.
3. **#6627** (closes #6623, current state) converts `VectorStore::search` into a **template
   method**: it is now trait-provided, clamps `limit` to `[1, MAX_SEARCH_LIMIT]` unconditionally,
   and delegates to a new required `search_clamped()` method that implementors supply instead of
   `search()` itself. This closes the gap where a 4th implementor (a test mock) had silently
   skipped the by-convention clamp in #6622 — the bound is now structurally reached by every
   call path regardless of implementor.

A one-shot `tracing::warn!` fires the first time a requested limit is actually reduced (each
clamping site warns independently, at most once per process).

### Key Invariants

- `limit`/`top_k` reaching Qdrant is always in `[1, MAX_SEARCH_LIMIT]`, regardless of call path —
  enforced structurally by `VectorStore::search`'s template method, not by per-implementor convention
- Implementors of `VectorStore` MUST implement `search_clamped`, NEVER override `search` — overriding
  `search` bypasses the clamp entirely
- `search_clamped` MUST NOT re-clamp `limit` — it is guaranteed already within bounds by `search`
- NEVER add a config field to raise `MAX_SEARCH_LIMIT` — this reopens the oversized-result-set DoS
  the constant exists to close; a config-driven candidate pool larger than 100 is silently truncated
  (with a one-shot warning), by design

### Related: Qdrant Endpoint Hardening Warning

Same PR (#6553/#6615) added a non-fatal `tracing::warn!` in `Config::validate` when
`memory.qdrant_url` points at a non-loopback host without TLS (`https://`) or an API key
(`memory.qdrant_api_key`) configured — memory content would otherwise travel in plaintext with
no server authentication. Loopback targets (`localhost`, `127.0.0.1`, `::1`) are exempt. This is
a warning, not a hard validation failure, since a remote Qdrant reachable only over an
already-trusted internal network is a legitimate deployment.

## Benna-Fusi Multi-Timescale SYNAPSE Edges (#3709, #3710, #3994)

### Fast/Slow Synaptic Variables (#3709)

Graph `Edge` gains two additional floating-point fields alongside the existing `confidence`:

| Field | Description |
|-------|-------------|
| `confidence_fast` | Short-timescale synaptic variable; high learning rate, fast decay |
| `confidence_slow` | Long-timescale synaptic variable; low learning rate, slow decay |

Both variables evolve on every reassertion (APEX and legacy paths) via a two-timescale
leaky cascade:

```
confidence_fast ← (1 - η_fast) * confidence_fast + η_fast * new_evidence
confidence_slow ← (1 - η_slow) * confidence_slow + η_slow * new_evidence
```

SYNAPSE spreading activation uses an `α * fast + (1 − α) * slow` blend as the traversal weight.
The `slow` variable gates the conflict resolver's recency fallback. Rates (`α`, `η_fast`,
`η_slow`) are config-tunable and validated at startup.

```toml
[memory.graph]
benna_fusi_alpha  = 0.7    # blend weight for fast variable in spread
benna_fusi_eta_fast = 0.3  # learning rate for fast variable
benna_fusi_eta_slow = 0.05 # learning rate for slow variable
```

### MemORAI Graph Retrieval Improvements (#3710)

Migration 096 adds `confidence_fast`, `confidence_slow`, and `turn_index` to `graph_edges`
(both SQLite and PostgreSQL schemas). A fail-open `MemoryWriteGate` prefilter in `insert_edges`
drops low-confidence, low-signal edges before storage. `turn_index` is threaded through
`GraphExtractionConfig` and both insert paths (APEX and legacy); population from the agent
turn counter is wired at extraction time.

### DeepReasoning Query-Conditioned Routing (#3994)

`memory.retrieval.deep_reasoning_query_conditioned = true` (opt-in, fail-open) routes
`DeepReasoning` tier calls through `recall_graph_hela` instead of the static-weight path.
The static-weight path remains the default when the flag is `false`.

```toml
[memory.retrieval]
deep_reasoning_query_conditioned = false   # opt-in
```

### Key Invariants

- `η_fast > η_slow` MUST be enforced at config validation — equal or reversed rates collapse the two-timescale model
- `α` MUST be in `[0.0, 1.0]` — validated at startup; out-of-range is a config error
- `confidence_fast` and `confidence_slow` are updated on every reassertion — NEVER skip the update for legacy insert paths
- Migration 096 is append-only — existing rows get `NULL` fast/slow until first reassertion; read code handles `NULL` gracefully
- `deep_reasoning_query_conditioned = true` must be fail-open — if `recall_graph_hela` errors, fall back to static-weight path

### `GraphConfig::retrieval_strategy` Wired to the Live Recall Path (#6597, BREAKING)

`retrieval_strategy` (`[memory.graph]`, values `synapse`/`bfs`/`astar`/`watercircles`/
`beam_search`/`hybrid`) was parsed, validated, and documented, but **not consulted** by the
live graph-recall path used by every real entry point (CLI/ACP/daemon/serve) — that path
(`SemanticMemoryBackend::recall_graph_facts`) only switched on `spreading_activation.enabled`
(SYNAPSE vs. plain BFS). The correct 6-way dispatch existed only in
`zeph-agent-context::helpers`, which had zero production callers. #6597 ports the dispatch into
the live path (`fetch_graph_facts` in `zeph-context` → `recall_graph_facts` in
`zeph-agent-context`) and removes the dead duplicate chain. Full strategy details, the variant
table, and the WaterCircles ring-hop fix live in [[012-graph-memory/spec]]; see that spec's
"Graph Retrieval Strategy Dispatch" section.

**BREAKING**: deployments on the default config (`spreading_activation.enabled = false`,
`retrieval_strategy` unset) now get SYNAPSE spreading-activation recall instead of the
previous — unintentional — plain-BFS fallback, since `retrieval_strategy`'s documented default
(`synapse`) now actually takes effect. Set `retrieval_strategy = "bfs"` under `[memory.graph]`
to keep the prior BFS-only behavior; benchmark before upgrading if graph-recall latency/cost
is a concern.

---

## JoinSet and CancellationToken Fixes

- `spawn_graph_extraction` now receives a `CancellationToken` from `LifecycleState` for clean shutdown (commit #4635)
- `spawn_asi_update` JoinSet reaps completed tasks before checking the cap — prevents the cap from being permanently hit after 8 completions (commit #4648, closes #4644)
- `maybe_refresh_communities` no longer spawns orphan tasks; uses `CancellationToken` guard (commit #4629)
- `hebbian_spread` timeout propagated through all call paths (commit #4638)

## Integration Points

- [[002-agent-loop/spec]] — context assembly calls recall pipeline
- [[001-system-invariants/spec]] — memory pipeline contract
- [[012-graph-memory/spec]] — optional graph-based entity tracking
- [[031-database-abstraction/spec]] — SQLite persistence layer

---

## Sources

### External
- **A-MEM** (NeurIPS 2025) — agentic write-time memory linking: https://arxiv.org/abs/2502.12110
- **Zep: Temporal Knowledge Graph** (Jan 2025) — temporal edges, LongMemEval +18.5%: https://arxiv.org/abs/2501.13956
- **TA-Mem** (Mar 2026) — adaptive retrieval dispatch: https://arxiv.org/abs/2603.09297
- **Episodic-to-Semantic Memory Promotion** (Jan 2025): https://arxiv.org/pdf/2501.11739
- **MAGMA** (Jan 2026) — multi-graph agent memory: https://arxiv.org/abs/2601.03236
- **Context Engineering in Manus** (Oct 2025) — tool output reference pattern: https://rlancemartin.github.io/2025/10/15/manus/
- **Structured Anchored Summarization** (Factory.ai, 2025) — typed schemas: https://factory.ai/news/compressing-context

### Internal
| File | Contents |
|---|---|
| `crates/zeph-memory/src/semantic/mod.rs` | `SemanticMemory`, recall pipeline, compaction |
| `crates/zeph-memory/src/graph/mod.rs` | Graph memory integration |
| `crates/zeph-llm/src/provider.rs` | `MessagePart`, `MessageMetadata` definitions |
| `crates/zeph-core/src/agent/mod.rs` | `MemoryState`, deferred summary apply logic |

---

## See Also

- [[MOC-specs]] — Master index of all specifications
- [[001-system-invariants/spec]] — System-wide non-negotiable rules
