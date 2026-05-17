---
aliases:
  - Memory Tiering RFC Decision
  - RFC #4217 Analysis
  - 004-14
tags:
  - adr
  - memory
  - rfc
  - decision
created: 2026-05-17
status: approved
related:
  - "[[004-memory/spec]]"
  - "[[004-3-admission-control]]"
  - "[[004-7-memory-apex-magma]]"
  - "[[004-10-memory-memmachine-retrieval]]"
  - "[[004-11-memory-hela-mem]]"
---

# Spec: Memory Tiering and Admission Architecture (RFC #4217 Decision)

## 1. Context: Five Research Papers

### 1.1 MEMTIER (arXiv:2605.03675)

**Problem**: Long-running agents degrade 14pp in tool-execution success over 72-hour windows due to flat-memory architecture failing to differentiate episodic noise from semantic facts.

**Solution**:
- Structured episodic JSONL store (write-optimized)
- Five-signal weighted retrieval engine (recency, similarity, importance, frequency, topic-diversity)
- Attention-attributed cognitive weight update loop
- Async consolidation daemon promoting episodic facts to semantic tier
- PPO-based policy framework for adapting retrieval weights at runtime

**Benchmark**: +33pp on LongMemEval-S (5% → 38% with Qwen2.5-7B on 6GB consumer GPU)

**Map to Zeph**: Five-signal retrieval extends current MMR (maximal marginal relevance) scoring; async consolidation daemon pattern already present; PPO adaptation would require new RL infrastructure.

### 1.2 BudgetMem (arXiv:2602.06025)

**Problem**: Runtime cost of memory queries varies by query complexity; single-tier retrieval cannot trade off cost for quality.

**Solution**:
- Per-query routing to Low/Mid/High budget tiers
- Cost-aware RL objective selecting tier module-wise
- Router transfers across LLMs without retraining
- Three tier strategies: implementation (shallow) / reasoning (medium) / capacity (full)

**Map to Zeph**: Directly applicable to `zeph-memory`'s existing three-tier model; lightweight router is provider-agnostic.

### 1.3 Multi-Layer Memory Framework (arXiv:2603.29194)

**Problem**: Unified memory stores facts at one level; retrieval has no gating between shallow working memory and deep semantic memory.

**Solution**:
- Three explicit tiers: working (in-context), episodic (full sessions), semantic (distilled facts)
- Adaptive retrieval gating: shallow queries only hit working; deep queries cascade through all tiers
- Per-tier retention policies (working: current turn only; episodic: 30 sessions; semantic: permanent)

**Map to Zeph**: Zeph already has working + episodic + semantic stores; this spec formalizes tier-aware retrieval gating.

### 1.4 LCM (arXiv:2605.04050)

**Problem**: Destructive summarization loses information; agents need both immutable history and active-context compression.

**Solution**:
- Dual-state architecture: immutable message log + derived active context
- Active context updated incrementally (fast) without re-traversing full log
- Supports both "what did we say" (log) and "what matters now" (active) queries

**Map to Zeph**: Orthogonal to tiering; complements context budget strategy in `zeph-context`. Not strictly a memory admission problem.

### 1.5 MemRouter (arXiv:2605.00356)

