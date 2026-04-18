---
aliases:
  - Slash Commands
  - Command Registry
  - Command Dispatch
tags:
  - sdd
  - spec
  - commands
  - dispatch
created: 2026-04-13
status: approved
related:
  - "[[MOC-specs]]"
  - "[[constitution]]"
  - "[[001-system-invariants/spec]]"
  - "[[002-agent-loop/spec]]"
  - "[[030-tui-slash-autocomplete/spec]]"
---

# Spec: Slash Command Registry (`zeph-commands`)

> [!info]
> Non-generic slash command registry, handler trait, channel sink abstraction, and static
> command metadata used by `zeph-core` for `/command` dispatch. Does not depend on `zeph-core`.

## 1. Overview

### Problem Statement

The original agent loop used a generic `Agent<C: Channel>` parameter that propagated
through every slash command handler. Adding or modifying a handler required recompiling
`zeph-core`. Registering a new command and exposing its metadata (for `/help` and TUI
autocomplete) required touching the monolith.

### Goal

Provide a compile-time-isolated crate (`zeph-commands`) that owns the command registry,
handler trait, dispatch algorithm, and static command list. `zeph-core` wires concrete
subsystem traits into `CommandContext` at dispatch time; handlers operate on trait objects.

### Out of Scope

- Agent loop control flow (owned by `zeph-core`)
- Concrete LLM, memory, or tool calls inside handlers (delegated through trait objects)
- TUI rendering and autocomplete (consume `COMMANDS` from this crate but render in `zeph-tui`)
- Telegram-specific command routing

---

## 2. User Stories

### US-001: Command Dispatch

AS A `zeph-core` agent loop
I WANT to dispatch a user input string to the matching slash command handler
SO THAT the handler runs without the agent loop knowing handler-specific logic.

**Acceptance criteria:**

```
GIVEN a CommandRegistry with registered handlers
WHEN dispatch() is called with "/plan confirm foo"
THEN the handler registered as "/plan confirm" is selected (longest-word-boundary match)
AND args = "foo" is passed to the handler
AND the CommandOutput is returned to the agent loop
```

### US-002: Unique Registration

AS A developer registering a new handler
I WANT the registry to panic on duplicate command names at initialization
SO THAT accidental duplicate registration is caught early, not at dispatch time.

**Acceptance criteria:**

```
GIVEN a registry with "/plan" already registered
WHEN register() is called with another "/plan" handler
THEN the process panics with a message containing "duplicate command name"
```

### US-003: Help Metadata

AS A `/help` handler
I WANT to list all registered commands with category and argument hints
SO THAT users see a complete and grouped help output.

**Acceptance criteria:**

```
GIVEN a registry with N handlers
WHEN list() is called
THEN N CommandInfo structs are returned in registration order
AND each struct has a non-empty name, description, and category
```

### US-004: ChannelSink Abstraction

AS A command handler
I WANT to send messages to the user via a ChannelSink trait object
SO THAT the handler is not coupled to any concrete Channel type.

**Acceptance criteria:**

```
GIVEN a handler receiving a &mut dyn ChannelSink
WHEN the handler calls sink.send("text").await
THEN the message is delivered to the underlying channel without the handler knowing its type
```

---

## 3. Functional Requirements

| ID | Requirement | Priority |
|----|------------|----------|
| FR-001 | WHEN `CommandRegistry::dispatch()` receives input not starting with `/` THEN the system SHALL return `None` | must |
| FR-002 | WHEN multiple handlers match (subcommand hierarchy) THEN the system SHALL select the handler with the longest matching command name | must |
| FR-003 | WHEN `register()` is called with a duplicate name THEN the system SHALL panic with an informative message | must |
| FR-004 | WHEN `list()` is called THEN the system SHALL return `CommandInfo` for every registered handler in registration order | must |
| FR-005 | WHEN a handler returns `CommandOutput::Exit` THEN the system SHALL propagate this to the caller and the agent loop SHALL terminate | must |
| FR-006 | WHEN a handler returns `CommandOutput::Silent` THEN the system SHALL produce no user-facing output for that command | must |
| FR-007 | WHEN a handler is feature-gated THEN it SHALL set `feature_gate: Some("feature-name")` in its `CommandInfo` so `/help` can annotate it | should |
| FR-008 | WHEN `CommandRegistry::find_handler()` is called THEN it SHALL return the handler index and name without executing the handler | should |

---

## 4. Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Isolation | `zeph-commands` must not depend on `zeph-core` |
| NFR-002 | Performance | Dispatch is O(N) linear scan over < 40 handlers — no hash map needed |
| NFR-003 | Object Safety | `CommandHandler<Ctx>` must be object-safe; uses `Pin<Box<dyn Future>>` not `async fn` |
| NFR-004 | Thread Safety | All handlers must be `Send + Sync` for use in async contexts |
| NFR-005 | Safety | No `unsafe` code |

