---
aliases:
  - Skills System
  - SKILL.md
  - SkillRegistry
tags:
  - sdd
  - spec
  - skills
  - agents
  - contract
created: 2026-04-08
status: approved
related:
  - "[[MOC-specs]]"
  - "[[001-system-invariants/spec#7. Skill Matching Contract]]"
  - "[[015-self-learning/spec]]"
  - "[[032-handoff-skill-system/spec]]"
---

# Spec: Skills System

> [!info]
> SKILL.md format specification, registry, hot-reload with notify crate and 500ms debounce,
> matching algorithm, skill injection into system prompt, trust governance via Wilson score.

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
- `disambiguation_threshold`: if top skill score < threshold, inject nothing. Default is **0.20** — avoids injecting near-irrelevant skills on almost every turn
- `min_injection_score`: minimum score a skill must achieve to be injected even when it clears the disambiguation threshold. Default 0.20, acts as a secondary quality gate independent of disambiguation
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

## Skills Matching Config

```toml
[skills]
disambiguation_threshold = 0.20   # skip injection when top score below this
min_injection_score = 0.20        # secondary quality gate for injection
max_active_skills = 3             # hard cap on skills injected per turn
two_stage_matching = false        # category-first coarse selection
confusability_threshold = 0.0     # 0.0 disables confusability reporting
```

---

## `load_skill` Tool

On-demand tool that fetches the full SKILL.md body for a named skill — allows agent to inspect skill details without injecting into every turn.

## Key Invariants

- `SkillRegistry` is always `Arc<RwLock<>>` — never cloned
- `max_active_skills` is a hard cap — never exceeded even if all skills match
- Hot-reload must not interrupt an in-progress turn
- Skills with `env` vars must call `set_skill_env()` on tool executor before tool execution
- `disambiguation_threshold` check runs before any skill injection; default is 0.20
- `min_injection_score` check is a secondary gate applied after disambiguation — both thresholds must be cleared for injection; default is 0.20
- NEVER inject a skill that fails the `min_injection_score` gate even if it clears `disambiguation_threshold`

---

## Dedicated Embedding Provider

Issue #2225. Skills embedding is decoupled from the active conversational provider.

`Agent` holds a dedicated `embedding_provider: AnyProvider` resolved once at bootstrap:
1. Prefers an entry in `[[llm.providers]]` with `embed = true`
2. Falls back to first entry with `embedding_model`
3. Falls back to primary provider

All 7 embedding call sites (skill matching, tool schema filter, MCP registry, semantic cache, plan cache, etc.) use `embedding_provider`. Switching active provider via `/provider switch` does not affect embeddings.

When active provider ≠ embedding provider, an info message is emitted to the user.

### Key Invariants

- `embedding_provider` is resolved once at bootstrap — never re-resolved per turn
- `/provider switch` MUST NOT change `embedding_provider`
- All embedding call sites must use `agent.embedding_provider`, not `agent.provider`
- NEVER fall back silently — if no embed-capable provider exists, log a warning

---

## FaultCategory Wiring

Issues #2207, #2224. Skill evolution uses typed `FaultCategory` signals, not string heuristics.

`From<ToolErrorCategory> for FailureKind` mapping:
- `PolicyBlocked` / `ToolNotFound` → `WrongApproach`
- `Timeout` → `Timeout`
- `InvalidParameters` / `TypeMismatch` → `SyntaxError`
- infrastructure errors → `Unknown`

`FaultCategory` enum path is wired in both `native.rs` and `legacy.rs` to ensure precise skill evolution signals in all execution paths.

### Key Invariants

- NEVER use string matching on error messages for `FailureKind` classification — use `ToolErrorCategory`
- Both `native.rs` and `legacy.rs` must wire `FaultCategory` — single-path wiring is incomplete

---

## Bundled Skill Security Scanning

Issue #2272. Bundled skills with security-awareness text do not produce false-positive `WARN`.

`build_registry()` checks the `.bundled` marker on a skill before emitting security scan warnings:
- `.bundled` skills with security text → `DEBUG` (vetted, suppressed)
- User-installed skills with security text → `WARN` (user-visible)

`managed_dir` is always included in `build_registry()` scan paths, even when `skills.paths` is customized.

### Key Invariants

- NEVER emit `WARN` for vetted bundled skills — only `DEBUG`
- `managed_dir` must always be scanned regardless of `skills.paths` customization

---

## Skill Trust Governance

`crates/zeph-skills/src/trust_score.rs` and `crates/zeph-skills/src/scanner.rs`. Implemented.

### Source URL and Git Hash Provenance

`SkillMeta` gains two provenance fields:

| Field | Type | Notes |
|-------|------|-------|
| `source_url` | `Option<String>` | URL from which the skill was downloaded or the marketplace entry |
| `git_hash` | `Option<String>` | SHA-1 of the skill file at load time |

