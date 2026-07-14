# zeph-agent-tools Guide

Tool-dispatch primitives consumed by the tool loop in `zeph-core`: the sealed `AgentChannel` trait, borrowed event carriers (`ToolEventStart`, `ToolEventOutput`), and doom-loop detection (`doom_loop_hash`).

- Start with crate-local checks: `cargo build -p zeph-agent-tools`, `cargo nextest run -p zeph-agent-tools`, `cargo clippy -p zeph-agent-tools --all-targets -- -D warnings`.
- Read `specs/006-tools/spec.md` before changing the dispatch contract or tool-result handling.
- Architecture invariant: this crate MUST NOT depend on `zeph-core` or `zeph-channels`. `AgentChannel` is a minimal, sealed trait specifically to avoid the circular dependency that `zeph-core::channel::Channel` would create — never break this by adding those deps.
- `AgentChannel` is sealed via the `Sealed` trait: external implementations are forbidden by design. `zeph-core` implements it through its local `AgentChannelView<'a, C>` adapter.
- Crate status: Phase-2 scaffolding (issue #3516, closed). The `AgentChannel` trait and borrowed event carriers are complete, but no `zeph-core` adapter implements them and no `ToolDispatcher` extraction has landed or is in flight — that plan was abandoned, not deferred; re-opening it requires a new tracking issue. Per #6222/#6084, the crate now declares only the 3 `zeph-*` deps it actually uses (`zeph-common`, `zeph-llm`, `zeph-tools`) — do not re-add `zeph-agent-persistence`/`zeph-config`/`zeph-context`/`zeph-mcp`/`zeph-orchestration`/`zeph-sanitizer`/`zeph-skills` speculatively; add a dependency only alongside the code that actually needs it.
- Doom-loop detection is agent-safety critical: any change to `doom_loop_hash` or its hashing inputs needs regression coverage so repeated tool calls are still detected.
- LLM serialization gate: once tool dispatch / batch tool-result processing is extracted here, changes to those paths require a live session test with a real tool call before merge.
