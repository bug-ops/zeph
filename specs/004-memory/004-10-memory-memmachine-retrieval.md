---
aliases:
  - MemMachine
  - Retrieval-Depth-First Memory
tags:
  - sdd
  - spec
  - memory
  - retrieval
  - research
created: 2026-04-24
status: implemented
related:
  - "[[MOC-specs]]"
  - "[[constitution]]"
  - "[[004-memory/spec]]"
  - "[[004-4-embeddings]]"
  - "[[024-multi-model-design/spec]]"
---

# Spec: MemMachine — Retrieval-Depth-First Personalized Memory

> Research analysis of MemMachine (arXiv:2604.04853). Resolves GitHub issues
> [#3325](https://github.com/bug-ops/zeph/issues/3325).

## Sources

### External
- **MemMachine: Ground-Truth-Preserving Personalized Memory** (arXiv:2604.04853)
- Benchmark: LongMemEvalS (93% accuracy reported)

### Internal
| File | Contents |
|---|---|
| `crates/zeph-memory/src/semantic/` | SemanticMemory orchestrator, MMR reranking |
| `crates/zeph-memory/src/retrieval.rs` | ANN search, candidate fetch |
| `crates/zeph-memory/src/config.rs` | Memory config structs |
| `crates/zeph-core/src/agent/context/` | ContextBuilder, memory injection |

---

## 1. Overview

### Problem Statement

Zeph's memory subsystem currently applies most optimization effort at **ingestion time** (summarization, chunking, graph extraction). MemMachine's empirical findings demonstrate that retrieval-stage improvements yield 11× higher accuracy gains than ingestion-stage improvements (9.5% vs 0.8% on LongMemEvalS), indicating a systematic under-investment in the retrieval path.

### Key Research Findings

| Optimization | Accuracy Gain |
|---|---|
| Retrieval depth tuning | +4.2% |
| Context formatting | +2.0% |
| Search prompt design | +1.8% |
| Query bias correction | +1.4% |
| Sentence chunking (ingestion) | +0.8% |

**Conclusion**: retrieval-stage improvements dominate. Current Zeph architecture is inverted — it invests heavily in ingestion (summarization pipeline) while retrieval uses fixed depth and generic prompts.

### Three-Layer Memory Model

1. **Short-term** — in-context window (already implemented)
2. **Long-term episodic** — full conversation episodes preserved verbatim alongside summaries
3. **Profile memory** — distilled user model updated incrementally

---

## 2. Requirements

### Functional

| ID | Requirement |
|---|---|
| MM-F1 | `[memory.retrieval]` config section with `depth` field (number of ANN candidates fetched before MMR/reranking, default 20) |
| MM-F2 | Configurable search prompt templates for memory queries (`search_prompt_template` field) |
| MM-F3 | Query bias correction: detect first-person queries vs. topic queries and apply separate embedding adjustment coefficients |
| MM-F4 | Episode preservation: store raw conversation episodes alongside existing summaries (non-destructive) |
| MM-F5 | Context formatting: structured output format for injected memory snippets (timestamp, source type, relevance score) |

### Non-Functional

| ID | Requirement |
|---|---|
| MM-NF1 | Retrieval depth increase from 10→20 must not increase p95 retrieval latency by more than 30% |
| MM-NF2 | Episode storage must not duplicate data already present in SQLite episode table |
| MM-NF3 | Query bias correction must add < 2ms overhead per query |

---

## 3. Design

### 3.1 Config Schema

```toml
[memory.retrieval]
depth = 0                           # ANN candidate count; 0 = legacy recall_limit * 2
search_prompt_template = ""         # query-side embedding template with {query} placeholder
query_bias_correction = true        # first-person vs topic detection
query_bias_profile_weight = 0.25    # blend weight for profile centroid shift
context_format = "structured"       # "structured" | "plain"

[memory.episodes]
preserve_raw = true                 # store verbatim episodes alongside summaries
```

> [!note] Implementation details for MM-F3 (query bias correction)
> First-person queries are shifted toward the user's profile centroid embedding before vector
> search. Centroid cached in a TTL-bounded `RwLock<Option<CachedCentroid>>` (default TTL 300 s).
> Computation failure is non-sticky: falls through to previous cache or no-op.
> Tracing spans: `memory.query_bias.apply` and `memory.query_bias.centroid` with structured
> debug events for bias applied/skipped, centroid computed, and centroid cache hits (#3379).

### 3.2 Query Bias Correction

Classify query intent at retrieval time using a lightweight heuristic (regex + token prefix check, no LLM call):

- **First-person query** (`I`, `me`, `my`, `we`): bias embedding toward profile memory vectors
- **Topic query** (noun-phrase dominant): bias toward episodic store

Implementation: weighted linear combination of raw query embedding and stored profile centroid.

### 3.3 Retrieval Depth

Current `SemanticMemory::search()` fetches a fixed number of candidates. Expose `retrieval_depth` as a runtime parameter resolved from config, replacing the hardcoded constant.

### 3.4 Context Formatting

Structured injection format:

```
[Memory | episodic | 2026-03-15 | relevance: 0.87]
User mentioned preference for concise code responses and dislikes verbose explanations.
```

---

## 4. Key Invariants

- **NEVER** delete raw episodes once stored — append-only for episodes
- **NEVER** apply query bias correction when `query_bias_correction = false` in config
- Search prompt template substitution must be injection-safe (no LLM-controlled template execution)

---

## 5. Acceptance Criteria

- [x] `depth` config field (`0` = legacy fallback to `recall_limit * 2`) implemented; `search_prompt_template` with `{query}` placeholder; `context_format` `structured`/`plain` switching (MM-F1/F2/F5 — implemented #3340, #3353)
- [x] Query bias correction: profile centroid fetched and blended with `query_bias_profile_weight`; TTL cache prevents redundant centroid computation (MM-F3 — implemented #3341, #3371, tracing #3379)
- [x] Episode preservation unconditional (data-integrity invariant): eviction sweep excludes episode-bound message IDs (MM-F4 — implemented #3341)
- [x] Structured context format renders in CLI and TUI
- [x] `cargo nextest run -p zeph-memory` passes with no regressions

---

## 6. Implementation Priority

**P2 — Medium priority.** Retrieval depth and search prompt improvements are low-risk, high-return. Query bias correction and episode preservation are medium complexity. Recommend implementing in order: depth config → search prompt template → context formatting → query bias correction → episode preservation.

---

## 7. Related Issues

- Closes #3325
