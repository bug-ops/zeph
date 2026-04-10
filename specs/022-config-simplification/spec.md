---
aliases:
  - Provider Registry
  - Config Simplification
  - Provider Architecture
tags:
  - sdd
  - spec
  - config
  - llm
created: 2026-03-22
status: approved
related:
  - "[[MOC-specs]]"
  - "[[003-llm-providers/spec]]"
  - "[[024-multi-model-design/spec]]"
  - "[[023-complexity-triage-routing/spec]]"
  - "[[020-config-loading/spec]]"
---

# Spec: Provider Registry Architecture (`[[llm.providers]]`)

> **Status**: Implemented
> **Author**: rust-architect
> **Date**: 2026-03-22 (updated 2026-03-26)
> **Crates**: `zeph-config`, `zeph-core` (bootstrap/provider.rs)

## 1. Overview

> This is the **canonical reference** for the `[[llm.providers]]` registry pattern.
> All new code, specs, and config examples MUST use this format.
> The migration context (§2) is preserved for historical clarity.

### Problem Statement

The legacy `[llm]` config section required users to define the same provider
parameters in up to three places:

1. **Top-level `[llm]`** fields (`base_url`, `model`, `embedding_model`)
2. **Provider-specific sections** (`[llm.cloud]`, `[llm.openai]`, `[llm.gemini]`)
3. **Orchestrator sub-provider entries** (`[llm.orchestrator.providers.<name>]`)

Additionally, router config (`[llm.router]`) exists as a separate concept from
the orchestrator, even though both do multi-provider dispatch. The result is
a 240-line `LlmConfig` struct, an 824-line `default.toml`, and a confusing
inheritance chain where `build_sub_provider()` must fall back from orchestrator
entry -> provider section -> top-level `[llm]` for every field.

### Goal

Reduce the `[llm]` config surface so that each provider is defined exactly once,
routing is a property of the provider pool rather than a separate concept, and
the minimal working config is 5-8 lines.

### Out of Scope

- Changes to `LlmProvider` trait or `AnyProvider` enum (runtime code is fine)
- Changes to non-LLM config sections (memory, skills, tools, etc.)
- Changes to the vault secret resolution mechanism
- Removing any provider backend (all 6 backends stay)

## 2. Analysis: Current Duplication

### 2.1 Field Duplication Map

The core problem is that `model`, `base_url`, `max_tokens`, and `embedding_model`
appear in multiple structs that represent the same logical provider:

| Field | `LlmConfig` (top-level) | `CloudLlmConfig` | `OpenAiConfig` | `GeminiConfig` | `OrchestratorProviderConfig` |
|-------|------------------------|-------------------|----------------|----------------|-------------------------------|
| `model` | yes | yes | yes | yes | yes (optional) |
| `base_url` | yes | -- | yes | yes | yes (optional) |
| `max_tokens` | -- | yes | yes | yes | -- |
| `embedding_model` | yes | -- | yes (opt) | yes (opt) | yes (optional) |

When `provider = "orchestrator"`, the user must define a Claude provider in
**both** `[llm.cloud]` (for `max_tokens`, `thinking`, `server_compaction`) AND
`[llm.orchestrator.providers.claude]` (for routing). The orchestrator entry
then falls back to `[llm.cloud]` for fields it does not override.

### 2.2 Bootstrap Code Duplication

`bootstrap/provider.rs` contains 6 parallel construction paths:

1. `named_claude()` -- reads `[llm.cloud]` + secrets
2. `pcfg_claude()` -- reads `OrchestratorProviderConfig` + falls back to `[llm.cloud]`
3. `build_sub_provider("claude")` -- reads `OrchestratorProviderConfig` + falls back to `[llm.cloud]`
4. `summary_claude()` -- reads `model_spec` string + falls back to `[llm.cloud]`

Similar 4-way duplication exists for OpenAI, Gemini, and partially for Ollama.

