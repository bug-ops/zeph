# zeph-agent-context

[![Crates.io](https://img.shields.io/crates/v/zeph-agent-context)](https://crates.io/crates/zeph-agent-context)
[![docs.rs](https://img.shields.io/docsrs/zeph-agent-context)](https://docs.rs/zeph-agent-context)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](../../LICENSE)

Agent context-assembly service for the [Zeph](https://github.com/bug-ops/zeph) AI agent.

Provides `ContextService` — a stateless façade for all context operations: system prompt rebuilds, memory injection, conversation compaction, and summarization. Previously this logic lived directly on `Agent<C>` inside `zeph-core`; extracting it means editing context assembly does not trigger recompilation of the tool dispatcher (`zeph-agent-tools`) or the persistence layer (`zeph-agent-persistence`).

## Installation

```toml
[dependencies]
zeph-agent-context = { version = "0.20", workspace = true }
```

> [!IMPORTANT]
> Requires Rust 1.95 or later (Edition 2024). This crate does **not** depend on `zeph-core` — only on lower-level crates (`zeph-memory`, `zeph-llm`, `zeph-skills`, `zeph-context`, `zeph-sanitizer`, `zeph-config`, `zeph-common`).

## Usage

All methods on `ContextService` are stateless. State flows exclusively through explicit borrow-lens view parameters — structs of `&`/`&mut` references that `zeph-core`'s shim layer constructs from disjoint `Agent<C>` fields. The borrow checker proves field disjointness at the literal struct expressions in the shim.

### Rebuild system prompt

```rust,no_run
use zeph_agent_context::{ContextService, ContextAssemblyView, MessageWindowView, ProviderHandles};

let svc = ContextService::new();

// `window` and `view` are constructed by zeph-core's shim from Agent<C> fields.
svc.rebuild_system_prompt(
    query,
    &mut window,
    &mut view,
    &providers,
    &trust_gate,
    &status_sink,
).await;
```

### Prepare context (memory injection)

```rust,no_run
svc.prepare_context(query, &mut window, &mut view, &providers, &status_sink)
    .await
    .map_err(AgentError::context)?;
```

### Compaction

```rust,no_run
svc.maybe_compact(&mut summ, &providers, &status_sink).await?;
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
| `index` | off | `zeph-index` integration via `IndexAccess` in assembly views |

The `self-check` feature was consolidated as always-on in v0.20.x — retrieved-memory mirror types
compile unconditionally. Only `index` remains optional.

```toml
zeph-agent-context = { version = "0.20", workspace = true, features = ["index"] }
```

## License

MIT — see [LICENSE](../../LICENSE).
