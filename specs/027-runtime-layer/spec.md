---
aliases:
  - RuntimeLayer
  - Hooks
  - Middleware
tags:
  - sdd
  - spec
  - core
  - hooks
  - runtime
  - contract
created: 2026-04-08
status: approved
related:
  - "[[MOC-specs]]"
  - "[[028-hooks/spec]]"
  - "[[001-system-invariants/spec#15. RuntimeLayer Contract]]"
  - "[[026-tui-subagent-management/spec]]"
---

# Spec: RuntimeLayer Middleware

> **Crate**: `zeph-core`

## 1. Overview

### Problem Statement

Cross-cutting behavior (logging, metrics, rate limiting, audit, token tracking)
was duplicated across the agent loop, tool executor, and channel adapters.
Each subsystem implemented its own version of the same pre/post hooks with no
shared contract, making it hard to add new behaviors consistently.

### Goal

Introduce a `RuntimeLayer` trait that defines `before_chat`, `after_chat`,
`before_tool`, and `after_tool` hooks. The agent loop calls these hooks
unconditionally at the appropriate points. New cross-cutting concerns are added
as `RuntimeLayer` implementations without touching the agent loop.

### Out of Scope

- Channel-level middleware (handled by `AnyChannel` dispatch)
- LLM provider interceptors (handled at `AnyProvider` level)
- Async streaming interception (hooks fire on full message, not per-chunk)

---

## 2. Trait Definition

```rust
// crates/zeph-core/src/runtime_layer.rs

/// Cross-cutting hooks invoked by the agent loop around LLM calls and tool execution.
///
/// All methods have default no-op implementations so that only relevant hooks
/// need to be overridden. The trait is object-safe; layers are stored as
/// `Arc<dyn RuntimeLayer>` in the agent.
pub trait RuntimeLayer: Send + Sync + 'static {
    /// Called immediately before the LLM `chat` / `chat_with_tools` call.
    ///
    /// `ctx.conversation_id`: the current session ID.
    /// `ctx.turn_number`: monotonically increasing turn counter, starting at 1.
    fn before_chat(&self, _ctx: &LayerContext) {}

    /// Called immediately after the LLM response is received (success or error).
    ///
    /// `result`: `Ok(())` on success, `Err(description)` on LLM error.
    fn after_chat(&self, _ctx: &LayerContext, _result: Result<(), &str>) {}

    /// Called immediately before a tool is executed.
    ///
    /// `tool_name`: the tool being invoked.
    fn before_tool(&self, _ctx: &LayerContext, _tool_name: &str) {}

    /// Called immediately after a tool execution completes.
    ///
    /// `tool_name`: the tool that was invoked.
    /// `result`: `Ok(())` on success, `Err(description)` on tool error.
    fn after_tool(&self, _ctx: &LayerContext, _tool_name: &str, _result: Result<(), &str>) {}
}
```

### NoopLayer

`NoopLayer` is the zero-cost default implementation:

```rust
pub struct NoopLayer;

impl RuntimeLayer for NoopLayer {}
```

Used when no layers are configured. The agent holds `Arc<dyn RuntimeLayer>`; with
`NoopLayer`, all calls are inlined and optimized away by the compiler.

### LayerContext

```rust
/// Contextual data passed to every `RuntimeLayer` hook.
#[derive(Debug, Clone)]
pub struct LayerContext {
    /// The current conversation/session identifier.
    pub conversation_id: String,

    /// Monotonically increasing turn counter for this session. Starts at 1.
    pub turn_number: u64,
}
```

`LayerContext` is constructed once per agent session and updated in-place at
the start of each turn (incrementing `turn_number`). The agent does not clone
it per-call; hooks receive a shared reference.

---

## 3. Agent Integration

The agent holds `layer: Arc<dyn RuntimeLayer>`. At the appropriate points:

```rust
// Before LLM call:
self.layer.before_chat(&self.layer_ctx);

// After LLM call:
self.layer.after_chat(&self.layer_ctx, result_as_str_ref);

// Before tool execution:
self.layer.before_tool(&self.layer_ctx, tool_name);

// After tool execution:
self.layer.after_tool(&self.layer_ctx, tool_name, result_as_str_ref);
```

`turn_number` is incremented at the start of `process_user_message()`, before
any hooks fire.

---

## 4. Config

No dedicated config section. The `RuntimeLayer` implementation is selected at
bootstrap based on enabled features. Multiple layers can be composed via a
`CompositeLayer` that delegates to a `Vec<Arc<dyn RuntimeLayer>>`.

```toml
# No user-facing config currently.
# Future: [agent.layers] enabled = ["audit", "metrics"]
```

---

## 5. Key Invariants

- All hook methods have default no-op implementations — `RuntimeLayer` is opt-in behavior
- `LayerContext.turn_number` is incremented exactly once per user turn — never skip or double-increment
- Hook failures are non-fatal — panics in hook implementations are caught via `catch_unwind` and logged; the agent turn continues
- `before_chat` fires before the LLM call, `after_chat` fires after (not before result is injected into context)
- `before_tool` fires before executor dispatch, `after_tool` fires after the `Option<ToolOutput>` is resolved
- NEVER perform blocking I/O in layer hooks — all hooks are called synchronously in the async agent loop
- `Arc<dyn RuntimeLayer>` is the only storage form — never `Box<dyn RuntimeLayer>` in the agent struct

