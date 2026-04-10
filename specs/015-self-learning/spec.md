---
aliases:
  - Self-Learning
  - Feedback Integration
  - Wilson Score
tags:
  - sdd
  - spec
  - learning
  - feedback
created: 2026-04-08
status: approved
related:
  - "[[MOC-specs]]"
  - "[[005-skills/spec]]"
  - "[[025-classifiers/spec]]"
---

# Spec: Self-Learning

> [!info]
> FeedbackDetector (multi-language), Wilson score confidence intervals,
> trust model (Untrusted → Provisional → Trusted), SAGE RL cross-session reward.

## Sources

### External
- **RouteLLM** (ICML 2024) — Thompson Sampling + EMA latency routing for provider selection: https://arxiv.org/abs/2406.18665
- **Llama Guard** (Meta AI, 2023) — binary classifier pattern applied to feedback signal design: https://arxiv.org/abs/2312.06674

### Internal
| File | Contents |
|---|---|
| `crates/zeph-core/src/agent/feedback_detector.rs` | `FeedbackDetector`, `JudgeDetector`, `CorrectionSignal`, `CorrectionKind` |
| `crates/zeph-skills/src/trust_score.rs` | `posterior_weight` (Wilson score), `rerank` |
| `crates/zeph-skills/src/evolution.rs` | `SkillMetrics`, `SkillEvaluation`, self-improvement |
| `crates/zeph-skills/src/registry.rs` | BM25 index, hybrid search, `max_active_skills` |

---

`crates/zeph-skills/src/`, `crates/zeph-core/src/agent/feedback_detector.rs` — feedback detection, skill ranking, trust model.

## Feedback Detection (Two Paths)

### Fast Path: FeedbackDetector (~1ms, no LLM)

Four regex pattern groups, checked in priority order:

| Kind | Confidence | Pattern |
|---|---|---|
| `SelfCorrection` | 0.80 | Checked first — avoids "actually" false-positive in AlternativeRequest |
| `ExplicitRejection` | 0.85 | Direct user rejection |
| `AlternativeRequest` | 0.70 | User requests different approach |
| `Repetition` | 0.75 | Jaccard token overlap > 0.8 with last 3 messages |

- Returns `None` if confidence < `confidence_threshold` (default 0.6)
- `CorrectionSignal` = `(confidence: f32, kind: CorrectionKind, feedback_text: String)`

### Slow Path: JudgeDetector (LLM, rate-limited)

Invoked only when FeedbackDetector returns `None` OR confidence falls in borderline zone `[adaptive_low, adaptive_high)`:

- Rate limiter: max 5 LLM calls / 60s sliding window — checked synchronously before spawning
- Escape injection defense: `<` and `>` escaped to HTML entities before sending to LLM
- Verdict `confidence` clamped to `[0.0, 1.0]` post-deserialization
- Kind normalization: spaces → underscores, lowercase, case-insensitive enum match

## Wilson Score (Bayesian Skill Ranking)

Formula: **lower bound of 95% one-sided Wilson confidence interval**

```
α = successes + 1
β = failures + 1
n = α + β
mean = α / n
std_err = sqrt(mean × (1 - mean) / n)
wilson_lower = mean - 1.645 × std_err    ← clamped to [0.0, 1.0]
```

- With 0 data: w ≈ 0.47 (prior penalizes absence of evidence)
- With 100 successes: w > 0.9
- With 100 failures: w ≈ 0.0

**Never change**: α=+1, β=+1, z=1.645 — all skill rankings depend on this formula.

## Skill Re-ranking

```
score_i = cosine_weight × cosine_i + (1 - cosine_weight) × wilson_lower_i
```

- `cosine_weight ∈ [0, 1]`: 0 = trust-only, 1 = cosine-only
- Sorted descending; highest score first
- Applied after BM25+embedding hybrid search, before `max_active_skills` cut

## BM25 + RRF Hybrid Search

When `hybrid_search = true`:

- BM25 score: term frequency in skill description/triggers vs query
- Embedding score: cosine similarity of skill embedding vs query embedding
- **RRF fusion**: `score = Σ 1 / (k + rank_i)` where k=60
- RRF score × Wilson multiplier = final ranking input

## Trust Model

```
TrustLevel: Untrusted → Provisional → Trusted
```

Trust transitions are one-step-at-a-time (cannot jump Untrusted → Trusted):
- `Untrusted → Provisional`: N consecutive positive signals
- `Provisional → Trusted`: sustained positive signals over M turns
- `Trusted → Provisional`: 3 negative signals in a window
- `Provisional → Untrusted`: persistent negative signals

Trust level is passed via `set_effective_trust()` to `ToolExecutor` before each turn.

## Provider EMA Routing

