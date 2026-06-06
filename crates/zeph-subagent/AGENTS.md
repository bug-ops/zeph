# zeph-subagent Guide

Sub-agent spawning, capability grants, transcript management, and lifecycle hooks live here.

- Start with crate-local checks: `cargo build -p zeph-subagent`, `cargo nextest run -p zeph-subagent`, `cargo clippy -p zeph-subagent --all-targets -- -D warnings`.
- Treat grant scoping and capability propagation as security-sensitive: a sub-agent must never inherit more permissions than explicitly granted.
- LLM serialization gate: changes to sub-agent message passing or transcript structs require a live session test with actual sub-agent spawning before merge.
- TUI: sub-agent spawning and active tasks must surface visible spinner/status feedback.
- If external behavior changes, update `crates/zeph-subagent/README.md` and the relevant sub-agent docs.
