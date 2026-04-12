# zeph-context

Context budget, lifecycle management, compaction strategy, and stateless context assembler for the
[Zeph](https://github.com/bug-ops/zeph) AI agent.

This crate contains the **stateless and data-only** parts of context management extracted from
`zeph-core`. It has no dependency on `zeph-core` — callers implement the `IndexAccess` trait and
populate `ContextMemoryView` before each assembly pass.

## Modules

- `budget` — `ContextBudget` and `BudgetAllocation` for token budget calculation
- `manager` — `ContextManager` state machine and `CompactionState` lifecycle tracking
- `assembler` — `ContextAssembler` parallel fetch coordinator
- `input` — `ContextAssemblyInput`, `ContextMemoryView`, `IndexAccess` trait
- `slot` — `ContextSlot`, `CompactionOutcome`, message-chunking helpers
- `error` — `ContextError`

## License

MIT OR Apache-2.0
