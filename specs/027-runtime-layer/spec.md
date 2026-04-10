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

## 7. References

- `crates/zeph-core/src/runtime_layer.rs` — trait definition
- `crates/zeph-core/src/agent/mod.rs` — hook call sites
- `001-system-invariants/spec.md` — Agent Loop Contract (§2)
