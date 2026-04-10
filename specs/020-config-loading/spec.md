---
aliases:
  - Config Loading
  - Configuration
tags:
  - sdd
  - spec
  - config
  - core
created: 2026-04-08
status: approved
related:
  - "[[MOC-specs]]"
  - "[[001-system-invariants/spec#8. Configuration Contract]]"
  - "[[022-config-simplification/spec]]"
  - "[[029-feature-flags/spec]]"
---

# Spec: Config Loading

> [!info]
> Config resolution order, mode-agnostic defaults, environment overrides;
> integrates with [[022-config-simplification/spec|Provider Registry]].

## Sources

| Area | File |
|---|---|
| Resolution logic | `crates/zeph-core/src/bootstrap/config.rs` |
| AppBuilder | `crates/zeph-core/src/bootstrap/mod.rs` |
| CLI args | `src/cli.rs` |
| Mode dispatch | `src/runner.rs` |
| ACP server | `src/acp.rs` |

---

> Defines the canonical config resolution contract for all launch modes.
> TUI mode is the reference implementation. All other modes must behave identically.

## Key Invariant

**Config path resolution is mode-agnostic.** Whether the agent is launched in CLI, TUI,
ACP stdio, or ACP HTTP mode, the config file is always resolved by the same function
`resolve_config_path()` in `crates/zeph-core/src/bootstrap/config.rs`.

No launch mode may apply different defaults, additional search paths, or skip any step
in the resolution order.

## Resolution Order

```
1. --config <PATH>           (CLI flag, highest priority)
2. ZEPH_CONFIG               (environment variable)
3. config/default.toml       (relative to CWD, only when the file exists)
4. ~/.config/zeph/config.toml (XDG fallback, always the final default)
```

Step 3 (`config/default.toml`) is conditional: it is returned only when the file
exists at that path relative to the CWD at process startup. This preserves current
behavior for CLI/TUI launches from the project root while allowing the resolution chain
to continue to step 4 when the agent is launched from an unrelated directory (e.g., by
an IDE running ACP stdio/HTTP).

Step 4 (`~/.config/zeph/config.toml`) is the XDG fallback. The path is constructed via
`dirs::config_dir()` and is always returned as the final default, whether or not the
file exists. The caller is responsible for handling a missing config at that path.

A `tracing::debug!` message is emitted at each resolved step indicating which source
was used and the resolved path.

## Mode Coverage

| Mode | Entry point | Config loaded via |
|---|---|---|
| CLI | `src/runner.rs` → `AppBuilder::new()` | `resolve_config_path(cli.config)` |
| TUI | `src/runner.rs` → `AppBuilder::new()` | `resolve_config_path(cli.config)` (**reference**) |
| ACP stdio | `src/acp.rs` → `build_acp_deps()` → `AppBuilder::new()` | `resolve_config_path(config_path)` |
| ACP HTTP | `src/acp.rs` → `AppBuilder::new()` | `resolve_config_path(config_path)` |

All four paths call `AppBuilder::new(config_path, ...)` which internally calls
`resolve_config_path()`. No mode has its own config loading logic.

## Early Logging Bootstrap

Before any mode branch in `src/runner.rs`, config is loaded once more **only** to
extract logging settings:

```rust
let config_path = resolve_config_path(cli.config.as_deref());
let base_logging = Config::load(&config_path).map(|c| c.logging).unwrap_or_default();
```

This uses the same `resolve_config_path()` call. The resolved path is NOT cached from
this early load — `AppBuilder::new()` calls `resolve_config_path()` again independently.

## Functional Requirements

- WHEN `--config <PATH>` is provided, THE SYSTEM SHALL use that path regardless of mode.

- WHEN `ZEPH_CONFIG` is set and `--config` is not provided,
  THE SYSTEM SHALL use the env var path regardless of mode.

- WHEN neither flag nor env var is set and `config/default.toml` exists relative to CWD,
  THE SYSTEM SHALL load `config/default.toml` in all modes (preserves CLI/TUI behavior).

- WHEN `config/default.toml` does not exist relative to CWD,
  THE SYSTEM SHALL fall back to `~/.config/zeph/config.toml` as the final default.

- WHEN config file is not found at the resolved path,
  THE SYSTEM SHALL return an error — no further silent fallback occurs.

## Agent Boundaries

### Always (without asking)
- Use `resolve_config_path()` for every new launch mode or transport
- Pass `cli.config.as_deref()` (or equivalent) into `AppBuilder::new()` — never hardcode a path

### Ask First
- Adding a new config search path beyond the current 4-step chain
- Changing the default filename from `config/default.toml`

### Never
- Add mode-specific config resolution logic outside `resolve_config_path()`
- Hardcode config paths inside mode-specific code (`run_tui_agent`, `run_acp_server`, etc.)
- Silently fall back to another config when the resolved path is missing
