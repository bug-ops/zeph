# zeph-agent-context

[![Crates.io](https://img.shields.io/crates/v/zeph-agent-context)](https://crates.io/crates/zeph-agent-context)
[![docs.rs](https://img.shields.io/docsrs/zeph-agent-context)](https://docs.rs/zeph-agent-context)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](../../LICENSE)
[![MSRV](https://img.shields.io/badge/MSRV-1.97-blue)](https://www.rust-lang.org)

Agent context-assembly service for the [Zeph](https://github.com/bug-ops/zeph) AI agent.

Provides `ContextService` — a stateless façade for context operations: memory injection, skill disambiguation, conversation compaction, and summarization. Previously this logic lived directly on `Agent<C>` inside `zeph-core`; extracting it means editing context assembly does not trigger recompilation of the tool dispatcher (`zeph-agent-tools`) or the persistence layer (`zeph-agent-persistence`).

> [!NOTE]
> System prompt rebuild (`rebuild_system_prompt`) stayed on `Agent<C>` in `zeph-core` — it was never migrated into a `ContextService` method, and an early dead stub of the same name was later removed from this crate.

## Installation

```toml
[dependencies]
zeph-agent-context = { version = "0.22", workspace = true }
```

> [!IMPORTANT]
> Requires Rust 1.97 or later (Edition 2024). This crate does **not** depend on `zeph-core` — only on lower-level crates (`zeph-memory`, `zeph-llm`, `zeph-skills`, `zeph-context`, `zeph-sanitizer`, `zeph-config`, `zeph-common`).

## Usage

All methods on `ContextService` are stateless. State flows exclusively through explicit borrow-lens view parameters — structs of `&`/`&mut` references that `zeph-core`'s shim layer constructs from disjoint `Agent<C>` fields. The borrow checker proves field disjointness at the literal struct expressions in the shim.

### Prepare context (memory injection)

```rust,no_run
use zeph_agent_context::ContextService;

let svc = ContextService::new();

// `window` and `view` are constructed by zeph-core's shim from Agent<C> fields.
let delta = svc.prepare_context(query, &mut window, &mut view).await?;
// `delta.code_context`, if present, is applied by the caller (zeph-core keeps
// `inject_code_context` on `Agent<C>` per the extraction scope decision).
```

### Compaction

```rust,no_run
// `status` implements the `StatusSink` trait so collected messages can be
// forwarded to the channel after the call returns.
svc.maybe_compact(&mut summ, &status).await?;
```

### Skill disambiguation

```rust,no_run
use zeph_agent_context::ContextService;

let svc = ContextService::new();
let chosen_order = svc.disambiguate_skills(query, &all_meta, &scored, &providers).await;
```

## Key Types

| Type | Purpose |
|---|---|
| `ContextService` | Stateless façade; zero-sized, all methods take `&self` |
| `ContextError` | Typed error enum (`thiserror`) for all fallible context operations |
| `MessageWindowView<'a>` | Borrow-lens over the conversation message buffer and deferred queues |
| `ContextAssemblyView<'a>` | Borrow-lens over all fields needed for `prepare_context` and `rebuild_system_prompt` |
| `ContextSummarizationView<'a>` | Borrow-lens over fields needed for compaction, scheduling, and pruning |
| `ProviderHandles` | Arc-cloned primary and embedding LLM provider handles |

`type_aware_compose::resolve_active_functional_types` resolves the MemGuard-inspired active
`FunctionalType` set (spec-004-16, #6086) that `prepare_context` uses to gate memory-fetcher
composition per turn — retrieval-only, no storage or write-path change; a byte-for-byte no-op
when `[memory.type_aware_compose]` is disabled (the default).

## Borrow-Lens Pattern

Views hold `&`/`&mut` references to field types from lower-level crates. No view embeds a whole `*State` aggregator from `zeph-core` — each field maps directly to a concrete type from `zeph-memory`, `zeph-skills`, `zeph-config`, etc.

```rust,no_run
// Constructed once per call site in zeph-core's shim; all borrows are disjoint.
let window = MessageWindowView {
    messages:                    &mut self.msg.messages,
    last_persisted_message_id:   &mut self.msg.last_persisted_message_id,
    deferred_db_hide_ids:        &mut self.msg.deferred_db_hide_ids,
    deferred_db_summaries:       &mut self.msg.deferred_db_summaries,
};
```

> [!NOTE]
> External callers cannot meaningfully construct views without access to `Agent<C>` internals, which acts as a soft seal without requiring a sealed trait.

## Features

| Feature | Default | Description |
|---|---|---|
| `sqlite` | on | SQLite backend forwarded to `zeph-memory`/`zeph-sanitizer`/`zeph-skills` |
| `postgres` | off | PostgreSQL backend forwarded to the same downstream crates |
| `index` | off | `zeph-index` integration via `IndexAccess` in assembly views |

The `self-check` feature was consolidated as always-on in v0.20.x — retrieved-memory mirror types
compile unconditionally. The backend features (`sqlite`/`postgres`) select the storage layer; `index` is the only capability toggle.

```toml
zeph-agent-context = { version = "0.22", workspace = true, features = ["index"] }
```

## License

Licensed under either of [MIT](../../LICENSE) or [Apache License, Version 2.0](../../LICENSE-APACHE) at your option.