These fields are populated when a skill is loaded from a file and committed via
`upsert_skill_trust_with_git_hash()`. They are stored in the `skill_trust` table
(migration 047 adds `git_hash TEXT`; `source_url` was added in an earlier migration).

### ScannerConfig

```toml
[skills.scanner]
injection_patterns = []              # additional regex patterns for injection detection
capability_escalation_check = true   # check for unexpected capability escalation
```

`ScannerConfig` controls the skill security scanner:
- `injection_patterns`: user-defined patterns added to the default injection detection regex list
- `capability_escalation_check`: when `true`, `check_capability_escalation()` is called on every skill load

### `check_capability_escalation()`

`check_capability_escalation(skill: &Skill, registry: &SkillRegistry)`:

Compares the tool and env declarations in the loaded skill against the currently
registered skill with the same name. If the loaded version requests capabilities
(tools, env keys, network access) not present in the existing version, a `WARN`
is emitted and the skill is quarantined for user review.

Escalation is defined as: new `tools` entries or new `env` keys not present in the
current registered version.

### `upsert_skill_trust_with_git_hash()`

`upsert_skill_trust_with_git_hash(skill_name, trust_level, git_hash)`:

Writes or updates the `skill_trust` row with the current `git_hash`. This is the
only write path for trust records that includes provenance. The older
`upsert_skill_trust()` without `git_hash` is retained for legacy call sites but
emits a `DEBUG` log noting absent provenance.

### Key Invariants

- `source_url` and `git_hash` are provenance metadata only — they do not affect skill matching or injection
- `check_capability_escalation()` is called at load time when `capability_escalation_check = true` — never at inference time
- Escalation detection compares **names** (tool IDs, env keys) — not capability semantics
- A skill with no prior registered version cannot trigger an escalation warning (no baseline to compare against)
- `git_hash` in `skill_trust` is `NULL` for legacy rows — never treat `NULL` as evidence of tampering
- NEVER auto-approve a skill that fails escalation check — always require explicit user action
- NEVER strip `source_url` from `SkillMeta` when writing to `skill_trust` — provenance must survive round-trips

---

## Skill Category System


Optional `category` field in SKILL.md frontmatter for grouping. All 26 bundled skills annotated (`web`, `data`, `dev`, `system`).

### Two-Stage Category-First Matching

When `two_stage_matching = true`: coarse category centroid selection followed by fine-grained within-category matching. Singleton-category skills fall back to the uncategorised pool.

### Confusability Report

`SkillMatcher::confusability_report()` — O(n²) pairwise cosine similarity with `spawn_blocking` offload. Lists skill pairs above `confusability_threshold`. Exposed via `/skills confusability` command.

### Config

```toml
[skills]
two_stage_matching = false
confusability_threshold = 0.0   # 0.0 disables confusability reporting
```

### Key Invariants

- `category` is optional — uncategorised skills are always in the matching pool
- `two_stage_matching` applies to matching only — skill injection, trust, and governance are unaffected
- Confusability report is O(n²) — NEVER compute it on the hot path; use `spawn_blocking`
- Bundled skills provisioned before the `.bundled` marker system are re-provisioned on upgrade to restore `category` without overwriting user-modified skills

---

## D2Skill: Step-Level Error Correction


D2Skill adds step-level error correction to skill execution. When a tool call within a skill-driven turn fails, the system captures the error context and fires a background LLM call to generate a corrected step variant. The correction is stored in `skill_step_corrections` and applied on the next occurrence of the same step pattern.

### Storage

`skill_step_corrections` table stores `(skill_name, step_hash, correction_body, confidence)`. `step_hash` is a BLAKE3 hash of the original step description + error category.

### Config

```toml
[skills]
d2skill_enabled = false
d2skill_correction_provider = ""   # provider for correction LLM call; empty = primary
d2skill_min_confidence = 0.6       # minimum confidence to apply a stored correction
```

### Key Invariants

- Corrections are applied lazily at step execution time — never retroactively
- `d2skill_enabled = false` disables all correction storage and application
- OOM cap: `read_f32_slice` for correction embeddings is bounded — reject oversized blobs with error, not panic
- Step corrections are per-skill-per-step — corrections never migrate across skills
- NEVER apply a correction with confidence below `d2skill_min_confidence`

---

## SkillOrchestra: RL Routing Head


`SkillOrchestra` wraps `SkillMatcher` with a LinUCB bandit routing head that selects which skill to inject based on turn-level reward signals (user feedback, task completion, tool success rate).

### LinUCB Bandit

- One arm per skill in the registry
- Context vector: query embedding + trust score + recency
- Reward: derived from `FeedbackDetector` signal at end of turn
- Weights persisted in `skill_orchestra_weights` SQLite table

