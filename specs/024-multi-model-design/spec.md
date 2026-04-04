# Spec: Multi-Model Design Principle

> **Status**: Approved (architectural standard, applies to all future work)
> **Date**: 2026-03-26
> **Crates**: `zeph-llm`, `zeph-core`, `zeph-memory`, `zeph-skills`, `zeph-tools`

## 1. Overview

### Problem Statement

Subsystems that call LLMs (graph extraction, compaction, orchestration planner,
STT, embeddings, skill generation, etc.) are hardcoded to use the main/default
provider. This couples complex expert tasks and cheap classification tasks to the
same model, ignoring the cost–quality tradeoff. New features that add LLM calls
have no clear contract on which provider to use or how to make it configurable.

### Goal

Every subsystem that calls an LLM MUST expose a `*_provider` config field that
references a named entry in `[[llm.providers]]`. All providers are declared once;
subsystems reference them by name. No model name or provider config is ever
inlined in a subsystem config section.

### Out of Scope

- The `routing` strategy selection (thompson / triage / cascade) — covered in
  `022-config-simplification/spec.md` and `023-complexity-triage-routing/spec.md`.
- Dynamic runtime model switching (`/provider` command) — covered in `003-llm-providers/spec.md`.
- Prompt caching and structured output capabilities — covered in `003-llm-providers/spec.md`.

---

## 2. Design Principle

### Complexity Tiers

All LLM calls are classified into four tiers based on task complexity and cost:

| Tier | Use Cases | Target Model Class |
|------|-----------|--------------------|
| `simple` | Entity extraction, keyword classification, yes/no decisions, routing decisions, STT transcription | Fast and cheap (e.g., `gpt-4o-mini`, `qwen3:8b`) |
| `medium` | Summarization, compaction, skill generation, tool orchestration, semantic ranking | Mid-tier (e.g., `gpt-4o`, `qwen3:30b-a3b`) |
| `complex` | Planning, code generation, multi-step reasoning, response verification | High-capability (e.g., `gpt-5.4`) |
| `expert` | Deep analysis, novel architecture decisions, long-horizon reasoning | Best available (e.g., `gpt-5.4`, `claude-opus-4`) |

### Provider Reference Pattern

All providers are declared once in `[[llm.providers]]` with a `name` field.
Subsystems reference a provider by name using a single `*_provider` field.
When the field is empty or absent, the subsystem falls back to the default
(first) provider in the pool.

```toml
# Declare providers once
[[llm.providers]]
name = "fast"
type = "openai"
model = "gpt-4o-mini"

[[llm.providers]]
name = "quality"
type = "openai"
model = "gpt-5.4"

# Subsystems reference by name — one line each
[memory.graph]
extract_provider = "fast"        # entity extraction → simple tier

[memory.compression]
compaction_provider = "fast"     # summarization/compaction → medium tier

[orchestration]
planner_provider = "quality"     # planning → complex tier
```

---

## 3. Functional Requirements

| ID | Requirement | Priority |
|----|------------|----------|
| FR-001 | EVERY subsystem that calls an LLM SHALL expose a `*_provider` config field that accepts a provider name from `[[llm.providers]]` | must |
| FR-002 | WHEN `*_provider` is empty or absent THE SYSTEM SHALL fall back to the main (first) provider in the pool | must |
| FR-003 | THE SYSTEM SHALL NEVER hardcode a model name, base URL, or provider type inside a subsystem config section | must |
| FR-004 | WHEN an unknown `*_provider` name is referenced THE SYSTEM SHALL log a warning and fall back to the main provider | must |
| FR-005 | THE SYSTEM SHALL expose `*_provider` fields for at minimum: graph extraction, compaction, orchestration planning, STT, embeddings, skill generation, probing | must |
| FR-006 | WHEN a subsystem's task maps clearly to a complexity tier, the default value for `*_provider` SHOULD reference a provider appropriate for that tier | should |
| FR-007 | THE SYSTEM SHALL document each `*_provider` field in the relevant config struct with the recommended tier | should |

---

## 4. Subsystem Provider Mapping

Required `*_provider` fields per subsystem (baseline set):