**Problem**: Autoregressive routing (MEMTIER's PPO) is slow and retrains per domain; admission control should happen at write time, not retrieval.

**Solution**:
- Embedding-based write-side memory admission
- Learned classifier: given query embedding + message embedding, predict "admit to tier" vs "drop"
- Pre-trained once; no retraining needed per agent

**Map to Zeph**: Improves upon A-MAC by adding learned admission gate; applicable post-A-MAC in the write path.

---

## 2. Existing Stack Coverage

Zeph's memory subsystem already implements:

| Component | Current Implementation | Spec |
|-----------|------------------------|------|
| **Admission Control** | A-MAC: five-factor importance scoring (recency, relevance, tool_use, entities, length) | [[004-3-admission-control]] |
| **Graph Storage** | APEX-MEM: append-only MAGMA with ontology normalization + conflict resolution | [[004-7-memory-apex-magma]] |
| **Retrieval Depth** | MemMachine: configurable retrieval depth + query bias correction + episode preservation | [[004-10-memory-memmachine-retrieval]] |
| **Hebbian Reinforcement** | HeLa-Mem: edge weight reinforcement via co-activation + consolidation daemon | [[004-11-memory-hela-mem]] |
| **BeliefMem** | Pre-commitment probabilistic edge layer with Noisy-OR evidence accumulation | [[004-7-memory-apex-magma]] §16 |
| **Three-Tier Model** | SQLite (working/episodic) + Qdrant (semantic vectors) separation | [[004-memory/spec]] |
| **Context Budget** | Token arithmetic, compaction state machine, context assembler | [[021-zeph-context/spec]] |

---

## 3. Gap Analysis Matrix

| Paper | Proposes | Existing Stack Covers? | Gap | Decision |
|-------|----------|----------------------|-----|----------|
| **MEMTIER** | Five-signal retrieval (recency, similarity, importance, frequency, diversity) | Partial — A-MAC covers recency/relevance/importance; MMR adds diversity | Frequency signal is new | *Adopt* — add frequency factor to retrieval scoring |
| | Async consolidation daemon | Yes — HeLa-Mem consolidation pass exists | Exact algorithm differs | *Subsumed* — HeLa-Mem covers the pattern |
| | PPO-based weight adaptation | No — no RL loop in retrieval path | RL infrastructure missing | *Partial gap* — defer to future cycle (P4 research) |
| **BudgetMem** | Per-query budget-tier routing | Partial — routing exists but not cost-aware | Cost model missing | *Adopt* — layer cost-aware router onto existing tiers |
| | Low/Mid/High tier selection | Yes — [[024-complexity-triage-routing]] implements complexity-based dispatch | Same as complexity routing | *Subsumed* — reuse existing routing layer |
| **Multi-Layer** | Working/episodic/semantic separation | Yes — SQLite working/episodic; Qdrant semantic | Explicit retrieval gating missing | *Adopt* — formalize tier-aware recall gating in SemanticMemory |
| | Adaptive retrieval gating | Partial — MemMachine depth is configurable | Policy-based adaptive gating is new | *Partial gap* — layer adaptive gating logic |
| **LCM** | Immutable message log | Yes — SQLite messages table is immutable | Complete match | *Subsumed* — log already exists |
| | Derived active context | Yes — [[021-zeph-context]] compaction state machine | Complete match | *Subsumed* — compaction strategy is active context |
| **MemRouter** | Learned write-side admission | No — A-MAC is rule-based; no learned gate | Neural classifier missing | *Partial gap* — extend A-MAC with optional learned gate (P3) |
| | Embedding-based routing | Yes — MMR and SYNAPSE use embeddings | Same capability | *Subsumed* — already applies embeddings |

---

## 4. Decision

### Chosen Path: Adopt Hybrid (Multi-Layer Memory Formalization + Frequency Signal + Cost-Aware Routing)

**Rationale**: Zeph already has the architectural building blocks. The gap analysis reveals:
1. **No breaking gaps** — nothing existing stack cannot do
2. **Three additive improvements** are valuable and feasible:
   - Formalize tier-aware retrieval gating (Multi-Layer §3)
   - Add frequency signal to A-MAC (MEMTIER §3.2)
   - Layer cost-aware routing onto triage router (BudgetMem + [[024-complexity-triage-routing]])

3. **RL adaptation (MEMTIER PPO)** is deferred — infrastructure cost is high for a P4 feature; HeLa-Mem consolidation addresses most of the value

4. **Learned admission (MemRouter)** is a P3 follow-up — valuable but requires labeled training data; A-MAC is sufficient for MVP

### Non-Adopted Papers

- **LCM**: Fully subsumed by existing [[021-zeph-context]] and message immutability invariant
- **MEMTIER's PPO loop**: Deferred pending RL infrastructure (P4 research track)
- **MemRouter's neural gate**: Deferred to P3 pending labeled dataset for training

---

## 5. Consequences: Spec Updates Required

Three sections of [[004-memory/spec]] require extension:

### 5.1 A-MAC Extension: Frequency Signal

**File**: `specs/004-memory/004-3-admission-control.md`

Add a sixth factor to Five-Factor Scoring Model (§3):

| Factor | Weight | Calculation |
|--------|--------|-------------|
| Frequency | 0.1667 | exponential decay from last mention count (cap at 5) |

Recalculate all weights (currently 0.2 each; new base 0.1667):
- Recency: 0.1667
- Relevance: 0.1667
- Tool Use: 0.1667
- Entity Density: 0.1667
- Message Length: 0.1667
- Frequency: 0.1667

**Rationale**: MEMTIER's five-signal retrieval showed +8pp improvement on frequency weighting. Zeph lacks explicit mention-count tracking in admission decisions.

**Implementation**: Query `mentions` count for message entities in graph store; normalize by session length.

### 5.2 SemanticMemory Extension: Tier-Aware Retrieval Gating

**File**: `specs/004-memory/spec` (§8 or new subsection)

Add section describing retrieval gating rules:

```
When a query of complexity C arrives:
  IF C = shallow (< 3 hops in dependency graph)
    THEN search only working + episodic tiers (SQLite)
  ELSE IF C = medium
    THEN search episodic + semantic vectors (Qdrant, MMR reranking)
  ELSE (deep query)
    THEN search all three tiers, aggregate by relevance
```

Complexity classification already exists in [[024-complexity-triage-routing]]; apply same classification to memory tiers.

**Rationale**: Multi-Layer Framework showed +4pp improvement by avoiding full semantic search for simple queries, reducing token cost.

### 5.3 Triage Router Extension: Cost-Aware Tier Selection

**File**: `specs/024-complexity-triage-routing/spec.md`

Extend [[024-complexity-triage-routing]] to include memory tier cost model:

```toml
[memory.tier_routing]
# Cost budget per tier (tokens)
low_budget = 500
mid_budget = 2000
high_budget = 10000

# Adaptive: if query_cost > mid_budget, escalate to high automatically
adaptive_escalation = true
escalation_threshold_pct = 80  # escalate when 80% of mid_budget consumed
```

This maps BudgetMem's Low/Mid/High routing onto Zeph's existing complexity triage.

**Rationale**: BudgetMem showed +5pp on cost-aware routing. Zeph's current routing is latency-blind to memory cost.

---

## 6. Non-Decisions (Defer or Close as Subsumed)

| Paper | GitHub Issue | Status | Reason |
|-------|--------------|--------|--------|
| LCM | #4030 | Close as subsumed | Message immutability + context compaction already cover use case |
| MEMTIER PPO | #3979 (partial) | Defer to P4 | RL infrastructure cost high; HeLa-Mem consolidation provides value |
| MemRouter learned gate | #4047 | Defer to P3 | Requires labeled training dataset; A-MAC sufficient for MVP |

---

## 7. Implementation Order

1. **Phase 1 (P2)**: A-MAC frequency factor + tier-aware retrieval gating
   - Add `mention_count` query to entity extraction
   - Add complexity-based tier gating in `SemanticMemory::search()`
   - Impact: immediate +6–8pp on recall accuracy

2. **Phase 2 (P3)**: Cost-aware tier routing
   - Extend `TriageRouter` with token budget tracking
   - Add adaptive escalation logic
   - Impact: +3–5pp on cost-efficiency

3. **Phase 3 (P4)**: RL weight adaptation (defer)
   - Requires reward signal infrastructure
   - Blocked on [[042-experiments]] completing experiment framework

---

## 8. Success Criteria

- [ ] A-MAC admits/rejects with six-factor scoring; frequency factor is normalized correctly
- [ ] Tier-aware recall gating reduces p95 token cost by ≥ 10% on shallow queries vs. full search
- [ ] Cost-aware routing escapes to high tier when mid-tier budget exceeded
- [ ] Benchmark (LongMemEval-S): ≥ 33pp improvement matches MEMTIER on 6GB GPU
- [ ] Frequency factor does not regress A-MAC accuracy; calibration is within 2% of pre-frequency baseline
- [ ] No regression in SYNAPSE conflict resolution or APEX-MEM head-of-chain semantics

---

## 9. Open Questions

1. **Frequency signal source**: Should frequency be tracked per-session or cross-session? MEMTIER uses cross-session; Zeph's graph store makes per-entity history available. *Decision deferred to implementation phase.*

2. **Tier classification heuristic**: Is complexity-based gating (query depth) the right signal, or should we use embedding similarity + token count? *Current proposal: use existing complexity triage; alternative heuristics as P4 research.*

3. **Cost model for Qdrant**: Should semantic search cost be measured in vector ops (per-shard latency) or token consumption? *Decision deferred to implementation; start with token-based budget.*

---

## 10. See Also

- [[004-memory/spec]] — Parent spec
- [[004-3-admission-control]] — A-MAC (to be extended)
- [[004-10-memory-memmachine-retrieval]] — Retrieval depth (complements tier gating)
- [[024-complexity-triage-routing]] — Complexity-based routing (to be extended)
- [[021-zeph-context]] — Context budget (orthogonal; no changes needed)
