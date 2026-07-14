# Repository Guidelines

## Project Structure & Module Organization
Zeph is a Rust workspace (`Cargo.toml`) with the CLI entrypoint in `src/` and domain crates in `crates/` (for example: `zeph-core`, `zeph-llm`, `zeph-memory`, `zeph-tools`, `zeph-tui`).
Top-level integration tests live in `tests/`, while crate-specific tests and benches live under each crate’s `tests/` and `benches/` directories. Documentation sources are in `book/src/` (mdBook), runtime defaults are in `config/default.toml`, and container assets are in `docker/`.
Do not create git worktrees inside this repository. Create separate worktrees only under the sibling directory `../worktrees/`.

## Specifications (MANDATORY)

All feature and system specifications live in `specs/`. **Compliance is non-negotiable.**

- **Before implementing any feature**: read the relevant spec in `specs/` and the system
  invariants in `specs/001-system-invariants/spec.md`.
- **Before modifying any subsystem**: read the corresponding spec document. The `## Key Invariants`
  and `NEVER` sections define hard constraints — violating them requires an explicit architectural
  decision, not just a code review.
- **Index**: `specs/README.md` — maps every subsystem to its spec file.
- **Constitution**: `specs/constitution.md` — project-wide non-negotiable rules that apply to every change.
- If a spec is missing or outdated for the area you are changing, create or update it before writing code.

## Agent Operating Notes

`AGENTS.md` is the primary instruction file for all coding agents (Codex, Copilot, etc.). `.claude/CLAUDE.md` and `.claude/rules/*` are Claude-specific or supplemental context — not the primary source of operating rules.
Path-specific instructions for GitHub Copilot live in `.github/instructions/*.instructions.md` with `applyTo` frontmatter.

- Use `cargo nextest run` as the default test runner.
- Keep Rust changes compatible with Edition 2024 and MSRV `1.97`.
- Prefer zero-warning `clippy`; avoid `unwrap`/`expect` in production code when proper error propagation is possible.
- Any user-facing change must update relevant docs, config defaults, and `CHANGELOG.md` (`[Unreleased]` section).
- For new functionality, provide all integration points: config section, CLI subcommand/argument, TUI command palette entry, `--init` wizard, `--migrate-config` migration step, live testing playbook in `.local/testing/playbooks/`, and coverage row in `.local/testing/coverage-status.md`.
- In the TUI, every background operation (LLM inference, memory search, tool execution, MCP connection, etc.) must have a visible spinner with a short, descriptive status message.

## Multi-Model Design

All new functionality must be designed for multi-model operation. Match models to task complexity:

- **Simple tasks** (entity extraction, embeddings, classification, routing): fast, cheap models (e.g. `gpt-4o-mini`, `qwen3:8b`).
- **Medium tasks** (summarization, compaction, skill matching): mid-tier models.
- **Complex tasks** (planning, code generation, multi-step reasoning): most capable models (e.g. `claude-opus-4`).

All providers declared once in `[[llm.providers]]` with a `name` field. Subsystems reference a provider by name via a single `*_provider` field — no inline provider config duplication. When the field is empty, fall back to the default provider. Never hardcode a model name.

## Secrets & Vault

All secrets and API keys are stored exclusively in the Zeph age vault. Never use environment variables, `.env` files, or shell profiles for secrets. All `ZEPH_*` keys are resolved automatically from the age vault at startup. To read a key for external scripts: `cargo run --features full -- vault get <KEY_NAME>`.

