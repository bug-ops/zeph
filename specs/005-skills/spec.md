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

## Construction-Time / Reload-Time Skill Prompt Contract (#6413)

Full-body skill injection (previous section) requires a per-turn `query` to run the matcher and
apply `disambiguation_threshold` / `min_injection_score` / `max_active_skills`. Two call sites
build (or rebuild) the system prompt *before* a query exists: agent construction
(`Agent::new_with_registry_arc`) and skill hot-reload (`reload_skills()`). Neither has a query to
match against, so neither may fall back to injecting every registered skill's full body — that
would silently bypass the `max_active_skills` cap (a hard invariant everywhere else) and force a
synchronous full-body disk read of every `SKILL.md`, both forbidden by `specs/001-system-invariants/spec.md`
§7.

Contract for both call sites:
- Build the skill listing from **metadata only** (`SkillMeta`: name + description) — never call
  into the registry's lazy full-body loader (`SkillRegistry::skill()`)
- Format with the catalog formatter (name + description only, no `<instructions>` body), the same
  formatter used for the *unmatched* remainder of skills in the per-turn path
- `last_skills_prompt` (token-accounting cache, never itself injected into the LLM-bound prompt —
  see `crates/zeph-core/src/agent/state/mod.rs`) is seeded/updated with this same catalog-only
  text, not the full unfiltered registry blob
- Full skill bodies are injected exclusively by the next `rebuild_system_prompt(query)` call, once
  a real per-turn query exists to run the matcher against
