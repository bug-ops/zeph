---
aliases:
  - CAM Spec
  - Context-Adaptive Memory
  - AFM
  - AgeMem
  - PAACE
tags:
  - context
  - memory
  - compaction
  - fidelity
created: 2026-05-28
status: approved
related:
  - "[[021-zeph-context/spec]]"
  - "[[004-memory/spec]]"
  - "[[009-orchestration/spec]]"
  - "[[043-zeph-common/spec]]"
issues:
  - "#4016"
  - "#4017"
  - "#4018"
---

# Context-Adaptive Memory (CAM) — Specification

## 1. Purpose and Scope

Context-Adaptive Memory (CAM) addresses a fundamental limitation in Zeph's current context management: reactive compaction triggers only after the context window is nearly exhausted, causing context blowouts mid-task and discarding all non-recent messages uniformly.

CAM introduces three coordinated mechanisms across a single cohesive subsystem:

- **AFM (#4017)** — Adaptive Fidelity Management: three-level representation (`Full / Compressed / Placeholder`) replacing binary keep/discard.
- **AgeMem (#4016)** — Proactive age-triggered regrading: fires before the compaction threshold using heuristic budget monitoring.
- **PAACE (#4018)** — Plan-Aware Adaptive Context Engineering: plan-hint scoring bias from orchestration DAG lookahead (data structure only in MVP; wiring deferred).

### Implemented Beyond MVP (v0.21–v0.22)

The following items were originally listed as out of scope but have since been implemented:

- **Per-message embedding-based semantic scoring** — `semantic_scoring_provider` field in `FidelityConfig` enables LLM-embed scoring alongside keyword heuristics. Concurrent pre-pass via `buffer_unordered` bounded by `embed_concurrency`. Input size capped by `max_embed_input_tokens`.
- **LLM-assisted compression for `Compressed` rendering** — `compress_provider` field enables LLM-compressed rendering with 30s timeout. Result cached in `msg.metadata.deferred_summary`. Input size capped by `max_compress_input_tokens`.
- **Orchestration DAG live wiring for PAACE** — PAACE lookahead wired from DAG into `ContextAssemblyInput.planned_next_tools` (commit #4633).
- **Fidelity state persistence to SQLite** — `fidelity_tag` column persisted in the `messages` table (migration 093); floor invariant maintained across turns (commit #4615).
- **Lookahead BFS guard** — BFS computation is skipped when fidelity scoring is disabled to avoid wasted work (commit #4641).

### Remaining Out of Scope

- RL-based trigger threshold learning
- Dynamic weight adaptation

---

## 2. System Invariants

These constraints are **NEVER** to be violated. Violation requires an explicit architectural decision reviewed by the team lead.

| ID | Invariant |
|---|---|
| INV-01 | Fidelity scoring MUST run AFTER `apply_prepared_context()` returns. Never score before all insertions, sanitization, trimming, and token recomputation are final. |
| INV-02 | Placeholder messages MUST NOT be included in hard compaction summarizer input. They contain no semantic content. |
| INV-03 | A tool-use message and its matching tool-result message MUST downgrade together to the same fidelity level, or not at all. |
| INV-04 | Consecutive same-role Placeholder messages MUST be merged into a single merged placeholder before returning the context window to the LLM. |
| INV-05 | Fidelity scores MUST be normalized by the sum of active weights (not a fixed constant). |
| INV-06 | `regraded_this_turn` MUST be checked before triggering a proactive regrade within a single turn. |
| INV-07 | System prompt (index 0) MUST always be exempt from fidelity downgrade. |
| INV-08 | Messages with `focus_pinned == true` MUST be exempt from fidelity downgrade. |
| INV-09 | Correction messages (content starts with `CORRECTIONS_PREFIX`) MUST be exempt from fidelity downgrade. |
| INV-10 | Freshly injected memory context messages (inserted by `apply_prepared_context`) MUST be exempt from fidelity downgrade within the same turn. |
| INV-11 | Fidelity scoring MUST be skipped entirely when `memory_first == true`. |
| INV-12 | Both Compressed and Placeholder rendering MUST clear `msg.parts` (no orphaned structured parts). |

---

## 3. Subsystem Boundaries

### Modified Crates

| Crate | Module | Change |
|---|---|---|
| `zeph-common` | `fidelity.rs` (new) | `ContextFidelity` enum, `PlannedToolHint` struct |
| `zeph-context` | `fidelity.rs` (new) | `FidelityScorer`, `FidelityConfig`, scoring logic, `apply_fidelity_to_messages()` |
| `zeph-context` | `manager.rs` | `should_proactively_regrade()` + `regraded_this_turn: bool` field |
| `zeph-context` | `input.rs` | Add `planned_next_tools` and `fidelity_config` to `ContextAssemblyInput` |
| `zeph-agent-context` | `service.rs` | Wire fidelity scoring after `apply_prepared_context()`, gated on `!memory_first` |
| `zeph-agent-context` | `summarization/scheduling.rs` | Proactive regrade trigger call site |
| `zeph-agent-context` | `summarization/compaction.rs` | Skip Placeholder messages in summarizer input |
| `zeph-llm` | `MessageMetadata` | Add `fidelity_tag: Option<ContextFidelity>` |
| `zeph-config` | config types | `[context.fidelity]` section |

### Read-Only Crates

- `zeph-orchestration` — DAG read only (lookahead in deferred phase)
- `zeph-memory` — no changes (`CompressionLevel` remains independent)

### Dependency Note

`zeph-llm` already depends on `zeph-common` (`zeph-common.workspace = true`), so `Option<ContextFidelity>` is used directly in `MessageMetadata` — no raw `u8` indirection required.

---

## 4. Data Model

### 4.1 ContextFidelity (zeph-common/src/fidelity.rs)

```rust
/// Fidelity level assigned to a message in the context window.
///
/// Determines how a historical message is rendered before sending to the LLM.
/// Assigned by `FidelityScorer` based on relevance signals; stored in
/// `MessageMetadata.fidelity_tag` for debug tracing and compaction filtering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(u8)]
pub enum ContextFidelity {
    /// Original message content, unchanged.
    #[default]
    Full = 0,
    /// Content truncated to `compressed_max_tokens` tokens (or replaced by
    /// `deferred_summary` when available).
    Compressed = 1,
    /// Content replaced by a compact placeholder tag; no semantic content
    /// survives.
    Placeholder = 2,
}
```

### 4.2 PlannedToolHint (zeph-common/src/fidelity.rs)

```rust
/// Hint about an upcoming tool call derived from the orchestration DAG.
///
/// Used by `FidelityScorer` to bias relevance scores toward messages that
/// contain context useful for the next planned operations.
#[derive(Debug, Clone)]
pub struct PlannedToolHint {
    /// Name of the planned tool.
    pub tool_name: String,
    /// Keywords extracted from the tool's planned arguments (best-effort).
    pub keywords: Vec<String>,
    /// Steps until this tool is scheduled. 1 = immediately next, capped at 5.
    pub distance_from_current: u8,
}
```

### 4.3 FidelityConfig (zeph-context/src/fidelity.rs)

```rust
/// Configuration for the heuristic fidelity scorer.
///
/// All weight fields must be positive. Weights are normalized at runtime by
/// the sum of active weights (INV-05).
#[derive(Debug, Clone, serde::Deserialize)]
pub struct FidelityConfig {
    /// Master switch. When false, no fidelity scoring occurs.
    pub enabled: bool,
    /// Cosine/keyword semantic relevance weight.
    pub w_semantic: f32,
    /// Recency weight.
    pub w_temporal: f32,
    /// Role-based importance weight.
    pub w_importance: f32,
    /// Plan-hint relevance weight (active only when `planned_tools` non-empty).
    pub w_plan: f32,
    /// Score threshold above which a message retains Full fidelity.
    pub full_threshold: f32,
    /// Score threshold above which a message is Compressed (not Placeholder).
    pub compressed_threshold: f32,
    /// Maximum tokens kept when rendering a Compressed message.
    pub compressed_max_tokens: usize,
    /// Budget ratio at which AgeMem triggers a proactive regrade.
    pub regrade_threshold: f32,
    /// Minimum query length for semantic signal to be active.
    pub min_query_length: usize,
    /// Maximum number of messages scored per turn (performance cap).
    pub max_scored_messages: usize,
    /// Number of newest messages always exempted from scoring (tail guard).
    pub exempt_tail_messages: usize,
    /// BFS lookahead depth for PAACE plan-hint extraction.
    pub lookahead_depth: u8,
    /// Optional provider name for embedding-based semantic scoring.
    /// When set, embeds each non-exempt message and scores via cosine similarity.
    /// Replaces keyword-overlap heuristic for the w_semantic signal.
    pub semantic_scoring_provider: Option<String>,
    /// Optional provider name for LLM-assisted compression.
    /// When set, calls the LLM to produce a compressed summary instead of truncating.
    /// Result is cached in `msg.metadata.deferred_summary`.
    pub compress_provider: Option<String>,
    /// Maximum number of concurrent embed requests in the embedding pre-pass.
    /// Default: 32. Zero is clamped to 1.
    pub embed_concurrency: usize,
    /// Maximum input token count per embed call. Content is truncated at a char
    /// boundary to approximately `max_embed_input_tokens * 4` bytes before embedding.
    /// `None` = no limit.
    pub max_embed_input_tokens: Option<usize>,
    /// Maximum input token count fed to the compress_provider LLM call.
    /// Content exceeding this is truncated before the LLM compress call.
    /// `None` = no limit (truncation falls back to `compressed_max_tokens`).
    pub max_compress_input_tokens: Option<usize>,
}
```

**Note**: The `w_keyword` alias used in some early versions of config documentation is
deprecated. Use `w_semantic` directly.

### 4.4 FidelityScore (internal, zeph-context/src/fidelity.rs)

Internal struct, not exposed outside `zeph-context`:

```rust
struct FidelityScore {
    /// Normalized composite score in [0.0, 1.0].
    score: f32,
    /// Resolved fidelity level after threshold comparison.
    level: ContextFidelity,
    /// Original token count (before rendering).
    original_tokens: u32,
}
```

---

## 5. Data Flow

```
User message arrives
        |
        v
ContextService::prepare_context()   [zeph-agent-context/service.rs]
  │
  ├─ 1. Remove stale injected messages (existing)
  │
  ├─ 2. Build ContextAssemblyInput
  │      ├─ existing fields
  │      ├─ NEW: planned_next_tools: &[PlannedToolHint]   (empty slice if no DAG)
  │      └─ NEW: fidelity_config: Option<&FidelityConfig> (None if disabled)
  │
  ├─ 3. ContextAssembler::gather(&input) -> PreparedContext (existing)
  │
  ├─ 4. apply_prepared_context() (existing)           ← returns (ContextDelta, usize)
  │      ├─ inserts memory messages at position 1      │
  │      ├─ sanitizes each via sanitize_memory_message │
  │      ├─ runs trim_messages_to_budget()             │
  │      ├─ runs credential scrubbing                  │
  │      └─ calls recompute_prompt_tokens()            │
  │                                                    └─ inserted_count (computed
  │                                                       incrementally, not hardcoded)
  │
  └─ 5. [NEW] Fidelity scoring (AFTER apply_prepared_context)
         │
         ├─ Guard: skip if memory_first == true         (INV-11)
         ├─ Guard: skip if fidelity_config is None
         │
         └─ FidelityScorer::score_and_apply(
               messages: &mut [Message],
               query: &str,
               planned_tools: &[PlannedToolHint],
               config: &FidelityConfig,
               tc: &dyn TokenCounting,
               inserted_count: usize,          ← exempt range [1..1+inserted_count]
             )
               │
               ├─ a. Build exempt set (INV-07 through INV-10)
               │
               ├─ b. Score each non-exempt message
               │      ├─ temporal = 1.0 - distance_from_end / max_dist
               │      ├─ importance = role_weight(role)
               │      │   [System=1.0, User=0.8, Assistant=0.6, ToolResult=0.4]
               │      ├─ semantic = keyword_overlap(content, query)
               │      │   (set 0.0 when query.len() < min_query_length — INV-05/D7)
               │      └─ plan = plan_relevance(content, planned_tools)
               │              (0.0 when hints empty)
               │
               ├─ c. Normalize by active weight sum (INV-05)
               │
               ├─ d. Resolve fidelity level from thresholds
               │
               ├─ e. Apply tool pair atomicity (INV-03)
               │      └─ Assign MINIMUM fidelity of each tool-use/tool-result pair
               │
               ├─ f. Apply fidelity rendering (INV-12)
               │      ├─ Compressed: truncate to compressed_max_tokens (primary)
               │      │              OR replace with deferred_summary (if Some)
               │      │              THEN clear msg.parts
               │      └─ Placeholder: replace content with placeholder tag
               │                      THEN clear msg.parts
               │                      Token count: tc.count_tokens() on rendered string
               │
               ├─ g. Consecutive same-role Placeholder merge (INV-04)
               │
               └─ h. recompute_prompt_tokens()

        Return ContextDelta (existing)
```

### 5.1 AgeMem Proactive Regrade Trigger

```
ContextService::maybe_compact()   [called from agent loop]
  │
  ├─ Guard 1: if regraded_this_turn → skip          (INV-06)
  ├─ Guard 2: if compaction_state.is_exhausted() → skip
  ├─ Guard 3: if server_compaction_active && budget_used < 95% → skip
  │
  ├─ Trigger: budget_used_ratio > regrade_threshold (default 0.6)
  │           task_horizon_estimate = 1.0 (constant in MVP, see §7)
  │
  ├─ Action: re-run FidelityScorer on current window
  │          set regraded_this_turn = true
  │          recompute_prompt_tokens()
  │          emit tracing::info! with fidelity distribution
  │
  └─ advance_turn(): regraded_this_turn = false
```

---

## 6. Scoring Rules

### 6.1 Weight Normalization (INV-05)

```
active_weights = [w_semantic, w_temporal, w_importance]
if !planned_tools.is_empty():
    active_weights.push(w_plan)
if query.len() < min_query_length:
    remove w_semantic from active_weights

raw_score = Σ(weight_i × signal_i for each active signal)
normalized_score = raw_score / Σ(active_weights)
```

Scores always range `[0.0, 1.0]` regardless of which signals are active.

### 6.2 Role Weights

| Role | Weight |
|---|---|
| System | 1.0 |
| User | 0.8 |
| Assistant | 0.6 |
| ToolResult | 0.4 |

### 6.3 Performance Invariant

Heuristic scoring MUST complete in `<2ms` for windows up to `max_scored_messages` (default 500) messages. For windows exceeding `max_scored_messages`, score only the oldest `window_len - 250` messages; the newest 250 default to `Full`. This caps O(N) work at `max_scored_messages` regardless of window size.

---

## 7. Fidelity Application Rules

### 7.1 Compressed Rendering

1. **Primary path**: truncate `msg.content` to first `compressed_max_tokens` tokens using `tc.count_tokens()`.
2. **Optimization path**: if `msg.metadata.deferred_summary` is `Some(summary)`, replace content with `summary` instead of truncating.
3. Clear `msg.parts` (INV-12).
4. Set `msg.metadata.fidelity_tag = Some(ContextFidelity::Compressed)`.

### 7.2 Placeholder Rendering

1. Replace `msg.content` with `[placeholder: role={role}, original_tokens={n}, importance={score:.2}]`.
2. Clear `msg.parts` (INV-12).
3. Set `msg.metadata.fidelity_tag = Some(ContextFidelity::Placeholder)`.
4. Token count: call `tc.count_tokens()` on the rendered string (not a constant).

### 7.3 Tool Pair Atomicity (INV-03)

Identify tool-use/tool-result pairs by matching `tool_call_id`. Both messages in a pair receive the **minimum** fidelity of their two individual scores. This is an O(N) backward scan. Both downgrade together or neither does.

### 7.4 Consecutive Same-Role Placeholder Merge (INV-04)

After fidelity application, scan for adjacent same-role Placeholder messages (excluding `Role::System`). Merge them into a single message:

```
[placeholder: {count} messages, role={role}, total_tokens={sum}, avg_importance={avg:.2}]
```

This pass applies ONLY to Placeholder messages. Full and Compressed messages retain their individual identity even if consecutive same-role.

### 7.5 Exempt Message Set

Never downgraded (INV-07 through INV-10):

1. System prompt at index 0.
2. Messages with `metadata.focus_pinned == true`.
3. Correction messages (content starts with `CORRECTIONS_PREFIX`).
4. Messages at indices `1..1+inserted_count` (freshly injected memory context).

---

## 8. Configuration

### 8.1 Config Section ([context.fidelity])

```toml
[context.fidelity]
enabled = false                 # off by default
w_semantic = 0.3
w_temporal = 0.3
w_importance = 0.2
w_plan = 0.2
full_threshold = 0.7
compressed_threshold = 0.3
compressed_max_tokens = 50      # tokens kept for Compressed rendering (truncation path)
regrade_threshold = 0.6         # AgeMem proactive trigger budget ratio
min_query_length = 8            # below this, semantic signal is zeroed
max_scored_messages = 500       # performance cap
exempt_tail_messages = 0        # newest N messages always exempt from scoring
lookahead_depth = 3             # BFS depth for PAACE plan-hint extraction

# Phase 2-C: LLM-compressed rendering (optional)
# compress_provider = "fast"   # named [[llm.providers]] entry; empty = truncation only
# max_compress_input_tokens = 4096  # cap input before LLM compress call

# Phase 2-D: Embedding-based semantic scoring (optional)
# semantic_scoring_provider = "embed"  # named [[llm.providers]] embed-capable entry
# embed_concurrency = 32               # max concurrent embed requests
# max_embed_input_tokens = 512         # cap input per embed call
```

### 8.2 Tuning Note

`compressed_max_tokens = 50` may be aggressive for tool-result messages that commonly exceed 1000 tokens. This default is deliberately conservative; adjustment based on live testing is expected. When `compress_provider` is set, the LLM produces a semantic summary instead of truncation, which is more accurate but adds latency.

### 8.3 Fidelity Persistence (Phase 2-B)

`fidelity_tag` values are persisted in the `messages` SQLite table via DB migration 093. At session resume, persisted fidelity levels are loaded and used as a floor: a message's fidelity can only descend (Full → Compressed → Placeholder), never ascend, across turns. This prevents context blowouts when resuming long sessions.

---

## 9. Integration Points

### 9.1 ContextAssemblyInput (zeph-context/src/input.rs)

```rust
pub planned_next_tools: &'a [PlannedToolHint],
pub fidelity_config: Option<&'a FidelityConfig>,
```

Default (empty): `planned_next_tools = &[]`, `fidelity_config = None`.

### 9.2 ContextManager (zeph-context/src/manager.rs)

```rust
pub(crate) regraded_this_turn: bool   // reset in advance_turn()

pub fn should_proactively_regrade(&self, cached_tokens: u64) -> bool
```

`advance_turn()` must set `self.regraded_this_turn = false`.

### 9.3 apply_prepared_context Return Value

```rust
async fn apply_prepared_context(...) -> (ContextDelta, usize)
//                                                    ^^^^^
//                                          inserted_count — computed
//                                          incrementally across all
//                                          insertion paths (graph_facts,
//                                          doc_rag, corrections, recall,
//                                          cross_session, summaries,
//                                          persona, trajectory, tree,
//                                          reasoning, code_context,
//                                          session_digest)
```

The count MUST be computed incrementally per insertion, not hardcoded as a constant.

### 9.4 CompactionState (unchanged)

No new variants. Regrade uses `regraded_this_turn: bool`, not the compaction FSM. This keeps the state machine clean.

### 9.5 compact_context (zeph-agent-context/summarization/compaction.rs)

The message selection loop building summarization input MUST skip messages where `metadata.fidelity_tag == Some(ContextFidelity::Placeholder)`. These messages carry no semantic content (INV-02). Compressed messages (`fidelity_tag == Some(Compressed)`) MAY be summarized.

### 9.6 MessageMetadata (zeph-llm)

```rust
pub fidelity_tag: Option<ContextFidelity>
```

Used for: debug tracing, compaction input filtering. NOT used for rendering decisions at the LLM layer.

### 9.7 Tracing Spans

| Span | Location | Captures |
|---|---|---|
| `context.fidelity.score` | `FidelityScorer::score_and_apply` | `{message_count, query_len}` |
| `context.fidelity.apply` | Rendering pass | `{full_count, compressed_count, placeholder_count, tokens_saved}` |
| `context.fidelity.regrade` | Proactive regrade trigger | `{budget_ratio, fidelity_distribution}` |
| `context.fidelity.merge` | Same-role merge pass | `{merged_count}` |

All spans follow the `<crate_short>.<subsystem>.<operation>` naming convention (per continuous-improvement spec).

### 9.8 TUI

A spinner MUST be displayed during fidelity scoring (per TUI rule: any background operation must have a visible system status indicator). Message: `Scoring context fidelity…`

---

## 10. MVP Scope vs. Deferred

### v0.21 MVP (implement)

| Feature | Issue | Deliverable |
|---|---|---|
| `ContextFidelity` enum | #4017 | 3 variants in `zeph-common/src/fidelity.rs` |
| `PlannedToolHint` struct | #4018 | Data struct in `zeph-common/src/fidelity.rs` |
| `FidelityConfig` | #4017 | Config struct in `zeph-context/src/fidelity.rs` |
| `FidelityScorer` | #4017 | Heuristic scorer with weight normalization |
| Tool pair atomic downgrade | #4017 | Paired scoring in `FidelityScorer` (INV-03) |
| Consecutive same-role merger | #4017 | Post-fidelity pass (INV-04) |
| Fidelity-aware assembly | #4017 | `score_and_apply()` after `apply_prepared_context()` (INV-01) |
| Proactive regrade trigger | #4016 | `should_proactively_regrade()` with `regraded_this_turn` guard (INV-06) |
| Hard compaction Placeholder exclusion | #4016 | Skip Placeholder in summarizer input (INV-02) |
| Config section | all | `[context.fidelity]` with all weights, thresholds, feature gate |
| Short query fallback | #4017 | `min_query_length` gate on semantic signal |
| Tracing instrumentation | all | 4 spans per §9.7 |
| `fidelity_tag` in MessageMetadata | #4017 | `Option<ContextFidelity>` field |
| TUI spinner | all | Spinner during scoring |

### Implemented (Originally Deferred)

| Feature | Status | Commit |
|---|---|---|
| Embedding-based scoring | ✓ Implemented via `semantic_scoring_provider` | #4626 |
| LLM-compressed fidelity | ✓ Implemented via `compress_provider`, 30s timeout | #4626 |
| Orchestration DAG live wiring | ✓ PAACE lookahead wired from DAG into `ContextAssemblyInput` | #4633 |
| Fidelity state persistence to SQLite | ✓ `fidelity_tag` column in `messages` table, migration 093 | #4615 |
| Concurrent embed pre-pass | ✓ `buffer_unordered` with `embed_concurrency` cap | #4634 |
| Binary search budget fit + batch fidelity updates | ✓ Replaced linear halving with binary search; fidelity tag writes batched | #4624 |

### Remaining Deferred

| Feature | Reason | Expected Phase |
|---|---|---|
| RL-based trigger threshold | No training infrastructure | post-v1.0 |
| Dynamic weight adaptation | Post-v1.0 | post-v1.0 |

---

## 11. Acceptance Criteria

| AC | Criterion | Verifiable By |
|---|---|---|
| AC-01 | `cargo nextest run -p zeph-context -E 'test(fidelity)'` passes with ≥ 10 test cases covering: empty window, all-exempt window, tool pair atomicity, same-role merge, score normalization, short query fallback, MemoryFirst bypass | Unit tests |
| AC-02 | Fidelity scoring does not run before `apply_prepared_context()` completes | Code inspection (call site ordering) |
| AC-03 | Score for any message is always in `[0.0, 1.0]` regardless of active signal subset | Unit test: property test with all combinations of empty query, empty tool hints, zero weights |
| AC-04 | Tool-use message and its tool-result always share the same fidelity level | Unit test: construct paired messages with divergent raw scores, verify min(a, b) applied |
| AC-05 | No consecutive same-role Placeholder messages in the final window | Unit test: input with 5 consecutive assistant messages all scoring below threshold |
| AC-06 | `compact_context` input never contains Placeholder-tagged messages | Unit test: seed window with Placeholder messages, assert none in summarizer input |
| AC-07 | `regraded_this_turn` resets to `false` after `advance_turn()` | Unit test |
| AC-08 | Proactive regrade does not fire twice in the same turn | Unit test: call `maybe_compact()` twice at 70% budget, assert regrade fires once |
| AC-09 | Fidelity scoring skipped when `memory_first == true` | Unit test |
| AC-10 | `enabled = false` in config disables all fidelity scoring without panics | Integration test with default config |
| AC-11 | Scoring ≤ 500 messages completes in < 2ms (measured via tracing span) | Benchmark in `zeph-bench` |
| AC-12 | `inserted_count` includes all message types inserted by `apply_prepared_context` (not hardcoded) | Code inspection + unit test with mock insertion counting |

---

## 12. Open Questions

All open questions from the architect review are resolved. No remaining open questions.

| Question | Resolution |
|---|---|
| Dependency cycle for ContextFidelity | Placed in `zeph-common` (both `zeph-llm` and `zeph-context` already depend on it) |
| Token count for Placeholder | `tc.count_tokens()` on rendered string, not a constant |
| task_horizon_estimate source | Always `1.0` in MVP; RL pipeline deferred |
| MemoryFirst interaction | Skip scoring entirely when `memory_first == true` |
| Short query handling | Set `w_semantic = 0.0`, exclude from active_weight_sum |
| CompactionState changes | None — regrade uses a simple `bool` field |
| Deferred summary availability | Truncation is primary path; deferred_summary is optimization |
| `focus_pinned` vs. `pinned` | Use existing `metadata.focus_pinned` field |
| `compressed_max_tokens` for tool results | Post-merge tuning concern; default 50 is conservative |
| `inserted_count` computation | Incremental across all insertion paths in `apply_prepared_context` |
