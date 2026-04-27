# zeph-agent-persistence

[![Crates.io](https://img.shields.io/crates/v/zeph-agent-persistence)](https://crates.io/crates/zeph-agent-persistence)
[![docs.rs](https://img.shields.io/docsrs/zeph-agent-persistence)](https://docs.rs/zeph-agent-persistence)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](../../LICENSE)

Agent persistence service for Zeph: loads conversation history from and writes messages to the
`SemanticMemory` backend (SQLite + Qdrant), with tool-pair sanitization and embedding decisions.

## Key types

| Type | Description |
|------|-------------|
| `PersistenceService` | Stateless façade; namespace for `load_history` and `persist_message` |
| `PersistMessageRequest` | Fully-owned request value carrying role, content, parts, and injection flags |
| `PersistMessageOutcome` | Result of a single persist call: DB ID, embedded flag, bytes written |
| `LoadHistoryOutcome` | Counts from a history load: messages, orphans removed, SQLite/Qdrant totals |
| `MemoryPersistenceView<'a>` | Borrow-lens over the agent's memory state (passed by `zeph-core`) |
| `SecurityView<'a>` | Read-only borrow-lens over security state (exfiltration guard flag) |
| `MetricsView<'a>` | Mutable borrow-lens over metrics counters |

## Usage

```rust,no_run
use zeph_agent_persistence::{
    PersistenceService,
    PersistMessageRequest,
    state::{MemoryPersistenceView, MetricsView, SecurityView},
};
use zeph_llm::provider::Role;

async fn example(
    memory_view: &mut MemoryPersistenceView<'_>,
    security: &SecurityView<'_>,
    config: &zeph_config::Config,
    metrics: &mut MetricsView<'_>,
) {
    let svc = PersistenceService::new();

    let req = PersistMessageRequest::from_borrowed(
        Role::Assistant,
        "Hello, world!",
        &[],
        false, // no injection flags
    );

    let mut last_id = None;
    let outcome = svc
        .persist_message(req, &mut last_id, memory_view, security, config, metrics)
        .await;

    if let Some(id) = outcome.message_id {
        println!("persisted as DB row {id}, embedded={}", outcome.embedded);
    }
}
```

## Architecture

`zeph-agent-persistence` depends on `zeph-memory`, `zeph-llm`, `zeph-context`, `zeph-config`,
and `zeph-common`. It does **not** depend on `zeph-core`. This is the core invariant that keeps
the persistence and tool-dispatch subsystems independently evolvable.

`zeph-core` depends on this crate and constructs the borrow-lens views (`MemoryPersistenceView`,
`SecurityView`, `MetricsView`) from disjoint field projections of `Agent<C>`, then delegates to
`PersistenceService` methods.

## License

MIT