Per-provider EMA (exponential moving average) latency:
- `ema_new = α × latency + (1 - α) × ema_old`, α = 0.1 (configurable)
- EMA is **per-provider**, not per-model — models under the same provider share the EMA
- Used by orchestrator alongside Thompson Sampling for model selection

## Key Invariants

- Wilson score formula (α+1, β+1, z=1.645) must never change — all rankings depend on it
- Confidence thresholds per kind are fixed: ExplicitRejection=0.85, SelfCorrection=0.80, Repetition=0.75, AlternativeRequest=0.70
- Self-correction checked first — prevents "actually" false-positive in AlternativeRequest
- Jaccard token overlap > 0.8 for repetition — changing threshold causes false positives/negatives
- JudgeDetector rate limiter (5/60s) is mandatory — no bypass
- JudgeDetector verdict confidence must be clamped [0.0, 1.0] — LLM can return out-of-range
- Trust transitions are one-step-at-a-time — no skipping levels
- `set_effective_trust()` must be called before each turn's tool execution

---

## Multi-Language FeedbackDetector

Issue #1424. `crates/zeph-core/src/agent/feedback_detector.rs`.

### Overview

`FeedbackDetector` detects implicit correction signals across 7 languages without LLM calls. All patterns are compiled once into a flat `Vec<(Regex, f32)>` per correction kind — no per-language routing. A single regex scan covers all languages simultaneously.

### Supported Languages

English, Russian, Spanish, German, French, Chinese (Simplified), Japanese.

### Dual Anchoring Strategy

Each language uses two pattern tiers:

| Tier | Anchor | Confidence |
|---|---|---|
| Anchored | `^` (message start) | Base confidence (e.g. 0.85 for `ExplicitRejection`) |
| Unanchored | Mid-sentence | Base confidence − 0.10 (more ambiguous position) |

Exception: English unanchored patterns retain base confidence because they are already multi-word, highly specific phrases ("don't do that", "that didn't work") that do not suffer from mid-sentence ambiguity.

### Pattern Registry (`LangPatterns`)

Compiled once into `LazyLock<LangPatterns>`:

```
rejection:       Vec<(Regex, f32)>  — per pattern, base confidence
alternative:     Vec<(Regex, f32)>
self_correction: Vec<(Regex, f32)>
```

Pattern matching: iterate the full flat list; take the **first** (highest-priority) match. Priority order within each kind is defined by list order, not confidence value.

### Known Limitations

- **CJK repetition gap**: `token_overlap()` uses whitespace tokenization; Chinese/Japanese text is not segmented by whitespace → CJK repetition detection falls through to JudgeDetector
- **CJK false-positive risk**: 2+ character patterns used for unanchored CJK to mitigate substring matches inside longer compounds
- **Unsupported languages** (Korean, Arabic, etc.): regex returns `None` → every message triggers a JudgeDetector call, rate-limited to 5/min

### Adding a New Language

1. Add anchored and unanchored patterns to `build_rejection_patterns()`, `build_alternative_patterns()`, `build_self_correction_patterns()`
2. Anchored pattern: base confidence; unanchored pattern: base confidence − 0.10 (except English)
3. Add test cases to the 137-test suite: positive (correct detection), negative (no false positive), edge cases (punctuation, capitalization)

### Key Invariants

- All patterns are compiled at program start via `LazyLock` — no runtime compilation
- English unanchored patterns are NOT reduced by 0.10 — only non-English unanchored patterns apply the reduction
- Pattern list order determines priority for the same correction kind — anchored patterns before unanchored
- CJK repetition falls through to JudgeDetector — this is intentional, not a bug
- NEVER route patterns by language before matching — the flat list approach is intentional for simplicity
- NEVER add patterns shorter than 2 characters for unanchored CJK to avoid false positives

---

## SAGE: RL Cross-Session Reward

`crates/zeph-skills/src/evolution.rs` and `crates/zeph-memory/src/semantic/mod.rs`. Implemented.

### Overview

SAGE (Self-Adaptive Generalization Engine) extends the skill trust model with
**cross-session reward aggregation**. A skill is promoted to `Trusted` only after
accumulating positive feedback across multiple distinct sessions, preventing
premature promotion from a single enthusiastic session.

### Cross-Session Rollout

`LearningConfig` gains two fields:

| Field | Type | Default | Notes |
|-------|------|---------|-------|
| `cross_session_rollout` | `bool` | `false` | Enable cross-session reward gating |
| `min_sessions_before_promote` | `u32` | `3` | Minimum distinct sessions required before `Provisional → Trusted` promotion |

When `cross_session_rollout = true`:
- `Provisional → Trusted` promotion is blocked until `distinct_session_count(skill_name) >= min_sessions_before_promote`
- Positive feedback within a single session counts as one session regardless of how many signals were received
- The session count is checked lazily at promotion time — not stored incrementally

### `distinct_session_count()`

