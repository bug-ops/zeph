---
aliases:
  - ReasoningBank
  - Reasoning Memory
  - Strategy Memory
tags:
  - sdd
  - spec
  - memory
  - reasoning
  - research
created: 2026-04-24
status: implemented
related:
  - "[[MOC-specs]]"
  - "[[constitution]]"
  - "[[004-memory/spec]]"
  - "[[024-multi-model-design/spec]]"
  - "[[004-10-memory-memmachine-retrieval]]"
---

# Spec: ReasoningBank — Distilled Reasoning Strategy Memory

> Research analysis of ReasoningBank (arXiv:2509.25140). Resolves GitHub issue
> [#3312](https://github.com/bug-ops/zeph/issues/3312).

## Sources

### External
- **ReasoningBank: Distilling Generalizable Reasoning Strategies from Agent Experience** (arXiv:2509.25140)
- Benchmarks: WebArena (web browsing), SWE-bench (software engineering)
- Related: MaTTS — Memory-Aware Test-Time Scaling for diverse experience generation

### Internal
| File | Contents |
|---|---|
| `crates/zeph-memory/src/` | Memory orchestrator, SQLite storage |
| `crates/zeph-memory/src/semantic/` | Qdrant-backed semantic retrieval |
| `crates/zeph-core/src/agent/context/` | ContextBuilder, memory injection |
| `crates/zeph-skills/src/learning.rs` | Self-learning: `successful_patterns`, `failure_patterns` |
| `crates/zeph-core/src/config.rs` | Agent config structs |

---

## 1. Overview

### Problem Statement

Zeph's SKILL.md self-learning captures coarse tool-usage patterns (`successful_patterns`, `failure_patterns`) but does not capture **why a reasoning approach worked or failed** at a finer granularity. Raw episodic memory stores trajectories verbatim — expensive to retrieve and hard to generalize. There is no structured middle layer between coarse skill patterns and raw episodes.

### Key Research Findings

ReasoningBank introduces a three-stage pipeline:

1. **Self-judgment** — after each completed task, the agent evaluates success/failure using a fast model
2. **Distillation** — successful and failed reasoning chains are compressed into short, generalizable strategy summaries (not raw trajectories)
3. **Retrieval** — at query time, embedding search returns top-k strategy summaries, which are injected into the context preamble before the LLM call

Contrastive signals from failures sharpen strategy boundaries. MaTTS extends this by allocating more compute per task to generate more diverse experience signals.

**Empirical results**: outperforms raw trajectory storage and success-only memory banks on both WebArena and SWE-bench benchmarks.

### Why This Complements Existing Zeph Memory

| Layer | Granularity | Current Zeph |
|---|---|---|
| Skills (`SKILL.md`) | Tool invocation patterns | Implemented |
| ReasoningBank | Reasoning strategy summaries | **Missing** |
| Episodic memory | Raw conversation episodes | Implemented |
| Semantic memory | Factual knowledge graph | Implemented |

---

## 2. Requirements

### Functional

| ID | Requirement |
|---|---|
| RB-F1 | `ReasoningMemory` store in `zeph-memory`: SQLite table for strategy summaries + Qdrant collection for strategy embeddings |
| RB-F2 | Self-judge step (configurable `extract_provider`): after each completed agent turn, evaluate success/failure and extract reasoning chain |
| RB-F3 | Distillation step (configurable `distill_provider`): compress reasoning chain into ≤ 3-sentence generalizable strategy summary |
| RB-F4 | Retrieval: top-k (`top_k`, default 3) strategy summaries fetched by embedding similarity to current task description |
| RB-F5 | Injection: retrieved strategies injected into `ContextBuilder` preamble before LLM call, with `[Reasoning Strategy]` prefix |
| RB-F6 | `[memory.reasoning]` config section with `enabled`, `extract_provider`, `distill_provider`, `top_k`, `store_limit` |
| RB-F7 | `store_limit` (default 1000): evict oldest strategies when limit reached (LRU by last-retrieval timestamp) |

### Non-Functional

| ID | Requirement |
|---|---|
| RB-NF1 | Self-judge + distillation run asynchronously after turn completion — must not add latency to the turn response |
| RB-NF2 | Strategy retrieval adds ≤ 5ms to context build time |
| RB-NF3 | Total injected strategy text ≤ 500 tokens (enforced by ContextBuilder token budget) |

---

## 3. Design

### 3.1 Config Schema

```toml
[memory.reasoning]
enabled = false                  # opt-in
extract_provider = "fast"        # fast model for self-judgment
distill_provider = "fast"        # mid-tier for distillation
top_k = 3                        # strategies injected per turn
store_limit = 1000               # max stored strategies (LRU eviction)
```

### 3.2 Data Model

```sql
CREATE TABLE reasoning_strategies (
    id           TEXT PRIMARY KEY,   -- UUID v4
    summary      TEXT NOT NULL,      -- distilled strategy (≤ 3 sentences)
    outcome      TEXT NOT NULL,      -- "success" | "failure"
    task_hint    TEXT NOT NULL,      -- short task description fingerprint
    created_at   INTEGER NOT NULL,
    last_used_at INTEGER NOT NULL,
    use_count    INTEGER NOT NULL DEFAULT 0
);
```

Qdrant collection `reasoning_strategies`: vector = embedding of `task_hint || summary`.

### 3.3 Self-Judge Prompt (extract_provider)

```
Given the following agent turn transcript, determine:
1. Did the agent successfully complete the user's request? (yes/no)
2. Extract the key reasoning steps the agent took.
3. Summarize the task in one sentence.

Respond in JSON: {"success": bool, "reasoning_chain": str, "task_hint": str}
```

### 3.4 Distillation Prompt (distill_provider)

```
Given this reasoning chain from a {success/failure} attempt:
{reasoning_chain}

Distill a generalizable strategy (≤ 3 sentences) that could help an agent
facing a similar task. Focus on the transferable principle, not the specific instance.
```

### 3.5 Context Injection

`ContextBuilder` fetches strategies before assembling the prompt:

```
[Reasoning Strategy — success]
When decomposing multi-file refactoring tasks, first map all call sites before
making any edits. This prevents cascading failures from missed dependencies.

[Reasoning Strategy — failure]
Avoid calling external APIs without checking rate limits first. Implement
exponential backoff before retry loops.
```

Token budget enforced by existing `ContextBudget` mechanism.

### 3.6 Integration Points

- `zeph-memory`: new `ReasoningMemory` struct, SQLite table, Qdrant collection
- `zeph-core/agent/context`: `ContextBuilder` gains `with_reasoning_strategies()` method
- `zeph-core/agent/loop`: post-turn hook calls `reasoning_memory.process_turn(transcript)`
- Config: `[memory.reasoning]` parsed into `ReasoningConfig` in existing `MemoryConfig`
- TUI: spinner `Distilling reasoning strategy…` during background distillation

---

## 4. Key Invariants

- **NEVER** run self-judge or distillation on the agent turn thread — async background only
- **NEVER** inject strategies that exceed the `ContextBudget` token allocation
- Strategy extraction must be graceful-fail: if `extract_provider` returns malformed JSON, skip silently and log a warning
- LRU eviction must not delete strategies with `use_count > 10` (high-value strategies are protected)

---

## 5. Acceptance Criteria

- [x] `reasoning_strategies` SQLite table created (migration 077); Qdrant collection `reasoning_strategies` provisioned on startup when `enabled = true` (implemented #3342, #3343)
- [x] After a completed turn, self-judge runs asynchronously fire-and-forget via three-stage pipeline (implemented #3342)
- [x] Strategy retrieved by embedding similarity; top-k injected into context preamble with `[Reasoning Strategy]` prefix (implemented #3343)
- [x] `store_limit` respected; LRU eviction with hot-row protection (`HOT_STRATEGY_USE_COUNT = 10`) (implemented #3343)
- [x] `enabled = false` produces zero side effects
- [x] Dedicated embed provider (`effective_embed_provider()`) passed to extraction pipeline, fixing 768 vs 1536 dimension mismatch (fixed #3382)
- [x] Self-judge evaluates only last `self_judge_window` messages (default 2) with `min_assistant_chars` guard (default 50) to prevent false Failure from multi-session context (fixed #3383)
- [x] Embed provider passed to `attach_reasoning_memory` for Qdrant dimension probe (fixed #3375, #3376)
- [x] `cargo nextest run -p zeph-memory` passes

## 6. Known Refinements (Post-Implementation)

| Issue | Fix | PR |
|---|---|---|
| Qdrant probe used primary router (excluded embed providers) → collection never created | Pass `build_memory_embed_provider()` into `attach_reasoning_memory`, fallback to primary when unset | #3375, #3376 |
| Extraction pipeline used primary routing provider → 768 vs 1536 dim mismatch on upsert | Use `SemanticMemory::effective_embed_provider()` in `process_reasoning_turn` | #3382 |
| Self-judge evaluated full conversation tail → false Failure from multi-session context noise | Evaluate only last `self_judge_window` messages; skip responses shorter than `min_assistant_chars` | #3383 |

---

## 6. Implementation Priority

**P2 — Medium priority.** Core read/write path is 2 weeks estimated complexity. MaTTS scaling variant (more compute per task for diverse experiences) is a separate follow-up sprint. Recommend: SQLite table + distillation pipeline first, Qdrant retrieval second, context injection third.

---

## 7. Related Issues

- Closes #3312
