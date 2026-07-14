# zeph-orchestration Guide

Multi-model task orchestration: DAG decomposition, concurrent sub-agent execution, failure propagation, and result synthesis live here.

- Start with crate-local checks: `cargo build -p zeph-orchestration`, `cargo nextest run -p zeph-orchestration`, `cargo clippy -p zeph-orchestration --all-targets -- -D warnings`. The default feature set is `sqlite` only — `llm-planning` (planner, aggregator, verifier, plan_cache, adaptorch, ensemble) is opt-in, so add `--features llm-planning` to actually build/test/lint that code (CI closed this exact PR-gating gap for `zeph-plugins`' analogous `registry` feature in #6189 — the same gap applies here for local runs).
- Read `specs/009-orchestration/spec.md` before changing `PlanVerifier`, `DagScheduler`, or task dispatch; also see `specs/073-orch-ensemble-merge/spec.md` (verifier ensemble) and `specs/075-orchestration-node-control-parity/spec.md` (per-task `TimeoutPolicy`/`RecoveryAction`) for the invariants behind those subsystems.
- Multi-model: planner and synthesizer use LLMs — expose `planner_provider` and `synthesizer_provider` config fields referencing `[[llm.providers]]` by name; use the most capable model for planning and reasoning tasks.
- DAG execution must handle partial failure gracefully; do not silently drop sub-task errors.
- Verifier grounding is load-bearing: `verify()`/`verify_plan()` must run the deterministic `ground()` stage against the real `ToolUse`/`ToolResult` trace before trusting the LLM verify-provider's `complete` verdict — NEVER let a narrated-but-unexecuted claim pass verification ungrounded (#6286, #6299).
- `TaskNode::network_scope: Deny` must stay enforced at dispatch via `zeph_subagent::NetworkDenyToolExecutor` (wired in both the spawned and `RunInline` paths) — never let it regress to advisory-only (#6161); MCP-provided tools remain a known, documented gap (specs/069-threat-model).
- LLM serialization gate: changes to task decomposition or result synthesis structs require a live multi-turn session test before merge.
- If external behavior changes, update `crates/zeph-orchestration/README.md` and the relevant orchestration docs.
