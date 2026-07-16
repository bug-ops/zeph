---
aliases:
  - Memory Skill Coevolution RFC Decision
  - RFC #4218 Analysis
  - 004-15
tags:
  - adr
  - memory
  - skills
  - rfc
  - decision
created: 2026-05-17
status: approved
related:
  - "[[004-memory/spec]]"
  - "[[004-11-memory-hela-mem]]"
  - "[[005-skills/spec]]"
  - "[[015-self-learning/spec]]"
---

# Spec: Memory–Skill Coevolution Architecture (RFC #4218 Decision)

## 1. Context: Six Research Papers

### 1.1 MemQ (arXiv:2605.08374)

**Problem**: Memory facts are static; they don't improve agent reasoning patterns over sessions.

**Solution**:
- Q-learning over memory provenance DAGs
- Each memory access is a state-action pair; outcome (success/failure) is the reward
- Learns value function: `Q(fact_id, retrieval_context) → expected_utility`
- Periodically promotes high-Q facts to skill layer

**Benchmark**: +28% on long-horizon reasoning tasks (Zeph target: skill learning from memory)

**Map to Zeph**: Directly applicable to `zeph-skills` evolution; memory facts already have provenance in APEX-MEM edges.

### 1.2 δ-mem (arXiv:2605.12357)

**Problem**: Long-context memory requires full replay; state snapshots are memory-inefficient.

**Solution**:
- Differential state updates: store only deltas between consecutive states
- Online summarization of deltas reduces effective context window by 50%
- Enables long-horizon agent operation with bounded memory footprint

**Benchmark**: +14pp on LongMemEval; works with any agent architecture

**Map to Zeph**: Complements context budget strategy ([[021-zeph-context]]); mostly a storage optimization, not a skill evolution mechanism.

### 1.3 EvolveMem (arXiv:2605.13941)

**Problem**: Memory retrieval parameters (depth, similarity threshold, etc.) are hand-tuned; they drift as sessions age.

**Solution**:
- Self-evolving configuration: measure retrieval success rate per turn
- Online parameter tuning: if success rate drops below threshold, increase retrieval depth or similarity tolerance
- Per-session personalization without retraining

**Benchmark**: +7pp on long-session consistency (72+ hours)

**Map to Zeph**: Maps onto existing [[024-complexity-triage-routing]] parameters; adds feedback loop.

### 1.4 SAGE: Self-Evolving Agentic Graph-Memory Engine (arXiv:2605.12061)

**Problem**: Graph memory structure (edges, weights, entity resolution) is static; valuable patterns are not automatically extracted.

**Solution**:
- Self-evolving entity merger: fuse coreference clusters via embedding similarity + LLM vote
- Automatic edge pruning: remove low-utility edges (weight below decay threshold)
- Attention-weighted spreading activation: learns path weights from success/failure feedback
- SAGE-RL layer: cross-session reward signal promotes emergent subgraphs to semantic clusters

**Benchmark**: +19% on LoCoMo (compared to static MAGMA)

**Map to Zeph**: CRITICAL: Name collision with existing SAGE RL in [[015-self-learning]] (cross-session reward model for skills). This proposal is SAGE-GraphMem; existing is SAGE-RL. Coexistence requires namespace disambiguation.

### 1.5 NanoResearch (arXiv:2605.10813)

**Problem**: Agent improvement is siloed — memory, skills, and policy evolve independently. Missing cross-layer feedback.

**Solution**:
- Tri-level coevolution: skills → memory (what to remember), memory → policy (how to decide), policy → skills (what to improve)
- Closed-loop feedback: measure end-to-end task success; backpropagate to all three layers
- Micro-evolution: per-session parameter tuning without global retraining

**Benchmark**: +31% on personalization metrics (user preference alignment)

**Map to Zeph**: Requires integration across `zeph-memory`, `zeph-skills`, and `zeph-core` agent loop; high architectural scope.

### 1.6 Cognifold (arXiv:2605.13438)

**Problem**: Memory organization is reactive — facts are added; nothing proactively reorganizes or surfaces patterns.

**Solution**:
- Cognitive folding: background process continuously reorganizes memory by clustering co-occurrence patterns
- Always-on: runs during idle turns without blocking agent
- Pattern extraction: folded clusters are candidates for skill promotion or user model updates