### 2.3 Router vs Orchestrator Conceptual Overlap

| Feature | Orchestrator | Router |
|---------|-------------|--------|
| Multi-provider | yes (named map) | yes (ordered chain) |
| Fallback chain | per task-type routes | chain order |
| Strategy | LLM-based classifier | EMA / Thompson / Cascade |
| Embedding delegation | dedicated `embed` field | inherits from first provider |
| Provider definition | `OrchestratorProviderConfig` (6 fields) | references `create_named_provider()` |

These are conceptually the same thing: "choose among N providers using strategy S".

## 3. Proposed Config Schema

### 3.1 Design Principles

1. **Define each provider exactly once** in a `[[llm.providers]]` array
2. **Routing is a property**, not a separate provider type
3. **The primary provider is always the first entry** (or marked `default = true`)
4. **Embedding provider is declared explicitly** via `embed = true`
5. **Provider-specific fields live inside the provider entry** (no separate sections)
6. **Backward compatibility** via `--migrate-config` and runtime deserialization shim

### 3.2 New Schema (TOML)

#### Minimal config (single Ollama provider)

```toml
[agent]
name = "Zeph"

[llm]
# First provider is the default. Single-provider config is trivially simple.
[[llm.providers]]
type = "ollama"
model = "qwen3:8b"
embedding_model = "qwen3-embedding"
# base_url defaults to http://localhost:11434 for ollama
```

5 lines for `[llm]` (down from 25+ today).

#### Cloud provider (Claude)

```toml
[llm]
[[llm.providers]]
type = "claude"
model = "claude-sonnet-4-6"
max_tokens = 4096
# thinking = { type = "enabled", budget_tokens = 10000 }
# server_compaction = false
# enable_extended_context = false
```

No separate `[llm.cloud]` section. All Claude-specific fields live inside the entry.

#### Multi-provider with routing

```toml
[llm]
routing = "cascade"  # "none" (default/single) | "round-robin" | "ema" | "thompson" | "cascade" | "task"

# Provider pool: ordered by cost (cheapest first for cascade)
[[llm.providers]]
name = "local"
type = "ollama"
model = "qwen3.5:9b"
embedding_model = "qwen3-embedding"
embed = true          # this provider handles embeddings

[[llm.providers]]
name = "cloud"
type = "claude"
model = "claude-sonnet-4-6"
max_tokens = 4096
default = true        # primary provider for chat (overrides position-based default)

# Task-based routing (only when routing = "task")
[llm.routes]
coding = ["cloud", "local"]
creative = ["cloud"]
general = ["local", "cloud"]
```

#### Compatible providers

```toml
[[llm.providers]]
name = "groq"
type = "compatible"
base_url = "https://api.groq.com/openai/v1"
model = "llama-3.3-70b-versatile"
max_tokens = 4096
```

#### Cascade with reputation

```toml
[llm]
routing = "cascade"

[llm.cascade]
quality_threshold = 0.5
max_escalations = 2
classifier_mode = "heuristic"

[llm.reputation]
enabled = true
decay_factor = 0.95
weight = 0.3

[[llm.providers]]
name = "cheap"
type = "compatible"
base_url = "http://localhost:11434/v1"
model = "qwen3.5:9b"
max_tokens = 8192

[[llm.providers]]
name = "quality"
type = "claude"
model = "claude-sonnet-4-6"
max_tokens = 4096
```

#### Summary provider

```toml
[llm]
# Inline shorthand (unchanged)
summary_model = "ollama/qwen3:1.7b"

# OR structured (references a named provider or defines a new one)
[llm.summary_provider]
type = "claude"
model = "claude-haiku-4-5-20251001"
max_tokens = 4096
```

### 3.3 Unified Provider Entry Schema

One struct replaces `CloudLlmConfig`, `OpenAiConfig`, `GeminiConfig`,
`CompatibleConfig`, and `OrchestratorProviderConfig`:

