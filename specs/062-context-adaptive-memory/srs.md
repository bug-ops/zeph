---
aliases:
  - CAM SRS
tags:
  - srs
  - context
  - memory
created: 2026-05-28
status: approved
related:
  - "[[062-context-adaptive-memory/spec]]"
  - "[[062-context-adaptive-memory/brd]]"
standard: "ISO/IEC/IEEE 29148:2018"
---

# Context-Adaptive Memory — Software Requirements Specification

## 1. Functional Requirements

Requirements use EARS notation: `WHEN <condition>, THE SYSTEM SHALL <action>`.

### 1.1 Fidelity Levels (AFM #4017)

**FR-001** — The system SHALL provide a `ContextFidelity` enum with exactly three variants: `Full`, `Compressed`, `Placeholder`.

**FR-002** — WHEN a message receives `Compressed` fidelity, the system SHALL replace its content with at most `compressed_max_tokens` tokens from the original content (primary path), OR with the value of `metadata.deferred_summary` when that field is `Some` (optimization path).

**FR-003** — WHEN a message receives `Placeholder` fidelity, the system SHALL replace its content with the string `[placeholder: role={role}, original_tokens={n}, importance={score:.2}]`.

**FR-004** — WHEN either `Compressed` or `Placeholder` fidelity is applied, the system SHALL clear `msg.parts` on the affected message.

**FR-005** — The system SHALL set `msg.metadata.fidelity_tag` to the resolved `ContextFidelity` value after rendering, for tracing and compaction input filtering.

### 1.2 Fidelity Scorer (AFM #4017)

**FR-006** — WHEN `fidelity_config.enabled == true` AND `memory_first == false`, the system SHALL run `FidelityScorer::score_and_apply()` on the context window AFTER `apply_prepared_context()` returns.

**FR-007** — The scorer SHALL compute a relevance score for each non-exempt message using up to four signals: `temporal`, `importance`, `semantic`, `plan`.

**FR-008** — WHEN `planned_tools` is empty, the system SHALL exclude the `w_plan` weight from both the numerator and denominator of the score computation.

**FR-009** — WHEN `query.len() < min_query_length`, the system SHALL set `semantic = 0.0` and exclude `w_semantic` from the active weight sum.

**FR-010** — The system SHALL normalize the composite score by the sum of active weights, ensuring all scores fall in `[0.0, 1.0]`.

**FR-011** — WHEN a message's normalized score is ≥ `full_threshold`, the system SHALL assign it `Full` fidelity.

**FR-012** — WHEN a message's normalized score is ≥ `compressed_threshold` AND < `full_threshold`, the system SHALL assign it `Compressed` fidelity.

**FR-013** — WHEN a message's normalized score is < `compressed_threshold`, the system SHALL assign it `Placeholder` fidelity.

**FR-014** — WHEN `fidelity_config.enabled == false` OR `fidelity_config` is absent, the system SHALL NOT perform any fidelity scoring or modification.

### 1.3 Exempt Message Set (AFM #4017)

**FR-015** — The system SHALL NEVER apply fidelity downgrade to the system prompt at index 0.

**FR-016** — The system SHALL NEVER apply fidelity downgrade to messages where `metadata.focus_pinned == true`.

**FR-017** — The system SHALL NEVER apply fidelity downgrade to correction messages (messages whose content starts with `CORRECTIONS_PREFIX`).

**FR-018** — The system SHALL NEVER apply fidelity downgrade to messages inserted by `apply_prepared_context()` in the current turn. The boundary is defined by `inserted_count` returned from that function.

**FR-019** — `inserted_count` SHALL be computed incrementally across all message insertion paths within `apply_prepared_context()`, not hardcoded as a constant.

### 1.4 Tool Pair Atomicity (AFM #4017)

**FR-020** — WHEN scoring a tool-use message, the system SHALL identify its matching tool-result message by `tool_call_id`.

**FR-021** — The system SHALL assign both the tool-use and its tool-result the MINIMUM fidelity level of their two individual scores.

**FR-022** — Tool-use and tool-result messages SHALL downgrade together or not at all.

### 1.5 Consecutive Same-Role Merge (AFM #4017)

**FR-023** — AFTER fidelity rendering, the system SHALL scan for adjacent Placeholder messages with the same role (excluding `Role::System`).

**FR-024** — WHEN two or more adjacent same-role Placeholder messages are found, the system SHALL merge them into a single message with content: `[placeholder: {count} messages, role={role}, total_tokens={sum}, avg_importance={avg:.2}]`.

**FR-025** — Full and Compressed messages SHALL NOT be merged even if they share the same role in adjacent positions.

### 1.6 Proactive Regrade Trigger (AgeMem #4016)

**FR-026** — WHEN `budget_used_ratio > regrade_threshold` AND `regraded_this_turn == false` AND `compaction_state.is_exhausted() == false`, the system SHALL run a proactive fidelity regrade on the current context window.

