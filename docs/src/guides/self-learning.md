# Enable Self-Learning Skills

This guide walks you through enabling and tuning Zeph's self-learning system so that skills automatically improve based on execution outcomes and user corrections.

For a full technical reference of the underlying mechanisms, see [Self-Learning Skills](../advanced/self-learning.md).

## Prerequisites

- Zeph installed and configured with at least one LLM provider
- Qdrant running locally (required for correction recall)
- At least one skill installed

## Step 1 — Enable Core Learning

Add the following to your `config/default.toml`:

```toml
[skills.learning]
enabled = true
auto_activate = false   # review LLM-generated improvements before they go live
min_failures = 3
improve_threshold = 0.7
```

With `auto_activate = false`, new skill versions are generated but held for your approval. Run `/skill versions` to review them and `/skill approve <id>` to promote one.

## Step 2 — Enable Implicit Feedback Detection

`FeedbackDetector` watches each user turn for implicit corrections — phrases like "that's wrong", "try again", or significant topic shifts. Detected corrections are stored and recalled automatically.

```toml
[agent.learning]
correction_detection = true
correction_confidence_threshold = 0.7  # tune sensitivity (lower = more corrections captured)
correction_recall_limit = 3
correction_min_similarity = 0.75
```

Corrections are stored in both SQLite and the `zeph_corrections` Qdrant collection. The top-3 most similar corrections are injected into the system prompt on relevant queries.

## Step 3 — Switch to Hybrid Skill Matching

BM25+cosine hybrid matching improves recall for skills with distinctive trigger keywords while keeping semantic matching for paraphrased queries.

```toml
[skills]
hybrid_search = true
cosine_weight = 0.7   # reduce to 0.5 to give BM25 more weight
```

When hybrid search is enabled, the system prompt includes skill health attributes (`trust`, `wilson`, `outcomes`) so the LLM can factor in reliability.

## Step 4 — Enable EMA Routing (Multi-Provider Setups)

If you run multiple providers via `provider = "orchestrator"` or `provider = "router"`, EMA routing continuously reorders providers by latency:

```toml
[llm]
router_ema_enabled = true
router_ema_alpha = 0.1       # lower = more weight on historical latency
router_reorder_interval = 10 # re-evaluate every 10 requests
```

## Monitoring

Use these in-session commands to monitor the system:

```
/skill stats       — Wilson scores, trust levels, outcome counts per skill
/skill versions    — list pending and approved LLM-generated versions
```

The TUI dashboard (`zeph --tui`) shows real-time confidence bars:

- **Green** bar — Wilson score ≥ 0.75
- **Yellow** — 0.40–0.74
- **Red** — below 0.40 (at risk of automatic demotion)

## Manually Triggering Improvement

If a skill is clearly wrong, reject it immediately instead of waiting for failures to accumulate:

```
/skill reject <name> <reason>
```

For example:

```
/skill reject docker "generates docker run commands without the -it flag for interactive shells"
```

This triggers the LLM improvement pipeline on the next agent cycle.

## Recommended Starting Configuration

```toml
[skills]
hybrid_search = true
cosine_weight = 0.7

[skills.learning]
enabled = true
auto_activate = false
min_failures = 3
improve_threshold = 0.7
rollback_threshold = 0.5
min_evaluations = 5
max_versions = 10
cooldown_minutes = 60

[agent.learning]
correction_detection = true
correction_confidence_threshold = 0.7
correction_recall_limit = 3
correction_min_similarity = 0.75
```

Keep `auto_activate = false` until you have enough history to trust the LLM-generated improvements.
