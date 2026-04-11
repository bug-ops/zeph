---
aliases:
  - Config Schema
  - TOML Configuration Schema
  - Configuration Reference
tags:
  - sdd
  - spec
  - config
  - cross-cutting
created: 2026-04-11
status: approved
related:
  - "[[MOC-specs]]"
  - "[[constitution]]"
  - "[[020-config-loading/spec]]"
  - "[[022-config-simplification/spec]]"
  - "[[029-feature-flags/spec]]"
---

# Spec: Configuration Schema (`zeph-config`)

> [!info]
> Canonical reference for the Zeph TOML configuration schema. Documents all
> top-level sections, their types, optionality, defaults, validation rules, and
> env-var override table. The loading order and secret resolution are in
> [[020-config-loading/spec]]; provider registry format is in [[022-config-simplification/spec]].

## Sources

### Internal
| File | Contents |
|---|---|
| `crates/zeph-config/src/root.rs` | `Config` struct — top-level schema |
| `crates/zeph-config/src/loader.rs` | `Config::load`, env-var override application |
| `crates/zeph-config/src/migrate.rs` | `ConfigMigrator` — `--migrate-config` |
| `crates/zeph-config/src/defaults.rs` | Default values for optional fields |
| `crates/zeph-config/src/env.rs` | Env-var override map |
| `crates/zeph-config/src/error.rs` | `ConfigError` typed enum |

---

## 1. Overview

### Responsibility

`zeph-config` owns all configuration type definitions and the TOML loader.
It is a **Layer 0** crate — no `zeph-*` crate dependencies except `zeph-common`
(for the `Secret` wrapper). Vault secret resolution happens in `zeph-core`
via the `SecretResolver` trait and is never part of this crate.

### What this spec covers

- Top-level `Config` section inventory
- Required vs optional sections and their defaults
- Validation rules enforced by `Config::validate()`
- Environment variable override table
- Migration mechanism

### Out of Scope

- Config file resolution order → [[020-config-loading/spec]]
- `[[llm.providers]]` schema → [[022-config-simplification/spec]]
- Feature flag definitions → [[029-feature-flags/spec]]

---

## 2. Top-Level Schema

All sections map to named fields on `Config`. Sections marked *optional* use
`Option<T>` in Rust and may be omitted from TOML. Sections marked *defaulted*
use `#[serde(default)]` — they may be omitted; defaults apply automatically.