**Benchmark**: +12% on implicit knowledge acquisition (patterns that the agent doesn't explicitly recall but leverages)

**Map to Zeph**: Orthogonal to skill evolution; focuses on memory organization via continuous clustering. Complements HeLa-Mem consolidation.

---

## 2. Existing Stack Coverage

Zeph's skill + memory + learning ecosystem already implements:

| Component | Current Implementation | Spec |
|-----------|------------------------|------|
| **Cross-Session Reward** | SAGE RL: measure skill success across sessions via feedback detection | [[015-self-learning]] |
| **Feedback Detection** | FeedbackDetector (regex) + JudgeDetector (LLM): implicit correction signals | [[054-agent-feedback]] |
| **Skill Evolution** | ARISE trace improvement + STEM pattern migration; Wilson score reranking | [[015-self-learning]] |
| **Memory Consolidation** | HeLa-Mem: periodic identification of dense clusters + consolidation daemon | [[004-11-memory-hela-mem]] |
| **Provenance Tracking** | APEX-MEM edges store episode_id, confidence, temporal metadata | [[004-7-memory-apex-magma]] |
| **Belief Staging** | BeliefMem: pre-commitment evidence accumulation via Noisy-OR | [[004-7-memory-apex-magma]] §16 |
| **Parameter Tuning** | [[024-complexity-triage-routing]]: complexity-aware dispatch (static config) | [[024-complexity-triage-routing]] |

---

## 3. Per-Source Gap Matrix

| Paper | Proposes | Existing Stack Covers? | Gap | Status |
|---|---|---|---|---|
| **MemQ** | Q-learning over memory DAGs for skill promotion | No — no Q-values in current memory model | Value function learning missing | **Partial Gap** |
| | Promotion of high-Q facts to skills | Partial — HeLa-Mem consolidation exists but is heuristic-based, not reward-driven | Decision rule differs | **Partial Gap** |
| | Reward signal from retrieval outcomes | Partial — success/failure exists as feedback; no explicit attribution to memory facts | Attribution missing | **Partial Gap** |
| **δ-mem** | Differential state representation | No — messages stored in full; no delta encoding | Optimization missing | **Subsumed** (orthogonal to coevolution) |
| | Online summarization via deltas | Partial — compaction exists; delta-based summarization is new | Summarization strategy differs | **Subsumed** (not skill-related) |
| **EvolveMem** | Self-tuning retrieval parameters | No — complexity routing is static config | Adaptive tuning missing | **Partial Gap** |
| | Per-session parameter drift detection | No — no feedback loop in routing layer | Drift detection missing | **Partial Gap** |
| | Online personalization | Partial — [[024-complexity-triage-routing]] has per-provider config but no feedback loop | Feedback loop missing | **Adopt** (layer feedback) |
| **SAGE (Graph-Mem)** | Entity merger via embedding + LLM | Partial — entity extraction exists; automated merger is new | Automated coreference missing | **Partial Gap** |
| | Edge pruning via weight decay | Yes — HeLa-Mem maintains edge weights | Same mechanism | **Subsumed** |
| | Attention-weighted spreading activation | Yes — SYNAPSE uses weighted edges | Same mechanism | **Subsumed** |
| | SAGE-RL cross-session reward for graph | No — but existing SAGE RL in [[015-self-learning]] (skills) is similar concept | **NAME CONFLICT** — see §4 |
| **NanoResearch** | Tri-level coevolution (skills↔memory↔policy) | No — layers evolve independently | Feedback loop missing | **Research Gap** |
| | Closed-loop backpropagation to all three layers | No — feedback terminates at skill layer | Multi-layer feedback missing | **Research Gap** |
| | Per-session micro-evolution | Partial — skill improvement happens; memory + policy tuning is missing | Partial implementation | **Partial Gap** |
| **Cognifold** | Background memory reorganization (idle-time clustering) | Partial — HeLa-Mem consolidation is periodic; idle-time specificity is new | Scheduler-aware consolidation missing | **Adopt** (extend HeLa-Mem) |
| | Co-occurrence pattern extraction | Partial — graph structure implicit; explicit pattern API missing | Pattern extraction API missing | **Partial Gap** |
| | Skill promotion from patterns | Partial — HeLa-Mem promotes clusters; no explicit pattern→skill pathway | Pathway missing | **Partial Gap** |

---

## 4. Namespace Conflict: SAGE RL vs. SAGE-GraphMem

**Issue**: [[015-self-learning]] spec already defines "SAGE RL" as the cross-session reward model for **skills** (arXiv:2405.12345 style). The research paper #4057 proposes "SAGE" for **graph-memory** evolution.

**Resolution**:
- Rename arXiv:2605.12061 implementation module to `SAGE-GraphMem` (or `SageGraph` in code) to avoid symbol collision
- Keep [[015-self-learning]] SAGE RL unchanged
- Document both in memory spec and self-learning spec with explicit cross-references

**Namespace mapping**:
```rust
// In zeph-memory/src/graph/sage.rs:
pub struct SageGraphMemory { ... }  // NOT SageRl

// In zeph-skills/src/sage.rs:
pub struct SageRL { ... }  // existing, unchanged
```

---

## 5. Decision

### Chosen Path: Adopt Cognifold (Extend HeLa-Mem) + EvolveMem (Layer Feedback) + MemQ (P3 Research Track)

**Rationale**:

1. **Cognifold is adoptable** (P2 follow-up to HeLa-Mem)
   - Idle-time memory reorganization is a natural extension of existing consolidation daemon
   - No new infrastructure needed; reuses graph traversal + clustering
   - +12% implicit knowledge gain is valuable

2. **EvolveMem is adoptable** (P2)
   - Feedback loop on routing parameters maps onto existing [[024-complexity-triage-routing]]
   - Success/failure signals already tracked via FeedbackDetector
   - Low implementation cost; +7pp on long-session consistency

3. **MemQ is deferred to P3** (research track)
   - Requires value function learning (Q-learning) infrastructure
   - High complexity; value signal attribution is non-trivial
   - Reserve for next learning cycle after [[042-experiments]] completes

4. **SAGE-GraphMem is deferred to P4** (architecture review needed)
   - Requires cross-layer optimization (skills + graph memory simultaneously)
   - Interacts with existing SAGE RL naming; architectural scope needs larger review
   - Recommend as input to agent decomposition effort ([[050-agent-decomposition]])

5. **NanoResearch, δ-mem are deferred**
   - NanoResearch is a full-system redesign; belongs in next major architecture cycle
   - δ-mem is a storage optimization, orthogonal to coevolution; defer to P4 performance track

### Non-Adopted Papers (Defer or Close)

| Paper | Issue | Status | Reason |
|-------|-------|--------|--------|
| δ-mem | #4049 | Defer to P4 | Storage optimization; not skill-related; low ROI for current cycle |
| SAGE-GraphMem | #4057 | Defer to P4 + architecture review | Name conflict with existing SAGE RL; needs decomposition alignment |
| NanoResearch | #4055 | Defer to v2.0 | Full-system redesign; scope beyond current cycle |
| MemQ | #4042 | Defer to P3 | Requires RL infrastructure; research track pending experiments framework |

---

## 6. Consequences: Spec Additions Required

### 6.1 HeLa-Mem Extension: Idle-Time Cognitive Folding

**File**: `specs/004-memory/004-11-memory-hela-mem.md` (new §4.2)

Add subsection "Cognitive Folding: Idle-Time Cluster Reorganization":

```
When an agent turn completes AND no new messages arrive within idle_window_ms:
  1. Trigger background consolidation (existing HeLa-Mem daemon)
  2. ADD: Run clustering pass on the memory graph:
     - Identify nodes with high co-occurrence (edge weight > clustering_threshold)
     - Extract dense subgraphs (cliques, stars with central hubs)
     - Compute cluster embedding as centroid of member vectors
  3. ADD: Surface clusters as promotion candidates:
     - Cluster size > min_cluster_size → skill draft
     - Cluster diversity > diversity_threshold → new entity for episodic memory
  4. Mark folded nodes with consolidated_at timestamp to avoid re-clustering

Configuration:
  [memory.hebbian]
  idle_folding_enabled = true
  idle_window_ms = 5000              # fold clusters after 5s idle
  clustering_threshold = 0.7         # edge weight > 0.7 is "co-active"
  min_cluster_size = 3               # at least 3 nodes per cluster
  diversity_threshold = 0.5          # cluster must represent ≥ 2 distinct topics
```

**Implementation**: Reuse existing HeLa-Mem consolidation daemon; add clustering step via `petgraph` algorithms (e.g., BLPA community detection for an initial cut).

### 6.2 Complexity Routing Extension: Feedback-Driven Parameter Tuning

**File**: `specs/024-complexity-triage-routing/spec.md` (new §3.3)

Add subsection "Adaptive Routing: Online Parameter Drift Detection":

```
When measuring retrieval success rate (via FeedbackDetector signals):
  1. Track success rate per tier over a sliding window (default: last 20 queries)
  2. IF success_rate[tier] < success_threshold (e.g., 0.6) for 3+ consecutive turns:
     a. Log anomaly: tier_degradation{tier, success_rate, turn_count}
     b. Escalate one tier up: if tier=low, try mid; if mid, try high
     c. Record escalation in metrics: tier_escalations_total{from_tier, to_tier}
  3. IF success_rate improves after escalation:
     a. Retain new tier; mark as preferred_tier in session state
  4. Cooldown: defer re-evaluation for N turns to avoid thrashing

Configuration:
  [memory.routing_feedback]
  feedback_enabled = true
  success_threshold = 0.6
  sliding_window_size = 20
  escalation_cooldown_turns = 5
```

**Rationale**: EvolveMem showed +7pp improvement on consistency by adapting to session drift. Existing success signals (from FeedbackDetector) provide feedback; layer a simple threshold-based escalation policy.

### 6.3 New Cross-Reference Section in Memory Spec

**File**: `specs/004-memory/spec.md` (new section before See Also)

Add "Skill Promotion Pathways":

```
# Skill Promotion Pathways

Memory clusters and dense graph regions can be promoted to the skills layer via two pathways:

1. **HeLa-Mem Consolidation Path** (implemented)
   - Periodic daemon identifies high-weight clusters
   - Clusters above consolidation_threshold → skill drafts
   - See [[004-11-memory-hela-mem]] §3.4 for details

2. **Cognitive Folding Path** (new, P2)
   - Idle-time reorganization identifies dense co-occurrence patterns
   - Patterns above diversity_threshold → episodic memory or skill candidates
   - See [[004-11-memory-hela-mem]] §4.2 for details

3. **MemQ Path** (future, P3)
   - Value function learning over provenance DAGs
   - High-Q facts promoted based on retrieval-context utility
   - Tracked in issue #4042

Promotion to skills goes through existing [[005-skills/spec]] registry; no new skill storage infrastructure.
```

---

## 7. Future Opportunities (P3, P4)

### P3: MemQ Value Learning

When [[042-experiments]] provides experiment framework:
- Track `(memory_fact_id, retrieval_context) → outcome` tuples
- Compute Q-values via temporal difference learning
- Promote facts with Q > threshold to skills

**Prerequisite**: Experiment framework completion (estimated Q3 2026)

### P4: SAGE-GraphMem (Rename + Integrate)

After agent decomposition review ([[050-agent-decomposition]]):
- Integrate SAGE-GraphMem as graph-layer evolution (separate from SAGE-RL in skills)
- Explicit namespace: `zeph_memory::graph::sage` vs. `zeph_skills::sage`
- Cross-reference both in [[015-self-learning]] and this spec

### P4: δ-mem Differential Representation

As part of context budget optimization:
- Store message deltas instead of full text
- Reduces memory footprint by ~50% on long sessions
- Orthogonal to skill coevolution; belongs in storage tier

---

## 8. Success Criteria

- [ ] Cognifold: idle-time clustering runs without blocking agent; 10+ clusters identified per 72-hour session
- [ ] Cognifold: cluster diversity > 0.5 correlates with skill promotion success (A/B test vs. HeLa-Mem only)
- [ ] EvolveMem: routing tier escalation fires < 5 times per session (low false-positive rate)
- [ ] EvolveMem: success rate after escalation is > success_threshold within 2 turns
- [ ] No regression in existing SAGE RL behavior; skill evolution metrics unchanged
- [ ] Namespace collision resolved: imports `SageGraphMemory` from `zeph-memory`; `SageRL` from `zeph-skills` work without conflict
- [ ] Benchmark: +12pp improvement on implicit knowledge acquisition (Cognifold) + +7pp on long-session consistency (EvolveMem)

---

## 9. Implementation Order

1. **Phase 1 (P2)**: Cognifold idle-time folding
   - Add clustering algorithm to HeLa-Mem consolidation daemon
   - Integrate with scheduler for idle-time trigger
   - Surface clusters as skill promotion candidates

2. **Phase 2 (P2)**: EvolveMem feedback-driven routing
   - Track success rate per tier in TriageRouter
   - Implement threshold-based escalation logic
   - Log anomalies and escalations to metrics

3. **Phase 3 (P3)**: MemQ research track
   - Awaits [[042-experiments]] framework completion
   - Design value function learning over provenance DAGs
   - Implement Q-value tracking and promotion policy

4. **Phase 4 (P4)**: SAGE-GraphMem (full integration)
   - Awaits agent decomposition review
   - Resolve namespace collision
   - Integrate graph-layer evolution with skill layer

---

## 10. Open Questions

1. **Cluster definition**: Is a dense subgraph (weight-based) the right unit for skill promotion, or should we use information-theoretic entropy? *Decision deferred to implementation; start with weight-based heuristic.*

2. **Feedback attribution**: How to attribute success/failure to specific memory facts when multiple facts contribute to a decision? *Current proposal: equally weight all retrieved facts; MemQ research track will refine this.*

3. **MemQ training data**: What constitutes a positive/negative outcome for Q-learning? Agent success (task completion), retrieval hit rate, or user feedback? *Decision deferred to P3 when experiment framework is ready.*

---

## 11. See Also

- [[004-memory/spec]] — Memory parent spec
- [[004-11-memory-hela-mem]] — HeLa-Mem (to be extended with Cognifold)
- [[005-skills/spec]] — Skills layer (promotion destination)
- [[015-self-learning/spec]] — SAGE RL and cross-session reward (namespace clarification needed)
- [[024-complexity-triage-routing]] — Triage router (to be extended with feedback loop)
- [[054-agent-feedback]] — Feedback detection (signals for routing escalation)
