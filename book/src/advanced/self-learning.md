# Self-Learning Skills

Zeph continuously improves its skills based on execution outcomes, user corrections, and provider performance. The self-learning system operates across four layers: failure classification, implicit feedback detection, Bayesian re-ranking, and hybrid search with EMA-based routing.

## Overview

When a skill fails or a user implicitly corrects the agent, Zeph records the signal, re-ranks affected skills, and — when failures cross a threshold — generates an improved skill version via LLM reflection.

```
User message
     │
     ▼
Skill matching (BM25 + cosine → RRF fusion)
     │
     ▼
Skill execution → SkillOutcome recorded
     │
     ├─ Success → Wilson score updated, EMA updated
     │
     └─ Failure → FailureKind classified
                       │
                       ├─ FeedbackDetector checks next user turn
                       │        └─ UserCorrection stored in SQLite + Qdrant
                       │
                       └─ repeated failures → LLM generates improved version
```

## Phase 1 — Failure Classification

Every skill invocation records a `SkillOutcome`. Tool failures now carry a `FailureKind` that distinguishes seven root causes:

| Variant | Meaning |
|---------|---------|
| `ExitNonzero` | The tool process exited with a non-zero exit code |
| `Timeout` | The tool call exceeded the configured timeout |
| `PermissionDenied` | Tool execution was blocked by the permission policy |
| `WrongApproach` | The skill used a command or method inappropriate for the task |
| `Partial` | The tool completed but produced incomplete or truncated output |
| `SyntaxError` | The generated command or script contained a syntax error |
| `Unknown` | Failure cause could not be classified from the error message |

The raw reason string is stored in the `outcome_detail` column (migration 018, `skill_outcomes` table) for later inspection and LLM-based improvement prompts.

### Rejecting a Skill

Use `/skill reject` to record an explicit user rejection and immediately trigger the improvement pipeline:

```
/skill reject <name> <reason>
```

Example:

```
/skill reject web-search "always uses the wrong search engine"
```

This is equivalent to `min_failures` consecutive failures — the improvement loop starts on the next agent cycle.

## Phase 2 — Implicit Feedback Detection

Zeph inspects each user turn for implicit corrections without requiring an explicit `/feedback` command. Two detection strategies are available, selected via `detector_mode`:

### Regex Detector (default)

`FeedbackDetector` uses pattern matching only — zero LLM calls.

**Detection signals:**

1. **Explicit rejection** (confidence 0.85) — phrases like "no", "wrong", "that's wrong", "that didn't work", "bad answer", "that's incorrect".
2. **Self-correction** — user corrects themselves (e.g., "I was wrong, the capital is Canberra"). Self-corrections are stored for analytics but do not penalize active skills.
3. **Alternative request** (confidence 0.70) — "instead use…", "try a different approach", "can you do it differently".
4. **Repetition** (confidence 0.75) — Jaccard token overlap > 0.8 against the last 3 user messages.

### Judge Detector (LLM-backed)

`JudgeDetector` uses an LLM call to classify borderline or missed cases. It is invoked only when regex confidence falls in the adaptive zone or regex returns no signal at all.

**How the adaptive zone works:**

| Regex result | Action |
|---|---|
| Confidence >= `judge_adaptive_high` (0.80) | Accepted without judge |
| Confidence in `[judge_adaptive_low, judge_adaptive_high)` | Judge invoked to confirm/override |
| Confidence < `judge_adaptive_low` (0.50) | Treated as "no correction" |
| No regex match | Judge invoked as fallback |

The judge call runs in a background `tokio::spawn` task and does not block the agent response loop. A sliding-window rate limiter caps judge calls at 5 per 60 seconds to control cost.

**Judge prompt design:**
- System prompt classifies user satisfaction into `explicit_rejection`, `alternative_request`, `repetition`, or `neutral`.
- User message content is XML-escaped to mitigate prompt injection via `</user_message>` tags.
- Response is parsed as structured JSON (`JudgeVerdict`) with confidence clamping to `[0.0, 1.0]`.

### Multi-Language Support

`FeedbackDetector` matches correction patterns across 7 languages:

| Language | Example rejection | Example alternative |
|----------|-------------------|---------------------|
| English | "that's wrong", "bad answer" | "try a different approach" |
| Russian | "неправильно", "неверно" | "попробуй по-другому" |
| Spanish | "eso esta mal", "incorrecto" | "intenta de otra manera" |
| German | "das ist falsch", "stimmt nicht" | "versuch es anders" |
| French | "c'est faux", "incorrect" | "essaie autrement" |
| Chinese | "错了", "不对" | "换个方法" |
| Japanese | "違います", "間違い" | "別の方法で" |

Each language uses **dual anchoring**: anchored patterns (`^`) for messages starting with the feedback phrase, and unanchored patterns for mid-sentence feedback. Confidence values are assigned per pattern: explicit rejections score 0.85, alternatives 0.70.