```rust
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProviderEntry {
    /// Required: provider backend type.
    #[serde(rename = "type")]
    pub provider_type: ProviderKind,

    /// Optional name for multi-provider configs. Auto-generated from type if absent.
    #[serde(default)]
    pub name: Option<String>,

    /// Model identifier. Required for all types except candle.
    #[serde(default)]
    pub model: Option<String>,

    /// API base URL. Each type has its own default.
    #[serde(default)]
    pub base_url: Option<String>,

    /// Max output tokens. Each type has its own default.
    #[serde(default)]
    pub max_tokens: Option<u32>,

    /// Embedding model. When set, this provider can handle embed() calls.
    #[serde(default)]
    pub embedding_model: Option<String>,

    /// Mark this entry as the embedding provider.
    #[serde(default)]
    pub embed: bool,

    /// Mark this entry as the default chat provider (overrides position).
    #[serde(default)]
    pub default: bool,

    // --- Claude-specific ---
    #[serde(default)]
    pub thinking: Option<ThinkingConfig>,
    #[serde(default)]
    pub server_compaction: bool,
    #[serde(default)]
    pub enable_extended_context: bool,

    // --- OpenAI-specific ---
    #[serde(default)]
    pub reasoning_effort: Option<String>,

    // --- Gemini-specific ---
    #[serde(default)]
    pub thinking_level: Option<GeminiThinkingLevel>,
    #[serde(default)]
    pub thinking_budget: Option<i32>,
    #[serde(default)]
    pub include_thoughts: Option<bool>,

    // --- Ollama-specific ---
    #[serde(default)]
    pub tool_use: bool,

    // --- Compatible-specific ---
    #[serde(default)]
    pub api_key: Option<String>,

    // --- Candle-specific ---
    #[serde(default)]
    pub candle: Option<CandleInlineConfig>,

    // --- Vision ---
    #[serde(default)]
    pub vision_model: Option<String>,

    /// Provider-specific instruction file.
    #[serde(default)]
    pub instruction_file: Option<std::path::PathBuf>,
}
```

Provider-specific fields are `#[serde(default)]` and ignored by types that
don't use them. This is the standard "flat union" pattern (same as OpenAPI
discriminated unions). Serde silently ignores unknown fields on deserialization
when using `#[serde(default)]`.

### 3.4 Simplified `LlmConfig`

```rust
#[derive(Debug, Deserialize, Serialize)]
pub struct LlmConfig {
    /// Provider pool. First entry is default unless one is marked `default = true`.
    pub providers: Vec<ProviderEntry>,

    /// Routing strategy for multi-provider configs.
    #[serde(default)]
    pub routing: RoutingStrategy,

    /// Task-based routes (only used when routing = "task").
    #[serde(default)]
    pub routes: HashMap<String, Vec<String>>,

    // --- Routing strategy configs ---
    #[serde(default)]
    pub cascade: Option<CascadeConfig>,
    #[serde(default)]
    pub ema: Option<EmaConfig>,
    #[serde(default)]
    pub thompson: Option<ThompsonConfig>,
    #[serde(default)]
    pub reputation: Option<ReputationConfig>,

    // --- Cross-cutting LLM settings ---
    #[serde(default)]
    pub response_cache_enabled: bool,
    #[serde(default = "default_response_cache_ttl_secs")]
    pub response_cache_ttl_secs: u64,
    #[serde(default)]
    pub semantic_cache_enabled: bool,
    #[serde(default = "default_semantic_cache_threshold")]
    pub semantic_cache_threshold: f32,
    #[serde(default = "default_semantic_cache_max_candidates")]
    pub semantic_cache_max_candidates: u32,

    #[serde(default)]
    pub stt: Option<SttConfig>,
    #[serde(default)]
    pub summary_model: Option<String>,
    #[serde(default)]
    pub summary_provider: Option<ProviderEntry>,
    #[serde(default)]
    pub instruction_file: Option<std::path::PathBuf>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RoutingStrategy {
    #[default]
    None,     // single provider or first-in-pool
    Ema,
    Thompson,
    Cascade,
    Task,     // replaces "orchestrator" with task-type routing
    Triage,   // pre-inference complexity classification (023-complexity-triage-routing)
    Bandit,   // PILOT LinUCB contextual bandit (see § PILOT Bandit Routing below)
}
```