### Cold Start

On a fresh database with no bandit state, `SkillOrchestra` falls back to standard `SkillMatcher` cosine matching until sufficient samples are available (`rl_min_samples`, default 50).

### Config

```toml
[skills]
rl_routing_enabled = false    # enable SkillOrchestra RL routing head
rl_min_samples = 50           # samples before RL head takes over from cosine fallback
rl_routing_provider = ""      # provider for any LLM-assisted reward shaping; empty = primary
```

### Key Invariants

- Cold start (fresh DB) MUST fall back to cosine matching — RL head must not be active with zero samples
- Bandit weights are persisted between sessions — never reset without explicit user action
- NEVER use RL head when `rl_routing_enabled = false`
- Reward shaping must not block the agent turn — fire-and-forget after turn end

---

## Channel Allowlist


Skills can declare a `channels` field in SKILL.md frontmatter to restrict which I/O channels they may be injected on. If the field is absent, the skill is available on all channels (legacy behavior).

### Frontmatter Field

```yaml
---
name: my-skill
channels: ["cli", "tui"]   # omit to allow all channels
---
```

Supported channel identifiers: `cli`, `tui`, `telegram`, `discord`, `slack`, `acp`.

### Key Invariants

- Absent `channels` field = allow all channels (backward compatible)
- Channel filtering applies at injection time only — skill trust and governance are unaffected
- NEVER inject a skill on a channel not in its allowlist, even if it scores above thresholds
- Channel identifier matching is case-insensitive

---

## NL Skill Generation and GitHub Repo Mining


Two new skill acquisition paths:

### NL Skill Generation

`/skill create <description>` triggers an LLM call to generate a SKILL.md from a natural-language description. Generated skills are saved at `quarantined` trust level. Description is capped at 2048 characters before being sent to the LLM.

### GitHub Repo Mining

`/skill mine <repo_url>` fetches SKILL.md files from a GitHub repository. Fetched skills are sanitized (injection patterns removed, URL domain validated against `[skills.scanner.url_domain_allowlist]`) and imported at `quarantined` trust.

### Deduplication

Before creating or importing a skill, the registry checks for an existing skill with a cosine similarity above `dedup_threshold` (default 0.90). If a near-duplicate is found, creation is silently skipped with an info log.

**Qdrant cold-start gap**: with the Qdrant vector backend, `skill_embedding()` may return `None` before any embeddings are stored. In this case dedup is skipped and the skill is created regardless of similarity.

### Config

```toml
[skills]
url_allowlist = []         # allowed domains for GitHub mining; empty = deny all external URLs
dedup_threshold = 0.90     # cosine similarity threshold for deduplication
```

```toml
[skills.scanner]
injection_patterns = []            # additional regex patterns
url_domain_allowlist = []          # domains permitted in skill body URLs
```

### Key Invariants

- Generated and mined skills ALWAYS start at `quarantined` — never skip trust governance
- Description cap (2048 chars) is enforced before LLM call — not after
- URL domain allowlist is checked at scan time on every load, not only at import
- Deduplication uses cosine similarity, not exact name match
- NEVER create a skill that fails injection sanitization
- `/skill create` with Qdrant backend: missing embedding returns `None` — treat as no-duplicate-found, proceed with creation

---

## Hub Skill Install Pipeline

Issue #3043 / #3050. The hub install pipeline fetches SKILL.md files from a configured skill hub (default: https://hub.agentskills.io), validates trust, and installs into the managed directory.

### Trust Escalation Filter for Bundled Skills

Skills installed via the hub that originate from `hub.agentskills.io` **and** are in the set of well-known bundled skill names receive a `.bundled` marker during installation. The `.bundled` marker exempts the skill from `WARN`-level security scan output and grants `Trusted` trust on first load (all other hub-sourced skills start at `Provisional`).

Install-time filtering:
1. Skill fetched from hub
2. Injection scan runs on SKILL.md body — hard block if positive
3. URL domain validation against `[skills.scanner.url_domain_allowlist]`
4. If skill name matches bundled allowlist AND source is the canonical hub → write `.bundled` marker
5. Trust set to `Trusted` for `.bundled` skills, `Provisional` for all others

At startup and on hot-reload, `build_registry()` assigns `Trusted` trust to all skills that carry a `.bundled` marker file. This initialization is unconditional — it does not wait for feedback accumulation.

### Key Invariants (Hub Install)

- `.bundled` marker is write-once at install time — never added post-install by the agent
- NEVER assign `Trusted` trust to a skill without a `.bundled` marker via the startup path
- Injection scan MUST run before writing `.bundled` — a skill that fails scan is never bundled
- `build_registry()` MUST assign `Trusted` to `.bundled` skills on every startup, including hot-reload

---

## Agent-Invocable Skills (`invoke_skill`)