`SqliteMemory::distinct_session_count(skill_name: &str) -> u32`:

Queries `user_corrections` (or the skill feedback table) to count distinct
`conversation_id` values where the skill received at least one positive feedback signal.

###: `git_hash` in `skill_trust` adds a `git_hash TEXT` column to the `skill_trust` table:

```sql
ALTER TABLE skill_trust ADD COLUMN git_hash TEXT;
```

`git_hash` stores the SHA-1 of the skill file at the time the trust record was last
updated. It is populated by `upsert_skill_trust_with_git_hash()` (see Skill Trust
Governance below). `NULL` means the provenance hash is unknown (legacy rows).

### Config

```toml
[skills.learning]
cross_session_rollout = false          # opt-in
min_sessions_before_promote = 3        # distinct sessions required for Trusted promotion
```

### Key Invariants

- `cross_session_rollout = false` restores the prior behavior (session-count gate inactive)
- `min_sessions_before_promote = 0` disables the gate (promote after any positive signal across sessions) — not recommended
- `distinct_session_count()` counts distinct `conversation_id` values, not turn count
- NEVER block `Trusted → Provisional` demotion on session count — demotion is always immediate
- NEVER short-circuit the session count gate based on total feedback count — sessions are the unit, not signals

---

## ARISE: Trace-Based Skill Improvement


After a successful multi-tool turn, `spawn_arise_trace_improvement()` fires a background LLM call that summarizes the tool sequence into an improved SKILL.md body. The new version is saved with `source = 'arise_trace'` at `quarantined` trust level — it never inherits the parent skill's trust.

### Config

```toml
[skills.learning]
arise_enabled = false
arise_min_tool_calls = 2      # minimum tool calls in turn to trigger
arise_trace_provider = ""     # provider for improvement LLM call; empty = primary
```

### Key Invariants

- ARISE-derived skills ALWAYS start at `quarantined` — never promote on creation
- Background LLM call must not block the agent turn
- `arise_min_tool_calls` prevents triggering on trivial single-tool sequences

---

## STEM: Pattern-to-Skill Conversion


`spawn_stem_detection()` logs every tool sequence to `skill_usage_log` after each turn. `find_recurring_patterns()` detects sequences meeting frequency and success-rate thresholds; qualifying patterns trigger a background LLM call to generate a SKILL.md candidate at `quarantined` trust level.

### SQLite adds `skill_usage_log` table.

### Config

```toml
[skills.learning]
stem_enabled = false
stem_min_occurrences = 3
stem_min_success_rate = 0.8
stem_retention_days = 30
```

### Key Invariants

- STEM-generated skills always start at `quarantined`
- `skill_usage_log` entries older than `stem_retention_days` are pruned automatically
- NEVER promote a STEM skill solely on pattern recurrence — trust governance applies normally

---

## ERL: Experiential Reflective Learning


`spawn_erl_reflection()` fires a background LLM call after each successful skill+tool turn to extract transferable heuristics. Heuristics are stored in `skill_heuristics` with Jaccard deduplication. At skill matching time, `build_erl_heuristics_prompt()` prepends a `## Learned Heuristics` section to skill context.

### SQLite adds `skill_heuristics` table.

### Config

```toml
[skills.learning]
erl_enabled = false
erl_max_heuristics_per_skill = 3
erl_min_confidence = 0.5
```

### Key Invariants

- Jaccard deduplication prevents semantically identical heuristics from accumulating
- `erl_max_heuristics_per_skill` caps storage per skill — oldest are evicted when exceeded
- Heuristics are injected at matching time only — never stored in SKILL.md directly

---

## learning.rs Split Into Focused Submodules


`crates/zeph-core/src/agent/learning/` was a single monolithic `learning.rs` file. split it into 11 focused submodules with no semantic changes:

| Module | Responsibility |
|--------|---------------|
| `mod.rs` | Public re-exports, `is_learning_enabled()`, `record_skill_outcomes()` |
| `arise.rs` | ARISE trace-based skill improvement spawning |
| `background.rs` | Background learning task coordination |
| `d2skill.rs` | D2Skill step-level error correction application |
| `erl.rs` | ERL experiential reflective learning spawning |
| `outcomes.rs` | Skill outcome recording and batch writes |
| `preferences.rs` | User preference inference from corrections |
| `rl.rs` | RL routing head reward updates |
| `skill_commands.rs` | `/skill` command handlers |
| `trust.rs` | Trust level update logic |
| `tests.rs` | Inline test suite (1357 lines) |

### Key Invariants

- All existing behavioral invariants from ARISE, STEM, ERL, D2Skill, and SkillOrchestra sections remain unchanged
- Module visibility is `pub(super)` for internal helpers — public API surface is unchanged
- `tests.rs` is `#[cfg(test)]` only — test module does not compile into production binary