### 3.5 Fields Removed

| Old Field | Replacement |
|-----------|-------------|
| `llm.provider` (ProviderKind) | Derived from `providers[0].type` or routing strategy |
| `llm.base_url` | Moved into each `ProviderEntry.base_url` |
| `llm.model` | Moved into each `ProviderEntry.model` |
| `llm.embedding_model` | Moved into each `ProviderEntry.embedding_model` |
| `llm.cloud` (CloudLlmConfig) | Inlined into `ProviderEntry` with `type = "claude"` |
| `llm.openai` (OpenAiConfig) | Inlined into `ProviderEntry` with `type = "openai"` |
| `llm.gemini` (GeminiConfig) | Inlined into `ProviderEntry` with `type = "gemini"` |
| `llm.ollama` (OllamaConfig) | `tool_use` field on `ProviderEntry` |
| `llm.candle` (CandleConfig) | `candle` inline config on `ProviderEntry` |
| `llm.compatible` (Vec) | `ProviderEntry` entries with `type = "compatible"` |
| `llm.orchestrator` (OrchestratorConfig) | `routing = "task"` + `[llm.routes]` + provider pool |
| `llm.router` (RouterConfig) | `routing` field + strategy-specific sections |
| `llm.router_ema_*` | Moved into `[llm.ema]` |
| `llm.vision_model` | Moved into each `ProviderEntry.vision_model` |

### 3.6 Before / After Comparison

**BEFORE (cloud.toml orchestrator, 35 lines):**

```toml
[llm]
provider = "orchestrator"
base_url = "http://localhost:11434"
model = "claude-sonnet-4-6"

[llm.cloud]
model = "claude-sonnet-4-6"
max_tokens = 4096

[llm.orchestrator]
default = "claude"
embed = "ollama"

[llm.orchestrator.providers.claude]
type = "claude"
model = "claude-sonnet-4-6"

[llm.orchestrator.providers.ollama]
type = "ollama"
base_url = "http://localhost:11434"
model = "qwen3.5:9b"
embedding_model = "qwen3-embedding"

[llm.orchestrator.routes]
chat = ["claude", "ollama"]
```

**AFTER (same behavior, 18 lines):**

```toml
[llm]
routing = "task"

[llm.routes]
chat = ["claude", "ollama"]

[[llm.providers]]
name = "claude"
type = "claude"
model = "claude-sonnet-4-6"
max_tokens = 4096
default = true

[[llm.providers]]
name = "ollama"
type = "ollama"
model = "qwen3.5:9b"
embedding_model = "qwen3-embedding"
embed = true
```

**BEFORE (testing.toml router, 15 lines):**

```toml
[llm]
provider = "router"
base_url = "https://api.openai.com/v1"
model = "gpt-4o-mini"
embedding_model = "text-embedding-3-small"

[llm.openai]
base_url = "https://api.openai.com/v1"
model = "gpt-4o-mini"
max_tokens = 4096
embedding_model = "text-embedding-3-small"

[llm.router]
chain = ["openai"]
```

**AFTER (same behavior, 9 lines):**

```toml
[llm]
routing = "ema"

[[llm.providers]]
name = "openai"
type = "openai"
base_url = "https://api.openai.com/v1"
model = "gpt-4o-mini"
max_tokens = 4096
embedding_model = "text-embedding-3-small"
```

## 4. Breaking Changes

This is a **breaking change** (no backward compatibility shim).

Old-format configs (`[llm.cloud]`, `[llm.openai]`, `[llm.orchestrator]`, `[llm.router]`) produce a clear config error on startup pointing to the migration guide.

