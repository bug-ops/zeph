# zeph-agent-tools Guide

Tool-dispatch primitives consumed by the tool loop in `zeph-core`: the sealed `AgentChannel` trait, borrowed event carriers (`ToolEventStart`, `ToolEventOutput`), and doom-loop detection (`doom_loop_hash`).

- Start with crate-local checks: `cargo build -p zeph-agent-tools`, `cargo nextest run -p zeph-agent-tools`, `cargo clippy -p zeph-agent-tools --all-targets -- -D warnings`.
- Read `specs/006-tools/spec.md` before changing the dispatch contract or tool-result handling.
- Architecture invariant: this crate MUST NOT depend on `zeph-core` or `zeph-channels`. `AgentChannel` is a minimal, sealed trait specifically to avoid the circular dependency that `zeph-core::channel::Channel` would create — never break this by adding those deps.
- `AgentChannel` is sealed via the `Sealed` trait: external implementations are forbidden by design. `zeph-core` implements it through its local `AgentChannelView<'a, C>` adapter.
- Crate status: Phase-2 scaffolding (issue #3516). Full `ToolDispatcher` extraction from `zeph-core` is a follow-up — keep changes minimal and aligned with that direction rather than adding speculative surface.
- Doom-loop detection is agent-safety critical: any change to `doom_loop_hash` or its hashing inputs needs regression coverage so repeated tool calls are still detected.
- LLM serialization gate: once tool dispatch / batch tool-result processing is extracted here, changes to those paths require a live session test with a real tool call before merge.
