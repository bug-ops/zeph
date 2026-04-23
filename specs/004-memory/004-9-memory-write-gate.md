---
aliases:
  - MemReader Gate
  - Memory Write Quality Gate
  - Write-Side Admission
tags:
  - sdd
  - spec
  - memory
  - admission
  - experimental
created: 2026-04-19
status: draft
related:
  - "[[MOC-specs]]"
  - "[[constitution]]"
  - "[[004-memory/spec]]"
  - "[[004-3-admission-control]]"
  - "[[024-multi-model-design/spec]]"
---

# Spec: MemReader Write Quality Gate

> [!info]
> Quality-scoring gate that runs **before** any memory write (SQLite, Qdrant, graph).
> Complements `[[004-3-admission-control|A-MAC]]` (which scores message importance)
> with a **content-quality** scorer that rejects redundant, reference-incomplete, or
> self-contradictory entries. Rule-based MVP with optional LLM-assisted scoring via
> `quality_gate_provider`. Resolves GitHub issue
> [#3222](https://github.com/rabax/zeph/issues/3222).

## Sources

### External
- **MemReader: Write-Side Quality Control for Agent Memory** (research memo, 2026) — key insight: recall performance collapses faster from write pollution than from scoring-model drift
- **LongMemEval** (2025) — demonstrates that ~40% of retrieved noise comes from low-quality writes, not poor retrieval

### Internal
| File | Contents |
|---|---|
| `crates/zeph-memory/src/admission.rs` | Existing A-MAC importance scorer |
| `crates/zeph-memory/src/facade.rs` | `SemanticMemory::remember()` entry point |
| `crates/zeph-memory/src/router.rs` | Memory routing decisions |
| `crates/zeph-memory/src/types.rs` | `Message`, `MessagePart` definitions |

---

## 1. Overview

### Problem Statement

A-MAC filters on *importance* but not on *quality*. Three failure patterns observed in
`.local/testing` sessions over CI-560..CI-592:

1. **Redundant writes.** The same fact gets stored 3–5 times in different paraphrases,
   bloating Qdrant and poisoning MMR retrieval.
2. **Reference-incomplete writes.** Messages mentioning pronouns ("he said yes") or
   relative times ("yesterday") without a concrete referent are stored, then recalled
   as useless excerpts.
3. **Contradictory writes.** New entries assert values that contradict existing entries
   without superseding them, leaving the store in an ambiguous state.

A-MAC does not detect any of these — it scores based on recency, relevance, tool use,
entity density, and length.

### Goal

Add a **write quality gate** that runs after A-MAC admission and before persistence.
The gate produces a `QualityScore` from three signals (value, completeness, consistency)
and rejects writes below a configurable threshold. A rule-based implementation ships as
MVP; LLM-assisted scoring is opt-in via `quality_gate_provider`.

### Out of Scope

- Replacing or removing A-MAC (the gates compose: A-MAC first, quality second)
- Retrospective cleanup of existing low-quality entries (handled by `#[forgetting]`)
- Graph-edge admission (see `[[memory-apex-magma/spec]]`)
- Tool-output admission (always admitted; quality runs only on conversational writes)

---

## 2. User Stories

### US-001: High-quality memory retrieval
AS A Zeph user
I WANT memory saves to be high-quality and non-redundant
SO THAT memory retrieval surfaces relevant context rather than noise

**Acceptance criteria:**
```
GIVEN a session where the same fact has been stated multiple times in different phrasings
WHEN memory retrieval is performed several turns later
THEN only one representative entry for that fact is returned
AND the retrieved entry is semantically complete (no dangling pronouns or relative times)
```

### US-002: Signal-proportional write costs
AS AN operator
I WANT memory write costs to be proportional to information value, not conversation length
SO THAT embedding and Qdrant costs scale with signal, not volume

**Acceptance criteria:**
```
GIVEN a conversation with 100 turns, 60 of which are low-information or redundant
WHEN the quality gate is enabled with default weights
THEN fewer than 50 writes reach Qdrant
AND the operator can observe the rejection ratio via metrics without additional tooling
```

### US-003: Rejection rate alarm
AS AN operator monitoring a production session
I WANT a rejection rate alarm when the gate filters aggressively
SO THAT I can diagnose misconfigured thresholds

**Acceptance criteria:**
```
GIVEN rejection_rate_alarm_ratio = 0.35
  AND the rolling 100-write window contains 40 rejections
WHEN the next write is evaluated
THEN a WARN log entry is emitted containing the current rejection ratio and a context snapshot
AND the operator can act without inspecting raw metrics
```

---

## 3. Functional Requirements

| ID | Requirement | Priority |
|----|------------|----------|
| FR-001 | WHEN `SemanticMemory::remember()` is called AND A-MAC admits the message THE SYSTEM SHALL compute a `QualityScore` before any persistence write | must |
| FR-002 | `QualityScore` SHALL combine three sub-scores in [0,1]: `information_value`, `reference_completeness`, `contradiction_risk` | must |
| FR-003 | WHEN the combined score is below `[memory.quality_gate].threshold` THE SYSTEM SHALL reject the write (return `Ok(None)` like A-MAC) and emit a rejection metric | must |
| FR-004 | `information_value` SHALL be computed as `1.0 - max_cosine_similarity(candidate, recent_writes_window)` using the effective embedding provider | must |
| FR-005 | `reference_completeness` SHALL be computed as `1.0 - unresolved_reference_ratio`; rule set: unresolved pronouns with no referent in the last N messages, deictic time expressions with no absolute timestamp | must |
| FR-006 | `contradiction_risk` SHALL search existing graph edges for `(subject, predicate)` conflicts; any conflicting edge older than `contradiction_grace_seconds` raises the risk | must |
| FR-007 | WHEN `quality_gate_provider` is set THE SYSTEM SHALL additionally call that provider with a structured scoring prompt and blend the LLM score into the final score with weight `llm_weight` | should |
| FR-008 | WHEN the gate rejects a write THE SYSTEM SHALL record the rejection in `MetricsSnapshot.memory.quality_rejections{reason}` | must |
| FR-009 | WHEN `[memory.quality_gate]` is disabled THE SYSTEM SHALL behave as today (A-MAC only) | must |
| FR-010 | Scoring failure (embed error, LLM error) SHALL be fail-open — admit on any error | must |
| FR-011 | WHEN rejection rate (rolling 100-write window) exceeds `rejection_rate_alarm_ratio` THE SYSTEM SHALL log at `WARN` with context snapshot for operator review | should |

---

## 4. Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Performance | Rule-based scoring for a single candidate SHALL complete in < 20 ms at p95 (embedding lookup from cache; no LLM call in the default path) |
| NFR-002 | Performance | LLM-assisted scoring path SHALL respect the 500 ms `tokio::time::timeout`; on timeout the gate falls back to the rule score without surfacing an error to the caller |
| NFR-003 | Performance | Embedding calls for `information_value` SHALL reuse the `effective_embed_provider()` shared cache; no redundant embed round-trips per write |
| NFR-004 | Reliability | Scoring failures (embed error, LLM error, graph query error) SHALL be fail-open — the write is admitted, not rejected; an error is never propagated to the caller as a gate rejection |
| NFR-005 | Reliability | Feature flag `enabled = false` SHALL reproduce pre-gate behavior byte-for-byte (A-MAC only); the gate must not alter the write path even transiently |
| NFR-006 | Reliability | Gate rejection MUST return `Ok(None)` — the same contract as A-MAC — never `Err(...)` for a quality-based rejection |
| NFR-007 | Maintainability | Sub-score rules are independently replaceable: each scorer (information_value, reference_completeness, contradiction_risk) is a separate pure function testable without the full gate stack |
| NFR-008 | Maintainability | Rejection reasons are an enum with a fixed, documented variant set; adding a new reason requires explicit review (affects downstream dashboards and alarm logic) |
| NFR-009 | Observability | Prometheus counters SHALL be exported: `memory_quality_rejections_total{reason}`, `memory_quality_llm_timeouts_total`, `memory_quality_gate_evaluated_total` |
| NFR-010 | Observability | Rolling rejection rate SHALL be inspectable via the existing `MetricsSnapshot` without additional tooling |
| NFR-011 | Security | The gate is a noise-control mechanism only — it MUST NOT be used as a security or access-control filter; this distinction must be documented in module-level docs |
| NFR-012 | Security | No candidate message content is logged at WARN or above; rejection log entries contain only reason enum, score value, and session turn index |

---

## 5. Scoring Model

### Final Score

```
rule_score = w_v * information_value
           + w_c * reference_completeness
           + w_k * (1 - contradiction_risk)

final_score = rule_score                       if quality_gate_provider unset
            = (1 - llm_weight) * rule_score
              + llm_weight * llm_score          otherwise
```

Default weights: `w_v = 0.4`, `w_c = 0.3`, `w_k = 0.3`. `llm_weight = 0.5`.

### Sub-score Rules (MVP)

| Signal | Rule |
|---|---|
| `information_value` | 1.0 − max cosine with last `recent_window = 32` writes; 1.0 if store empty |
| `reference_completeness` | count(unresolved_pronouns) + count(relative_time_without_anchor); normalized by token count |
| `contradiction_risk` | 1.0 if ≥1 graph edge conflicts on `(subject, predicate)`; 0.5 if uncertain; 0.0 otherwise |

`unresolved_pronouns`: tokenized heuristic over `{he, she, they, it}` with no named
entity in ±3 sentences. `relative_time`: regex over `{yesterday, tomorrow, last week,
next …}` without a co-occurring absolute date token.

### LLM-Assisted (optional)

When `quality_gate_provider` is set, a single compact prompt returns JSON:

```json
{"information_value": 0.8, "reference_completeness": 0.9, "contradiction_risk": 0.1, "reason": "..."}
```

Latency budget 500 ms (`tokio::time::timeout`); on timeout, fall back to rule score.
Provider must be fast-tier per `[[024-multi-model-design/spec]]`.

---

## 6. Key Invariants

### Always (without asking)
- Quality gate runs **after** A-MAC admission — never replaces it
- Gate rejection returns `Ok(None)`, not an error (same contract as A-MAC)
- Scoring failures fail-open (admit) — never fail-closed (reject on error)
- All embedding calls use `effective_embed_provider()` (shared with A-MAC)
- LLM calls respect `llm_weight` and the 500 ms timeout
- Rejection reason is enumerable: `{redundant, incomplete_reference, contradiction, llm_low_confidence}` — never free-form
- Metrics are always updated, even when gate disabled (counter simply stays zero)

### Ask First
- Changing default weights (`w_v`, `w_c`, `w_k`, `llm_weight`)
- Raising `rejection_rate_alarm_ratio` above 0.5
- Changing the `recent_window` beyond 128 (embedding cost impact)
- Adding new rejection reasons (affects downstream metrics dashboards)

### Never
- Use quality gate as a security filter (it's for noise control, mirroring A-MAC)
- Block on the LLM scorer — 500 ms timeout is mandatory
- Persist partial state when the gate rejects (all-or-nothing write contract)
- Call `provider.embed()` directly — always via `effective_embed_provider()`

---

## 7. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|-------------------|
| Empty memory store | `information_value = 1.0`; gate admits if other signals pass |
| Candidate is identical to existing entry (cosine ≥ 0.99) | Rejected as `redundant`; counter increments |
| Pronoun-heavy short message (e.g., "yeah he will") | Rejected as `incomplete_reference` |
| Contradiction on a fact the new write is explicitly superseding | Supersedes flag on the new write → treated as `contradiction_risk = 0` (see APEX-MEM) |
| LLM provider offline | Fall back to rule score; log at `DEBUG` |
| Embed failure | Skip `information_value` (treat as 0.5); fail-open |
| Feature disabled | Gate skipped entirely; counter stays zero |
| All writes rejected for many turns | `WARN` when rolling ratio exceeds alarm; operator diagnostic |

---

## 8. Config

```toml
[memory.quality_gate]
enabled = true
threshold = 0.55
recent_window = 32
contradiction_grace_seconds = 300

# weights
information_value_weight = 0.4
reference_completeness_weight = 0.3
contradiction_weight = 0.3

# optional LLM scoring — references [[llm.providers]] by name
quality_gate_provider = ""        # empty = rule-based only
llm_weight = 0.5
llm_timeout_ms = 500

rejection_rate_alarm_ratio = 0.35
```

---

## 9. Success Criteria

- [ ] Rule-based gate rejects ≥ 80% of seeded redundant writes in unit test
- [ ] Rule-based gate rejects ≥ 60% of seeded pronoun-only writes
- [ ] `Ok(None)` contract parity with A-MAC (unit test for both gates)
- [ ] Rejection metric exported in Prometheus snapshot with per-reason labels
- [ ] LLM-assisted path respects 500 ms timeout under induced latency (integration test)
- [ ] Rolling rejection-rate alarm fires in property test (simulated 200-write burst)
- [ ] Disabled flag reproduces current behavior byte-for-byte
- [ ] Fail-open: induced embed error still admits the write (unit test)

---

## 10. Acceptance Criteria

```
GIVEN a candidate message "yeah he confirmed it"
  AND no entity "he" resolvable in ±3 messages
WHEN remember() is called with quality_gate enabled
THEN the gate rejects with reason = "incomplete_reference"
AND Ok(None) is returned
AND memory.quality_rejections{reason="incomplete_reference"} increments

GIVEN a candidate semantically identical to an entry from 2 writes ago
WHEN remember() is called
THEN information_value ≈ 0
AND the gate rejects with reason = "redundant"

GIVEN quality_gate_provider = "fast"
  AND the provider times out after 600 ms
WHEN remember() is called
THEN scoring falls back to the rule score (no error to caller)
AND the LLM timeout counter increments
```

---

## 11. Implementation Notes

- New module: `crates/zeph-memory/src/quality_gate.rs`
- Composition: `SemanticMemory::remember()` calls A-MAC first (existing), then `QualityGate::evaluate()` if A-MAC admitted
- `QualityGate` is a struct with `Arc`-shared config and optional `Arc<dyn LlmProvider>`
- Rejection reason is an enum serialized lowercase-snake; metrics labels match
- Tests: synthetic redundant/incomplete/contradiction fixtures in `testing.rs`
- `rejection_rate_alarm_ratio` rolling window implemented via existing `RollingCounter` utility (check `zeph-common` before adding new)
- `contradiction_risk` reuses graph-store query — does not mandate APEX-MEM, but integrates cleanly when APEX-MEM lands

---

## 12. Open Questions

> [!question]
> - **Multilingual `reference_completeness` support**: the pronoun heuristic and relative-time regex in FR-005 are English-only in the MVP. Timeline for extending to other languages (especially Russian, given Zeph's primary user base) needs to be decided before v1.0.0 — or explicitly documented as a known limitation.

---

## 13. See Also

- [[constitution]] — project principles
- [[004-memory/spec]] — memory pipeline
- [[004-3-admission-control]] — upstream A-MAC admission
- [[memory-apex-magma/spec]] — graph-side conflict handling (composes with `contradiction_risk`)
- [[024-multi-model-design/spec]] — provider tier guidance
- [[MOC-specs]] — all specifications
