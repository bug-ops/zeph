# zeph-agent-tools Guide

Doom-loop detection (`doom_loop_hash`) consumed by the tool loop in `zeph-core`.

- Start with crate-local checks: `cargo build -p zeph-agent-tools`, `cargo nextest run -p zeph-agent-tools`, `cargo clippy -p zeph-agent-tools --all-targets -- -D warnings`.
- Architecture invariant: this crate MUST NOT depend on `zeph-core` or `zeph-channels`.
- Crate status: previously carried a sealed `AgentChannel` dispatcher-extraction trait (issue #3516, closed) with zero implementors anywhere in the workspace; removed as dead code (issue #6480). That dispatcher-extraction plan is abandoned, not deferred — reviving it requires a new tracking issue. Add a dependency only alongside the code that actually needs it.
- Doom-loop detection is agent-safety critical: any change to `doom_loop_hash` or its hashing inputs needs regression coverage so repeated tool calls are still detected.
