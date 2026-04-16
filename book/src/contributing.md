# Contributing

Thank you for considering contributing to Zeph.

## Getting Started

1. Fork the repository
2. Clone your fork and create a branch from `main`
3. Install Rust 1.94+ (Edition 2024 required, resolver 3)
4. Install [sccache](https://github.com/mozilla/sccache) for build caching (optional but recommended)
5. Run `cargo build` to verify the setup
6. Install [cargo-nextest](https://nexte.st/) for running tests

## Development

### Build

```bash
cargo build
```

### Test

```bash
# Run unit tests only (exclude integration tests)
cargo nextest run --workspace --lib --bins

# Run all tests including integration tests (requires Docker)
cargo nextest run --workspace --profile ci
```

**Nextest profiles** (`.config/nextest.toml`):
- `default`: Runs all tests (unit + integration)
- `ci`: CI environment, runs all tests with JUnit XML output for reporting

### Integration Tests

Integration tests use [testcontainers-rs](https://github.com/testcontainers/testcontainers-rs) to automatically spin up Docker containers for external services (Qdrant, etc.).

**Prerequisites:** Docker must be running on your machine.

```bash
# Run only integration tests
cargo nextest run --workspace --test '*integration*'

# Run unit tests only (skip integration tests)
cargo nextest run --workspace --lib --bins

# Run all tests
cargo nextest run --workspace
```

Integration test files are located in each crate's `tests/` directory and follow the `*_integration.rs` naming convention.

### Lint

```bash
cargo +nightly fmt --check
cargo clippy --all-targets
```

### Benchmarks

```bash
cargo bench -p zeph-memory --bench token_estimation
cargo bench -p zeph-skills --bench matcher
cargo bench -p zeph-core --bench context_building
```

### Coverage

```bash
cargo llvm-cov --all-features --workspace
```

## Workspace Structure

| Crate | Purpose |
|-------|---------|
| `zeph-common` | Shared primitives: `Secret`, `VaultError`, common types |
| `zeph-config` | Pure-data configuration types, TOML loader, env overrides, migration |
| `zeph-vault` | `VaultProvider` trait + env and age-encrypted backends |
| `zeph-db` | Database abstraction (SQLite + PostgreSQL) |
| `zeph-llm` | `LlmProvider` trait, Ollama + Claude + OpenAI + Gemini + Candle backends |
| `zeph-memory` | SQLite + Qdrant memory, semantic search, document loaders |
| `zeph-tools` | `ToolExecutor` trait, shell sandbox, file ops, web scraper |
| `zeph-skills` | SKILL.md parser, registry, embedding matcher, hot-reload |
| `zeph-index` | AST-based code indexing, semantic retrieval, repo map (always-on) |
| `zeph-sanitizer` | Content sanitization, PII filter, exfiltration guard |
| `zeph-experiments` | Autonomous experiment engine, LLM-as-judge evaluation |
| `zeph-subagent` | Sub-agent lifecycle, grants, transcripts, hooks |
| `zeph-orchestration` | DAG-based task orchestration, planner, router, aggregator |
| `zeph-core` | Agent loop, `AppBuilder` bootstrap, context builder, metrics |
| `zeph-channels` | Telegram, Discord, Slack adapters |
| `zeph-mcp` | MCP client via rmcp, multi-server lifecycle (optional) |
| `zeph-acp` | ACP server for IDE integration (optional) |
| `zeph-a2a` | A2A protocol client + server (optional) |
| `zeph-gateway` | HTTP webhook gateway (optional) |
| `zeph-scheduler` | Cron task scheduler with SQLite persistence (optional) |
| `zeph-tui` | ratatui TUI dashboard with real-time metrics (optional) |

## Spec-Driven Development

Zeph follows a spec-driven development process. **Code changes come after spec changes, not before.**

### Before writing any code

1. Read the relevant specification in `specs/` — every subsystem has a corresponding `spec.md`.
   Start with `specs/constitution.md` for project-wide invariants.
2. If your change affects an existing subsystem, open the matching spec and review the
   `## Key Invariants` and `NEVER` sections. These are hard constraints.
3. **Propose the spec change first.** Open a GitHub issue or discussion describing:
   - What you want to change and why
   - Which spec sections are affected
   - Whether any invariants need to be updated or explicitly overridden
4. Once the spec change is agreed upon, update the spec file and open a PR that includes
   both the spec update and the implementation together.
5. If no spec exists for the area you are changing, create one in `specs/<area>/spec.md`
   before writing code. Use the existing specs as a template.

This process ensures that architectural decisions are made deliberately and documented before
they become code — not reverse-engineered from a diff after the fact.

## Pull Requests

1. Create a feature branch: `feat/<scope>/<description>` or `fix/<scope>/<description>`
2. Keep changes focused — one logical change per PR
3. Add tests for new functionality
4. Ensure all checks pass: `cargo +nightly fmt`, `cargo clippy`, `cargo nextest run --lib --bins`
5. Write a clear PR description following the template
6. If the PR touches a specced subsystem, reference the relevant `specs/` file and confirm
   that the implementation is compliant with the current spec

## Commit Messages

- Use imperative mood: "Add feature" not "Added feature"
- Keep the first line under 72 characters
- Reference related issues when applicable

## Code Style

- Follow workspace clippy lints (pedantic enabled)
- Use `cargo +nightly fmt` for formatting
- Avoid unnecessary comments — code should be self-explanatory
- Comments are only for cognitively complex blocks

## License

By contributing, you agree that your contributions will be licensed under the [MIT License](https://github.com/bug-ops/zeph/blob/main/LICENSE).