### 4.1 `--migrate-config`

`ConfigMigrator::migrate_llm_to_providers()` handles conversion:

1. Detects old-format `[llm]` sections (presence of `provider` key)
2. Converts to new `[[llm.providers]]` format
3. Preserves all user values
4. Writes the new config in-place (creates `.bak` backup)

### 4.2 Removed Fields

See CHANGELOG.md for the full list of removed fields.

## 5. Key Invariants

1. **Single source of truth**: Each provider's parameters are defined in exactly one
   `[[llm.providers]]` entry. No inheritance, no fallback chain between config sections.
2. **Position-based default**: The first entry in `providers` is the default unless
   one entry has `default = true`. Exactly one provider is the default.
3. **Explicit embed**: Exactly one provider must have `embed = true` or an
   `embedding_model` set. If none, the default provider is used for embeddings
   (degraded: error if it does not support embeddings).
4. **Routing is optional**: `routing = "none"` (default) uses the default provider
   for all requests. No routing infrastructure is initialized.
5. **Provider names are unique**: Two entries with the same `name` is a config error.
   When `name` is absent, it defaults to the `type` string; if that causes a collision,
   it is auto-suffixed (`ollama`, `ollama-2`, etc.).
6. **Hard break**: Old-format configs produce a clear startup error with migration instructions.

## 6. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|-------------------|
| Empty `providers` array | Config error: "at least one LLM provider must be configured" |
| `routing = "cascade"` with 1 provider | Warning log + cascade degrades to single-provider |
| `routing = "task"` with no `[llm.routes]` | All tasks go to default provider |
| Provider type requires API key but vault has none | Bootstrap error (unchanged from today) |
| Both old and new format in same file | Config error: "cannot mix legacy [llm.cloud]/[llm.openai] with [[llm.providers]]" |
| `default = true` on multiple entries | Config error: "only one provider can be marked as default" |
| No `embed = true` and no `embedding_model` on any entry | Warning: embeddings disabled. Memory semantic search will not work. |
| Candle entry without `[candle]` inline section | Uses candle defaults from `CandleInlineConfig::default()` |

## 7. Success Criteria