| TOML Section | Type | Presence | Notes |
|---|---|---|---|
| `[agent]` | `AgentConfig` | required | Core agent behaviour |
| `[llm]` + `[[llm.providers]]` | `LlmConfig` | required | Provider registry; see spec #022 |
| `[skills]` | `SkillsConfig` | required | Skill paths and matching config |
| `[memory]` | `MemoryConfig` | required | SQLite + Qdrant configuration |
| `[telegram]` | `TelegramConfig` | optional | Telegram channel (feature: telegram) |
| `[discord]` | `DiscordConfig` | optional | Discord channel (feature: discord) |
| `[slack]` | `SlackConfig` | optional | Slack channel (feature: slack) |
| `[tools]` | `ToolsConfig` | defaulted | Shell executor, web scraper, filters |
| `[a2a]` | `A2aServerConfig` | defaulted | A2A protocol server |
| `[mcp]` | `McpConfig` | defaulted | MCP client configuration |
| `[index]` | `IndexConfig` | defaulted | Code indexing (feature: index) |
| `[vault]` | `VaultConfig` | defaulted | Vault backend selection |
| `[security]` | `SecurityConfig` | defaulted | Injection defense, SSRF |
| `[timeouts]` | `TimeoutConfig` | defaulted | Per-operation timeout limits |
| `[cost]` | `CostConfig` | defaulted | Budget limits, cost tracking |
| `[observability]` | `ObservabilityConfig` | defaulted | Legacy observability settings |
| `[gateway]` | `GatewayConfig` | defaulted | HTTP gateway (feature: gateway) |
| `[daemon]` | `DaemonConfig` | defaulted | Daemon mode settings |
| `[scheduler]` | `SchedulerConfig` | defaulted | Cron scheduler (feature: scheduler) |
| `[tui]` | `TuiConfig` | defaulted | TUI dashboard settings |
| `[acp]` | `AcpConfig` | defaulted | ACP transport settings |
| `[agents]` | `SubAgentConfig` | defaulted | Subagent defaults and limits |
| `[orchestration]` | `OrchestrationConfig` | defaulted | DAG planner settings |
| `[classifiers]` | `ClassifiersConfig` | defaulted | ML classifier thresholds |
| `[experiments]` | `ExperimentConfig` | defaulted | Experimental feature toggles |
| `[debug]` | `DebugConfig` | defaulted | Debug output and dump settings |
| `[logging]` | `LoggingConfig` | defaulted | Log level, structured output |
| `[hooks]` | `HooksConfig` | defaulted | Reactive file/cwd event hooks |
| `[lsp]` | `LspConfig` | defaulted | LSP integration settings |
| `[magic_docs]` | `MagicDocsConfig` | defaulted | Auto-maintained markdown docs |
| `[telemetry]` | `TelemetryConfig` | defaulted | Profiling and tracing (spec #035) |
| `[metrics]` | `MetricsConfig` | defaulted | Prometheus metrics (spec #036) |

### Key sections: agent

```toml
[agent]
name = "Zeph"                        # display name in prompts
max_tool_iterations = 10             # max tool calls per turn
max_tool_retries = 2                 # retries for failed tool calls
tool_repeat_threshold = 2            # consecutive identical tool calls before bail
max_retry_duration_secs = 30         # total retry budget per tool call
instruction_auto_detect = true       # auto-load AGENT.md from cwd
instruction_files = []               # explicit instruction file paths
auto_update_check = true             # check for new Zeph versions
budget_hint_enabled = true           # inject context budget hints
```

### Key sections: memory

```toml
[memory]
sqlite_path = "~/.local/share/zeph/zeph.db"  # SQLite database path
qdrant_url = "http://localhost:6334"          # Qdrant gRPC endpoint
history_limit = 100                           # max turns retained in session
embedding_dimensions = 1536                   # must match embedding model output
```

---

## 3. Environment Variable Overrides

Applied after TOML load in `Config::load`. Override values are coerced to the
target field's type. Validation runs on the merged result.

| Variable | Field Overridden |
|---|---|
| `ZEPH_LLM_PROVIDER` | `llm.providers[0].provider_type` |
| `ZEPH_LLM_MODEL` | `llm.providers[0].model` |
| `ZEPH_SQLITE_PATH` | `memory.sqlite_path` |
| `ZEPH_QDRANT_URL` | `memory.qdrant_url` |
| `ZEPH_LOG_LEVEL` | `logging.level` |
| `ZEPH_AGENT_NAME` | `agent.name` |
| `ZEPH_VAULT_BACKEND` | `vault.backend` |
| `ZEPH_GATEWAY_PORT` | `gateway.port` |
| `ZEPH_GATEWAY_BIND` | `gateway.bind` |
| `ZEPH_TUI_ENABLED` | *(binary flag, not config field)* |

**Secret env-vars are NOT part of the config schema.** `ZEPH_CLAUDE_API_KEY`,
`ZEPH_OPENAI_API_KEY`, etc. are resolved exclusively through the vault.
Never use `ZEPH_VAULT_BACKEND=env` — see [[010-security/010-1-vault]].

---

## 4. Validation Rules

`Config::validate()` enforces these constraints after loading and env-var
application. Violations return `ConfigError::Validation`.

| Rule | Checked Field | Constraint |
|---|---|---|
| History limit | `memory.history_limit` | ≥ 1 |
| Tool iterations | `agent.max_tool_iterations` | ≥ 1, ≤ 100 |
| Retry duration | `agent.max_retry_duration_secs` | ≥ 0 |
| Embedding dimensions | `memory.embedding_dimensions` | 128 ≤ d ≤ 4096 |
| Qdrant URL | `memory.qdrant_url` | valid URL, non-empty |
| Provider list | `llm.providers` | at least one entry |
| Provider name | `providers[*].name` | unique, non-empty |
| Gateway port | `gateway.port` | 1–65535 |
| Cost budget | `cost.max_usd_per_session` | ≥ 0.0 (0 = unlimited) |
| Timeout values | `timeouts.*` | ≥ 0, 0 = disabled |

---

## 5. Migration Mechanism

`ConfigMigrator::migrate(toml: &str) -> Result<MigrationResult>` in
`crates/zeph-config/src/migrate.rs`.

**How it works:**
1. Parse existing TOML to identify present top-level keys.
2. For each known section not present in input, append the section as
   commented-out defaults with a `# Added by migrate-config` comment.
3. Preserve all existing values verbatim — no mutation of user config.
4. Return `MigrationResult { output: String, added_count: usize, changes: Vec<String> }`.

**Invariants:**
- WHEN a section is already present THE SYSTEM SHALL NOT modify its values.
- WHEN a new parameter is added within an existing section THE SYSTEM SHALL append the parameter as a commented-out entry inside the section.
- Migration is **non-destructive** — output is never shorter than input (ignoring whitespace).
- Migration is **idempotent** — running twice produces the same output.

**CLI integration:**
```
zeph --migrate-config [--config path]
```
Writes the migrated TOML back to the config file path.

---

## 6. Key Invariants

### Always
- `Config::load` MUST call `Config::validate()` before returning — callers receive a pre-validated config or an error.
- Env-var overrides MUST be applied after TOML deserialization and before validation.
- Secret fields (`ResolvedSecrets`) MUST use `#[serde(skip)]` — never serialized to TOML.
- New config sections MUST implement `Default` and use `#[serde(default)]` unless the section is semantically required.

### Ask First
- Adding a required (non-defaulted) section — breaks existing configs without migration step.
- Renaming a section key — requires migration step and CHANGELOG entry.
- Adding `validate()` constraints to existing fields — may reject previously-valid configs.

### Never
- Store secrets or API keys as plain strings in any config struct.
- Use `serde_yaml` or `serde_yml` — TOML only for config files.
- Add `ZEPH_VAULT_BACKEND=env` as a documented or supported pattern.

---

## 7. Adding New Config Sections

Checklist for adding a new config section to `Config`:

1. Create `crates/zeph-config/src/<section>.rs` with `#[derive(Debug, Deserialize, Serialize)]` struct.
2. Implement `Default` with sensible defaults.
3. Add `#[serde(default)] pub <section>: <SectionConfig>` to `Config` struct in `root.rs`.
4. Add validation rules to `Config::validate()` if the section has numeric bounds.
5. Add migration support in `migrate.rs` so `--migrate-config` surfaces new options.
6. Add any env-var overrides to `env.rs`.
7. Add `--init` wizard prompts if the section is commonly customized.
8. Update this spec (037) with the new section in the table above.

---

## 8. See Also

- [[020-config-loading/spec]] — config file resolution order, mode-agnostic defaults
- [[022-config-simplification/spec]] — `[[llm.providers]]` canonical format and routing
- [[029-feature-flags/spec]] — feature flags that gate optional config sections
- [[010-security/010-1-vault]] — vault backend and secret resolution
- [[constitution]] — project-wide rules (no secrets in config, TOML only)
