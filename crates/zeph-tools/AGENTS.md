# zeph-tools Guide

Tool execution, shell permissions, filtering, scraping, and audit behavior live here.

- Start with crate-local checks: `cargo build -p zeph-tools`, `cargo nextest run -p zeph-tools`, `cargo clippy -p zeph-tools --all-targets -- -D warnings`.
- Treat shell execution, permissions, trust gating, network access, and audit logging as security-sensitive behavior.
- Prefer explicit safeguards over implicit defaults; regressions here can affect the whole agent.
- `ToolExecutor`/`ErasedToolExecutor` have no permissive default method bodies (removed in #6067 after 5 recurring silent-forwarding bugs) — any new wrapper impl must explicitly forward every method. Use the `tool_executor_forward!`/`erased_tool_executor_forward!` macros in `executor_delegate.rs`, and prefer the `DynExecutor`/`ErasedToolExecutor` adapter over a hand-written `impl ToolExecutor` wrapper.
- This crate depends on `zeph-llm` for `ImageData` (`ToolOutput.media: Vec<ImageData>`, spec-072 MCP image passthrough plumbing, #6238). Media is ephemeral-only — never persisted — and `ImageData`'s `Debug` impl is redacted; do not add a `Debug`/log path that exposes raw image bytes.
- If external behavior changes, update `crates/zeph-tools/README.md` and the relevant tools/security docs.
