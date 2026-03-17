# Spec: Self-Learning

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
