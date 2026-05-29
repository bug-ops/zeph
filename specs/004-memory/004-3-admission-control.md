---
aliases:
  - Admission Control
  - A-MAC
  - Importance Scoring
tags:
  - sdd
  - spec
  - memory
  - admission
created: 2026-04-10
status: approved
related:
  - "[[004-memory/spec]]"
  - "[[004-1-architecture]]"
  - "[[004-2-compaction]]"
  - "[[004-4-embeddings]]"
---

# Spec: Memory Admission Control (A-MAC & Importance Scoring)

> [!info]
> Adaptive Memory Admission Control (A-MAC): five-factor importance scoring
> based on the A-MAC paper (arXiv:2603.04549), with optional goal-conditioned extension,
> admission gates, and graceful degradation.

## Overview

Not all messages should be stored in memory. A-MAC scores importance and decides
whether to admit messages based on future utility, factual confidence, semantic novelty,
recency, and content-type prior.

### Goal

Implement adaptive admission control that filters noise while preserving critical context.

---

## Functional Requirements

| ID | Requirement | Priority |
|----|------------|----------|
| FR-001 | WHEN remember() called, THE SYSTEM SHALL score message importance | must |
| FR-002 | WHEN score < admission_threshold, message rejected (returns None) | must |
| FR-003 | Scoring SHALL consider 5 core factors: future_utility, factual_confidence, semantic_novelty, temporal_recency, content_type_prior | must |
| FR-004 | OPTIONALLY extend with goal-conditioned utility factor when goal_conditioned_write enabled | should |

---

## Key Invariants

### Always
- Admission check returns `Result<Option<MessageId>>` — None means rejected, not error
- When [memory.admission] enabled=false, ALL messages admitted (pass-through)
- Scoring failure is fail-open — admit on any error
- A-MAC must use `effective_embed_provider()` for all embedding calls — never the primary conversational provider directly. This applies to both the admission `evaluate()` path and the A-MAC bootstrap fallback. `effective_embed_provider()` resolves: first `[[llm.providers]]` entry with `embed = true`, then first with `embedding_model`, then primary.
- All embedding call sites in semantic submodules (`summarization.rs`, `cross_session.rs`, `recall.rs`) must call `effective_embed_provider()`, not `provider` — this is enforced by `#3035` and `#3154`/`#3162`.

### Never
- Treat None from remember() as an error
- Use admission control as security gate (it's for noise filtering only)
- Call `provider.embed()` directly in any memory submodule — always use `effective_embed_provider()`

---

## Five-Factor Scoring Model (A-MAC)

The core A-MAC model uses five factors normalized to [0.0, 1.0] range:

| Factor | Default Weight | Calculation |
|--------|---------|-------------|
| `future_utility` | 0.30 | LLM-estimated reuse probability; defaults to 0.5 on fast path or failure |
| `factual_confidence` | 0.15 | Inverse hedging heuristic: high confidence → high score |
| `semantic_novelty` | 0.30 | 1.0 minus max similarity to top-3 neighbors; 1.0 when memory is empty |
| `temporal_recency` | 0.10 | Always 1.0 at write time (decay applied at recall, not admission) |
| `content_type_prior` | 0.15 | Prior based on message role (e.g., user/assistant/tool) |

**Composite score** = Σ (factor × weight), normalized so weights sum to 1.0. Range: [0.0, 1.0].

### Future Utility Factor

Evaluates whether a message is likely to be reused in future interactions:
- **Fast path**: heuristic score computed; if score ≥ threshold + fast_path_margin, LLM call skipped (fast admission)
- **Slow path**: LLM provider queries estimated reuse probability (provider specified by `admission_provider` config)
- **Failure**: defaults to 0.5 (neutral) on LLM error

### Optional Goal-Conditioned Extension (Feature #2408)

When `goal_conditioned_write = true`, an optional sixth factor can extend the model:

| Factor | Purpose |
|--------|---------|
| `goal_utility` | Cosine similarity between goal embedding and candidate memory |

This factor is applied only when a goal is active; zero when goal text is absent/trivial. If enabled, its weight is redistributed from `future_utility`.

---

## Config

```toml
[memory.admission]
enabled = false
threshold = 0.40
fast_path_margin = 0.15
admission_provider = ""  # falls back to primary provider if unset
goal_conditioned_write = false

[memory.admission.weights]
# Per-factor weights; normalized at runtime to sum to 1.0
future_utility = 0.30
factual_confidence = 0.15
semantic_novelty = 0.30
temporal_recency = 0.10
content_type_prior = 0.15
goal_utility = 0.0  # only non-zero when goal_conditioned_write is true
```

---

## Integration Points

- [[004-1-architecture]] — called in remember() method
- [[004-2-compaction]] — scores before compaction decision
- [[004-4-embeddings]] — uses embeddings for semantic novelty factor

---

## Historical Note

The A-MAC model was refined from an earlier six-factor design (recency, relevance, tool_use, unique_entities, length, frequency) to the current five-factor model based on the A-MAC paper (arXiv:2603.04549, tracked in issue #4141). The paper model emphasizes LLM-based future utility and semantic novelty (via embedding similarity) as primary signals, with temporal recency and content-type prior as supporting factors. This shift reflects advances in learned importance signals over hand-engineered entity and frequency heuristics.

---

## See Also

- [[004-memory/spec]] — Parent
- [[004-1-architecture]] — Core pipeline where admission is checked
- A-MAC paper: arXiv:2603.04549 (#4141)