| ID | Metric | Target |
|----|--------|--------|
| SC-001 | Lines in `LlmConfig` struct | < 60 (down from ~80) |
| SC-002 | Provider config structs | 1 (`ProviderEntry`) instead of 6 |
| SC-003 | Bootstrap construction functions | 1 `build_provider_from_entry()` instead of 6 parallel paths |
| SC-004 | Minimal working config lines | <= 8 for single provider |
| SC-005 | All existing tests pass | All green |
| SC-006 | Old-format configs produce clear error | Actionable startup error message |
| SC-007 | `--migrate-config` converts old to new | Tested on all .local/config/*.toml |

## 8. Agent Boundaries

### Always (without asking)
- Run `cargo +nightly fmt --check`, `cargo clippy --workspace --features full -- -D warnings`, `cargo nextest run --config-file .github/nextest.toml --workspace --features full --lib --bins` before committing
- Preserve all existing test coverage
- Update `config/default.toml` to use new format
- Update `--init` wizard and `--migrate-config`

### Ask First
- Removing any field that has env-var override support (need to update env.rs)
- Changing the `ProviderKind` enum variants (used across many crates)

### Never
- Delete the backward-compatibility shim before the next minor release
- Change vault secret resolution logic
- Modify non-LLM config sections in this PR

## 9. Implementation Approach

### New Config Types (non-breaking)

**Goal**: Add `ProviderEntry`, `RoutingStrategy`, and new `LlmConfig` alongside
old types. No behavior change.

**Files**:
- `crates/zeph-config/src/providers.rs` — add `ProviderEntry`, `RoutingStrategy`
- `crates/zeph-config/src/lib.rs` — re-export new types

**Acceptance**:
- New types compile and have `Default` impls
- Old code untouched
- All tests pass

### Migration Tooling

**Goal**: `--migrate-config` converts old-format LLM sections to new format. Old-format configs produce a clear startup error.

**Files**:
- `crates/zeph-config/src/migrate.rs` — add `migrate_llm_to_providers()`
- `crates/zeph-config/src/providers.rs` — add detection of old-format keys + helpful error message
- `config/default.toml` — rewrite to new format

**Acceptance**:
- `--migrate-config` on each `.local/config/*.toml` produces valid new-format TOML
- Starting with old-format config prints actionable error: "run --migrate-config"
- Migrated configs load and produce identical runtime behavior
- `--init` wizard generates new-format config

### Bootstrap Unification

**Goal**: Replace parallel `named_*`/`pcfg_*`/`build_sub_provider` functions
with a single `build_provider_from_entry()`.

**Files**:
- `crates/zeph-core/src/bootstrap/provider.rs` — rewrite

**Acceptance**:
- `create_provider()` dispatches on `RoutingStrategy` instead of `ProviderKind`
- Single `build_provider_from_entry(&ProviderEntry, &Config)` function
- All tests pass
- Live session test with orchestrator config (mandatory per LLM serialization gate)

### Env Override Update

**Goal**: Update `ZEPH_LLM_*` env overrides to work with new format.

**Files**:
- `crates/zeph-config/src/env.rs`

**Acceptance**:
- `ZEPH_LLM_PROVIDER` sets `providers[0].type` (or errors if ambiguous)
- `ZEPH_LLM_MODEL` sets `providers[0].model`
- `ZEPH_LLM_BASE_URL` sets `providers[0].base_url`
- All env-override tests pass

### Cleanup

**Goal**: Remove old config types and legacy shim.

**Files**:
- `crates/zeph-config/src/providers.rs` — remove old structs
- `crates/zeph-config/src/providers_legacy.rs` — delete
- `crates/zeph-core/src/config.rs` — remove old re-exports

**Acceptance**:
- No `CloudLlmConfig`, `OpenAiConfig`, `GeminiConfig`, `OllamaConfig`,
  `OrchestratorConfig`, `RouterConfig` types remain
- All tests pass
- Config default.toml uses only new format

## 10. Risks and Mitigations

| Risk | Impact | Probability | Mitigation |
|------|--------|-------------|------------|
| Custom deserializer bugs cause silent data loss | High | Medium | Extensive round-trip tests; snapshot tests on all .local/config/*.toml |
| Env override semantics become ambiguous with multi-provider | Medium | Medium | Env overrides only affect the default provider; document clearly |
| LLM serialization gate failures after bootstrap rewrite | High | Low | Live session test required per spec; test with all provider types |
| Migration tool corrupts user configs | High | Low | Backup before migrate; dry-run mode |

## 11. References

- Current config structs: `crates/zeph-config/src/providers.rs`
- Bootstrap provider construction: `crates/zeph-core/src/bootstrap/provider.rs`
- Config loading spec: `.local/specs/020-config-loading/spec.md`
- LLM providers spec: `.local/specs/003-llm-providers/spec.md`
- Constitution: `.local/specs/constitution.md` (Section I, II, IV)

---

## PILOT: LinUCB Bandit Routing

> **Issue**: #2230

### Overview

`routing = "bandit"` activates `LlmRoutingStrategy::Bandit`, which uses a
**LinUCB contextual bandit** to select among the provider pool based on a learned
context-reward relationship. Unlike EMA (latency-only) or cascade (quality gate),
the bandit jointly optimizes for quality, cost, and latency by learning which
context features predict provider success.

### Config

```toml
[llm]
routing = "bandit"

[llm.router.bandit]
alpha = 1.0                         # exploration / exploitation trade-off
dim = 64                            # context feature dimension
cost_weight = 0.3                   # relative weight of cost in reward
decay_factor = 0.99                 # per-round reward decay (temporal forgetting)
embedding_provider = "fast"         # provider used to embed context into feature vectors
embedding_timeout_ms = 500          # timeout for context embedding call
cache_size = 256                    # LRU cache for context embedding vectors

[[llm.providers]]
name = "fast"
type = "openai"
model = "gpt-4o-mini"
embed = true

[[llm.providers]]
name = "quality"
type = "claude"
model = "claude-sonnet-4-6"
```

### Bandit State Persistence

The LinUCB weight matrix and reward history are persisted to
`~/.config/zeph/router_bandit_state.json` after each round. On startup, the
bandit warm-starts from this file if it exists, preserving learned preferences
across agent sessions.

| File | Purpose |
|------|---------|
| `~/.config/zeph/router_bandit_state.json` | Per-provider LinUCB matrices (`A`, `b`) and round counter |

State file is written atomically (temp file + rename) to prevent corruption on crash.

### BanditConfig Fields

| Field | Type | Default | Notes |
|-------|------|---------|-------|
| `alpha` | `f64` | `1.0` | UCB confidence bound width |
| `dim` | `usize` | `64` | Context embedding dimension |
| `cost_weight` | `f64` | `0.3` | Cost component of reward `[0, 1]` |
| `decay_factor` | `f64` | `0.99` | Reward decay per round `[0, 1]` |
| `embedding_provider` | `Option<String>` | `None` | Provider for context embedding; falls back to embed provider |
| `embedding_timeout_ms` | `u64` | `500` | Context embedding timeout |
| `cache_size` | `usize` | `256` | LRU cache for context vectors |

### Key Invariants

- State file writes are atomic (temp + rename) — a crash mid-write must not produce a corrupt file
- When state file is missing or corrupt, the bandit starts from a uniform prior — hard error is not acceptable
- `embedding_timeout_ms` is a hard timeout; if exceeded, fall back to the default provider for that round
- `alpha`, `cost_weight`, `decay_factor` are validated at bootstrap; values outside `[0.0, ∞)` / `[0.0, 1.0]` are a config error
- `dim` must match the embedding dimension of the configured `embedding_provider` — mismatch causes a bootstrap error
- `LlmRoutingStrategy::Bandit` requires `[llm.router.bandit]` to be present; its absence is a hard bootstrap error
- NEVER persist bandit state to the project directory — always `~/.config/zeph/`

---

## BaRP: Cost-Weight Dial


`cost_weight` penalises UCB arm scores during provider selection in addition to the existing reward-signal penalty. Higher values bias the bandit toward cheaper providers at inference time. Static cost tier heuristics based on provider name and model identifier.

| Field | Type | Range | Notes |
|-------|------|-------|-------|
| `cost_weight` | `f64` | `[0.0, 1.0]` | Cost penalty in arm score; `0.0` = no cost bias |

`cost_weight` is clamped to `[0.0, 1.0]` at bootstrap. Added to `[llm.router.bandit]`.

### Key Invariants

- `cost_weight = 0.0` fully disables cost penalty — operator intent is respected without code path changes
- Cost tier heuristics are static (provider/model name matching) — never query a pricing API at runtime

---

## MAR: Memory-Augmented Routing


When the top-1 semantic recall score for the current query meets or exceeds `memory_confidence_threshold`, the bandit biases toward fast/cheap providers. Signal propagated from `SemanticMemory::recall` through `ContextSlot::SemanticRecall` to `RouterProvider`.

| Field | Type | Default | Notes |
|-------|------|---------|-------|
| `memory_confidence_threshold` | `f64` | `0.9` | Recall score at which MAR bias activates |

### Key Invariants

- MAR has no effect when `cost_weight = 0.0` — operator intent respected
- `ContextSlot::SemanticRecall` is the only channel for propagating recall confidence to the router
- Routing bias is a soft preference — bandit may still select a non-cheap provider based on UCB