| Subsystem | Config field | Default tier | Crate |
|-----------|-------------|-------------|-------|
| Graph entity extraction | `[memory.graph] extract_provider` | simple | `zeph-memory` |
| Memory compaction | `[memory.compression] compaction_provider` | medium | `zeph-memory` |
| Summarization | `[memory.compression] summarize_provider` | medium | `zeph-memory` |
| Compaction probe | `[memory.compression] probe_provider` | simple | `zeph-memory` |
| Orchestration planner | `[orchestration] planner_provider` | complex | `zeph-core` |
| Skill generation | `[skills.learning] generate_provider` | medium | `zeph-skills` |
| STT transcription | `[llm.stt] provider_name` → or unified via `[[llm.providers]]` | simple | `zeph-llm` |
| Embeddings | via `[[llm.providers]] embedding_model` on selected entry | simple | `zeph-llm` |
| Response verification | `[agent] verify_provider` | complex | `zeph-core` |

---

## 5. STT Unification (Issue #2175)

The current `[llm.stt]` section is a standalone config block with its own
`provider`, `model`, `base_url`, and `language` fields. This duplicates provider
configuration that already exists in `[[llm.providers]]`.

### Goal

Unify STT under the `[[llm.providers]]` registry:

```toml
[[llm.providers]]
name = "stt"
type = "openai"
model = "gpt-4o-mini-transcribe"
base_url = "https://api.openai.com/v1"

[llm.stt]
provider_name = "stt"   # references [[llm.providers]] name
language = "auto"
```

### Requirements

| ID | Requirement | Priority |
|----|------------|----------|
| STT-001 | WHEN `[llm.stt] provider_name` is set THE SYSTEM SHALL resolve the STT provider from `[[llm.providers]]` by name | must |
| STT-002 | WHEN `provider_name` is empty THE SYSTEM SHALL fall back to the legacy inline `provider`/`model`/`base_url` fields for backward compatibility | must |
| STT-003 | WHEN `--migrate-config` is run THE SYSTEM SHALL convert legacy `[llm.stt]` inline fields to `provider_name` reference if a matching provider exists in `[[llm.providers]]` | should |
| STT-004 | THE SYSTEM SHALL preserve `language` and other non-provider fields in `[llm.stt]` | must |

---

## 6. Known Issues

| Issue | Description | Severity |
|-------|-------------|----------|
| #2173 | `/provider status` shows provider type ("openai") instead of configured `name` field ("fast", "mini2") | Medium |
| #2174 | Tool schema filter (semantic tool selection) is disabled when triage router is active — triage router does not expose `embed()` | High |
| #2175 | STT is not unified under `[[llm.providers]]` — requires separate `base_url`/`model` duplication | Medium |

---

## 7. Key Invariants

```
NEVER inline a model name or provider config inside a subsystem config section.
NEVER skip *_provider for a new subsystem that calls an LLM.
NEVER resolve a provider by type ("openai") — always by configured name.
ALWAYS fall back to the main provider when *_provider is empty.
ALWAYS use [[llm.providers]] as the single registry for all provider declarations.
```

---

## 8. Agent Boundaries

### Always (without asking)
- Add `*_provider` field to any new subsystem that makes LLM calls
- Fall back to main provider when field is empty
- Reference providers by `name` field from `[[llm.providers]]`

### Ask First
- Renaming an existing `*_provider` field (may break configs)
- Changing the default fallback tier for an existing subsystem
- Removing the legacy `[llm.stt]` inline fields before migration tooling exists

### Never
- Inline model name or base URL in a subsystem config section
- Add a new `[llm.*]` config section with its own provider credentials
- Bypass the `[[llm.providers]]` registry for provider resolution

---

## 9. References

- `022-config-simplification/spec.md` — `[[llm.providers]]` unified registry design
- `023-complexity-triage-routing/spec.md` — ComplexityTier, TriageRouter
- `003-llm-providers/spec.md` — LlmProvider trait, AnyProvider, capabilities
- CLAUDE.md `## Multi-Model Design Principle` — project-level rule (applies to all new code)
- Issue #2173: `/provider` name display bug
- Issue #2174: tool schema filter disabled in triage mode
- Issue #2175: STT unification
