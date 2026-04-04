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
