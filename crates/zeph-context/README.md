# zeph-context

[![Crates.io](https://img.shields.io/crates/v/zeph-context)](https://crates.io/crates/zeph-context)
[![docs.rs](https://img.shields.io/docsrs/zeph-context)](https://docs.rs/zeph-context)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/License-MIT%20OR%20Apache--2.0-yellow.svg)](../../LICENSE)
[![MSRV](https://img.shields.io/badge/MSRV-1.96-blue)](https://www.rust-lang.org)

Context budget, lifecycle management, compaction strategy, and stateless context assembler for the
[Zeph](https://github.com/bug-ops/zeph) AI agent.

This crate contains the **stateless and data-only** parts of context management extracted from
`zeph-core`. It has no dependency on `zeph-core` — callers implement the `IndexAccess` trait and
populate `ContextMemoryView` before each assembly pass.

## Modules

- `budget` — `ContextBudget` and `BudgetAllocation` for token budget calculation
- `manager` — `ContextManager` state machine and `CompactionState` lifecycle tracking
- `assembler` — `ContextAssembler` parallel fetch coordinator; classifies each slot into a `TypedPage`, enforces per-type fidelity invariants at compaction boundaries, and emits a `CompactionAuditSink` record per compacted page
- `input` — `ContextAssemblyInput`, `ContextMemoryView`, `IndexAccess` trait
- `slot` — `ContextSlot`, `CompactionOutcome`, message-chunking helpers
- `typed_page` — `TypedPage` (BLAKE3 content-hash id), `PageType` enum (`ToolOutput`, `ConversationTurn`, `MemoryExcerpt`, `SystemContext`), per-type `PageInvariant` implementations, `InvariantRegistry`, and the `CompactionAuditSink` bounded async mpsc channel that records one audit entry per compacted page
- `summarization` — pure prompt-building helpers for context compaction
- `tool_result_compress` — per-result and batch-level token-budget enforcement for tool outputs
- `compression_feedback` — context-loss detection and failure classification
- `microcompact` — low-value tool detection helpers for time-based microcompact
- `fidelity` — heuristic fidelity scorer for Context-Adaptive Memory (CAM)
- `turn_context` — `TurnId` and `TurnContext` per-turn carrier
- `error` — `AssemblerError`

## ClawVM typed-page compaction

`ContextAssembler::gather()` classifies every context slot into a `TypedPage` keyed by a BLAKE3 content hash. Each `PageType` carries a `PageInvariant` that declares the minimum-fidelity guarantee for compaction:

| PageType | Invariant |
|----------|-----------|
| `ToolOutput` | Preserve tool name, return code, and first/last N bytes of output |
| `ConversationTurn` | Preserve role, first sentence, and any code blocks |
| `MemoryExcerpt` | Preserve entity names and relationship predicates |
| `SystemContext` | Never compact — always injected verbatim |

At every compaction boundary the invariant is checked before the LLM compaction call. If the post-compaction result would violate the invariant the compaction is rejected and the original content is retained.

## License

Licensed under either of [MIT](../../LICENSE) or [Apache License, Version 2.0](../../LICENSE-APACHE) at your option.