---

## 6. Agent Boundaries

### Always (without asking)
- Implement `Send + Sync + 'static` bounds on all `RuntimeLayer` implementations
- Default no-op for all unimplemented hooks
- Increment `turn_number` before any hook fires in a turn

### Ask First
- Adding new hook methods to `RuntimeLayer` (breaks all existing implementations unless defaulted)
- Adding fields to `LayerContext` (affects all hook signatures)

### Never
- Block the agent loop in a hook implementation
- Store mutable state without interior mutability (`Arc<Mutex<T>>` is acceptable)
- Call `before_chat` more than once per turn

---

## 6b. Plugin Config Overlay Merge

Issue #3145. Plugin config overlays (`<plugin>/.plugin.toml`) are merged into the live `Config` at bootstrap, before the agent starts. The merge is tighten-only:

| Key | Merge strategy |
|-----|---------------|
| `tools.shell.blocked_commands` | Union (grows monotonically) |
| `tools.shell.allowed_commands` | Intersection with base (base must be non-empty for intersection to narrow it) |
| `skills.disambiguation_threshold` | Max across all plugins |

`apply_plugin_config_overlays(config, plugins_dir)` is called from `AppBuilder` after the base config is loaded and before bootstrap completes. `ResolvedOverlay` is returned for diagnostic logging and `zeph plugin list` display.

### Install-Time Value Validation

Issue #3159. When a plugin is installed (via `zeph plugin install`), the values in `.plugin.toml` are validated against the safelisted keys. Invalid values (e.g., a `blocked_commands` entry that is not a valid command name, or `disambiguation_threshold` outside `[0.0, 1.0]`) cause the install to fail with a clear error message.

### Hot-Reload Behavior

`blocked_commands` changes (union of plugin overlays) take effect **immediately and atomically** on plugin overlay reload. `ShellExecutor` holds an `ArcSwap<ShellPolicy>` handle; `handle.rebuild()` swaps the policy without restarting the agent. `allowed_commands` changes still require a full agent restart and emit a `WARN` banner at reload time.

New types introduced in `zeph-tools` to support atomic reload:

| Type | Role |
|------|------|
| `ShellPolicy` | Immutable snapshot of blocked/allowed command rules |
| `ShellPolicyHandle` | `Arc`-wrapped `ArcSwap<ShellPolicy>` shared across `ShellExecutor`, `LifecycleState`, and `acp/daemon/runner` |
| `compute_blocked_commands` | Pure fn that rebuilds the policy from a `ResolvedOverlay` |

### Diagnostics: `skipped_plugins` and `source_plugins`

`ResolvedOverlay` surfaces two diagnostic fields:

- `source_plugins` — list of plugins that contributed to the merged overlay
- `skipped_plugins` — list of plugins skipped due to load/validation errors (non-fatal)

Both fields are exposed in:
- `zeph plugin list --overlay` (CLI)
- `/plugins overlay` TUI slash command
- `PluginListOverlay` TUI palette entry

### Plugin Manifest Integrity (sha256)

At install time, a sha256 digest of each `.plugin.toml` is computed and written to `<data_root>/.plugin-integrity.toml` (outside `plugins_dir` to prevent TOCTOU attacks). At startup and on every hot-reload, the digest is re-computed and compared against the stored value. Manifests whose digest does not match are rejected — the plugin is treated as if it were skipped and recorded in `skipped_plugins`.

### Key Invariants

- Plugin overlays are **tighten-only** — plugins cannot weaken security posture
- `allowed_commands` intersection: if the base config has no `allowed_commands` (empty = unrestricted), the intersection is a no-op — plugins cannot re-enable `DEFAULT_BLOCKED` commands
- `blocked_commands` hot-reload is **atomic** — `ArcSwap` swap is the only permitted update path; no restart required
- `allowed_commands` changes require restart — emit a `WARN` banner at reload time and do NOT apply dynamically
- `.plugin-integrity.toml` MUST reside outside `plugins_dir` — storing it inside `plugins_dir` would allow a plugin to tamper with its own digest
- Integrity check runs at both startup and hot-reload — a tampered manifest is always rejected, never silently accepted
- `plugins_dir` missing → silently treated as empty; `plugins_dir` exists but unreadable → `PluginError::Io`
- Per-plugin failures are recorded in `ResolvedOverlay::skipped_plugins` — a bad plugin skips, it does not abort the entire overlay
- Plugin I/O operations (reading `.plugin.toml`) run in `spawn_blocking` — never block the async runtime
- Value validation runs at install time, not at load time — invalid values should never reach `apply_plugin_config_overlays`

---

## 7. References

- `crates/zeph-core/src/runtime_layer.rs` — trait definition
- `crates/zeph-core/src/agent/mod.rs` — hook call sites
- `001-system-invariants/spec.md` — Agent Loop Contract (§2)