Mixed-language inputs are supported. CJK patterns use 2+ character minimums for unanchored matching to reduce false positives from substring matches. Unsupported languages (Korean, Arabic, etc.) produce no regex signal, causing every message to trigger a judge call (rate-limited to 5/min).

### Storage

Detected corrections are stored as `UserCorrection` records in:
- SQLite (`zeph_corrections` table) — persistent, queryable
- Qdrant (`zeph_corrections` collection) — vector-indexed for similarity recall

On each subsequent query, the top-3 most similar corrections (cosine similarity >= 0.75) are injected into the system prompt to steer the agent away from repeating the same mistake.

### Configuration

```toml
[skills.learning]
detector_mode = "regex"              # "regex" (default) or "judge"
judge_model = ""                     # Model for judge calls (empty = use primary provider)
judge_adaptive_low = 0.5            # Below this, regex "no correction" is trusted (default: 0.5)
judge_adaptive_high = 0.8           # At or above, regex result accepted without judge (default: 0.8)

[agent.learning]
correction_detection = true           # Enable FeedbackDetector (default: true)
correction_confidence_threshold = 0.7 # Confidence threshold to accept a candidate (default: 0.7)
correction_recall_limit = 3           # Max corrections injected into system prompt (default: 3)
correction_min_similarity = 0.75      # Minimum cosine similarity for correction recall (default: 0.75)
```

> Setting `detector_mode = "judge"` does not disable regex — regex always runs first. The judge is invoked only for borderline or missed cases, keeping LLM costs minimal.

## Phase 3 — Bayesian Re-Ranking and Trust Transitions

### Wilson Score Confidence Interval

Skill success/failure outcomes feed a Wilson score calculator that produces a lower-bound confidence interval. This replaces the raw success-rate sort used previously:

```
wilson_lower = (successes + z²/2) / (n + z²) - z * sqrt(n * p*(1-p) + z²/4) / (n + z²)
```

where `z = 1.96` (95% CI). Skills with few observations are naturally ranked lower until they accumulate evidence.

### Auto Promote / Demote

`check_trust_transition()` runs after each outcome and applies automatic trust level changes:

| Condition | Action |
|-----------|--------|
| Wilson score ≥ 0.85 and ≥ 10 evaluations | Promote to `trusted` |
| Wilson score < 0.40 and ≥ 5 evaluations | Demote to `quarantined` |
| Quarantined skill improves above 0.70 | Promote back to `verified` |

Trust transitions are logged via `tracing` and reflected immediately in `/skill stats` output.

### TUI Confidence Bars

The TUI dashboard (`--tui`) shows a per-skill confidence bar in the Skills panel:

- **Green** — Wilson score ≥ 0.75 (high confidence)
- **Yellow** — Wilson score 0.40–0.74 (moderate)
- **Red** — Wilson score < 0.40 (low confidence, at risk of demotion)

The bar width is proportional to the score and updates in real time as outcomes are recorded.

## Phase 4 — Hybrid Search and EMA Routing

### BM25 + Cosine Hybrid Search

Skill matching now combines two signals via Reciprocal Rank Fusion (RRF):

| Signal | Description |
|--------|-------------|
| BM25 | Term-frequency keyword match against skill names, descriptions, and trigger phrases |
| Cosine | Embedding similarity of the query against skill body vectors |

```
rrf_score(d) = 1/(k + rank_bm25(d)) + 1/(k + rank_cosine(d))     k = 60
```

The `cosine_weight` parameter scales the cosine component relative to BM25 before RRF:

```toml
[skills]
cosine_weight = 0.7    # Weight for cosine signal in fusion (default: 0.7)
hybrid_search = true   # Enable BM25+cosine fusion (default: true)
```

When `hybrid_search = false`, the previous cosine-only matching is used.

### EMA-Based Provider Routing

`EmaTracker` maintains an exponential moving average of response latency per provider. When `router_ema_enabled = true`, the router re-orders providers by EMA score every `router_reorder_interval` requests, preferring providers with consistently lower latency.

```toml
[llm]
router_ema_enabled = false      # Enable EMA-based provider reordering (default: false)
router_ema_alpha = 0.1          # EMA smoothing factor, 0.0–1.0 (default: 0.1)
router_reorder_interval = 10    # Re-order every N requests (default: 10)
```

A lower `router_ema_alpha` gives more weight to historical latency; a higher value tracks recent performance more aggressively.

### Skill Health in System Prompt

When `hybrid_search = true`, active skills include XML health attributes in the injected system prompt block:

```xml
<skill name="git" trust="trusted" reliability="91%" uses="47">
  ...skill body...
</skill>
```

These attributes let the LLM factor in skill reliability when choosing between overlapping skills.

## Complete Configuration Reference