- Any code path that mutates `messages[0]` outside `push_message`'s incremental accounting (e.g.
  `reload_skills()`) MUST recompute the cached prompt-token count from the mutated message
  afterward — a stale count is a state-consistency bug even though it self-corrects on the next
  turn's `rebuild_system_prompt`

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
- The system prompt built at agent construction or by `reload_skills()` (no per-turn query
  available) is catalog-only (name + description); NEVER force-load or inject full skill bodies
  at these call sites — see § Construction-Time / Reload-Time Skill Prompt Contract (#6413)

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
- The trust check applied to `invoke_skill`/`load_skill` is a per-turn weakest-link fold (`TrustGateExecutor::effective_trust`, computed in `zeph_core::agent::context::assembly`), not a per-skill check: if ANY skill active this turn is `Quarantined`, the call is denied for the whole turn regardless of which skill is targeted. A denial reflects the turn's overall trust floor and is not necessarily about the specific skill/tool targeted by the call — it may or may not itself be the quarantined one — see #5729 and `TrustGateExecutor`'s doc comment
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

---

## Data-Instruction Boundary for SKILL.md Descriptions (#4135, #4232)

Untrusted SKILL.md `description` fields are wrapped in `<data-description>` boundary tags
before context injection to prevent LLM confusion between skill descriptions and agent
instructions.

### `sanitize_skill_metadata()`

Called in `zeph-skills` before any skill description is injected into the system prompt:

1. XML-escape inner content (`&`, `<`, `>`, `"`, `'`)
2. Strip common instruction-prefix patterns (e.g., `Ignore previous instructions`, `You are now`)
3. Wrap in `<data-description>…</data-description>`
4. Char-truncate at `floor_char_boundary()` for UTF-8 safety

### Per-Invocation Blake3 Re-Hash

When `SkillTrust::requires_trust_check = true` is set on a skill, a blake3 hash of the
skill content is computed at **every invocation** and compared against the stored hash at
load time. A mismatch (post-load mutation) triggers `SkillError::TamperDetected` and
blocks invocation.

```toml
# Per-skill in SKILL.md frontmatter (or set by the registry on high-privilege install)
requires_trust_check = true
```

#### Automatic Activation on Promotion (#6087)

`requires_trust_check` is armed **automatically** whenever an operator promotes a skill to
`Trusted` or `Verified` — the choke point where a skill's body is dispatched verbatim without
sanitization — via `[skills.trust] require_integrity_check_on_promote` (default `true`). Both
operator-facing promotion handlers apply this: CLI `zeph skill trust <name> trusted|verified`
(`src/commands/skill.rs`) and in-session `/skill trust <name> trusted|verified`
(`crates/zeph-core/src/agent/trust_commands.rs`). `--require-check`/`--no-require-check`
(mutually exclusive) on the promoting command always override the config default. Promotion to
`Quarantined`/`Blocked` leaves `requires_trust_check` untouched — it is irrelevant at those
levels and a previously armed flag must survive a temporary demotion.

Self-learning/heuristic auto-promotion (`crates/zeph-core/src/agent/learning/trust.rs`) and
reload trust-assignment (`crates/zeph-core/src/agent/skill_reload.rs`) also raise a skill's
trust level to `Trusted`/`Verified` but are **not** operator promotion and do not arm
`requires_trust_check` — the threat this default addresses is an operator promoting a skill
and forgetting to arm the re-check, not autonomous promotion paths. This is an intentional
scope boundary, not an oversight, for #6087; a follow-up issue may be filed to cover the
auto-promotion paths separately.

### Recursive Nested Skill Discovery (#4682, #4684)

`WalkDir`-based discovery replaces the flat `read_dir` loop in the skill scanner. The traversal uses
pre-order DFS with lexicographic sibling ordering (max depth 16, no symlink follow). The first skill
with a given name wins; deeper duplicates are silently skipped. The existing `RecursiveMode::Recursive`
hot-reload watcher already covers subdirectories, so no watcher change is needed.

### Key Invariants

- Max walkdir depth is 16 — NEVER recurse further (prevents cycles on unusual filesystems)
- First-name-wins rule applies across depths — NEVER accept a deeper skill that duplicates a shallower name
- Symlinks are NOT followed — `follow_links(false)` is non-negotiable

---

## Skill Extension Manifest (`SkillExtensions`) (#4705, #4683)

`crates/zeph-skills/src/extensions.rs` adds an optional `extensions:` block in SKILL.md
frontmatter. Fields:

```
SkillExtensions {
    ui: Vec<SkillUiElement>,          // hotkey/button declarations
    keybindings: Vec<SkillKeybinding>,
    monitors: Vec<SkillMonitor>,      // background watch expressions
}
```

`SkillMeta.extensions: Option<SkillExtensions>` is populated by `parse_extensions()` with an
8 KiB byte cap. Parse failure returns `None` — existing SKILL.md files without an `extensions:`
block load unchanged. `serde_norway` is used for runtime YAML parsing within the cap.

### Key Invariants

- Extensions block is optional — absent `extensions:` never fails skill load
- 8 KiB cap is enforced before `serde_norway::from_str` — NEVER pass uncapped bytes to the deserializer
- Parse errors are silently ignored (return `None`) — NEVER propagate extension parse failure as a skill load error

---

## Concurrent Semantic Scan (#4705, #4683)

`semantic_scan_plugin_add` replaces sequential scanning with `buffer_unordered(4)` and a 300s
aggregate `tokio::time::timeout`. Each future carries its own `(skill_name, verdict)` tuple so
rejection messages always name the correct skill regardless of completion order.

### Key Invariants

- Scan concurrency cap is 4 — NEVER set above 4 without benchmarking under load
- Aggregate timeout is 300s — NEVER lower it below the per-skill LLM call p99 latency
- Rejection messages MUST include the specific skill name — never a positional index

---

## Skill Egress Attribution (#4682, #4684)

`ToolCall`, `AuditEntry`, and `EgressEvent` gain `skill_name: Option<Vec<String>>` carrying the
names of all skills injected into the system prompt for the current turn. Attribution is
turn-scoped, not per-call. All decorator executors (`ScopedToolExecutor`, policy gate, adversarial
gate) and all `scrape.rs` egress sites propagate the field. `ToolCall` derives `Default` to avoid
breaking existing struct literals.

### Key Invariants

- Attribution is turn-scoped — NEVER per-call attribution (that would require per-tool injection tracking)
- All executor decorators must propagate `skill_name` — single-path propagation is incomplete
- NEVER emit a non-`None` `skill_name` for turns with no injected skills

---

## Stage-2 LLM Semantic Scan for Third-Party Skills (#3947, #4696)

Defends against Semantic Compliance Hijacking (SCH) attacks (arXiv:2605.14460) where malicious
third-party skills encode harmful instructions in SKILL.md without explicit code payloads that
Stage-1 regex patterns would catch.

### `SkillSemanticScanner`

`crates/zeph-skills/src/semantic_scanner.rs`. Uses `chat_typed_erased` with a configurable
fast provider. Content cap: 8 KiB with head+tail sampling for larger skills.

XML delimiter-escape neutralization: any `</skill_content>` sequences in the skill body are
neutralized before interpolation into the prompt to prevent prompt-frame escapes.

Verdicts:

| `ScanVerdict` | Action |
|--------------|--------|
| `Allow` | Skill passes; proceed with installation/execution |
| `Warn` | Advisory; skill logged at WARN but not blocked |
| `Block` | Skill blocked; installation or execution rejected |

Unknown LLM output tokens fall back to `Block` (fail-closed).

### Integration Points

- **Plugin add**: `zeph-plugins` calls `scan_targets()` to extract SKILL.md candidates from an
  archive before installation. The `zeph-plugins` crate itself remains LLM-free; the scan
  is performed in `zeph-core` via `semantic_scan_plugin_add`, which wires the scanner.
- **Fail-closed on config error**: `semantic_scan = true` with an empty `semantic_scan_provider`
  returns a config error — never proceeds with an unconfigured scanner.

### Config

```toml
[skills]
semantic_scan = false              # opt-in Stage-2 semantic scan
semantic_scan_provider = ""        # [[llm.providers]] name (required when semantic_scan = true)
```

### Key Invariants

- NEVER proceed when `semantic_scan = true` and `semantic_scan_provider` is empty
- XML delimiter-escape neutralization (`</skill_content>` → escaped form) MUST run before interpolation — NEVER interpolate raw skill content
- Unknown scanner output tokens MUST produce `Block` verdict — NEVER default to `Allow` on parse failure
- `scan_targets()` in `zeph-plugins` extracts candidates without LLM calls — keeps `zeph-plugins` LLM-free
- NEVER apply Stage-2 scan to bundled skills (`.bundled` marker) — bundled skills are pre-vetted

---

## Stage-1 Advisory SKILL.md Scan (#4132)

Before executing a skill, the system runs a lightweight static scan over the SKILL.md body
to detect high-risk patterns (e.g., `eval`, `exec`, `import os`, network exfil keywords)
and emits an advisory `SecurityEvent::SkillAdvisory` with severity and matched pattern.

- Advisory scan is non-blocking: it does NOT prevent skill execution
- `SkillEmbedding::from_raw()` visibility tightened to `pub(crate)` — external callers must use the public `SkillEmbedding::new()` constructor which enforces dimension validation

### Key Invariants

- `sanitize_skill_metadata()` MUST run before EVERY description injection — no bypass path
- Blake3 re-hash only applies to skills with `requires_trust_check = true`; normal skills use load-time trust only
- `requires_trust_check` is armed by default on promotion to `Trusted`/`Verified` (`require_integrity_check_on_promote`, default `true`) — NEVER silently leave it off without an explicit `--no-require-check` or config override (#6087)
- Promotion to `Quarantined`/`Blocked` MUST NOT clear `requires_trust_check` — NEVER reset the flag on demotion
- Advisory scan result MUST NOT block skill invocation in v1 — advisory only
- NEVER store the raw unsanitized description in the system prompt
- NEVER proceed when `semantic_scan = true` but `semantic_scan_provider` is empty — return a config error (fail-closed, #4706, #4709)

---

## Group-Structured Retrieval (GoSkills)

RFC #4219. Closes #4000, #4064, #4065, #4090, #4091, #4125, #4166, #4195.

### Motivation

The current skill injection pipeline produces a flat ranked list of skills injected as peer `<skill>` elements into Block 3. When a turn activates two or more related skills, the LLM receives no signal about which skill is primary and which are auxiliary, what inter-skill dependencies exist, or which failure modes to avoid. GoSkills addresses this presentation gap by adding a post-selection grouping step that structures the injection around an entry-point skill and its support skills, without changing any upstream matching, trust, or governance logic.

### Approach

After the existing BM25 + embedding hybrid matching and RRF fusion produce a ranked candidate list, a new `group_skills()` post-processing step reformats the injection payload:

1. **Compute inter-skill cosine similarity** between the top-1 candidate (entry point) and each remaining candidate using their already-cached embeddings. This is a dot-product operation on vectors that are already in memory — no additional embedding calls.
2. **Group decision**:
   - If any candidate's cosine similarity to the entry point exceeds `support_similarity_threshold` → form a `SkillGroup`: entry point + qualifying support skills.
   - If no candidate clears the threshold (all top-N skills are dissimilar to each other) → fall back to flat injection (existing behaviour, all skills as peers).
3. **Structured injection**: when a group is formed, replace the flat `<skill>` element list with a role-labelled format (see Injection Format below).

The grouping step is purely a presentation-layer transform. All upstream selection (SkillMatcher, SkillOrchestra, Wilson scoring, trust governance, `max_active_skills` cap) continues to operate on individual skills, unchanged.

### Data Structures

```
SkillGroup {
    entry_point: Skill,             // primary skill for the turn
    support: Vec<Skill>,            // helper skills (capped at max_active_skills - 1)
    requirements: Vec<String>,      // extracted from frontmatter: tools, env, channels
    failure_notes: Vec<String>,     // ERL heuristics + D2Skill corrections for entry_point
    role_labels: HashMap<String, SkillRole>,  // entry_point | support | context
}

enum SkillRole {
    EntryPoint,
    Support,
    Context,
}
```

### Injection Format

When `group_structured = true` and a group is formed, Block 3 uses role-labelled sections instead of flat `<skill>` elements:

```xml
<active_skill role="entry_point" name="{name}">
{sanitized skill body}
</active_skill>

<active_skill role="support" name="{name}">
{sanitized skill body}
</active_skill>

<skill_requirements>
- tool: shell (required by entry_point)
- env: API_KEY (required by support)
</skill_requirements>

<failure_avoidance>
- {ERL heuristic 1}
- {D2Skill correction note}
</failure_avoidance>
```

When `group_structured = false` (default) or when the fallback condition is triggered (all top-N inter-similarity < threshold), the existing flat format is used unchanged.

### Trust and Sanitization Invariants Within Groups

`format_skills_prompt()` currently applies per-skill trust-based sanitization, health attributes, quarantine wrapping, and XML escaping. When group-structured output is enabled, this function must be refactored to accept a `SkillGroup` (or `Vec<SkillGroup>`) rather than a flat `&[Skill]`, while preserving all per-skill invariants:

- Trust-based sanitization runs independently for each skill in the group (entry point and each support skill).
- Quarantine wrapping applies per-skill — a quarantined skill cannot become a support skill in a group, just as it cannot be injected flat.
- XML escaping and `sanitize_skill_metadata()` apply per-skill within the group, not to the group framing.
- `max_skill_body_bytes` limit applies per-skill, not to the combined group body.
- The framing overhead (~50–100 tokens for role labels, requirements, failure notes) is within the existing per-turn budget and does not require a new budget parameter.

### SkillOrchestra Interaction

SkillOrchestra's LinUCB bandit head selects individual skills (one arm per skill). The bandit action space must not change when `group_structured = true`. Grouping is applied **after** bandit selection as a presentation transform:

1. Bandit selects top-N individual skills (unchanged).
2. `group_skills()` receives the bandit-selected list and applies the grouping/fallback logic.
3. Rewards are attributed to individual skills (unchanged) — the bandit never observes a group as a unit.

This preserves exploration/exploitation convergence: adding grouping does not expand the bandit action space from N to O(N²).

### Multi-Entry Fallback

When the user query requires genuinely independent skills (e.g., "deploy to staging and notify Slack"), the top-N selected skills will have low inter-similarity. In this case, forcing single-entry grouping would mislead the LLM by demoting an independent skill to "support" role. The fallback rule handles this correctly:

- Compute pairwise cosine similarity between all top-N candidates.
- If no pair exceeds `support_similarity_threshold` → flat injection (all skills as peers, existing behaviour).
- If at least one pair exceeds the threshold → form a group with the highest-similarity pair as entry_point + first support skill; remaining candidates that also clear the threshold are added as additional support.

### Threshold Semantics

The `support_similarity_threshold` is defined as **inter-skill cosine similarity** between skill embedding vectors — not as a reuse of RRF-fused matcher scores. The rationale: RRF scores reflect relevance to the query, not semantic relatedness between skills. Two skills can both be highly relevant to a query without being semantically related to each other.

Implementation: after matching produces the top-N list, `group_skills()` computes dot products between the entry-point embedding and each candidate embedding (embeddings are already cached in `SkillMatcher` state). Default threshold `0.50` corresponds to a moderate semantic overlap (≈30° angle).

### Config

```toml
[skills]
group_structured = false                # opt-in; default off
support_similarity_threshold = 0.50    # inter-skill cosine similarity; range [0.0, 1.0]
```

Both parameters are additive — no existing parameters are removed or renamed.

### A/B Validation Requirement

Enabling `group_structured = true` changes the LLM prompt format in Block 3. This is not a code-level breaking change (existing behaviour is preserved when disabled), but it IS a prompt regression surface. Before `group_structured` is changed to `true` as the default:

- Run an A/B experiment via `zeph-experiments` comparing flat vs. group-structured injection.
- Measure: task completion rate, user correction frequency, and token efficiency.
- A/B results must show no regression on flat-injection baselines before the default is changed.

### Migration Notes

- Default `group_structured = false` preserves existing behaviour for all existing configs — no migration required.
- No new database tables or storage migrations are needed — grouping is stateless and computed per-turn.
- `format_skills_prompt()` in `crates/zeph-skills/src/prompt.rs` requires a signature change to accept `SkillGroup` input. The existing flat-list overload must be retained (or a compatibility wrapper provided) for the `group_structured = false` path.
- `Agent::rebuild_system_prompt` (`crates/zeph-core/src/agent/mod.rs`; `SkillState` itself is defined in `crates/zeph-core/src/agent/state/mod.rs`) must route to the new group-aware formatter when `group_structured = true`.

### Future Consideration: Toward Explicit Skill Dependencies

GoSkills groups are a stepping stone toward an explicit skill dependency graph (RFC #4125). If multi-step skill composition becomes a validated user need, the `requirements` field in `SkillGroup` can be extended to carry typed dependency edges rather than plain strings. This extension is deferred until composition is a confirmed bottleneck.

### Key Invariants

- `group_structured = false` MUST produce output identical to the current flat injection — no behaviour change on the default path
- Trust-based sanitization, quarantine wrapping, and XML escaping MUST apply per-skill within a group, not to the group as a whole
- NEVER include a `Quarantined` skill as a support skill in a group
- SkillOrchestra bandit MUST select individual skills; grouping is post-selection only — NEVER change the bandit action space to groups
- When no pair of top-N skills exceeds `support_similarity_threshold`, MUST fall back to flat injection — NEVER force a group when skills are semantically independent
- Inter-skill cosine similarity threshold is computed on skill embeddings, NOT on RRF-fused matcher scores
- `max_active_skills` cap applies before grouping — NEVER inject more skills in a group than the cap allows
- `group_structured = true` MUST NOT become the default until A/B validation via `zeph-experiments` shows no regression

---

## Appendix: RFC #4219 Comparison Matrix

Eight approaches were evaluated against the current skill system baseline before GoSkills was selected. This matrix provides traceability for the decision to close issues #4000, #4064, #4065, #4090, #4091, #4125, #4166, #4195.

### Current System Baseline Summary

At time of evaluation, Zeph's skill system already provided: BM25 + embedding hybrid matching with RRF fusion and Wilson score re-ranking; trust governance (Untrusted → Provisional → Trusted) with capability escalation detection and injection sanitization; self-learning via ARISE, STEM, ERL, and D2Skill; SkillOrchestra LinUCB bandit routing; SkillEvaluator critic LLM scoring; and two-stage category-first matching. Any addition must clear a high bar against this baseline.

### Comparison Matrix

| # | Issue | Approach | Discovery Quality | Composition Support | Maintenance Cost | Implementation Complexity | Redundancy with Existing Spec | Decision |
|---|-------|----------|------------------|--------------------|-----------------|--------------------------|-----------------------------|----------|
| 1 | #4000 | SkillMaster | +1: counterfactual probe eval | 0: no composition model | Low | Medium | **High**: ARISE + SkillEvaluator already cover trajectory-informed review | Rejected |
| 2 | #4064 | Corpus2Skill | +1: tree navigation at 100+ skills | +1: tree implies hierarchy | **High**: tree rebuilt on changes | **High**: offline compilation, tree-nav LLM call per turn | Medium: two_stage_matching already does coarse→fine | Rejected |
| 3 | #4065 | Bilevel MCTS | 0: structure search only | 0: single-skill optimization | **High**: MCTS rollouts | **Very High**: MCTS + LLM inner loop | Medium: overlaps ARISE + SkillEvaluator | Rejected |
| 4 | #4090 | MIND-Skill | +1: reconstruction loss quality signal | 0: single-skill induction | Medium: dual-agent loop | High: TextGrad optimization | High: SkillEvaluator 3-dim scoring already equivalent | Rejected |
| 5 | **#4091** | **GoSkills** | **+2: role-labeled groups** | **+2: entry-point + support skills** | **Low: presentation-layer only** | **Low–Medium: no new infra** | **Low: genuine gap in flat injection** | **Selected** |
| 6 | #4125 | SkillGraph | +1: prerequisite edges | +2: DAG enables multi-step planning | **High**: edges must be maintained | High: graph storage + path-finding | Medium: overlaps category system | Rejected (deferred) |
| 7 | #4166 | SkillOS | 0: curation only | 0: merging reduces library | Medium | Medium | High: Wilson score + dedup_threshold already handle this | Rejected |
| 8 | #4195 | 4-stage loop | +1: data selection filter | 0: meta-framework | Low | Low–Medium | **Very High**: maps 1:1 onto ARISE/SkillEvaluator/STEM/SkillOrchestra | Rejected |

**Scoring**: 0 = no improvement over baseline, +1 = modest improvement, +2 = significant improvement.

### Rejection Rationale Summary

**Rejected for high redundancy** (#4000, #4090, #4195, #4166): counterfactual probe eval, TextGrad dual-agent induction, 4-stage meta-framework, and skill curation/merging all map closely onto capabilities already in ARISE, SkillEvaluator, D2Skill, Wilson score, and dedup_threshold. The incremental value does not justify the added complexity.

**Rejected for complexity vs. scale mismatch** (#4064, #4065): hierarchical tree navigation and Bilevel MCTS are research-grade techniques suited to skill libraries of 100+. Zeph's practical library (26 bundled + user-generated) does not justify offline tree compilation or MCTS rollouts.

**Deferred, not rejected** (#4125 SkillGraph): prerequisite edges and DAG path-finding are architecturally appealing for multi-step composition, but require graph maintenance burden (edge discovery, weighting, pruning) without validated demand. GoSkills `requirements` field is a stepping stone; revisit when multi-step composition is a confirmed user need.

**Selected** (#4091 GoSkills): the only approach addressing a genuine gap (how matched skills are presented to the LLM) at low implementation cost and with zero breaking changes to the default path.