**FR-027** — WHEN a proactive regrade fires, the system SHALL set `regraded_this_turn = true` and emit a `tracing::info!` event with fidelity distribution counters.

**FR-028** — The system SHALL reset `regraded_this_turn = false` in `advance_turn()`.

**FR-029** — WHEN `server_compaction_active == true` AND `budget_used < 95%`, the system SHALL skip the proactive regrade.

**FR-030** — A proactive regrade SHALL NOT set `CompactedThisTurn`. Soft and hard compaction MAY still fire after a regrade.

### 1.7 Hard Compaction Placeholder Exclusion (AgeMem #4016)

**FR-031** — WHEN `compact_context()` builds its summarizer input, the system SHALL skip all messages where `metadata.fidelity_tag == Some(ContextFidelity::Placeholder)`.

**FR-032** — Messages with `metadata.fidelity_tag == Some(ContextFidelity::Compressed)` MAY be included in summarizer input.

### 1.8 Plan-Aware Hints (PAACE #4018)

**FR-033** — The system SHALL provide a `PlannedToolHint` struct with fields: `tool_name: String`, `keywords: Vec<String>`, `distance_from_current: u8` (capped at 5).

**FR-034** — WHEN `planned_next_tools` is non-empty, the system SHALL compute `plan_relevance` as a keyword overlap score between message content and the tool hints, weighted by inverse distance.

**FR-035** — `ContextAssemblyInput` SHALL include a `planned_next_tools: &[PlannedToolHint]` field. When no orchestration DAG is active, this field SHALL default to an empty slice.

---

## 2. Non-Functional Requirements

### 2.1 Performance

**NFR-P01** — Fidelity scoring for ≤ 500 messages SHALL complete in < 2ms wall time (measured via `context.fidelity.score` tracing span).

**NFR-P02** — WHEN the context window exceeds `max_scored_messages` (default 500) messages, the system SHALL score only the oldest `window_len - 250` messages; the newest 250 default to `Full`. This bounds scoring work at O(`max_scored_messages`).

**NFR-P03** — Fidelity scoring SHALL NOT make any I/O calls, LLM calls, or embedding lookups. It is a pure CPU-bound operation.

### 2.2 Backward Compatibility

**NFR-B01** — WHEN `context.fidelity.enabled = false` (default), the system SHALL produce identical behavior to the pre-CAM implementation. No existing session, test, or integration SHALL be affected.

**NFR-B02** — Adding `fidelity_tag: Option<ContextFidelity>` to `MessageMetadata` SHALL default to `None`. Existing serialized sessions SHALL deserialize without error (serde `default` attribute required).

### 2.3 Observability

**NFR-O01** — The system SHALL emit four tracing spans per turn when fidelity scoring is active: `context.fidelity.score`, `context.fidelity.apply`, `context.fidelity.regrade`, `context.fidelity.merge`.

**NFR-O02** — Each `context.fidelity.apply` span SHALL record: `full_count`, `compressed_count`, `placeholder_count`, `tokens_saved`.

### 2.4 Maintainability

**NFR-M01** — `FidelityScorer` SHALL have ≥ 10 unit tests covering the cases listed in AC-01 (spec.md §11).

**NFR-M02** — All public types in `zeph-common/src/fidelity.rs` and `zeph-context/src/fidelity.rs` SHALL have doc comments with `# Examples` sections per project API documentation rules.

### 2.5 Dependency Constraint

**NFR-D01** — `ContextFidelity` and `PlannedToolHint` SHALL be defined in `zeph-common` to avoid a dependency cycle between `zeph-context` and `zeph-llm`.

**NFR-D02** — `MessageMetadata.fidelity_tag` SHALL use `Option<ContextFidelity>` directly (not `Option<u8>`) since `zeph-llm` already depends on `zeph-common`.

---

## 3. Traceability

| FR / NFR | BRD Requirement | Spec Section |
|---|---|---|
| FR-001 through FR-013 | Token reduction; preserved structural history | spec.md §4, §6, §7 |
| FR-014 | No behavioral regression | spec.md §10 |
| FR-015 through FR-019 | Preserved structural history | spec.md §7.5 |
| FR-020 through FR-022 | No LLM API contract breakage | spec.md INV-03, §7.3 |
| FR-023 through FR-025 | No LLM API contract breakage | spec.md INV-04, §7.4 |
| FR-026 through FR-030 | Fewer context reloads | spec.md §5.1 |
| FR-031 through FR-032 | Quality of compaction summaries | spec.md INV-02 |
| FR-033 through FR-035 | Foundation for PAACE scoring | spec.md §4.2 |
| NFR-P01 through NFR-P03 | Latency constraint < 2ms | spec.md §6.3, AC-11 |
| NFR-B01 through NFR-B02 | No behavioral regression | spec.md §10 |
| NFR-O01 through NFR-O02 | Observability requirement | spec.md §9.7 |
| NFR-M01 through NFR-M02 | Quality gates | spec.md §11 |
| NFR-D01 through NFR-D02 | Clean architecture | spec.md §3 |
