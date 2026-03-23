# zeph-config

Pure-data configuration types, TOML loader, environment variable overrides, and migration helpers for Zeph.

Extracted from `zeph-core` in epic #1973 (Phase 1a/1b). `zeph-core` re-exports all public types via `pub use` for backward compatibility.

## Purpose

`zeph-config` owns every configuration struct and enum used across the workspace. It provides:

- All TOML configuration types (`Config`, `AgentConfig`, `LlmConfig`, `MemoryConfig`, etc.)
- TOML file loading with environment variable overrides (`ZEPH_*` prefixes)
- Default value helpers and legacy-path detection
- Config migration (`--migrate-config`) so existing configs can be upgraded without manual editing

No runtime logic lives in this crate — it is pure data plus serialization. Vault secret resolution is handled by `zeph-vault` and `zeph-core`.

## Key Types

| Type | Description |
|------|-------------|
| `Config` | Root configuration struct, deserialized from `config.toml` |
| `ResolvedSecrets` | Resolved API keys and secrets after vault lookup |
| `AgentConfig` | Agent loop settings: model, system prompt, context budget, compaction |
| `LlmConfig` | Provider selection and provider-specific params |
| `MemoryConfig` | SQLite path, Qdrant URL, semantic search settings, graph memory |
| `SkillsConfig` | Skills directory, prompt mode, hot-reload |
| `SecurityConfig` | Timeout, trust, sandbox, and content isolation configuration |
| `VaultConfig` | Vault backend selection (env or age) and file paths |
| `ContentIsolationConfig` | Sanitization pipeline settings (max size, spotlighting, injection detection) |
| `ExperimentConfig` | Autonomous experiment engine settings |
| `SubAgentConfig` | Subagent defaults: tool policy, memory scope, permission mode |
| `TuiConfig` | TUI dashboard settings |
| `AcpConfig` | ACP server settings: transports, max sessions, idle timeout |

## Modules

| Module | Contents |
|--------|----------|
| `root` | Top-level `Config` struct and `ResolvedSecrets` |
| `agent` | `AgentConfig`, `FocusConfig`, `SubAgentConfig`, `SubAgentLifecycleHooks` |
| `providers` | All LLM provider configs — unified `ProviderEntry` list (`[[llm.providers]]`) |
| `memory` | `MemoryConfig`, `SemanticConfig`, `GraphConfig`, `CompressionConfig` |
| `features` | Feature-specific configs: `DebugConfig`, `GatewayConfig`, `SchedulerConfig`, `VaultConfig` |
| `security` | `SecurityConfig`, `TimeoutConfig`, `TrustConfig` |
| `sanitizer` | `ContentIsolationConfig`, `PiiFilterConfig`, `ExfiltrationGuardConfig`, `QuarantineConfig` |
| `subagent` | `HookDef`, `HookMatcher`, `HookType`, `MemoryScope`, `PermissionMode`, `ToolPolicy` |
| `ui` | `AcpConfig`, `TuiConfig`, `AcpTransport` |
| `channels` | `TelegramConfig`, `DiscordConfig`, `SlackConfig`, `McpConfig`, `A2aServerConfig` |
| `logging` | `LoggingConfig`, `LogRotation` |
| `learning` | `LearningConfig`, `DetectorMode` |
| `experiment` | `ExperimentConfig`, `ExperimentSchedule`, `OrchestrationConfig` |
| `loader` | `load_config()` — reads TOML file and applies `ZEPH_*` env overrides |
| `env` | Environment variable override logic |
| `migrate` | `--migrate-config` migration steps |
| `defaults` | Default path helpers and legacy path detection |

## Feature Flags

| Feature | Default | Description |
|---------|---------|-------------|
| `guardrail` | off | Enables `GuardrailConfig`, `GuardrailAction`, `GuardrailFailStrategy` |
| `lsp-context` | off | Enables `LspConfig`, `DiagnosticsConfig`, `HoverConfig`, `DiagnosticSeverity` |
| `compression-guidelines` | off | Enables compression failure strategy in `MemoryConfig` |
| `experiments` | off | Enables `ExperimentConfig` fields that require `ordered-float` |
| `policy-enforcer` | off | Enables policy enforcer configuration in `SecurityConfig` |

## Integration with zeph-core

`zeph-core` depends on `zeph-config` and re-exports all config types at the crate root:

```rust
// In your code, both of these resolve to the same type:
use zeph_config::Config;
use zeph_core::Config; // re-exported
```

The `AppBuilder::from_env()` bootstrap function calls `zeph_config::loader::load_config()` to read the TOML file, then passes the resulting `Config` to downstream subsystems.

## Common Use Cases

### Loading a configuration file

```rust
use zeph_config::loader::load_config;

let config = load_config(Some("config.toml"))?;
println!("Model: {}", config.llm.model);
```

### Building a config for tests

```rust
use zeph_config::{Config, AgentConfig};

let config = Config {
    agent: AgentConfig {
        model: "qwen3:8b".into(),
        ..Default::default()
    },
    ..Default::default()
};
```

### Accessing content isolation settings

```rust
use zeph_config::ContentIsolationConfig;

let iso = ContentIsolationConfig::default();
assert!(iso.enabled);
assert_eq!(iso.max_content_size, 65_536);
```

## Source Code

[`crates/zeph-config/`](https://github.com/bug-ops/zeph/tree/main/crates/zeph-config)