## Continuous Improvement Protocol
- Continuous-improvement sessions are read-only: use them for live manual testing, anomaly detection, research, dependency review, and issue triage; do not make code changes in those sessions.
- Do not treat unit/integration tests as sufficient validation for user-facing or agent-loop changes. Run real end-to-end agent sessions with real LLM/tool execution via `cargo run --features full -- --config .local/config/testing.toml` and use `--tui` or `RUST_LOG=debug` when needed.
- For continuous-improvement and ACP/IDE test runs, override runtime artifact paths in the active config to absolute temporary locations so logs, SQLite state, debug dumps, audit files, and other ephemeral outputs land under a dedicated temp directory instead of polluting the default user data directories. Prefer a per-session root such as `/tmp/zeph-ci/<timestamp>/`.
- At minimum, override `logging.file`, `memory.sqlite_path`, `debug.output_dir`, and any other runtime-writing paths used by the scenario under test to absolute temp paths before launching the agent.
- After each live test session, review the resolved debug dump and log directories from the active config, plus metrics and agent responses, for correctness, regressions, retries, timeouts, hallucinations, and tool-use quality.
- Treat the configured runtime artifact directory as the primary evidence location for live runs: request/response payloads in the configured debug dump directory and runtime traces in the configured log directory should be checked before drawing conclusions from terminal output alone.
- For GonkaGate continuous-improvement runs, use `.local/config/testing-gonkagate.toml`. It must resolve `ZEPH_COMPATIBLE_GONKAGATE_API_KEY` from the age vault only, target `https://api.gonkagate.com/v1`, use `Qwen/Qwen3-235B-A22B-Instruct-2507-FP8`, and keep logs/debug/SQLite/audit artifacts under `/tmp/zeph-ci/gonkagate`.
- Record every session in `.local/testing/journal.md`, including positive confirmations, failures, scope, config used, and component status. When behavior changes, reset the affected component to untested until re-verified.
- When you find an anomaly, reproduce it, classify severity, and file a GitHub issue with clear repro steps and expected vs actual behavior. Fixes must happen later in a dedicated implementation session, not during the continuous-improvement pass.
- Testing is a hard gate: when many components are untested or a batch of core changes landed, prioritize live validation before research, dependency updates, or new feature work.
- Keep the test environment clean when relevant: clear the temporary runtime artifact directory, test DB state, audit logs, session logs, and overflow files before comprehensive runs; do not delete `.local/testing/`.
- Maintain the testing knowledge base in `.local/testing/`: `journal.md`, `process-notes.md`, `playbooks/`, and `regressions.md`. Add regressions for newly found bugs and capture process improvements after each session.
- Research new agent/testing techniques and monitor dependency health (`cargo outdated --workspace`, `cargo deny check advisories`) as part of the cycle, but only after current critical functionality has been live-tested.

## Build, Test, and Development Commands
- `cargo build --workspace --features full`: Build the full workspace feature set used in CI.
- `cargo run -- --help`: Run the CLI locally and list commands.
- `cargo nextest run --workspace --lib --bins`: Fast unit/binary test pass.
- `cargo nextest run --config-file .github/nextest.toml --workspace --profile ci --features full`: Full suite with the CI config and full feature set (Docker required).
- `cargo +nightly fmt --check`: Verify formatting.
- `cargo clippy --profile ci --workspace --features full -- -D warnings`: Run CI-equivalent lint checks with the full feature set.
- `cargo llvm-cov --all-features --workspace`: Generate coverage locally.

## Coding Style & Naming Conventions
Use Rust 2024 edition and MSRV `1.97`. Follow `rustfmt` defaults (4-space indentation) and keep Clippy warnings at zero where practical.
Use `snake_case` for functions/modules/files, `PascalCase` for types/traits, and `SCREAMING_SNAKE_CASE` for constants. Prefer small modules with explicit responsibilities; keep public APIs in `lib.rs` minimal and re-export intentionally.

## Testing Guidelines
Use `cargo-nextest` as the default runner. Name integration tests with `*_integration.rs` and keep deterministic unit tests close to implementation.
For features touching external systems (for example Qdrant), use container-backed tests and document required services. Add regression tests for every bug fix.

## Commit & Pull Request Guidelines
Git history follows Conventional Commits with scopes, e.g. `feat(zeph-tools): ...`, `fix: ...`, `ci: ...`.
Keep subject lines imperative and concise, and reference issues/PRs when relevant (`(#736)`).
Before every commit, run the full validation suite against the CI feature set (`--features full` where supported) and do not commit if any step fails:
- `cargo +nightly fmt --check`
- `cargo clippy --profile ci --workspace --features full -- -D warnings`
- `cargo nextest run --config-file .github/nextest.toml --workspace --profile ci --features full`
- `cargo check --workspace --features full`
- `cargo build --workspace --features full`
PRs should include: a clear problem statement, implementation summary, test evidence (commands + results), and docs/config updates when behavior changes.

## Security & Configuration Tips
Do not commit secrets. All secrets must go through the Zeph age vault (`zeph vault` commands) — never environment variables or `.env` files. Review `SECURITY.md` before changing tool execution, sandboxing, or permission flows.
