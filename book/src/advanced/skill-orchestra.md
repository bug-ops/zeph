# SkillOrchestra: RL-Based Skill Routing

SkillOrchestra adds a reinforcement learning routing head on top of the standard BM25+cosine skill matcher. It learns from execution outcomes to adjust skill selection probabilities, preferring skills that succeed for a given query type over time.

## How It Works

The standard skill matcher selects the top-K skills by semantic similarity. SkillOrchestra wraps this with a contextual bandit algorithm (LinUCB) that re-ranks candidates based on historical outcomes:

```
User query
    |
    v
BM25 + Cosine matcher --> top-K candidates
    |
    v
SkillOrchestra RL head --> re-ranked candidates
    |
    v
Top skill injected into prompt
```

After each skill execution, the outcome (success/failure) is fed back to the RL model as a reward signal. Over time, the model learns which skills work best for which types of queries, even when multiple skills have similar embeddings.

## Cold Start

When SkillOrchestra has insufficient observations for a query type, it falls back to the standard BM25+cosine ranking. The transition from cold-start to RL-guided routing is gradual — the RL head's confidence increases as observations accumulate, and its influence on the final ranking scales accordingly.

## Configuration

```toml
[skills]
rl_routing_enabled = true      # Enable RL-based skill routing (default: false)
```

SkillOrchestra requires `[skills.learning] enabled = true` to collect outcome data. Without the learning system, there are no reward signals to train on.

## RL Routing Configuration

The SkillOrchestra routing head is a linear layer that takes a query embedding as input and produces a score for each skill candidate. Scores are blended with cosine similarity via `rl_weight`. Weights are updated via REINFORCE after each observed outcome and persisted to SQLite every `rl_persist_interval` updates.

**Thompson Sampling / RL update cycle:**

1. At match time, cosine similarity candidates are re-ranked using the linear head's predicted scores.
2. The blend formula is: `final_score = (1 - rl_weight) * cosine + rl_weight * rl_score`.
3. After execution, the outcome (success = 1.0, failure = 0.0) is used as the REINFORCE reward to update the head weights.
4. For the first `rl_warmup_updates` weight updates, the RL score is not blended — the routing head observes outcomes but does not influence selection. This prevents cold-start bias.

Enable RL routing only after the agent has accumulated at least 50 turns of skill usage so the warmup phase completes quickly and the head has enough signal to learn meaningful routing patterns.

```toml
[skills]
rl_routing_enabled   = true   # Enable RL routing head (default: false)
rl_learning_rate     = 0.01   # REINFORCE weight update step size (default: 0.01)
rl_weight            = 0.3    # Blend: (1-rl_weight)*cosine + rl_weight*rl_score (default: 0.3)
rl_persist_interval  = 10     # Persist weights every N updates; 0 = every update (default: 10)
rl_warmup_updates    = 50     # Updates before RL score influences ranking (default: 50)
rl_embed_dim         = 768    # Must match embedding provider output dim; None → 1536 (default: null)
```

> [!IMPORTANT]
> `rl_embed_dim` must match the vector dimension produced by your embedding provider. Mismatches cause a dim mismatch error at startup and the routing head falls back to cosine-only ranking. For Ollama providers using `nomic-embed-text` or similar 768-dim models, set `rl_embed_dim = 768`. For OpenAI `text-embedding-3-small`, set `rl_embed_dim = 1536`.

## When to Enable

Enable SkillOrchestra when:

- You have **10+ skills** with overlapping descriptions that confuse the cosine matcher
- Skills with similar embeddings have **different success rates** for different query types
- You run Zeph over extended periods and want skill selection to improve automatically

Do not enable it for small skill sets (<5 skills) or short-lived sessions where the RL model cannot accumulate enough observations.

## Interaction with Other Systems

- **D2Skill**: D2Skill corrects individual steps within a skill; SkillOrchestra selects which skill to use in the first place. They operate at different levels and complement each other.
- **Wilson Score**: Wilson scores measure per-skill reliability. SkillOrchestra uses them as a feature in the bandit model alongside query-skill similarity and historical outcome patterns.
- **Hybrid Search**: SkillOrchestra operates after BM25+cosine fusion. It does not replace hybrid search — it re-ranks its output.

## Monitoring

Use `/skill stats` to see RL routing metrics alongside Wilson scores:

```
/skill stats
```

The output includes the RL exploration rate and per-skill selection counts when SkillOrchestra is active.

## Next Steps

- [Self-Learning Skills](self-learning.md) — the full learning pipeline
- [Skills](../concepts/skills.md) — how skill matching works
- [Enable Self-Learning Skills](../guides/self-learning.md) — setup guide