Issue #3127. Agents can invoke a named skill by calling the `invoke_skill` native tool. This differs from `load_skill` (preview/read-only) — `invoke_skill` carries intent-to-apply semantics: the active skill for the current turn is updated and the skill's system-prompt injection is applied immediately.

### `invoke_skill` Tool

| Field | Description |
|-------|-------------|
| `name` | Skill name to activate |
| `reason` | Optional free-text rationale for the invocation |

The tool returns a confirmation message with the skill's name and first 200 chars of its description. On failure (skill not found, below trust gate), it returns an error with category `ToolErrorCategory::ToolNotFound` or `PolicyBlocked`.

### Security Gate

`invoke_skill` checks:
1. Skill exists in the registry
2. Skill trust level is ≥ `Provisional` — `Quarantined` skills cannot be invoked
3. Skill is not in the channel blocklist for the current channel

### Key Invariants

- `invoke_skill` is always exempt from the utility gate and the adversarial policy gate — listed in both `UtilityScoringConfig::exempt_tools` and `AdversarialPolicyConfig::exempt_tools` by default
- `invoke_skill` and `load_skill` are both in `QUARANTINE_DENIED` — they cannot be triggered by quarantined skill content
- `invoke_skill` carries intent-to-apply semantics: the invoked skill IS injected; `load_skill` is preview-only and does NOT update the active skill
- NEVER invoke a `Quarantined` skill via this tool — trust gate must check before injection

---

## Skill Evaluator

External-feedback skill evaluator (#3319, #3350). After skill generation, a critic LLM call
scores the skill on three dimensions before writing to disk.

Config section `[skills.evaluation]` (disabled by default):

```toml
[skills.evaluation]
enabled = false
evaluate_provider = "fast"   # named provider reference
weight_correctness = 0.50
weight_reusability = 0.25
weight_specificity = 0.25
pass_threshold = 0.60        # minimum weighted score to accept skill
```

Behavior: `SkillEvaluationConfig` passed to `SkillEvaluator`; if evaluator errors, skill is
accepted (fail-open). Tracing spans under `skills.eval.*`.

### Key Invariants

- Evaluation is optional and fail-open — a missing or erroring evaluator must never block skill creation
- `evaluate_provider` resolves via named `[[llm.providers]]` reference
- NEVER write skill to disk if score < `pass_threshold` (when evaluation is enabled and succeeds)

## Proactive World-Knowledge Exploration

Proactive skill generation before each LLM call (#3320, #3350). The agent classifies the
query domain (keyword-based) and generates a `world-knowledge-{domain}` SKILL.md when none exists.

Config section `[skills.proactive_exploration]` (disabled by default):

```toml
[skills.proactive_exploration]
enabled = false
generate_provider = "fast"   # named provider reference
```

**MVP trade-off**: generated skill is visible from the **next** turn (not the current one), to
keep turn latency bounded. Tracing spans under `core.proactive.*`.

### Key Invariants

- Generated skill is intentionally deferred to the next turn — NEVER inject into the current turn's context
- Domain classification is keyword-based (no LLM call) — NEVER use LLM for domain classification

## Bare-Mode Skip

`build_skill_matcher` is skipped in `--bare` mode to prevent the `zeph_skills` Qdrant collection
from being destroyed on CI startup (#3390, #3395).

The embed provider model name used for Qdrant collection versioning must be stable — a changing
model name causes collection oscillation and near-zero cosine scores (#3391). The stable embed
provider model name is resolved once at bootstrap from `[[llm.providers]]` with `embed = true`.

### Key Invariants

- NEVER call `build_skill_matcher` in `--bare` mode
- Qdrant collection name for skills must be derived from a stable embed model name, not the resolved display name of the active provider

## SKILL.md Injection Sanitization


Skill bodies are scanned for injection patterns at load time and before injection into the system prompt. Detected patterns are replaced with `[sanitized]`. The scanner also validates URLs in the skill body against `[skills.scanner.url_domain_allowlist]`.

### Trust Fallback Fix

When a skill's trust level cannot be resolved from the database (e.g., first load), the skill defaults to `Provisional` trust rather than `Trusted`. This prevents new skills from gaining full trust on their first appearance.

### Input Injection Hard Block

When `/skill create` is called, the description input is scanned for injection patterns before being passed to the LLM. Detected injection in the input triggers a hard block — the skill is not created and the user sees an error.

### Key Invariants

- Injection sanitization runs on every load — not only on import
- URL domain validation blocks URLs whose host is not in the allowlist when the allowlist is non-empty
- Trust fallback is `Provisional`, not `Trusted` — NEVER assume full trust on first load
- Low-confidence skill injection is blocked: score must clear both `disambiguation_threshold` and `min_injection_score`
- Input injection scan for `/skill create` must run BEFORE the LLM call — not after generation