```toml
[skills]
cosine_weight = 0.7    # Cosine signal weight in BM25+cosine fusion (default: 0.7)
hybrid_search = true   # Enable hybrid BM25+cosine skill matching (default: true)

[llm]
router_ema_enabled = false      # EMA-based provider latency routing (default: false)
router_ema_alpha = 0.1          # EMA smoothing factor (default: 0.1)
router_reorder_interval = 10    # Provider re-order interval in requests (default: 10)

[agent.learning]
correction_detection = true           # Implicit correction detection (default: true)
correction_confidence_threshold = 0.7 # Jaccard overlap threshold (default: 0.7)
correction_recall_limit = 3           # Corrections injected into system prompt (default: 3)
correction_min_similarity = 0.75      # Min cosine similarity for correction recall (default: 0.75)

[skills.learning]
enabled = true
auto_activate = false     # Require manual approval for new versions (default: false)
min_failures = 3          # Failures before triggering improvement
improve_threshold = 0.7   # Success rate below which improvement starts
rollback_threshold = 0.5  # Auto-rollback when success rate drops below this
min_evaluations = 5       # Minimum evaluations before rollback decision
max_versions = 10         # Max auto-generated versions per skill
cooldown_minutes = 60     # Cooldown between improvements for same skill
detector_mode = "regex"   # "regex" (default) or "judge"
judge_model = ""          # Model for judge calls (empty = primary provider)
judge_adaptive_low = 0.5  # Regex confidence floor for judge bypass (default: 0.5)
judge_adaptive_high = 0.8 # Regex confidence ceiling for judge bypass (default: 0.8)
```

## Feedback Command

The `/feedback` command records explicit user feedback about the agent's most recent response. Positive or neutral feedback stores a `user_approval` outcome; negative feedback stores `user_rejection`. Approval and rejection outcomes are excluded from Wilson score calculations — they are tracked for analytics only and do not dilute execution-based success rate metrics. Positive feedback also skips `generate_improved_skill()` to avoid unnecessary LLM calls when a skill is working correctly.

## Chat Commands

| Command | Description |
|---------|-------------|
| `/skill stats` | View execution metrics, Wilson scores, and trust levels per skill |
| `/skill versions` | List auto-generated versions |
| `/skill activate <id>` | Activate a specific version |
| `/skill approve <id>` | Approve a pending version |
| `/skill reset <name>` | Revert to original version |
| `/skill reject <name> <reason>` | Record user rejection and trigger improvement |
| `/feedback` | Provide explicit quality feedback (positive or negative) |

## Storage

| Store | Table / Collection | Contents |
|-------|--------------------|----------|
| SQLite | `skill_outcomes` | Per-invocation outcomes with `outcome_detail` (migration 018) |
| SQLite | `skill_versions` | LLM-generated skill versions |
| SQLite | `zeph_corrections` | Detected user corrections with metadata |
| Qdrant | `zeph_corrections` | Vector-indexed corrections for similarity recall |

## How Improvement Works

1. Failures accumulate against a skill, each tagged with a `FailureKind` and stored in `outcome_detail`.
2. When the failure count reaches `min_failures` and success rate drops below `improve_threshold`, Zeph prompts the LLM with the skill body, recent failure details, and any recalled corrections.
3. The LLM generates a new SKILL.md body. The new version is stored in `skill_versions` and either auto-activated or held pending approval depending on `auto_activate`.
4. The Wilson score and EMA metrics continue to accumulate on the new version. If performance drops below `rollback_threshold`, automatic rollback restores the previous version.

> Set `auto_activate = false` (default) to review LLM-generated improvements before they go live. Use `/skill versions` and `/skill approve <id>` to inspect and promote candidates manually.

## D2Skill: Step-Level Error Correction

D2Skill (Dynamic Dual-loop Skill learning) extends the improvement pipeline with step-level error correction. Instead of regenerating an entire skill body after failures, D2Skill identifies the specific step within a multi-step skill that failed and generates a targeted correction.

When a skill execution fails partway through a multi-step sequence, D2Skill records which step failed and why. On subsequent improvement cycles, only the failing step is regenerated — preserving working steps and reducing LLM cost.

### SkillOrchestra RL Routing Head

SkillOrchestra adds a reinforcement learning routing head on top of the skill matcher. When `rl_routing_enabled = true`, the RL head learns from execution outcomes to adjust skill selection probabilities, preferring skills that succeed for a given query type over time.

```toml
[skills]
rl_routing_enabled = true      # Enable RL-based skill routing (default: false)
```

The RL head uses a contextual bandit algorithm. Cold start is handled by falling back to the standard BM25+cosine matcher until sufficient observations accumulate.

Enable D2Skill in the learning config:

```toml
[skills.learning]
d2skill_enabled = true         # Enable step-level error correction (default: false)
```

## ARISE Trace Evolution

ARISE (Adaptive Reinforcement of Instruction-Skill Evolution) tracks execution traces — the sequence of tool calls and their outcomes during skill execution — and uses them to evolve skill instructions over time.

Key components:

- **STEM pattern-to-skill**: detects recurring tool-call patterns (e.g., "read file, then grep, then edit") across sessions and proposes new skills to codify them
- **ERL heuristics**: Exploration-Reinforcement-Learning heuristics that balance trying new skill variations against exploiting known-good ones

ARISE operates in the background and surfaces proposals via `/skill versions` for manual review.
