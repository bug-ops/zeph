# Spec: Skills System

## Sources

### External
- SKILL.md format specification: https://agentskills.io/specification.md

### Internal
| File | Contents |
|---|---|
| `crates/zeph-skills/src/registry.rs` | `SkillRegistry`, hot-reload, `max_active_skills` |
| `crates/zeph-skills/src/trust_score.rs` | Wilson score, `posterior_weight`, `rerank` |
| `crates/zeph-skills/src/evolution.rs` | `SkillMetrics`, `SkillEvaluation`, self-improvement |
| `crates/zeph-core/src/agent/mod.rs` | `SkillState`, skill injection into system prompt |
| `crates/zeph-core/src/agent/feedback_detector.rs` | `FeedbackDetector`, signal attribution |

---

`crates/zeph-skills/` — SKILL.md format, registry, matching, hot-reload.

## SKILL.md Format

Skills are Markdown files following the agentskills.io specification:
- Frontmatter (YAML): `name`, `description`, `version`, `triggers`, `tools`, `env`
- Body: instructions injected into the system prompt when skill is active
- Tool definitions: optional `## Tools` section with tool specs

## SkillRegistry

```
SkillRegistry (Arc<RwLock<>>)
├── skills: HashMap<String, Skill>   — indexed by name
├── loaded_paths: HashMap<PathBuf, String>  — path → skill name
└── managed_dir: Option<PathBuf>     — auto-scan directory
```

- Thread-safe: always accessed via `Arc<RwLock<SkillRegistry>>`
- Hot-reloadable: file watcher (`notify` crate, 500ms debounce) triggers `reload_skill(path)`
- Reload must not block the agent loop — runs in background task, notifies via channel

## Skill Matching

Per-turn selection algorithm:

1. **BM25 + embedding hybrid** (if `hybrid_search = true`): BM25 score + cosine similarity, RRF fusion
2. **Pure embedding** (if hybrid disabled): cosine similarity only
3. **Keyword fallback**: substring match on `triggers` field

Constraints:
- `disambiguation_threshold`: if top skill score < threshold, inject nothing
- `max_active_skills`: hard cap on skills injected into the system prompt per turn
- Active skill names logged as `active_skill_names` for debugging

## Skill Injection

Active skills are injected into Block 3 of the system prompt (volatile section):
- Full skill body is included (up to `max_skill_body_bytes` limit)
- Tool definitions from skills are merged into the main tool catalog for the turn
- Skills can define `env` variables that are passed to the tool executor via `set_skill_env()`

## Self-Learning Integration

`FeedbackDetector` monitors responses for implicit quality signals:
- Positive: user confirms, thanks, or follows up productively
- Negative: user corrects, asks to redo, expresses frustration
- Wilson score: Bayesian lower-bound confidence interval on positive/total feedback
- Skills ranked by Wilson score; low-confidence skills demoted in selection
- Trust transitions: `Untrusted → Provisional → Trusted` based on accumulated feedback

## `load_skill` Tool

On-demand tool that fetches the full SKILL.md body for a named skill — allows agent to inspect skill details without injecting into every turn.

## Key Invariants

- `SkillRegistry` is always `Arc<RwLock<>>` — never cloned
- `max_active_skills` is a hard cap — never exceeded even if all skills match
- Hot-reload must not interrupt an in-progress turn
- Skills with `env` vars must call `set_skill_env()` on tool executor before tool execution
- `disambiguation_threshold` check runs before any skill injection
