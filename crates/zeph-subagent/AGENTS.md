# zeph-subagent Guide

Sub-agent spawning, capability grants, transcript management, and lifecycle hooks live here.

- Start with crate-local checks: `cargo build -p zeph-subagent`, `cargo nextest run -p zeph-subagent`, `cargo clippy -p zeph-subagent --all-targets -- -D warnings`.
- Read `specs/044-subagent-lifecycle/spec.md` before changing spawn, grant, or constraint-propagation logic; see `specs/064-durable-execution/spec.md` for the durable-resume contract below.
- Treat grant scoping and capability propagation as security-sensitive: a sub-agent must never inherit more permissions than explicitly granted. Granted secrets must be TTL-rechecked before every tool call (not only at delivery), and secret-request lookups must key on the specific task id — never pop-then-filter a shared queue, which can drop a concurrent sibling's pending request (#6123).
- `filter.rs` hosts `FilteredExecutor`, `PlanModeExecutor`, and `NetworkDenyToolExecutor` — all `ErasedToolExecutor` decorators. `ToolExecutor`/`ErasedToolExecutor` have no default bodies for `requires_confirmation`, `execute_tool_call_confirmed`, the checkpoint trio, or `is_tool_speculatable` (#6067 breaking change): every wrapper here must explicitly forward each of these to its inner executor, and every new wrapper needs regression coverage per forwarded method — this exact silent-fallback-to-default defect has recurred 5+ times.
- On a resumed durable execution, check for an already-resolved child promise (`try_replay_durable_subagent`) before spawning a new child — never blindly respawn a subagent whose result was already journaled, which would duplicate LLM/tool side effects (#6014).
- Every fallible pre-loop setup step inside `spawn()`'s task closure must send a terminal `Failed` status (via `spawn_oneshot_classified`), not leave the status channel in its initial `Submitted` state — otherwise `poll_subagents()` can never reclaim the `max_concurrent` slot, leaking a permanent zombie task (#6283).
- LLM serialization gate: changes to sub-agent message passing or transcript structs require a live session test with actual sub-agent spawning before merge.
- TUI: sub-agent spawning and active tasks must surface visible spinner/status feedback.
- If external behavior changes, update `crates/zeph-subagent/README.md` and the relevant sub-agent docs.
