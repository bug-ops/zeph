# Crate Extraction — Epic #1973

## Background

Before epic #1973, `zeph-core` was a god crate: it owned the agent loop, configuration loading, secret resolution, content sanitization, experiment logic, subagent management, and task orchestration — all in a single crate. This made the code harder to reason about, slowed incremental compilation, and made it impossible to test subsystems in isolation.

Epic #1973 extracted six focused crates from `zeph-core` in five phases (Phase 1a through Phase 1e), each merged as an independent PR.

## Extraction Phases

| Phase | PR | Crate Extracted | What Moved |
|-------|----|-----------------|------------|
| 1a | #2006 | `zeph-config` | All configuration types, TOML loader, env overrides, migration helpers |
| 1b | #2006 | Config loaders | `loader.rs`, `env.rs`, `migrate.rs` split from monolithic config |
| 1c | #2007 | `zeph-vault` | `VaultProvider` trait, `EnvVaultProvider`, `AgeVaultProvider` |
| 1d | #2008 | `zeph-experiments` | Experiment engine, evaluator, benchmark datasets, hyperparameter search |
| 1e | #2009 | `zeph-sanitizer` | `ContentSanitizer`, PII filter, exfiltration guard, quarantine |

In addition, two crates were created to consolidate previously scattered logic:

- **`zeph-subagent`** — subagent spawning, grants, transcripts, and lifecycle hooks (previously spread across `zeph-core` and `zeph-a2a`)
- **`zeph-orchestration`** — DAG task graph, scheduler, planner, and router (previously in `zeph-core::orchestration`)

## Why Extract Crates?

### Faster Incremental Compilation

Cargo recompiles a crate when any of its source files change. A large `zeph-core` meant that touching any configuration struct or sanitizer type would trigger a full recompile of the entire agent core. Extracting focused crates ensures that a change to `zeph-config` only recompiles `zeph-config` and its downstream dependents — not the full graph.

### Testability in Isolation

Each extracted crate can be tested independently without instantiating the full agent stack. For example:

```bash
# Test only configuration loading — no LLM, no SQLite, no agent loop
cargo nextest run -p zeph-config

# Test only sanitization logic
cargo nextest run -p zeph-sanitizer

# Test only vault backends
cargo nextest run -p zeph-vault
```

### Clear Dependency Ownership

Before extraction, dependencies like `age` (for vault encryption) and `regex` (for injection detection) were mixed into `zeph-core`'s dependency tree. After extraction, each crate declares only the dependencies it actually needs, making the graph auditable at a glance.

### Layer Model

The extraction introduced an explicit layer model:

```
Layer 0: zeph-common       — primitives with no workspace deps
Layer 1: zeph-config, zeph-vault — configuration and secrets
Layer 2: zeph-llm, zeph-memory, zeph-tools, zeph-skills — domain crates
Layer 3: zeph-sanitizer, zeph-experiments, zeph-subagent, zeph-orchestration — agent subsystems
Layer 4: zeph-core          — agent loop, AppBuilder, context engineering
Layer 5: I/O and optional extensions
```

Each layer only depends on layers below it. This prevents circular dependencies and makes the architecture self-documenting.

## Backward Compatibility

`zeph-core` re-exports all public types from the extracted crates via `pub use` shims, so downstream code that imports from `zeph_core::config::Config` or `zeph_core::sanitizer::ContentSanitizer` continues to compile without changes. Consumers can migrate to importing directly from the extracted crates at their own pace.

## Crate Publication

| Crate | Published to crates.io | Notes |
|-------|----------------------|-------|
| `zeph-config` | Yes | `publish = true` |
| `zeph-vault` | Yes | `publish = true` |
| `zeph-orchestration` | Yes | `publish = true` |
| `zeph-experiments` | No | `publish = false`, internal-only |
| `zeph-sanitizer` | No | `publish = false`, internal-only |
| `zeph-subagent` | No | `publish = false`, internal-only |

## Further Reading

- [Crates Overview](crates-overview.md) — full workspace layout and dependency graph
- [zeph-config reference](https://docs.rs/zeph-config/latest/zeph_config/)
- [zeph-vault reference](https://docs.rs/zeph-vault/latest/zeph_vault/)
- [zeph-experiments reference](https://docs.rs/zeph-experiments/latest/zeph_experiments/)
- [zeph-sanitizer reference](https://docs.rs/zeph-sanitizer/latest/zeph_sanitizer/)
- [zeph-subagent reference](https://docs.rs/zeph-subagent/latest/zeph_subagent/)
- [zeph-orchestration reference](https://docs.rs/zeph-orchestration/latest/zeph_orchestration/)