---

## 5. Data Model

| Entity | Description | Key Attributes |
|--------|-------------|----------------|
| `CommandRegistry<Ctx>` | Registry storing boxed handlers | `Vec<Box<dyn CommandHandler<Ctx>>>` |
| `CommandHandler<Ctx>` | Object-safe handler trait | `name()`, `description()`, `args_hint()`, `category()`, `feature_gate()`, `handle()` |
| `CommandContext` | Concrete dispatch context | Trait-object fields for each subsystem access trait |
| `CommandOutput` | Result of a dispatch | Variants: `Message(String)`, `Silent`, `Exit`, `Continue` |
| `CommandError` | Handler error | Wraps agent-level errors as `String` |
| `CommandInfo` | Static command metadata | `name`, `args`, `description`, `category`, `feature_gate` |
| `SlashCategory` | Display grouping | Variants: `Session`, `Configuration`, `Memory`, `Skills`, `Planning`, `Debugging`, `Integration`, `Advanced` |
| `ChannelSink` | Minimal async I/O trait | `async fn send(&mut self, msg: &str)` |
| `NullSink` | No-op sink for tests | Discards all messages |

---

## 6. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|-------------------|
| Input is "/" with no command name | `dispatch()` returns `None` |
| Input matches no handler | `dispatch()` returns `None` |
| Handler returns `Err(CommandError)` | Agent loop logs and reports to user; does not crash |
| Registry is empty | `dispatch()` always returns `None`; `list()` returns empty Vec |
| `/plan` and `/plan confirm` both registered, input is "/plan" | `/plan` handler selected (exact match, not prefix) |

---

## 7. Success Criteria

| ID | Metric | Target |
|----|--------|--------|
| SC-001 | Longest-match dispatch | Unit tests cover exact match, subcommand match, and no-match cases |
| SC-002 | Compile isolation | `cargo check -p zeph-commands` succeeds without `zeph-core` in the graph |
| SC-003 | Duplicate guard | Test confirms `register()` panics on duplicate name |

---

## 8. Agent Boundaries

### Always (without asking)
- Run `cargo nextest run -p zeph-commands` after changes
- Keep handlers object-safe (no generic `async fn` in trait)

### Ask First
- Adding a new `SlashCategory` variant (affects `/help` grouping and TUI autocomplete)
- Changing `CommandOutput` variants (affects all dispatch sites in `zeph-core`)
- Adding dependencies to `zeph-commands`

### Never
- Add a dependency on `zeph-core`
- Make `CommandHandler` non-object-safe
- Use `unsafe` blocks

---

## 11. Session Recap (`/recap`)

Issue #3136. `/recap` generates an on-demand session summary using the same `SessionDigest` pipeline used for auto-summaries at startup.

### Behavior

- Calls `Agent::build_recap()` which generates a digest from up to `recap.max_input_messages` most recent messages
- Output sent directly to the user channel via `ChannelSink`
- **Deduplication**: if an auto-recap was already shown at session resume and no new messages have been added since, `/recap` returns the cached digest without a new LLM inference call (`recap_is_duplicate()` check)

### Auto-Recap on Resume

When `recap.auto_on_resume = true` and the session has a cached digest from a prior session:
1. The agent shows the digest immediately after the session loads — before the first user turn
2. `auto_recap_shown` is set to `true` in `DigestRuntime`
3. Subsequent `/recap` calls within the same resumed session are deduplicated until new messages arrive

### Config

```toml
[recap]
enabled = true                # master switch; /recap always available when true
auto_on_resume = false        # show auto-summary on session resume
max_input_messages = 50       # max recent messages fed to the digest LLM
max_tokens = 512              # max tokens in the recap output
recap_provider = ""           # provider for recap LLM call; empty = primary provider
```

### Key Invariants

- `/recap` is always available when `recap.enabled = true` — no feature gate
- Deduplication check (`recap_is_duplicate()`) compares `current_non_system_message_count` at resume time against the count at auto-recap emit — they must match for dedup to fire
- NEVER deduplicate when new messages have arrived since auto-recap was shown
- `recap_provider` must resolve via the provider registry — NEVER hardcode a model in the recap path
- `build_recap()` is fallible — errors return `CommandError`, not panic

---

## 9. Open Questions

None.

---

## 10. See Also

- [[constitution]] — project principles
- [[002-agent-loop/spec]] — consumes `CommandRegistry` at dispatch time
- [[030-tui-slash-autocomplete/spec]] — reads `COMMANDS` for autocomplete suggestions
- [[MOC-specs]] — all specifications
